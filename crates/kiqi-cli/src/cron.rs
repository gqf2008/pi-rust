//! `.ki/cron.json` 定时任务：配置解析、cron 表达式与宿主侧调度核心。
//!
//! 本模块只做三件事：把 `.ki/cron.json` 解析成 [`CronJob`]、把 5 字段 cron
//! 表达式（以及 `@hourly` 等宏）解析成 [`Schedule`]、用一个可注入时钟的
//! [`CronCore`] 判断“这一分钟该不该触发”。真正把提示词交给 agent、或执行
//! 命令，由调用方实现 [`CronSink`] 完成——本模块不碰任何执行器。
//!
//! 产品约定：
//!
//! - 任务只在会话打开时跳动；错过的分钟直接跳过，从不补跑。
//! - 同一进程内，同一 `(任务名, 分钟)` 最多触发一次。
//! - 配置文件按 mtime 变化重载；一次重载只发一条诊断，合法任务继续跑。
//! - 一个任务要么是 `prompt`（原文作为 user 消息注入），要么是 `command`
//!   （在会话工作目录执行，stdout+stderr 合并后交给 agent），二者恰有其一。
//!
//! 日期语义沿用 Vixie cron：日（day-of-month）与星期（day-of-week）都受限
//! 时取“或”，否则取“与”。星期 0 与 7 都表示周日。

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, TimeZone, Timelike,
};
use serde_json::Value;

/// 配置文件相对会话工作目录的路径。
pub const CRON_CONFIG_RELATIVE: &str = ".ki/cron.json";

/// `timeoutSecs` 缺省值。
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// `timeoutSecs` 上限。
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// 配置重载的轮询上限：即使下一个任务在很久以后，线程也最多睡这么久，
/// 以便及时发现 mtime 变化。这是一次普通定时等待，不是忙等。
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// `next_due` 的搜索上限（约 9 年）。覆盖 2 月 29 日 + 星期组合的最坏情况；
/// 超过仍无解（如 `0 0 30 2 *`）则返回 `None`，绝不无限循环。
const MAX_LOOKAHEAD_DAYS: i64 = 366 * 9;

/// 会话工作目录下的配置文件路径。
pub fn config_path(cwd: &Path) -> PathBuf {
    cwd.join(CRON_CONFIG_RELATIVE)
}

/// 任务被触发后要做的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobAction {
    /// 原文作为一条 user 消息注入。
    Prompt(String),
    /// 在会话工作目录执行；stdout 与 stderr 合并后交给 agent。
    Command(String),
}

/// 一个合法任务。调度器只负责判定到期并把它交给 [`CronSink`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronJob {
    pub name: String,
    pub schedule: Schedule,
    pub action: JobAction,
    pub enabled: bool,
    pub timeout_secs: u64,
}

/// 单条配置问题。`job`/`field` 为 `None` 表示顶层或结构性问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub job: Option<String>,
    pub field: Option<String>,
    pub message: String,
}

impl ConfigError {
    fn top(message: impl Into<String>) -> Self {
        Self {
            job: None,
            field: None,
            message: message.into(),
        }
    }

