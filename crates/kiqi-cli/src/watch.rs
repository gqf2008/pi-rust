//! `.ki/watch.json` 事件式唤醒：配置解析、探测循环与宿主侧注入。
//!
//! 与 [`crate::cron`] 的分工很直接：cron 是**时间驱动**（到点必发），watch 是
//! **变化驱动**（探针输出变了才发）。一条 watch 在会话打开时跑第一遍只建立
//! 基线、不注入，之后每隔 `intervalSecs` 跑一次，输出（退出码 + 合并后的
//! stdout/stderr）与基线不同才把 `prompt` 连同新输出注入会话。
//!
//! 产品约定与 cron 保持一致：
//!
//! - 只在交互式会话里调度；无头模式（`-p`/`--mode json`/`--server`/ACP）不启动。
//! - 同一进程内同一条 watch 不自我重叠：上一轮探针还在跑就跳过本轮。
//! - 配置文件按 mtime 重载；一次重载只发一条诊断，合法条目继续跑。
//! - `.ki/watch.json` 是项目资源：项目不受信任时不读取、不调度。
//! - 命令在会话工作目录执行，超时强杀整个进程树，输出上限 64 KiB。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::cron::{self, ConfigError, CronInject, CronLog, CronTrust};

/// 配置文件相对会话工作目录的路径。
pub const WATCH_CONFIG_RELATIVE: &str = ".ki/watch.json";

/// `intervalSecs` 缺省值。
pub const DEFAULT_INTERVAL_SECS: u64 = 60;
/// `intervalSecs` 下限：比这更密就只是在烧 CPU。
pub const MIN_INTERVAL_SECS: u64 = 5;
/// `intervalSecs` 上限。
pub const MAX_INTERVAL_SECS: u64 = 3600;
/// `timeoutSecs` 缺省值。
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// `timeoutSecs` 上限。
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// 配置重载的轮询上限（与 cron 一致）。
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 会话工作目录下的配置文件路径。
pub fn config_path(cwd: &Path) -> PathBuf {
    cwd.join(WATCH_CONFIG_RELATIVE)
}

/// 一条合法的 watch。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchSpec {
    pub name: String,
    pub command: String,
    pub prompt: String,
    pub interval_secs: u64,
    pub timeout_secs: u64,
    pub enabled: bool,
}

/// 一次解析的结果：合法条目 + 全部问题。
#[derive(Debug, Default)]
pub struct WatchLoad {
    pub watches: Vec<WatchSpec>,
    pub errors: Vec<ConfigError>,
}

impl WatchLoad {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

const WATCH_FIELDS: &[&str] = &[
    "name",
    "command",
    "prompt",
    "intervalSecs",
    "timeoutSecs",
    "enabled",
];

/// 解析 `.ki/watch.json` 的文本。接受 `{"watches":[...]}` 与裸数组两种形状。
pub fn parse_config(raw: &str) -> WatchLoad {
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(err) => {
            return WatchLoad {
                watches: Vec::new(),
                errors: vec![ConfigError {
                    job: None,
                    field: None,
                    message: format!("不是合法 JSON：{err}"),
                }],
            }
        }
    };
    let entries = match value {
        Value::Array(entries) => entries,
        Value::Object(mut object) => {
            let mut errors = Vec::new();
            for key in object.keys() {
                if key != "watches" {
                    errors.push(ConfigError {
                        job: None,
                        field: Some(key.clone()),
                        message: "未知字段".to_string(),
                    });
                }
            }
            match object.remove("watches") {
                Some(Value::Array(entries)) => entries,
                Some(other) => {
                    errors.push(ConfigError {
                        job: None,
                        field: Some("watches".to_string()),
                        message: format!("必须是数组，实际是{}", json_type(&other)),
                    });
                    Vec::new()
                }
                None => {
                    errors.push(ConfigError {
                        job: None,
                        field: Some("watches".to_string()),
                        message: "缺少该字段".to_string(),
                    });
                    Vec::new()
                }
            }
        }
        other => {
            return WatchLoad {
                watches: Vec::new(),
                errors: vec![ConfigError {
                    job: None,
                    field: None,
                    message: format!("顶层必须是对象或数组，实际是{}", json_type(&other)),
                }],
            }
        }
    };

