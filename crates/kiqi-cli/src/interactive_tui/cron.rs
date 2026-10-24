//! 会话内定时任务接线：把 [`crate::cron`] 的到点任务接进 TUI。
//!
//! 调度核心在 `crate::cron`，这里只负责两件事：
//!
//! 1. 交互式会话启动时，只有项目受信任且工作目录里有 `.ki/cron.json` 才启动
//!    调度线程；会话结束或 `/reload` 时停止（reload 重新判定信任后再启动）。
//! 2. 把注入文本交给 agent，**复用普通用户消息的两条既有路径**：
//!    - 回合在跑：`lane.steer` 入队，与用户此时敲下消息完全相同；
//!    - 回合空闲：走 TUI 主循环（`TuiMessage::CronInject`），和编辑器提交一样
//!      渲染气泡并开始一轮。
//!
//! 无头模式（`-p`、`--mode json`、`--server`）不构造本模块的任何东西，因此
//! 不会调度——定时任务的产品语义是“会话打开时才跳动”。

use super::*;
use std::sync::atomic::Ordering;

/// 一个正在运行的会话内调度器。
pub(super) struct SessionCron {
    handle: crate::cron::CronHandle,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionCron {
    /// 先标记关闭，再停调度线程，并由句柄取消且 join 全部在途命令。
    pub(super) fn stop(self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.handle.stop();
    }
}

/// 为交互式会话启动调度器；项目不受信任或没有 `.ki/cron.json` 时返回 `None`。
/// prompt 与 command 都是项目内容，不能只拦截其中有副作用的一支。
pub(super) fn start_session_cron(
    cwd: &std::path::Path,
    project_trusted: bool,
    args: &Args,
    lane: &Arc<dyn AgentLane>,
    state: &Arc<TuiState>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    tui: &Arc<TuiAltScreen>,
    dedup: Arc<crate::cron::CronDedup>,
) -> Option<SessionCron> {
    if !project_trusted || !crate::cron::config_path(cwd).is_file() {
        return None;
    }
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let shutting_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let inject = Arc::new(TuiCronInject {
        lane: Arc::clone(lane),
        state: Arc::clone(state),
        tx: tx.clone(),
        tui: Arc::clone(tui),
        runtime,
        shutting_down: Arc::clone(&shutting_down),
    });
    let log: crate::cron::CronLog = Arc::new(|message: &str| diagnostic_log(message));
    let trust_args = args.clone();
    let trust_cwd = cwd.to_path_buf();
    let trust: crate::cron::CronTrust =
        Arc::new(move || crate::session::resolve_project_trust(&trust_args, &trust_cwd));
    let handle = crate::cron::start_if_configured(
        cwd,
        inject,
        log,
        Arc::clone(&shutting_down),
        trust,
        dedup,
    )?;
    Some(SessionCron {
        handle,
        shutting_down,
    })
}

/// `CronInject` 的 TUI 实现。注入动作必须异步完成（`lane.steer`/主循环都是
/// 异步的），所以这里在调度线程上只做一次 `spawn`，随即返回，绝不阻塞调度。
struct TuiCronInject {
    lane: Arc<dyn AgentLane>,
    state: Arc<TuiState>,
    tx: mpsc::UnboundedSender<TuiMessage>,
    tui: Arc<TuiAltScreen>,
    runtime: tokio::runtime::Handle,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
}

impl crate::cron::CronInject for TuiCronInject {
    fn inject(&self, text: String) -> Result<(), String> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("会话正在关闭".to_string());
        }

        let lane = Arc::clone(&self.lane);
        let state = Arc::clone(&self.state);
        let tx = self.tx.clone();
        let tui = Arc::clone(&self.tui);
        self.runtime.spawn(async move {
            // 在途回合：和用户此刻输入一样 steer 入队，不取消、不插队。
            if *state.status.lock().unwrap() != RunStatus::Idle {
                let message = user_message_with_images(&text, Vec::new());
                match lane.steer(message).await {
                    Ok(_) => {
                        refresh_pending_messages(&state, &lane).await;
                        tui.request_render(false);
                    }
                    Err(error) => {
                        diagnostic_log(&format!("cron: 注入失败（steer）：{error}"));
                    }
                }
                return;
            }

            // 空闲回合：交给主循环，与编辑器提交走同一条 `TuiMessage` 管道。
            if tx.send(TuiMessage::CronInject(text)).is_err() {
                diagnostic_log("cron: 注入失败：TUI 通道已关闭");
            }
        });
        Ok(())
    }
}