    fn job_only(job: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            job: Some(job.into()),
            field: None,
            message: message.into(),
        }
    }

    fn job(job: impl Into<String>, field: &str, message: impl Into<String>) -> Self {
        Self {
            job: Some(job.into()),
            field: Some(field.to_string()),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.job, &self.field) {
            (Some(job), Some(field)) => {
                write!(f, "任务 `{job}` 的 `{field}` 字段：{}", self.message)
            }
            (Some(job), None) => write!(f, "任务 `{job}`：{}", self.message),
            (None, Some(field)) => write!(f, "`{field}` 字段：{}", self.message),
            (None, None) => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

/// 一次配置解析的结果：合法任务 + 每条问题。问题不会吞掉合法任务。
#[derive(Debug, Clone, Default)]
pub struct ConfigLoad {
    pub jobs: Vec<CronJob>,
    pub errors: Vec<ConfigError>,
}

impl ConfigLoad {
    /// 没有任何问题。
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// 解析 `.ki/cron.json` 的文本。接受 `{"jobs":[...]}` 与裸数组两种形状。
///
/// 未知字段是错误（fail loud）；非法任务逐条报告并丢弃，合法任务保留。
pub fn parse_config(raw: &str) -> ConfigLoad {
    let mut out = ConfigLoad::default();
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(err) => {
            out.errors
                .push(ConfigError::top(format!("JSON 无效：{err}")));
            return out;
        }
    };

    let items = match value {
        Value::Array(items) => items,
        Value::Object(mut object) => {
            let jobs = object.remove("jobs");
            for key in object.keys() {
                out.errors
                    .push(ConfigError::top(format!("未知顶层字段 `{key}`")));
            }
            match jobs {
                Some(Value::Array(items)) => items,
                Some(other) => {
                    out.errors.push(ConfigError::top(format!(
                        "`jobs` 必须是数组，实际是{}",
                        json_type(&other)
                    )));
                    return out;
                }
                None => {
                    out.errors.push(ConfigError::top("缺少 `jobs` 数组"));
                    return out;
                }
            }
        }
        other => {
            out.errors.push(ConfigError::top(format!(
                "顶层必须是对象或数组，实际是{}",
                json_type(&other)
            )));
            return out;
        }
    };

    let mut seen = HashSet::new();
    for (index, item) in items.into_iter().enumerate() {
        match parse_job(index, item, &mut seen) {
            Ok(job) => out.jobs.push(job),
            Err(err) => out.errors.push(err),
        }
    }
    out
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}

const JOB_FIELDS: &[&str] = &[
    "name",
    "schedule",
    "prompt",
    "command",
    "enabled",
    "timeoutSecs",
];

fn parse_job(
    index: usize,
    value: Value,
    seen: &mut HashSet<String>,
) -> Result<CronJob, ConfigError> {
    let object = match value {
        Value::Object(object) => object,
        other => {
            return Err(ConfigError::job_only(
                format!("#{index}"),
                format!("任务必须是 JSON 对象，实际是{}", json_type(&other)),
            ));
        }
    };

    let labelled_name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("#{index}"));

    for key in object.keys() {
        if !JOB_FIELDS.contains(&key.as_str()) {
            return Err(ConfigError::job(&labelled_name, key, "未知字段"));
        }
    }

    let name = match object.get("name") {
        Some(Value::String(name)) if !name.trim().is_empty() => name.clone(),
        Some(Value::String(_)) => {
            return Err(ConfigError::job(&labelled_name, "name", "不能为空"));
        }
        Some(other) => {
            return Err(ConfigError::job(
                &labelled_name,
                "name",
                format!("必须是字符串，实际是{}", json_type(other)),
            ));
        }
        None => {
            return Err(ConfigError::job(&labelled_name, "name", "缺少该字段"));
        }
    };
    let schedule_spec = match object.get("schedule") {
        Some(Value::String(spec)) => spec,
        Some(other) => {
            return Err(ConfigError::job(
                &name,
                "schedule",
                format!("必须是字符串，实际是{}", json_type(other)),
            ));
        }
        None => {
            return Err(ConfigError::job(&name, "schedule", "缺少该字段"));
        }
    };
    let schedule = Schedule::parse(schedule_spec)
        .map_err(|err| ConfigError::job(&name, "schedule", err.to_string()))?;

    let action = match (object.get("prompt"), object.get("command")) {
        (Some(_), Some(_)) => {
            return Err(ConfigError::job(
                &name,
                "prompt",
                "`prompt` 与 `command` 只能有其一",
            ));
        }
        (None, None) => {
            return Err(ConfigError::job(
                &name,
                "prompt",
                "必须提供 `prompt` 或 `command`",
            ));
        }
        (Some(Value::String(prompt)), None) => JobAction::Prompt(prompt.clone()),
        (Some(other), None) => {
            return Err(ConfigError::job(
                &name,
                "prompt",
                format!("必须是字符串，实际是{}", json_type(other)),
            ));
        }
        (None, Some(Value::String(command))) => JobAction::Command(command.clone()),
        (None, Some(other)) => {
            return Err(ConfigError::job(
                &name,
                "command",
                format!("必须是字符串，实际是{}", json_type(other)),
            ));
        }
    };

    let enabled = match object.get("enabled") {
        None => true,
        Some(Value::Bool(enabled)) => *enabled,
        Some(other) => {
            return Err(ConfigError::job(
                &name,
                "enabled",
                format!("必须是布尔值，实际是{}", json_type(other)),
            ));
        }
    };

    let timeout_secs = match object.get("timeoutSecs") {
        None => DEFAULT_TIMEOUT_SECS,
        Some(Value::Number(number)) => match number.as_u64() {
            Some(secs) if (1..=MAX_TIMEOUT_SECS).contains(&secs) => secs,
            Some(secs) => {
                return Err(ConfigError::job(
                    &name,
                    "timeoutSecs",
                    format!("必须在 1..={MAX_TIMEOUT_SECS} 之间，实际是 {secs}"),
                ));
            }
            None => {
                return Err(ConfigError::job(&name, "timeoutSecs", "必须是非负整数秒"));
            }
        },
        Some(other) => {
            return Err(ConfigError::job(
                &name,
                "timeoutSecs",
                format!("必须是整数秒，实际是{}", json_type(other)),
            ));
        }
    };

    // 只有完全合法的任务才占用名字：前面被丢弃的同名任务不会连累后来者。
    if !seen.insert(name.clone()) {
        return Err(ConfigError::job(&name, "name", "与其他任务重名"));
    }

    Ok(CronJob {
        name,
        schedule,
        action,
        enabled,
        timeout_secs,
    })
}

/// 一个字段的解析结果：命中集合 + 是否受限（供日/星期“或”语义判断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Field {
    mask: u64,
    restricted: bool,
}