    let mut watches = Vec::new();
    let mut errors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, entry) in entries.into_iter().enumerate() {
        match parse_watch(index, entry, &mut seen) {
            Ok(watch) => watches.push(watch),
            Err(error) => errors.push(error),
        }
    }
    WatchLoad { watches, errors }
}

fn parse_watch(
    index: usize,
    value: Value,
    seen: &mut std::collections::HashSet<String>,
) -> Result<WatchSpec, ConfigError> {
    let object = match value {
        Value::Object(object) => object,
        other => {
            return Err(ConfigError {
                job: Some(format!("#{index}")),
                field: None,
                message: format!("条目必须是 JSON 对象，实际是{}", json_type(&other)),
            })
        }
    };
    let labelled = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("#{index}"));

    if let Some(unknown) = object
        .keys()
        .find(|key| !WATCH_FIELDS.contains(&key.as_str()))
    {
        return Err(ConfigError {
            job: Some(labelled),
            field: Some(unknown.clone()),
            message: "未知字段".to_string(),
        });
    }

    let name = match object.get("name") {
        Some(Value::String(name)) if !name.trim().is_empty() => name.trim().to_string(),
        Some(Value::String(_)) => {
            return Err(ConfigError {
                job: Some(labelled),
                field: Some("name".to_string()),
                message: "不能为空".to_string(),
            })
        }
        Some(other) => {
            return Err(ConfigError {
                job: Some(labelled),
                field: Some("name".to_string()),
                message: format!("必须是字符串，实际是{}", json_type(other)),
            })
        }
        None => {
            return Err(ConfigError {
                job: Some(labelled),
                field: Some("name".to_string()),
                message: "缺少该字段".to_string(),
            })
        }
    };

    let string_field = |field: &str| -> Result<String, ConfigError> {
        match object.get(field) {
            Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.trim().to_string()),
            Some(Value::String(_)) => Err(ConfigError {
                job: Some(name.clone()),
                field: Some(field.to_string()),
                message: "不能为空".to_string(),
            }),
            Some(other) => Err(ConfigError {
                job: Some(name.clone()),
                field: Some(field.to_string()),
                message: format!("必须是字符串，实际是{}", json_type(other)),
            }),
            None => Err(ConfigError {
                job: Some(name.clone()),
                field: Some(field.to_string()),
                message: "缺少该字段".to_string(),
            }),
        }
    };
    let command = string_field("command")?;
    let prompt = string_field("prompt")?;

    let enabled = match object.get("enabled") {
        None => true,
        Some(Value::Bool(enabled)) => *enabled,
        Some(other) => {
            return Err(ConfigError {
                job: Some(name),
                field: Some("enabled".to_string()),
                message: format!("必须是布尔值，实际是{}", json_type(other)),
            })
        }
    };

    let interval_secs = match object.get("intervalSecs") {
        None => DEFAULT_INTERVAL_SECS,
        Some(Value::Number(number)) => match number.as_u64() {
            Some(secs) if (MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&secs) => secs,
            Some(secs) => {
                return Err(ConfigError {
                    job: Some(name),
                    field: Some("intervalSecs".to_string()),
                    message: format!(
                        "必须在 {MIN_INTERVAL_SECS}..={MAX_INTERVAL_SECS} 之间，实际是 {secs}"
                    ),
                })
            }
            None => {
                return Err(ConfigError {
                    job: Some(name),
                    field: Some("intervalSecs".to_string()),
                    message: "必须是非负整数秒".to_string(),
                })
            }
        },
        Some(other) => {
            return Err(ConfigError {
                job: Some(name),
                field: Some("intervalSecs".to_string()),
                message: format!("必须是整数秒，实际是{}", json_type(other)),
            })
        }
    };

    let timeout_secs = match object.get("timeoutSecs") {
        None => DEFAULT_TIMEOUT_SECS,
        Some(Value::Number(number)) => match number.as_u64() {
            Some(secs) if (1..=MAX_TIMEOUT_SECS).contains(&secs) => secs,
            Some(secs) => {
                return Err(ConfigError {
                    job: Some(name),
                    field: Some("timeoutSecs".to_string()),
                    message: format!("必须在 1..={MAX_TIMEOUT_SECS} 之间，实际是 {secs}"),
                })
            }
            None => {
                return Err(ConfigError {
                    job: Some(name),
                    field: Some("timeoutSecs".to_string()),
                    message: "必须是非负整数秒".to_string(),
                })
            }
        },
        Some(other) => {
            return Err(ConfigError {
                job: Some(name),
                field: Some("timeoutSecs".to_string()),
                message: format!("必须是整数秒，实际是{}", json_type(other)),
            })
        }
    };

    if !seen.insert(name.clone()) {
        return Err(ConfigError {
            job: Some(name),
            field: Some("name".to_string()),
            message: "与其他任务重名".to_string(),
        });
    }

    Ok(WatchSpec {
        name,
        command,
        prompt,
        interval_secs,
        timeout_secs,
        enabled,
    })
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

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// 探测完成后再比对；基线跨调度器代际共享（`/reload` 不会让所有 watch
/// 重新"发现一次变化"）。
pub type WatchBaselines = Arc<Mutex<HashMap<String, String>>>;

