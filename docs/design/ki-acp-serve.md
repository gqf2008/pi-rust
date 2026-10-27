> 历史存档：本文保留 ACP 各批次的实施坐标；当前协议 crate 为 crates/kiqi-acp。

# 方案：`ki acp serve` — 宿主级 ACP server

> 状态：**已落地**（批次 1/2 于 2026-10-16，批次 3 于 2026-10-22）。目标：让 Zed（及任何 ACP 客户端）把 `ki` 当外部 coding
> agent 用——客户端拉起 `ki acp serve`，在 stdin/stdout 上走 Agent Client Protocol 的 JSON-RPC。
> 关联：`extension-abi.md` §6.2（acp 不迁 wasm 的裁定，2026-10-16）；`extension-process-capability.md`
> （同套格式的上一个立项）；前身实现 `rpi-package/packages/kiqi-acp`（ACP↔RPC 桥）；
> 参考实现 `../abb`（`abb-agent` 手写 ACP server + `src/runtime/acp.rs` 客户端）。

## 1. 要解决什么（实核证据）

| 事实 | 证据 |
|---|---|
| acp 不能做成插件：要独占宿主进程 stdin/stdout，跨 SessionStart→Shutdown 维持服务线程 | `extension-abi.md:177-179`（§6.2 裁定）；原始实现的声明面 `rpi-package/packages/kiqi-acp/src/lib.rs:4-8,67-70` |
| ki 已有 headless 事件驱动先例 `--mode rpc`：prompt 在独立 task 跑、主循环持续读 stdin、abort 可在轮中送达 | `crates/kiqi-cli/src/app.rs:224-236`、`modes.rs:320,343,500`；宿主对「拥有 stdio 的 ACP/MCP 型 server」已有概念与生命周期（`app.rs:231-240`、`app.rs:450-470`） |
| 进程内组装点已存在：`session::build` 一站式产出 `AgentHarness`（工具/JSONL 存储/插件工具合并/权限钩子/事件总线/崩溃恢复） | `crates/kiqi-cli/src/session.rs:209`；`kiqi-harness` 的 `AgentHarness::create`（`agent_harness.rs:665`）、`AgentLane` trait（`:318`） |
| 事件投影原料齐全：`AgentEvent` broadcast（core 刻意不 serde，投影类型单独定义是既有做法） | `kiqi-agent/src/events.rs:26`（`type_tag` :82）；投影先例 `remote/protocol.rs:30`（`RemoteEvent`）、`modes.rs` 的 `agent_event_json` |
| 全栈 tokio（multi_thread flavor 是硬要求：harness 用 `block_in_place` 桥接 provider） | `bin/ki.rs:9-13`、根 `Cargo.toml:54-55`、`agent_harness.rs:2425` |
| 官方 SDK 存在且活跃：`agent-client-protocol` 3.3.0（2026-10-09 发布；1.0.0 2026-06-24 起 4 个月 12 个 release；3.0.0 于 2026-10-06 大改版） | crates.io 版本史；仓库 `agentclientprotocol/rust-sdk`（Apache-2.0，维护者含 Zed 的 benbrandt/agu-z）；agent 侧入口 `Agent.builder().on_receive_request(..).connect_to(Stdio::new())`（`examples/simple_agent.rs`） |
| ki 工作区 0 引用该 crate；abb 两端也 0 引用（纯手写） | 两仓库 `Cargo.lock` grep 无 `agent-client-protocol` |

## 2. 本方案解决 / 不解决