impl Field {
    fn contains(self, value: u32) -> bool {
        self.mask & (1u64 << value) != 0
    }
}

/// cron 表达式解析错误。`field` 是分钟/小时等字段名，便于精确定位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleError {
    pub field: &'static str,
    pub message: String,
}

impl ScheduleError {
    fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.field, self.message)
    }
}

impl std::error::Error for ScheduleError {}

/// 5 字段 cron 表达式（本地时间）+ `@hourly`/`@daily`/`@weekly`/`@monthly`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minutes: Field,
    hours: Field,
    days_of_month: Field,
    months: Field,
    days_of_week: Field,
}

impl Schedule {
    /// 解析一条表达式。字段顺序：分 时 日 月 星期。
    pub fn parse(spec: &str) -> Result<Self, ScheduleError> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(ScheduleError::new("schedule", "不能为空"));
        }

        let expanded;
        let text = if spec.starts_with('@') {
            expanded = match spec.to_ascii_lowercase().as_str() {
                "@hourly" => "0 * * * *",
                "@daily" => "0 0 * * *",
                "@weekly" => "0 0 * * 0",
                "@monthly" => "0 0 1 * *",
                other => {
                    return Err(ScheduleError::new(
                        "schedule",
                        format!("未知的宏 `{other}`"),
                    ));
                }
            };
            expanded
        } else {
            spec
        };

        let fields: Vec<&str> = text.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(ScheduleError::new(
                "schedule",
                format!("需要 5 个字段，实际是 {}", fields.len()),
            ));
        }

        Ok(Self {
            minutes: parse_field(fields[0], 0, 59, "minute", identity)?,
            hours: parse_field(fields[1], 0, 23, "hour", identity)?,
            days_of_month: parse_field(fields[2], 1, 31, "day-of-month", identity)?,
            months: parse_field(fields[3], 1, 12, "month", identity)?,
            // 星期接受 0..=7，7 与 0 都映射到周日。
            days_of_week: parse_field(fields[4], 0, 7, "day-of-week", normalize_dow)?,
        })
    }

    /// 严格晚于 `after` 的下一个到期时刻（分钟粒度、本地时间）。
    ///
    /// 不存在的本地时间（夏令时跳过的那一小时）会被跳过；二义时刻取较早者。
    /// 约 9 年内无解时返回 `None`。
    pub fn next_due(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let start =
            after.naive_local().with_second(0)?.with_nanosecond(0)? + ChronoDuration::minutes(1);
        let start_date = start.date();

        for offset in 0..=MAX_LOOKAHEAD_DAYS {
            let date = start_date.checked_add_signed(ChronoDuration::days(offset))?;
            if !self.day_matches(date) {
                continue;
            }
            let first_minute = if offset == 0 {
                start.hour() * 60 + start.minute()
            } else {
                0
            };
            for minute_of_day in first_minute..(24 * 60) {
                let hour = minute_of_day / 60;
                let minute = minute_of_day % 60;
                if !self.hours.contains(hour) || !self.minutes.contains(minute) {
                    continue;
                }
                let Some(naive) = date.and_hms_opt(hour, minute, 0) else {
                    continue;
                };
                if let Some(local) = Local.from_local_datetime(&naive).earliest() {
                    return Some(local);
                }
            }
        }
        None
    }

    /// `now` 所在的那一分钟是否命中（秒与纳秒忽略）。
    pub fn is_due(&self, now: DateTime<Local>) -> bool {
        self.day_matches(now.date_naive())
            && self.hours.contains(now.hour())
            && self.minutes.contains(now.minute())
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        if !self.months.contains(date.month()) {
            return false;
        }
        let day_of_month = self.days_of_month.contains(date.day());
        let day_of_week = self
            .days_of_week
            .contains(date.weekday().num_days_from_sunday());
        if self.days_of_month.restricted && self.days_of_week.restricted {
            day_of_month || day_of_week
        } else {
            day_of_month && day_of_week
        }
    }
}