/// 调度核心：持有已加载条目、每条的上一轮探测时间与配置 mtime。
pub struct WatchCore {
    config_path: PathBuf,
    watches: Vec<WatchSpec>,
    last_probe: HashMap<String, Instant>,
    last_mtime: Option<SystemTime>,
    loaded: bool,
}

impl WatchCore {
    pub fn new(config_path: PathBuf) -> Self {
        Self {
            config_path,
            watches: Vec::new(),
            last_probe: HashMap::new(),
            last_mtime: None,
            loaded: false,
        }
    }

    pub fn for_cwd(cwd: &Path) -> Self {
        Self::new(config_path(cwd))
    }

    pub fn watches(&self) -> &[WatchSpec] {
        &self.watches
    }

    /// mtime 变化时重载配置。返回 `None` 表示没变化；`Some(errors)` 表示刚重载。
    pub fn reload_if_changed(&mut self) -> Option<Vec<ConfigError>> {
        let current = file_mtime(&self.config_path);
        if self.loaded && current == self.last_mtime {
            return None;
        }
        self.loaded = true;
        self.last_mtime = current;
        let load = match std::fs::read_to_string(&self.config_path) {
            Ok(raw) => parse_config(&raw),
            Err(err) if err.kind() == io::ErrorKind::NotFound => WatchLoad::default(),
            Err(err) => WatchLoad {
                watches: Vec::new(),
                errors: vec![ConfigError {
                    job: None,
                    field: None,
                    message: format!("读取 {} 失败：{err}", self.config_path.display()),
                }],
            },
        };
        self.watches = load.watches;
        // 已删掉的名字不再保留探测时间，重新添加时从"立即探测一次"开始。
        let live: std::collections::HashSet<&str> = self
            .watches
            .iter()
            .map(|watch| watch.name.as_str())
            .collect();
        self.last_probe
            .retain(|name, _| live.contains(name.as_str()));
        Some(load.errors)
    }