| | 项 | 现状 | 本方案 |
|---|---|---|---|
| ✅ | Zed 等 ACP 客户端驱动 ki | 无入口 | `ki acp serve`（v1：initialize / session/new / prompt / cancel / load / close） |
| ✅ | 事件→`session/update` 投影 | 事件在，无 ACP 面 | 逐 delta 投影（§4.5），abb 的 `event_updates` 做语义对照 |
| ✅ | 取消贯穿（cancel 打断在途回合） | harness 原语齐 | abb 三件套落到 ki 原语上（§4.4） |
| ⏳ | `session/request_permission`（插件 UI 对话框→客户端授权） | `UiDialogMailbox` 桥已在 RPC 通道打通 | 批次 2（§4.6） |
| ❌ | wasm 化 acp | 已裁定不做 | §9；`extension-abi.md` §6.2 |
| ❌ | `_session/steering`、`_meta.abbSandbox`、terminal/*、fs/* 委托、auth 方法 | — | §9 明确不做 |

## 3. 三个参考实现，各取什么

| 来源 | 形态 | 取什么 | 不取什么 |
|---|---|---|---|
| `rpi-package/packages/kiqi-acp`（3281 行，带测试） | **桥**：Zed → `ki --acp`（插件声明 ownsStdio）→ 每个 ACP 会话 spawn 一个 `ki --mode rpc` 子进程翻译事件 | 完整语义蓝本：逐会话子进程、`translate_event`/plan/permission 映射（`session.rs:539,671,432`）、`session/load` 回放（`history.rs`）、Zed settings 注册（`zed.rs`，原子替换+备份+幂等+dry-run）、stderr 纪律与收尾一行日志（`protocol.rs:239` 的 `run()` 收尾） | 桥形态本身（§4.2）；ownsStdio 插件声明面（宿主级不再需要） |
| `../abb` `abb-agent`（acp.rs 1961 行 + wire.rs 194 行） | **进程内**手写：ACP server 直接驱动 ki 库的 agent 循环 | 语义层模板（**同一 `AgentEvent` 家族**，`event_updates` acp.rs:976-1017 可直接对照移植）：派发与取消的血泪教训（§4.4）、stdout 纯协议/配置响亮失败 exit 2/单行畸形 warn 继续（wire.rs:1-8、main.rs:16-25,50-56）、失败回 JSON-RPC error 不自造 stopReason（acp.rs:584-592）、能力声明与实现同真且是唯一闸门不做错误码探测（acp.rs 客户端 :494-516 实测 codex-acp 反例）、会话参数第一刀解析不静默忽略（acp.rs:771-775） | 手写 wire（用官方 crate 替代）；ABB 特有扩展（steering/sandbox） |
| 官方 `agent-client-protocol` crate | 协议 SDK（v3：`Builder`/`Dispatch`/`role::acp` + `Stdio` transport；无默认 feature） | 协议样板层：framing、id 匹配、typed request/notification/params（abb 实测这部分服务端约 194 行 wire + ~95 行 params 解析 + 散落 json! 手拼） | 它买不到的语义层（abb 报告原话：并发派发形状、TurnState、事件投影、超时/清理策略都得自己写） |

## 4. 设计取舍

### 4.1 宿主级子命令，而非插件（已裁定，不重复论证）

`extension-abi.md` §6.2 的裁定不变：acp 要独占 stdio、跨会话生命周期，正确形态是宿主功能。
本方案即该裁定的落地文件。

### 4.2 进程内驱动，而非 kiqi-acp 的桥接形态

kiqi-acp 之所以是桥（ACP server → 每会话 spawn 一个 `ki --mode rpc`），唯一原因是它是
**插件**、不能拥有进程。`ki acp serve` 是宿主本体，这个约束消失，桥的多余一跳
（双重序列化、跨进程取消延迟、每会话进程开销）没有存在理由：

- 驱动层用 **`AgentHarness` + `AgentLane`**，不从 `kiqi-agent::Agent` 裸起步——harness 自带
  持久化/恢复/压缩/预算/插件钩子，`modes::rpc` 已证明 headless 驱动可行；
- 每个 ACP 会话 = 一次 `session::build`（`session.rs:209`），`cwd` 取 `session/new` 参数，
  会话目录按 cwd 天然隔离（`session.rs:188` `<cwd>/.ki/sessions/`）；
- 形态先例：abb-agent 就是「ACP server 进程内驱动 ki 库」的完整样板。

进程内多会话的共享状态注意：`compiled_extension_hosts()` 是进程级 `OnceLock`（同二进制共享
无害，但要写进文档）；settings/models 解析走 per-cwd 的 `resolve_for_cwd`（`provider.rs:401`）。

### 4.3 协议层用官方 crate，语义层自己写

- **采纳官方 crate**（预立项讨论已定方向）：v3 的 typed 面 + framing + Stdio transport 替代
  手写 wire（abb 服务端这块约 300 行、kiqi-acp 的 `protocol.rs` 548 行）；
- **风险与缓解**：3.0.0 大改版距今仅 10 天（2026-10-06），churn 期——**锁精确版本 `=3.3.0`**；
  协议层与语义层之间留 seam（投影/分发不依赖 crate 内部类型，只依赖协议 schema 类型），
  真被卡住时退回「移植 kiqi-acp 的 protocol.rs」成本可控；
- 无默认 feature：显式开 `stdio`（与 `process` 不用——那是客户端拉起 agent 用的）。

### 4.4 分发与取消：abb 的教训落到 ki 的原语上

abb 的历史教训（`acp.rs:11-18`）：第一版读一行同步 `dispatch().await`，`session/prompt`
占住调用栈，cancel 排在回合后面读不到、回合开头又清取消标记 ⇒ `stopReason:"cancelled"`
基本不可达。修正后的形状与 ki 的对应物：

| abb 的做法 | ki 对应物 |
|---|---|
| 长任务 spawn，cancel/steering 在读循环内同步处理（acp.rs:185-249） | `session/prompt` spawn 独立 task，读循环持续读 stdin（`modes::rpc` 同形状，modes.rs:500） |
| `in_flight` 守卫必须**在读循环里抢**，spawn 出的任务里抢会有竞态：同 burst 的 cancel 被读循环先读、任务未调度 ⇒ 取消被当「无在途回合」丢弃——实测 0ms 间隔 4/4 复现（acp.rs:209-227,433-455） | prompt 的在途标记在**读循环**里置位后再 spawn；对应 ki 的 `RunStatus::Aborting` 先例（TUI Esc 路径，interactive_tui.rs:2095-2119） |
| `TurnState{in_flight, cancelled}` 双标志：cancel 只在 `in_flight` 时置位（空转取消不许打中下一回合）；`begin_turn` 在 `swap(true)` **之后**才清 cancelled；`InFlightGuard` Drop 保证 panic 也解锁（acp.rs:96-104,452-454,855-862） | `lane.abort()` 幂等取消 `CancellationToken`（`agent.rs:359`、ActiveRun per-run token `agent.rs:447`），双标志语义在 ACP 侧保留 |
| 100ms tick 兜底补发 abort：cancel 处理时回合任务可能还没 poll 过（per-run token 未建立），那次 abort 会丢；卡 HTTP 时无事件，必须有固定唤醒源（acp.rs:478-497,508-513） | 批次 1 一致性测试钉两条：① prompt 期间 cancel 必达；② cancel-先于-首事件（回合未建立 token）不丢。若 harness 的 `request_abort`（`agent_harness.rs:1259`）已覆盖②，则 tick 只作保险 |
| 失败不自造 stopReason，回 JSON-RPC error（acp.rs:584-592） | `HarnessRunOutcome::Failed` → error；映射表见 §5 |

官方 crate 的分发模型是否内建上述并发形状，是**批次 1 的第一个验证项**（写一致性测试验证，
不读源码想当然）。

### 4.5 文本投影：逐 delta，不是 abb 的整条

abb 出于简单选「MessageEnd 整条发」（对端累加等价）。ki 选**逐 delta**
（`AgentEvent::MessageUpdate` 的 text/thinking 增量 → `agent_message_chunk` /
`agent_thought_chunk`）：kiqi-acp 已验证该映射（`session.rs` 测试
`text_delta_becomes_agent_message_chunk` 等），Zed 有打字机体验。Start/End 不发、空 delta
丢弃、`ToolExecutionUpdate` 不投影（abb 同款取舍：多发=新行为）。

### 4.6 权限：v1 不实现，批次 2 用现成桥

abb 刻意不做 request_permission（约定由 AGENTS.md 承担，acp.rs:26-27）；ki 比 abb 多一层
真实需求：插件有 UI 对话框（ask-user/permissions）。现成桥已存在——RPC 通道已把
`ui_dialog_request/response` 打通（`modes.rs:388-433` 的 `UiDialogMailbox`）。
批次 2 把它映射成 `session/request_permission`（kiqi-acp 的 DialogTasks 簿记与竞态测试
直接当蓝本：迟到应答不许污染下一个对话框、cancel 清未应答请求——`session.rs:852-966`）。
v1 的 `initialize` 如实**不声明** permission 能力。

### 4.7 会话参数第一刀解析，不静默忽略（abb 教训 7）

`session/new` 参数：`cwd` 收下（harness 根）；`systemPrompt` 收下（映射 harness 的
system prompt 覆盖）；`mcpServers` v1 **明确拒绝**（-32602 带说明「ki 的 MCP 走
kiqi-mcp-adapter 插件，暂不接受会话级注入」），宁可报错也不静默丢——「abb 给了、agent
不看」是第一刀的典型失效形态。

## 5. 架构与接口草案

### 5.1 入口与模块

- 子命令：`ki acp serve`。挂 `app.rs:139` 的首参数 match（无 clap derive，加一行
  `Some("acp") => crate::acp::run(&argv[1..]).await`）；生命周期复用 `RunMode` 的
  stdio-claim 骨架（BeforeTuiStart veto → SessionStart → 协议循环 → SessionShutdown，
  `app.rs:450-470`），保住插件事件面正确性；
- 模块：批次 1–3 时代在 `crates/kiqi-cli/src/acp/`（见 §16——已整体提取为
  `crates/kiqi-acp`，会话组装根留在 kiqi-cli `src/acp.rs`）；
- 进程：multi-thread tokio runtime（`bin/ki.rs:9-13` 硬要求，`block_in_place` 依赖）；
- 依赖：`agent-client-protocol =3.3.0`（显式 `features=["stdio"]`）。

### 5.2 方法覆盖

| 方法 | v1 | 说明 |
|---|---|---|
| `initialize` | ✅ | 应答 `protocolVersion: 1`（本端只讲 v1；实测 ABB 的 v2 请求照常接受，见 §7）；能力如实声明：`loadSession` true、不声明 auth |
| `authenticate`/`logout` | 容忍 | 凭据走 ki 自己的配置（`~/.ki/agent/auth.json`、env），无 ACP auth；stray 调用回 `{}`（kiqi-acp 先例） |
| `session/new` | ✅ | §4.7 参数立场；每次创建并插入一个独立 harness，其他会话活跃不构成拒绝理由（§15） |
| `session/prompt` | ✅ | ContentBlock 提文本（`prompt_text` 语义照搬 kiqi-acp）；spawn 回合 task；每个会话至多一个活跃回合 |
| `session/cancel` | ✅ | 通知，不应答；§4.4；只取消 `sessionId` 指定的会话 |
| `session/close` | ✅ | 从会话表移除后取消并等待在途回合，再释放会话资源；其他会话不受影响 |
| `session/load` | 批次 2 | resume harness（`allow_existing_session` 路径，`session.rs:706-713`）+ 把 JSONL 分支回放成 `session/update`（kiqi-acp `history.rs` 的回放语义） |
| `session/set_mode` | ❌ | ki 无模式概念 |
| `session/request_permission` | 批次 2 | §4.6 |
| `_session/steering`、`_meta.*` 扩展 | ❌ | 不声明不实现（ABB 侧如实降级，§7） |
| `fs/*`、`terminal/*` | ❌ | ki 工具本地执行，无需委托客户端 |

### 5.3 事件投影（AgentEvent → `session/update`）

| AgentEvent（`events.rs:26`） | ACP | 备注 |
|---|---|---|
| `MessageUpdate`（text delta） | `agent_message_chunk` | 逐 delta（§4.5） |
| `MessageUpdate`（thinking delta） | `agent_thought_chunk` | 只投 delta |
| `ToolExecutionStart` | `tool_call`(pending) + `tool_call_update`(in_progress) | `kind`/`title` 映射表照 kiqi-acp `kind_for_tool`/`title_for_tool`；edit 类带 diff contents（`diff_contents` 语义） |
| `ToolExecutionEnd` | `tool_call_update`(completed) | `isError` 透传 |
| `ToolExecutionUpdate` | 不投影 | abb 同款取舍 |
| `TurnEnd`/`AgentEnd` | 回合收尾 | stopReason 映射见下 |
| `HarnessEvent::CompactionProgress` | `session/update` compact 边界 | 批次 2+，v1 可忽略 |
| 插件 UI 对话框 | `session/request_permission` | 批次 2（§4.6） |

### 5.4 stopReason 映射

| `HarnessRunOutcome`（`agent_harness.rs:94`） | ACP |
|---|---|
| `Completed` | `end_turn` |
| `Aborted`（cancel 路径） | `cancelled` |
| `Suspended` | `max_tokens`（预算语义；批次 1 核对实际语义再定名） |
| `Failed` | **JSON-RPC error**，不自造 stopReason（abb 教训） |

白名单外取值一律视为协议错误——与 abb 客户端（:87-98）对表。

## 6. stdio 纪律与失败面（abb §4 全盘继承）

- stdout **只跑协议**；日志一律 stderr（`diagln` 风格从 kiqi-acp 移植；`KI_ACP_DEBUG=1`
  追踪方法流）；收尾必打一行原因+exit code（Zed 把它当 agent stderr 落日志）；
- 配置失败**响亮**：模型/凭据解析失败 exit 2（print/RPC 已有 fast-fail 先例
  `app.rs:292`），后端装配错误启动时定一次，不等第一条消息；
- 单行畸形 warn 继续；EOF = 正常收工并逐会话 kill；写出失败（管道断）= exit 1；
- spawn 侧纪律（给未来的客户端实现者）：`kill_on_drop` + unix `process_group(0)`
  （abb 客户端 :614-698 的教训，防 MCP 孙进程孤儿）。

## 7. 第一验证方：ABB 客户端（`../abb/src/runtime/acp.rs`）

ABB 已有一个 4710 行的 ACP 客户端。它对 agent 的六点硬假设（实测得出），ki 方案逐条对表：

| ABB 假设 | ki 立场 |
|---|---|
| `protocolVersion: 2` 协商 | **已实测无冲突**（2026-10-16）：v1 守卫在 `unstable_protocol_v2` feature 门后未启用；ABB 的 v2 initialize 照常成功、应答 1，ABB 不读应答版本 |
| `_meta.steering.supported` 如实声明，声明是扩展方法的唯一闸门，禁止错误码探测 | ki 不声明 steering，不实现 `_session/steering`——自洽 |
| 受限会话 fail-closed：`_meta.abbSandbox` 缺失即拒建受限会话 | ki 不声明 sandbox ⇒ ABB 的受限会话会被 ABB 侧拒——**这是 ABB 的决策点，不是 ki 缺陷**；普通会话零改动可用 |
| stopReason 白名单内 | §5.4 映射表对表 |
| `session/new` 收 `systemPrompt`（Field 传输） | §4.7 收下 |
| stdout 纯协议，stderr 才是日志 | §6 |

验收路径：批次 3 用 ABB 客户端实际驱动 `ki acp serve` 跑通一轮（prompt→tool→cancel→恢复），
记录在 handover 文档。

## 8. 触达与验收

- **一致性测试**（进程内，DuplexStream 打协议面，kiqi-acp 测试风格）：分发竞态两条
  （§4.4）、投影单测（`projection.rs` 纯函数）、stopReason 映射表、参数第一刀解析
  （不支持的 MCP transport 拒绝、systemPrompt 生效）、`session/load` 回放、对话簿记竞态三条
  （迟到应答/cancel 清理/关窗不阻塞——kiqi-acp `session.rs:852-966` 同款）、多会话并发与隔离
  （`abb_multi_chat_sessions_run_close_and_reopen`、
  `turns_and_cancellation_are_independent_between_sessions`、
  `session_cwd_system_prompt_and_context_do_not_leak`、
  `close_cancels_the_turn_before_releasing_the_session`，§15）；
- **门禁**：常规 fmt/check/test + 文档快照；新增依赖锁 `=3.3.0`；
- **人工验收**：Zed 真机一轮（批次 2 起）；ABB 客户端一轮（批次 3）。

## 9. 明确不做（边界）

- **不迁 wasm**：`extension-abi.md` §6.2 裁定（独占 stdio + 跨会话服务线程），且
  `837444e` 已记录排除理由；
- 不做 steering、sandbox 元数据、terminal/*、fs/* 委托、auth 方法、`session/set_mode`；
- 不做会话级 `mcpServers` 注入（§4.7 明确拒绝）；
- 不做 ki 会话树到 ACP 的完整暴露（多 lane/分支导航是 TUI 语义；ACP 面只暴露
  load/resume 的活动分支）；
- v1 不做第三方 agent 发现/注册中心（zed 注册是本地 settings one-shot）。

## 10. 批次拆分

| 批次 | 内容 | 验收 |
|---|---|---|
| 1（骨架+验证） | 依赖引入（pin 3.3.0）、`ki acp serve` 入口+生命周期挂接、initialize/session/new/prompt/cancel、投影层、stderr 纪律、响亮失败；**官方 crate 分发模型实测**（§4.4 两条一致性测试） | 进程内协议测试全绿；门禁全绿 |
| 2（交互面） | `session/request_permission`（UiDialogMailbox 桥 + 簿记竞态测试）、`session/load` 回放、Zed 真机验收 | kiqi-acp 同款竞态测试移植全绿；Zed 手测一轮 |
| 3（固化+对接） | `ki acp install-zed`/`--print-zed` one-shot（zed.rs 移植）、ABB 客户端实测（§7）、文档（handover §7 指针、extension-abi §7 待定项销账、README） | ABB 驱动一轮记录在案 |

## 11. 已定 / 待定

**已定**（2026-10-16 立项）：宿主级子命令（§4.1）；进程内驱动、不做桥（§4.2）；协议层
官方 crate pin `=3.3.0`（§4.3）；逐 delta 投影（§4.5）；权限批次 2（§4.6）；abb 的
stdio/失败面纪律全盘继承（§6）。

**待定**：
- ~~官方 crate 分发模型实测结论~~（✅ 批次 1 已收敛，见 §12）；
- ~~`Suspended` → stopReason 的准确命名~~（✅ 批次 1 落定：`end_turn` + stderr 说明，
  deferred resume 留批次 2+）；
- ~~多会话共享进程的插件宿主语义上限~~（批次 1 曾临时裁定为**每进程一个活跃会话**；
  ✅ 2026-10-24 已改为按 `sessionId` 路由的多会话模型，见 §15。legacy compiled-extension
  sink 与部分第三方插件读取进程 cwd 的限制仍未按会话隔离，见 `kiqi-acp/src/sessions.rs` 模块注释，
  不再用它拒绝第二个普通 ACP 会话）；
- zed 注册是否进 `ki acp serve` 默认行为（现按 one-shot 子命令设计）；
- ~~ABB 客户端 `protocolVersion: 2` 握手~~（✅ 2026-10-16 探针实测：**冲突不存在**——
  v1 守卫整个在 `unstable_protocol_v2` feature 门后，ki 只启 `stdio`，编译进去的是
  放行空壳（`protocol_compat.rs:1-46`）；ABB 原版 v2 initialize 照常成功、应答 1，
  ABB 不读应答版本。**注意**：日后若启用该 feature，守卫会激活、ABB 的 2 将被拒，
  届时必须连带配 `protocol_router().with_v1(...)`）。

## 12. 批次 1 状态（2026-10-16，完成）

提交 `9482790`（分支 `feat/acp-serve`，已 ff-合入 main）。交付：`crates/kiqi-cli/src/acp/`
（mod/sessions/projection/diag，1551 行）+ app.rs 接线 + 依赖引入；20 个进程内测试
（投影单测、stopReason 映射表、TurnState/守卫语义、6 个 duplex e2e——FauxProvider
脚本 provider，含 cancel-during-prompt 突变验证）。门禁全绿：workspace 1899 通过/0 失败、
features 门 538 通过、fmt/docs 快照干净。

**§4.4 验证项实测结论**：官方 crate 3.3.0 的分发是**串行内联**（读循环逐帧 await
handler，`incoming_actor.rs:72,101-160`；文档明言回调期间不处理其他消息），逃生口是
`ConnectionTo::spawn` 任务池（与读循环并行的 task_actor）。批次 1 照搬 abb 修正形状：
prompt handler 内联抢 `in_flight` → 移交 `Responder` 进 spawn → 立即 `Ok(())`；
cancel 内联秒达；**100ms tick 兜底确认必要**——lane 在 run 注册前（`acquire_run` 之前的
`NoActiveRun` 窗口）会吞掉单次 abort，三镜头对抗审查用实证探针独立复现（同 burst
cancel 10/10 全量跑完仍答 cancelled），修复为 tick 重发 abort 直到 run 接收，并把
同 burst 测试强化为断言流被截断（原测试在坏/好两个世界都绿，遮住了缺陷）。

**审查轮战果**：8 条发现（去重后 5 个独立问题）全部核实修复——同 burst cancel 丢失
（高危）、Lagged 排干中断（低）、Failed-后-cancel 必须答 cancelled 而非 error（协议
MUST，`was_cancelled` 判定提到 outcome 之前）、`additional_directories` 静默忽略违反
第一刀解析立场（改为 -32602 响亮拒绝）、EOF 收尾的 abort 宽限机制是死代码（连接
future 先于 close 回调被 drop，机制移除、注释修正）。

**遗留到批次 2**：`session/request_permission`（UiDialogMailbox 桥）、`session/load`
回放、zed 注册；**批次 3**：ABB 客户端实测（§7 六点对表 + V1 握手裁决）。

## 13. 批次 2 状态（2026-10-16，wave 1 完成；wave 2 进行中）

提交 `9422100`（`feat/acp-serve-batch2` 分支，已 ff-合入 main）。同树并行完成了一轮
`/code-review max` 清扫（15 项修复一并入库）。门禁：workspace 1932 通过/0 失败、
features 门 571 通过、fmt/文档快照干净；acp 套件 53 个测试。

**session/load**（侦察两项关键事实落地）：ACP id 改为 `ki-<uuidv7>` 从 ki 会话 id 派生
（零持久化、重启稳定）；恢复只走 **ById 语义 + 存在性预检**——`ByExactId` 缺席会新建
空会话（`session.rs:2069-2081`），禁用于 load；`AgentHarness::create` 先完成中断抢救
再回放；活动分支最旧优先回放（仅 message 条目；压缩条目客户端侧损失已注明）；load
尊重 settings `sessionDir`（`resolve_session_dir` 抽取）；`_meta.systemPrompt` 与
session/new 同款收下或 -32602。

**zed 注册**：`ki acp install-zed`/`print-zed` one-shot；**审查头号战果**——从 kiqi-acp
原样继承的 splice 缺陷（分隔符逗号落入尾注释 → 整个 settings.json 不可解析，两种损坏
形状都经独立编译复现）重写为注释/字符串感知的 `live_tail` 扫描（无注释文档字节不变）；
macOS settings 路径按 Zed 文档修正为 `~/.config/zed/`（kiqi-acp 的 Application Support
路径是错的）。

**新待定（harness 层，独立设计活）**：
- ~~`abort()` 在 run 注册前（`acquire_run` 之前）的拒绝被静默丢弃~~（✅ 2026-10-16 已修：
  harness 引入「启动窗口」——`StartingWindow{lane,id}` 绑归属，abort 只 latch 自己窗口、
  `acquire_run` 只消费自己窗口 id，run 以预取消状态起步；空闲 abort 仍严格 no-op。ACP 侧
  100ms tick 与 modes.rs 单发 abort 从此只是冗余保险。见提交 `1a1a31d`）；
- TurnState 与 harness `acquire_run` 双份「单运行」不变量会漂移——根因是 harness 不暴露
  per-run 取消句柄。

**wave 2（`session/request_permission`）方向已定**：mailbox 是纯轮询模型且阻塞发生在
插件 runtime action 内部（`drive_turn` 无感）→ 每会话一个 dialog pump 任务
（`take_pending` → permission 形状经 `cx.send_request(...).block_task()` 往返 →
`respond`/`cancel` 回 mailbox；`purpose=question` 一律取消回原 id、永不重标）；
`session/cancel` 与 EOF 路径必达 `cancel_all`/`detach`（否则插件 poll 挂死，测试钉死）。

**wave 2 完成**（提交 `d6e0be5`，已 ff-合入 main）：`crates/kiqi-cli/src/acp/dialogs.rs`
（552 行）——映射层 + 每会话 DialogPump 落地如上；`session/cancel` 扩展
`pump.cancel_all`，close 先 detach 再派 SessionShutdown，load 同样起泵。acp 套件 68 个
测试；门禁：workspace 1947 通过/0 失败、features 门 586 通过/0 失败。

审查轮 2 条发现：「`$/cancel_request` 义务无测试钉住」被**实证突变驳回**——删
`abort_tasks` 后 `session_cancel_tells_the_client_over_the_wire` 以
"no $/cancel_request reached the client" 红掉，义务早已被钉死（审查者的突变分析结论
错误）；「任务先完成后注册」竞态属实并修复（占位槽与句柄填充在同一把注册表锁下完成）。

**批次 2 至此完成**。批次 3 剩余：ABB 侧部署接线（其命令解析硬接线随包 agent，换解析
目标即可）+ 一轮真实会话实测记录；ki 侧实测**零代码改动**（探针已证 initialize /
session/new / EOF 全通，应答见 §7 表）。Zed 真机手测仍待人工。

## 14. 批次 3 状态（2026-10-22，完成）

**ki 侧零代码改动**（如实：本仓库本批只有文档销账与对接记录）。abb 侧部署接线与验收轮：

- **ABB 回落接线**（abb 线程 `abb-ki-acp-serve-fallback-20261022`，walgit collab 全流程
  issue→claim→patch→review(approve)→merge_result→closed，合并提交 abb `c8a19813` 已推
  origin/main）：abb `src/service.rs` 执行层解析引入 `AgentCmd { program, args }`——随包
  `abb-agent`（主执行层）逐字节不变；无随包（开发/自签构建）回落从 PATH `pi-acp`
  （rpi-package 的 ACP↔RPC 桥，前身实现）换成 **`<ki> acp serve`**（PATH 全路径 + 子命令
  参数；无 ki 保留裸名、spawn 失败如实可见，#246④ 口径）；`AgentConfig.args` 在聊天句柄与
  oneshot 两路径透传；syschat「执行层不可用」文案同步指向 ki。
- **验收轮（§7 对表逐条实证）**：abb `probe_real_ki_acp_serve_lifecycle`（`#[ignore]` +
  `KI_ACP_E2E_BIN` 门控，`src/runtime/acp.rs`）——ABB 客户端真 spawn `<ki> acp serve`
  （pi-rust main 构建，provider deepseek/deepseek-flash）：initialize（v2 握手，ki 应答 1、
  不声明 steering ⇒ ABB 能力探测 false，§7 第一行成立）→ session/new（tempdir cwd）→
  工具轮（真实 LLM，bash 工具落盘 `hello-ki-e2e`，EndTurn）→ 取消轮（`sleep 30` 工具挂住，
  3s idle 超时后 `cancel_with_cleanup` 打断，Cancelled——对应批次 1 钉死的取消贯穿语义）→
  同会话续轮（EndTurn）。作者轮 12.77s / 评审轮 9.30s 各复绿一次；协议流纯净（ki 插件日志
  全在 stderr）。`session/load` 不在 ABB 客户端面上（其重连不回放），如实不在本轮范围。
- **abb 门禁**：fmt/clippy 干净；全量 bin 单测 1346 passed / 2 failed——两条
  `ui::headless_render` 失败在 base `74600f3` 对照实证**同红**（无显示环境先在）；
  `tools/check_test_isolation.sh` 红——泄漏对照实锤为 `mediaplay::poster_is_extracted_and_cached`
  在 base 即向 `<HOME>/.agent-bridge/syschat/media-cache/` 写海报缓存（`3317aca` 引入，
  先在红）。两者均为基线问题，与本批无关。评审：abb-reviewer-145 在钉定提交独立复核
  （fmt/clippy/11 条触面测试/探针复跑 9.30s），approve（blocking=0 non-blocking=2：
  harness 启动日志缺子命令形状、探针 tempdir 泄漏）。walgit CI 分支任务无 runner 认领，
  验证由本地门禁 + 评审独立复跑承担（已记录在 merge_result note）。
- **文档销账（本仓库，本提交）**：README 增 Editor integration (ACP) 小节；`extension-abi.md`
  「明确不迁」名单 `acp` 销账（「不迁 wasm」裁定不变，落地形态即宿主功能）；
  `extension-abi-handover.md` §7 行销账；本文档头部状态行更新为已落地。
- **Zed 真机手测：关闭（2026-10-22，业主裁定）**。实测到一半：`ki acp install-zed` 注册 +
  Zed 真机拉起 `ki acp serve` 并保持连接（spawn/注册面通过，进程与 initialize 实证）；
  完整回合由业主终止——Zed 弃用并卸载。abb 生产发行包继续走随包 abb-agent，`ki acp serve`
  回落路径仅在开发/自签构建触达。

## 15. 多会话模型修正（2026-10-24）

**动机是 ABB 的多聊天渠道生产用法。** ABB 在一个 `ki acp serve` 进程上复用多个聊天
渠道（其 harness 以 `HashMap<Uuid, ChannelMeta>` 保存渠道），而旧实现把「进程内只能有
一个活跃会话」当成硬约束：第一个会话尚未关闭时，第二次 `session/new` 会以
`ki acp serve supports one live session per process; the current session is still live`
失败。旧 ABB agent 支持多会话，因此这不是客户端应适应的新协议限制，而是回落到 ki 后
产生的兼容性回归。

现行模型与不变量：

- `ServerShared::live` 是 `HashMap<SessionId, LiveSession>`。`session/new` 每次创建并
  插入独立会话；只要 id 不冲突，其他会话是否活跃、是否有回合在运行都**不影响新建**。
  `session/load` 同样插入；同一 id 已活跃仍按重复激活拒绝，不能覆盖旧实例。
- `session/prompt`、`session/cancel`、`session/close` 全按 `sessionId` 路由。
  **单活跃回合的粒度是每个会话**：同一会话的第二个并发 prompt 仍返回
  `a prompt turn is already in flight`，不同会话则可并行流式输出；取消 A 不得截断 B。
  取消的 100ms `CANCEL_ABORT_TICK` 兜底唤醒保持不变。
- close 先把 id 从路由表移除，再取消并等待该会话的回合退出，最后关闭对话框泵并释放
  harness / 插件 / MCP 上下文。未知或已经关闭的 id 在 prompt/close 上继续得到
  `-32602` + `unknown session <id>`；load 的未知 id 继续得到原有的
  `unknown sessionId <id>` 错误形状。连接 EOF 仍逐一关闭当时所有活跃会话。
- 每个 `LiveSession` 独立持有 cwd、harness/上下文、事件接收器、system prompt 覆盖、
  turn/cancel 与对话框状态，禁止串话。`initialize` 的协议版本协商和客户端能力快照
  （包括 notices）属于**连接**，能力在会话激活时复制，不随某个会话关闭而丢失。
- 事件 sink 已按会话隔离（2026-10-27）：每次 `session::build` 把
  `install_compiled_event_sink` 返回的 sink 挂到该会话的 `RegistrySnapshot`，`translate`
  的派发点优先走快照；进程级槽位只作无快照路径的 fallback。因此 ACP 同进程多会话的
  extension 事件不再串话。仍属进程级共享的残余是 `extensions::COMPILED_CAPS`
  （工具/命令/probe/prompt transform 的能力集，last-writer-wins）及第三方插件在调用时
  读取进程 cwd 的既有边界，如实保留在 `kiqi-acp/src/sessions.rs`。内置工具的显式 cwd、ACP
  run/cancel、标题与上下文不经过这些全局槽。

兼容性测试以真实协议面钉住上述变化：旧的
`second_live_session_is_refused_with_server_error` 拒绝测试由
`abb_multi_chat_sessions_run_close_and_reopen` 取代，覆盖 A 活跃时新建 B、按 id 分流、
关闭 A 后新建 C、B 始终可继续使用，以及未知/已关闭 id 的错误形状；其余三条分别覆盖
跨会话取消独立、cwd/system prompt/上下文隔离、close 先取消再释放。协议版本协商、
能力声明和同会话单回合错误均未改变。

## 16. crate 提取（2026-10-25）

`crates/kiqi-cli/src/acp/` 六模块整体 `git mv` 为 `crates/kiqi-acp`（§12 批次 1 的
"长大再提 crate" 兑现）。切割面按依赖方向定：

- **协议 crate 持有**：协议机器（initialize / session new·load·prompt·cancel·close、
  多会话路由）、`sessions.rs` 注册表（`LiveSession` / `TurnState` / `InFlightGuard`）、
  `dialogs` / `projection` / `zed` / `diag`，以及 `SessionFactory` /
  `SessionDeps` / `SessionParams` / `SessionRequest` 契约类型。
- **宿主（kiqi-cli `src/acp.rs`）注入**：[`RunOptions`] 两半——生产 factory
  （`session::build` 全管线：provider 解析、chdir、`ownsStdio` 检查、
  `BeforeTuiStart` veto、`SessionStart` advisory）与 `session/load` 存在性
  pre-check（目录解析 + 仓库列表 + `match_session_id`）。`ReloadContext` 不跨
  crate：`SessionDeps` 只带协议侧要驱动的两件东西——`mcp`（边界 drain）与
  `shutdown`（预构建的 `SessionShutdown` advisory future，future 持有 context，
  cdylib 活到 dispatch 之后）。
- **MCP 边界胶水下沉 `kiqi-mcp::boundary`**：`drain_mcp_boundary` /
  `take_mcp_boundary` / `append_notice_batch` / `replace_mcp_tools` 从
  kiqi-cli `session.rs` 迁出（两侧共用；`kiqi-mcp` 由此新增 `kiqi-harness`
  依赖边，manager 本体仍不碰 harness）。
- legacy-layout 迁移调用随 dispatch 移到 kiqi-cli 侧；进程退出码常量
  （1/2）在协议 crate 本地重声明。
- 门禁：workspace 1439 通过（另 feature 门 318：ai 206 / tools 34 / sdk 78；kiqi-acp 72 个 duplex e2e 原样搬入）。