fn identity(value: u32) -> u32 {
    value
}

fn normalize_dow(value: u32) -> u32 {
    value % 7
}

fn parse_field(
    spec: &str,
    min: u32,
    max: u32,
    label: &'static str,
    normalize: fn(u32) -> u32,
) -> Result<Field, ScheduleError> {
    if spec.is_empty() {
        return Err(ScheduleError::new(label, "不能为空"));
    }

    let mut mask = 0u64;
    let mut restricted = false;
    for piece in spec.split(',') {
        if piece.is_empty() {
            return Err(ScheduleError::new(label, "列表中有空项"));
        }

        let (base, step) = match piece.split_once('/') {
            Some((base, step_text)) => {
                let step: u32 = step_text.parse().map_err(|_| {
                    ScheduleError::new(label, format!("步长 `{step_text}` 不是数字"))
                })?;
                if step == 0 {
                    return Err(ScheduleError::new(label, "步长不能为 0"));
                }
                (base, Some(step))
            }
            None => (piece, None),
        };

        if !base.starts_with('*') {
            restricted = true;
        }

        let (lo, hi) = if base == "*" {
            (min, max)
        } else if let Some((start_text, end_text)) = base.split_once('-') {
            let start = parse_field_value(start_text, min, max, label)?;
            let end = parse_field_value(end_text, min, max, label)?;
            if start > end {
                return Err(ScheduleError::new(
                    label,
                    format!("范围 {start}-{end} 的起点大于终点"),
                ));
            }
            (start, end)
        } else {
            if step.is_some() {
                return Err(ScheduleError::new(
                    label,
                    format!("步长只能用于 `*` 或 `a-b`，不能用于 `{base}`"),
                ));
            }
            let value = parse_field_value(base, min, max, label)?;
            (value, value)
        };

        let step = step.unwrap_or(1);
        let mut value = lo;
        while value <= hi {
            mask |= 1u64 << normalize(value);
            match value.checked_add(step) {
                Some(next) => value = next,
                None => break,
            }
        }
    }

    Ok(Field { mask, restricted })
}

fn parse_field_value(
    text: &str,
    min: u32,
    max: u32,
    label: &'static str,
) -> Result<u32, ScheduleError> {
    let value: u32 = text
        .parse()
        .map_err(|_| ScheduleError::new(label, format!("`{text}` 不是数字")))?;
    if value < min || value > max {
        return Err(ScheduleError::new(
            label,
            format!("{value} 超出范围 {min}-{max}"),
        ));
    }
    Ok(value)
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// 可注入的时钟。生产用 [`SystemClock`]；测试用它驱动 [`CronCore`] 而不睡眠。
pub trait CronClock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Local>;
}

/// 真实本地时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl CronClock for SystemClock {
    fn now(&self) -> DateTime<Local> {
        Local::now()
    }
}

/// 到期任务的接收方。实现方决定如何把任务交给 agent。
pub trait CronSink: Send + Sync + 'static {
    /// 一个任务在它的分钟里第一次到期。
    fn fire(&self, job: &CronJob);

    /// 一次配置重载发现的所有问题，已合并成一条。默认丢弃。
    fn diagnostic(&self, _message: &str) {}
}

/// 调度核心：持有已加载任务、去重集合与配置 mtime。
///
/// 线程版 [`CronHandle`] 只是“取当前时间 → [`CronCore::reload_if_changed`] →
/// [`CronCore::tick`] → 睡到下一个检查点”的循环；测试可以直接用固定时钟调
/// 这两个方法，完全不需要睡眠。
pub struct CronCore {
    config_path: PathBuf,
    jobs: Vec<CronJob>,
    /// `(任务名, 分钟序号)`：保证同一分钟最多触发一次。
    fired: HashSet<(String, i64)>,
    last_mtime: Option<SystemTime>,
    loaded: bool,
}

impl CronCore {
    pub fn new(config_path: PathBuf) -> Self {
        Self {
            config_path,
            jobs: Vec::new(),
            fired: HashSet::new(),
            last_mtime: None,
            loaded: false,
        }
    }

    /// 以会话工作目录构造，配置文件为 `<cwd>/.ki/cron.json`。
    pub fn for_cwd(cwd: &Path) -> Self {
        Self::new(config_path(cwd))
    }

    /// 当前生效的合法任务。
    pub fn jobs(&self) -> &[CronJob] {
        &self.jobs
    }