    /// 返回 `now` 应该探测的条目，并记下本轮时间。
    pub fn tick(&mut self, now: Instant) -> Vec<WatchSpec> {
        let mut due = Vec::new();
        for watch in &self.watches {
            if !watch.enabled {
                continue;
            }
            let ready = match self.last_probe.get(&watch.name) {
                None => true,
                Some(previous) => {
                    now.saturating_duration_since(*previous)
                        >= Duration::from_secs(watch.interval_secs)
                }
            };
            if ready {
                self.last_probe.insert(watch.name.clone(), now);
                due.push(watch.clone());
            }
        }
        due
    }

    /// 距离下一次需要醒来还有多久（绝不 0，避免忙等）。
    pub fn time_until_next_tick(&self, now: Instant) -> Duration {
        let mut wait = CONFIG_POLL_INTERVAL;
        for watch in self.watches.iter().filter(|watch| watch.enabled) {
            let due_in = match self.last_probe.get(&watch.name) {
                None => Duration::ZERO,
                Some(previous) => Duration::from_secs(watch.interval_secs)
                    .saturating_sub(now.saturating_duration_since(*previous)),
            };
            if due_in < wait {
                wait = due_in;
            }
        }
        wait.max(Duration::from_millis(1))
    }
}

/// 探测结果的接收方。实现方决定如何注入。
pub trait WatchSink: Send + Sync + 'static {
    fn fire(&self, watch: &WatchSpec);
    fn diagnostic(&self, _message: &str) {}
}

/// 后台调度线程句柄。`stop`（或 drop）会唤醒线程并等待它退出。
pub struct WatchHandle {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
    workers: Option<Arc<WatchWorkerState>>,
}