    /// mtime 变化时重载配置。返回 `None` 表示“没变化、没重载”；
    /// `Some(errors)` 表示刚重载，`errors` 是本次的全部问题（可能为空）。
    ///
    /// 文件不存在等价于“没有任务”，不算错误。重载后合法任务立即生效。
    pub fn reload_if_changed(&mut self) -> Option<Vec<ConfigError>> {
        let current_mtime = file_mtime(&self.config_path);
        if self.loaded && current_mtime == self.last_mtime {
            return None;
        }
        self.loaded = true;
        self.last_mtime = current_mtime;

        let load = match std::fs::read_to_string(&self.config_path) {
            Ok(raw) => parse_config(&raw),
            Err(err) if err.kind() == io::ErrorKind::NotFound => ConfigLoad::default(),
            Err(err) => ConfigLoad {
                jobs: Vec::new(),
                errors: vec![ConfigError::top(format!(
                    "读取 {} 失败：{err}",
                    self.config_path.display()
                ))],
            },
        };

        self.jobs = load.jobs;
        Some(load.errors)
    }

    /// 返回 `now` 这一分钟应当触发的任务（每个任务最多一次）。
    pub fn tick(&mut self, now: DateTime<Local>) -> Vec<CronJob> {
        let key = minute_key(now);
        // 只保留最近两分钟的记录：长期运行也不会无限增长。
        self.fired.retain(|(_, minute)| *minute >= key - 2);

        let mut due = Vec::new();
        for job in &self.jobs {
            if !job.enabled || !job.schedule.is_due(now) {
                continue;
            }
            if self.fired.insert((job.name.clone(), key)) {
                due.push(job.clone());
            }
        }
        due
    }

    /// 距离下一次需要醒来还有多久。上限是 [`CONFIG_POLL_INTERVAL`]，
    /// 下限 1ms（绝不 0，避免忙等）。
    pub fn time_until_next_tick(&self, now: DateTime<Local>) -> Duration {
        let mut wait = CONFIG_POLL_INTERVAL;
        for job in self.jobs.iter().filter(|job| job.enabled) {
            if let Some(next) = job.schedule.next_due(now) {
                let delta = next
                    .signed_duration_since(now)
                    .to_std()
                    .unwrap_or(Duration::ZERO);
                if delta < wait {
                    wait = delta;
                }
            }
        }
        wait.max(Duration::from_millis(1))
    }
}

fn minute_key(now: DateTime<Local>) -> i64 {
    now.timestamp() / 60
}

/// 后台调度线程句柄。`stop`（或 drop）会唤醒线程并等待它退出。
pub struct CronHandle {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl CronHandle {
    /// 用真实时钟启动调度线程。
    pub fn start(cwd: impl AsRef<Path>, sink: Arc<dyn CronSink>) -> Self {
        Self::start_with_clock(cwd, sink, Arc::new(SystemClock))
    }

    /// 用注入的时钟启动，便于测试线程循环本身。
    pub fn start_with_clock(
        cwd: impl AsRef<Path>,
        sink: Arc<dyn CronSink>,
        clock: Arc<dyn CronClock>,
    ) -> Self {
        let config_path = config_path(cwd.as_ref());
        let (stop, stop_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut core = CronCore::new(config_path);
            loop {
                let now = clock.now();
                if let Some(errors) = core.reload_if_changed() {
                    if !errors.is_empty() {
                        let message = errors
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("；");
                        sink.diagnostic(&message);
                    }
                }
                for job in core.tick(now) {
                    sink.fire(&job);
                }
                match stop_rx.recv_timeout(core.time_until_next_tick(now)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
        });

        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// 停止并等待调度线程。
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CronHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Weekday;
    use std::sync::Mutex;
    use std::time::Duration as StdDuration;

    /// 固定本地时间，测试里只构造确定不会撞上夏令时跳变的时刻。
    fn local(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("测试时刻必须是唯一的本地时间")
    }

    fn schedule(spec: &str) -> Schedule {
        Schedule::parse(spec).unwrap_or_else(|err| panic!("`{spec}` 应可解析：{err}"))
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn every_five_minutes() {
        let s = schedule("*/5 * * * *");
        let next = s.next_due(local(2026, 10, 24, 9, 2)).unwrap();
        assert_eq!((next.hour(), next.minute()), (9, 5));
        // 恰好在命中分钟上，严格取下一次。
        let next = s.next_due(local(2026, 10, 24, 9, 5)).unwrap();
        assert_eq!((next.hour(), next.minute()), (9, 10));
        // 跨小时。
        let next = s.next_due(local(2026, 10, 24, 9, 58)).unwrap();
        assert_eq!((next.hour(), next.minute()), (10, 0));

        assert!(s.is_due(local(2026, 10, 24, 9, 5)));
        // 秒被忽略：9:05:59 仍属于 9:05 这一分钟。
        assert!(s.is_due(
            Local
                .with_ymd_and_hms(2026, 10, 24, 9, 5, 59)
                .single()
                .unwrap()
        ));
        assert!(!s.is_due(local(2026, 10, 24, 9, 6)));
    }

    #[test]
    fn hourly() {
        let s = schedule("0 * * * *");
        let next = s.next_due(local(2026, 10, 24, 9, 30)).unwrap();
        assert_eq!((next.hour(), next.minute()), (10, 0));
        assert_eq!(next.date_naive(), date(2026, 10, 24));
    }

    #[test]
    fn weekly_is_sunday_midnight() {
        let s = schedule("@weekly");
        assert_eq!(s, schedule("0 0 * * 0"));
        // 2026-10-24 是周六。
        let next = s.next_due(local(2026, 10, 24, 12, 0)).unwrap();
        assert_eq!(next.date_naive(), date(2026, 10, 25));
        assert_eq!(next.weekday(), Weekday::Sun);
        assert_eq!((next.hour(), next.minute()), (0, 0));
    }

    #[test]
    fn sunday_accepts_zero_and_seven() {
        let zero = schedule("0 0 * * 0");
        let seven = schedule("0 0 * * 7");
        assert_eq!(zero, seven);
        assert!(zero.is_due(local(2026, 10, 25, 0, 0)));
        assert!(!zero.is_due(local(2026, 10, 24, 0, 0)));
    }

    #[test]
    fn month_boundary_and_monthly_macro() {
        let s = schedule("@monthly");
        assert_eq!(s, schedule("0 0 1 * *"));
        let next = s.next_due(local(2026, 1, 31, 23, 59)).unwrap();
        assert_eq!(next.date_naive(), date(2026, 2, 1));
        assert_eq!((next.hour(), next.minute()), (0, 0));
    }

    #[test]
    fn leap_day_is_found_across_years() {
        let s = schedule("0 0 29 2 *");
        let next = s.next_due(local(2023, 3, 1, 0, 0)).unwrap();
        assert_eq!(next.date_naive(), date(2024, 2, 29));
    }

    #[test]
    fn lists_ranges_and_steps() {
        let s = schedule("1,3,5-7 */6 1-3 * 1-5");
        // 2026-10-02 是周五：日 2 命中 1-3，星期 5 命中 1-5，小时 6 命中 */6，
        // 分钟 5 命中列表。
        assert!(s.is_due(local(2026, 10, 2, 6, 5)));
        // 分钟 4 不在列表里。
        assert!(!s.is_due(local(2026, 10, 2, 6, 4)));
    }

    #[test]
    fn invalid_expressions_report_the_field() {
        let err = Schedule::parse("60 * * * *").unwrap_err();
        assert_eq!(err.field, "minute");
        assert!(err.to_string().contains("60"), "{err}");

        let err = Schedule::parse("* 24 * * *").unwrap_err();
        assert_eq!(err.field, "hour");

        let err = Schedule::parse("* * 0 * *").unwrap_err();
        assert_eq!(err.field, "day-of-month");

        let err = Schedule::parse("* * * 13 *").unwrap_err();
        assert_eq!(err.field, "month");

        let err = Schedule::parse("* * * * 8").unwrap_err();
        assert_eq!(err.field, "day-of-week");

        assert!(Schedule::parse("*/0 * * * *").is_err());
        assert!(Schedule::parse("5-1 * * * *").is_err());
        assert!(Schedule::parse("1/5 * * * *").is_err());
        assert!(Schedule::parse("0 0 * *").is_err());
        assert!(Schedule::parse("@yearly").is_err());
        assert!(Schedule::parse("   ").is_err());
    }

    #[test]
    fn config_accepts_wrapper_and_bare_array() {
        let raw = r#"{
            "jobs": [
                {"name": "digest", "schedule": "@daily", "prompt": "总结今天的变更"},
                {"name": "build", "schedule": "*/10 * * * *", "command": "cargo check", "timeoutSecs": 120}
            ]
        }"#;
        let load = parse_config(raw);
        assert!(load.is_ok(), "{:?}", load.errors);
        assert_eq!(load.jobs.len(), 2);
        assert_eq!(
            load.jobs[0].action,
            JobAction::Prompt("总结今天的变更".to_string())
        );
        assert!(load.jobs[0].enabled);
        assert_eq!(load.jobs[0].timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(
            load.jobs[1].action,
            JobAction::Command("cargo check".to_string())
        );
        assert_eq!(load.jobs[1].timeout_secs, 120);

        let bare = r#"[{"name": "only", "schedule": "@hourly", "prompt": "ping"}]"#;
        let load = parse_config(bare);
        assert!(load.is_ok(), "{:?}", load.errors);
        assert_eq!(load.jobs.len(), 1);
    }

    #[test]
    fn config_reports_each_bad_job_and_keeps_valid_ones() {
        let raw = r#"[
            {"name": "good", "schedule": "@hourly", "prompt": "ok"},
            {"name": "both", "schedule": "@hourly", "prompt": "x", "command": "y"},
            {"name": "neither", "schedule": "@hourly"},
            {"name": "surprise", "schedule": "@hourly", "prompt": "x", "extra": 1},
            {"name": "badcron", "schedule": "99 * * * *", "prompt": "x"},
            {"name": "good", "schedule": "@hourly", "prompt": "duplicate"}
        ]"#;
        let load = parse_config(raw);
        assert_eq!(load.jobs.len(), 1, "{:?}", load.jobs);
        assert_eq!(load.jobs[0].name, "good");
        assert_eq!(load.errors.len(), 5, "{:?}", load.errors);

        let rendered: Vec<String> = load.errors.iter().map(ToString::to_string).collect();
        assert!(rendered
            .iter()
            .any(|e| e.contains("both") && e.contains("prompt")));
        assert!(rendered
            .iter()
            .any(|e| e.contains("neither") && e.contains("prompt")));
        assert!(rendered
            .iter()
            .any(|e| e.contains("surprise") && e.contains("extra")));
        assert!(rendered
            .iter()
            .any(|e| e.contains("badcron") && e.contains("schedule")));
        assert!(rendered
            .iter()
            .any(|e| e.contains("good") && e.contains("重名")));
    }

    #[test]
    fn an_invalid_job_does_not_reserve_its_name() {
        let load = parse_config(
            r#"[{"name": "same", "schedule": "99 * * * *", "prompt": "bad"},
                {"name": "same", "schedule": "@hourly", "prompt": "good"}]"#,
        );
        assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
        assert_eq!(load.errors[0].field.as_deref(), Some("schedule"));
        assert_eq!(load.jobs.len(), 1);
        assert_eq!(load.jobs[0].name, "same");
    }

    #[test]
    fn config_validates_timeout_and_enabled() {
        let load = parse_config(
            r#"[{"name": "a", "schedule": "@hourly", "prompt": "x", "timeoutSecs": 600},
                {"name": "b", "schedule": "@hourly", "prompt": "x", "timeoutSecs": 0},
                {"name": "c", "schedule": "@hourly", "prompt": "x", "timeoutSecs": 601},
                {"name": "d", "schedule": "@hourly", "prompt": "x", "enabled": false}]"#,
        );
        assert_eq!(load.jobs.len(), 2, "{:?}", load.errors);
        assert_eq!(load.jobs[0].timeout_secs, MAX_TIMEOUT_SECS);
        assert!(!load.jobs[1].enabled);
        assert_eq!(load.errors.len(), 2);
        assert!(load
            .errors
            .iter()
            .all(|err| err.field.as_deref() == Some("timeoutSecs")));
    }