impl WatchHandle {
    fn start(
        cwd: &Path,
        sink: Arc<dyn WatchSink>,
        trust: CronTrust,
        workers: Arc<WatchWorkerState>,
    ) -> Self {
        let config_path = config_path(cwd);
        let (stop, stop_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            let mut core = WatchCore::new(config_path);
            loop {
                if !trust() {
                    break;
                }
                let now = Instant::now();
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
                let wait = core.time_until_next_tick(now);
                for watch in core.tick(now) {
                    if !trust() {
                        return;
                    }
                    sink.fire(&watch);
                }
                match stop_rx.recv_timeout(wait) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
            workers: Some(workers),
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(workers) = &self.workers {
            workers.cancel_and_join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 一代调度器拥有的探测线程状态。
#[derive(Default)]
struct WatchWorkerState {
    running: Mutex<std::collections::HashSet<String>>,
    workers: Mutex<HashMap<u64, JoinHandle<()>>>,
    next_worker_id: std::sync::atomic::AtomicU64,
    cancelled: AtomicBool,
}

impl WatchWorkerState {
    fn reap_finished(&self) {
        let finished = {
            let mut workers = self.workers.lock().unwrap();
            let ids: Vec<u64> = workers
                .iter()
                .filter(|(_, worker)| worker.is_finished())
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| workers.remove(&id))
                .collect::<Vec<_>>()
        };
        for worker in finished {
            let _ = worker.join();
        }
    }

    fn cancel_and_join(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        for (_, worker) in workers {
            let _ = worker.join();
        }
        self.running.lock().unwrap().clear();
    }
}

/// 注入文本：一行标记 + 空行 + 新输出。
pub fn watch_injection_text(name: &str, output: &cron::CommandOutput) -> String {
    let code = output.exit_code.unwrap_or(-1);
    let body = if output.text.is_empty() {
        "(无输出)"
    } else {
        output.text.as_str()
    };
    format!("⏰ watch:{name} 输出变化（退出码 {code}）\n\n{body}")
}

/// 宿主侧 [`WatchSink`]：跑探针、比对基线、只在变化时注入。
pub struct SessionWatchSink {
    cwd: PathBuf,
    inject: Arc<dyn CronInject>,
    log: CronLog,
    shutting_down: Arc<AtomicBool>,
    workers: Arc<WatchWorkerState>,
    baselines: WatchBaselines,
}

impl SessionWatchSink {
    pub fn new(
        cwd: PathBuf,
        inject: Arc<dyn CronInject>,
        log: CronLog,
        shutting_down: Arc<AtomicBool>,
        workers: Arc<WatchWorkerState>,
        baselines: WatchBaselines,
    ) -> Self {
        Self {
            cwd,
            inject,
            log,
            shutting_down,
            workers,
            baselines,
        }
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
}

impl WatchSink for SessionWatchSink {
    fn fire(&self, watch: &WatchSpec) {
        if self.is_shutting_down() {
            (self.log)(&format!(
                "watch: 会话正在关闭，丢弃 `{}` 的本次探测",
                watch.name
            ));
            return;
        }
        self.workers.reap_finished();
        {
            let mut running = self.workers.running.lock().unwrap();
            if !running.insert(watch.name.clone()) {
                drop(running);
                (self.log)(&format!(
                    "watch: `{}` 上一轮探针仍在运行，本次跳过",
                    watch.name
                ));
                return;
            }
        }

        let cwd = self.cwd.clone();
        let name = watch.name.clone();
        let worker_name = name.clone();
        let command = watch.command.clone();
        let prompt = watch.prompt.clone();
        let timeout_secs = watch.timeout_secs;
        let inject = Arc::clone(&self.inject);
        let log = Arc::clone(&self.log);
        let workers = Arc::clone(&self.workers);
        let baselines = Arc::clone(&self.baselines);
        let shutting_down = Arc::clone(&self.shutting_down);

        let thread_name = name.clone();
        let spawned = thread::Builder::new()
            .name(format!("ki-watch-{thread_name}"))
            .spawn(move || {
                let result = cron::run_job_command_cancellable(
                    &cwd,
                    &command,
                    timeout_secs,
                    Some(&workers.cancelled),
                );
                workers.running.lock().unwrap().remove(&worker_name);

                if shutting_down.load(Ordering::SeqCst) {
                    log(&format!("watch: 会话已关闭，丢弃 `{name}` 的探测结果"));
                    return;
                }
                let output = match result {
                    Ok(output) => output,
                    Err(error) => {
                        log(&format!("watch: `{name}` 的命令无法执行：{error}"));
                        return;
                    }
                };
                if output.timed_out {
                    log(&format!(
                        "watch: `{name}` 的命令超时（{timeout_secs}s），已终止"
                    ));
                }
                let key = format!(
                    "{}\n{}",
                    output
                        .exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "signal".to_string()),
                    output.text
                );
                // 第一轮只建立基线：会话一打开就注入一次没有信息量。
                let changed = {
                    let mut baselines = baselines.lock().unwrap();
                    match baselines.get(&name) {
                        None => {
                            baselines.insert(name.clone(), key);
                            false
                        }
                        Some(previous) if *previous == key => false,
                        Some(_) => {
                            baselines.insert(name.clone(), key);
                            true
                        }
                    }
                };
                if !changed {
                    return;
                }
                let text = format!("{prompt}\n\n{}", watch_injection_text(&name, &output));
                if let Err(error) = inject.inject(text) {
                    log(&format!("watch: 注入失败：{error}"));
                }
            });

        match spawned {
            Ok(worker) => {
                let worker_id = self.workers.next_worker_id.fetch_add(1, Ordering::Relaxed);
                self.workers
                    .workers
                    .lock()
                    .unwrap()
                    .insert(worker_id, worker);
            }
            Err(error) => {
                self.workers.running.lock().unwrap().remove(&watch.name);
                (self.log)(&format!(
                    "watch: 无法为 `{}` 启动探测线程：{error}",
                    watch.name
                ));
            }
        }
    }

    fn diagnostic(&self, message: &str) {
        (self.log)(&format!("watch: {message}"));
    }
}

/// 项目受信任时启动探测线程；否则返回 `None`。配置文件由线程按 mtime 自行发现。
pub fn start_if_configured(
    cwd: &Path,
    inject: Arc<dyn CronInject>,
    log: CronLog,
    shutting_down: Arc<AtomicBool>,
    trust: CronTrust,
    baselines: WatchBaselines,
) -> Option<WatchHandle> {
    // 文件是否存在不参与判定：`watch` 工具会在会话中途创建它，线程按 mtime
    // 自行发现。
    if !trust() {
        return None;
    }
    let workers = Arc::new(WatchWorkerState::default());
    let sink = Arc::new(SessionWatchSink::new(
        cwd.to_path_buf(),
        inject,
        log,
        shutting_down,
        Arc::clone(&workers),
        baselines,
    ));
    Some(WatchHandle::start(cwd, sink, trust, workers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, interval_secs: u64) -> WatchSpec {
        WatchSpec {
            name: name.to_string(),
            command: "true".to_string(),
            prompt: "changed".to_string(),
            interval_secs,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            enabled: true,
        }
    }

    #[test]
    fn parses_object_and_bare_array() {
        let object = parse_config(
            r#"{"watches":[{"name":"ci","command":"gh run list","prompt":"look","intervalSecs":30}]}"#,
        );
        assert!(object.is_ok(), "{:?}", object.errors);
        assert_eq!(object.watches.len(), 1);
        assert_eq!(object.watches[0].interval_secs, 30);

        let array = parse_config(r#"[{"name":"ci","command":"true","prompt":"look"}]"#);
        assert!(array.is_ok(), "{:?}", array.errors);
        assert_eq!(array.watches[0].interval_secs, DEFAULT_INTERVAL_SECS);
        assert_eq!(array.watches[0].timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert!(array.watches[0].enabled);
    }

    #[test]
    fn rejects_out_of_range_interval() {
        for interval in [0u64, MIN_INTERVAL_SECS - 1, MAX_INTERVAL_SECS + 1] {
            let load = parse_config(&format!(
                r#"[{{"name":"a","command":"true","prompt":"x","intervalSecs":{interval}}}]"#
            ));
            assert_eq!(load.watches.len(), 0);
            assert_eq!(load.errors.len(), 1, "interval {interval} must be rejected");
        }
    }

    #[test]
    fn rejects_duplicate_names_and_unknown_fields() {
        let duplicate = parse_config(
            r#"[{"name":"a","command":"true","prompt":"x"},
                {"name":"a","command":"true","prompt":"y"}]"#,
        );
        assert_eq!(duplicate.watches.len(), 1);
        assert_eq!(duplicate.errors.len(), 1);

        let unknown = parse_config(r#"[{"name":"a","command":"true","prompt":"x","sched":"*"}]"#);
        assert!(unknown.errors[0].to_string().contains("未知字段"));
    }

    #[test]
    fn core_probes_immediately_then_on_the_interval() {
        let mut core = WatchCore::new(PathBuf::from("/nonexistent/watch.json"));
        core.watches = vec![spec("a", 60)];
        let start = Instant::now();
        assert_eq!(
            core.tick(start).len(),
            1,
            "first probe establishes the baseline"
        );
        assert!(core.tick(start + Duration::from_secs(59)).is_empty());
        assert_eq!(core.tick(start + Duration::from_secs(60)).len(), 1);
    }

    #[test]
    fn disabled_watches_never_probe() {
        let mut core = WatchCore::new(PathBuf::from("/nonexistent/watch.json"));
        let mut disabled = spec("a", 5);
        disabled.enabled = false;
        core.watches = vec![disabled];
        assert!(core.tick(Instant::now()).is_empty());
    }

    #[test]
    fn injection_text_names_the_watch_and_the_exit_code() {
        let output = cron::CommandOutput {
            exit_code: Some(3),
            text: "boom".to_string(),
            truncated: false,
            timed_out: false,
        };
        let text = watch_injection_text("ci", &output);
        assert!(text.contains("watch:ci"));
        assert!(text.contains("退出码 3"));
        assert!(text.contains("boom"));
    }
}