    #[test]
    fn config_rejects_unknown_top_level_fields_but_keeps_jobs() {
        let load = parse_config(
            r#"{"jobs": [{"name": "a", "schedule": "@hourly", "prompt": "x"}], "extra": true}"#,
        );
        assert_eq!(load.jobs.len(), 1);
        assert_eq!(load.errors.len(), 1);
        assert!(load.errors[0].to_string().contains("extra"));
    }

    #[test]
    fn config_rejects_malformed_shapes() {
        assert!(parse_config("not json").errors[0]
            .to_string()
            .contains("JSON"));
        assert!(parse_config("[]").is_ok());
        assert!(parse_config(r#"{"jobs": {}}"#).errors[0]
            .to_string()
            .contains("数组"));
        assert!(parse_config("{}").errors[0].to_string().contains("jobs"));
        assert!(parse_config("42").errors[0].to_string().contains("顶层"));
    }

    fn write_config(dir: &Path, raw: &str) {
        std::fs::create_dir_all(dir.join(".ki")).unwrap();
        std::fs::write(dir.join(CRON_CONFIG_RELATIVE), raw).unwrap();
    }

    #[test]
    fn core_fires_at_most_once_per_minute() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"[{"name": "tick", "schedule": "* * * * *", "prompt": "ping"}]"#,
        );

        let mut core = CronCore::for_cwd(dir.path());
        let errors = core.reload_if_changed().expect("首次必然加载");
        assert!(errors.is_empty(), "{errors:?}");

        let now = local(2026, 10, 24, 9, 5);
        let fired = core.tick(now);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].name, "tick");
        assert!(core.tick(now).is_empty(), "同一分钟不该再触发");

        let next_minute = now + ChronoDuration::minutes(1);
        assert_eq!(core.tick(next_minute).len(), 1);
        let fired = core.tick(now + ChronoDuration::minutes(2));
        assert_eq!(fired[0].action, JobAction::Prompt("ping".to_string()));
    }

    #[test]
    fn core_skips_disabled_jobs() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"[{"name": "off", "schedule": "* * * * *", "prompt": "x", "enabled": false}]"#,
        );
        let mut core = CronCore::for_cwd(dir.path());
        assert!(core.reload_if_changed().unwrap().is_empty());
        assert!(core.tick(local(2026, 10, 24, 9, 5)).is_empty());
    }

    #[test]
    fn core_reloads_when_the_file_changes_and_clears_jobs_when_removed() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"[{"name": "a", "schedule": "@hourly", "prompt": "x"}]"#,
        );
        let mut core = CronCore::for_cwd(dir.path());
        assert!(core.reload_if_changed().is_some());
        assert_eq!(core.jobs().len(), 1);

        // 没有变化时不重载。
        assert!(core.reload_if_changed().is_none());

        // 删除文件：mtime 从 Some 变 None，重载为空任务集，且不算错误。
        std::fs::remove_file(dir.path().join(CRON_CONFIG_RELATIVE)).unwrap();
        let errors = core.reload_if_changed().expect("mtime 变了应重载");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(core.jobs().is_empty());
        assert!(core.reload_if_changed().is_none());
    }

    #[test]
    fn core_reports_config_errors_but_keeps_valid_jobs_running() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"[{"name": "good", "schedule": "* * * * *", "prompt": "ok"},
                {"name": "bad", "schedule": "nope nope", "prompt": "x"}]"#,
        );
        let mut core = CronCore::for_cwd(dir.path());
        let errors = core.reload_if_changed().expect("首次必然加载");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].job.as_deref(), Some("bad"));
        assert_eq!(core.jobs().len(), 1);
        assert_eq!(core.tick(local(2026, 10, 24, 9, 5)).len(), 1);
    }

    #[test]
    fn core_never_waits_forever_and_never_busy_waits() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"[{"name": "a", "schedule": "0 0 1 1 *", "prompt": "x"}]"#,
        );
        let mut core = CronCore::for_cwd(dir.path());
        core.reload_if_changed().unwrap();
        let wait = core.time_until_next_tick(local(2026, 10, 24, 9, 5));
        assert!(wait >= StdDuration::from_millis(1));
        assert!(wait <= CONFIG_POLL_INTERVAL);
    }

    struct MockClock {
        current: Mutex<DateTime<Local>>,
    }

    impl CronClock for MockClock {
        fn now(&self) -> DateTime<Local> {
            *self.current.lock().unwrap()
        }
    }

    struct ChannelSink {
        jobs: mpsc::Sender<CronJob>,
        diagnostics: Mutex<Vec<String>>,
    }

    impl CronSink for ChannelSink {
        fn fire(&self, job: &CronJob) {
            let _ = self.jobs.send(job.clone());
        }

        fn diagnostic(&self, message: &str) {
            self.diagnostics.lock().unwrap().push(message.to_string());
        }
    }

    #[test]
    fn handle_fires_through_the_sink_with_a_mock_clock() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"[{"name": "boot", "schedule": "* * * * *", "prompt": "hi"}]"#,
        );

        let (tx, rx) = mpsc::channel();
        let sink = Arc::new(ChannelSink {
            jobs: tx,
            diagnostics: Mutex::new(Vec::new()),
        });
        let clock = Arc::new(MockClock {
            current: Mutex::new(local(2026, 10, 24, 9, 5)),
        });

        let handle = CronHandle::start_with_clock(dir.path(), sink.clone(), clock);
        let job = rx
            .recv_timeout(StdDuration::from_secs(5))
            .expect("启动后应立即触发到期任务");
        assert_eq!(job.name, "boot");
        // 线程不会在同一个固定分钟里再次触发。
        assert!(rx.recv_timeout(StdDuration::from_millis(1200)).is_err());
        handle.stop();
        assert!(sink.diagnostics.lock().unwrap().is_empty());
    }
}
