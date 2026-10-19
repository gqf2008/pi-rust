# 方案：软中断（Esc 只停当前动作，不终止整轮）

> 状态：**阶段 1、2、3 已全部落地（2026-10-19，`4902ab7` / `3fa2aa7` / `cf7e9c5`+`7e9377f`）**。目标：把 Esc 从“终止整个 run”改为
> “取消当前工具调用/模型流，保留 run 与 steering 队列”，由 LLM 在看到插入消息后
> 决定停止、收尾还是继续。
> 关联：`signal-driven-agent.md` §5（MUI 输入）、`plugin-abi.md` §6.38
> （wasm `proc.cancel-requested`）、`ki-mcp-builtin.md`。

## 1. 现状（实核）

| 事实 | 证据 |
|---|---|
| TUI 的 Esc 调用 `abort_run_restoring_queue` | `crates/kiqi-cli/src/interactive_tui.rs` 的 INTERRUPT 分支 |
| 它先把 steering/follow-up 还回编辑器，再 `lane.abort()` | `crates/kiqi-cli/src/interactive_tui/events.rs` 的 `abort_run_restoring_queue` |
| `abort` 取消 run 的根 token，整个 run 结束 | `crates/kiqi-harness/src/agent_harness.rs`（`signal` 是根 token 的子 token） |
| 工具收到的是 run signal 的 child token | `crates/kiqi-agent/src/agent_loop.rs`（`let child_token = config.signal.child_token();`） |
| provider 流也使用 run signal | `crates/kiqi-agent/src/hooks.rs`（`to_stream_options` 把 `self.signal` 放进 `SimpleStreamOptions.signal`） |
| 流返回 `Aborted` 时 loop 直接结束 run | `crates/kiqi-agent/src/agent_loop.rs`（`StopReason::Error | Aborted` 分支） |

因此现在是：**Esc = 硬取消整轮**。排队的 steering 不丢（回到编辑器），但 LLM 没有机会
在当前上下文里看到它，当前轮的任务被终止。

## 2. 目标语义

| 输入 | 行为 |
|---|---|
| **单次 Esc（Working 中）** | 软中断：取消当前工具或模型流；**run 继续**；loop 随后 drain steering，把排队消息注入上下文；LLM 决定停止/收尾/继续 |
| **双次 Esc（Working 中，500ms 内）** | 硬中断：保持现有 `abort_run_restoring_queue`（还回队列 + 终止 run） |
| **Esc（Idle）** | 保持现有 double-escape 行为（默认 tree 选择器等），不改变 |
| 不可取消的工具（write/edit 等） | 不硬杀；工具完成后在第一个安全点注入 steering |
| 软中断后没有排队消息 | 取消当前动作，run 继续；LLM 看到被取消的工具结果后自行决定 |

软中断**不**设置 signal/goal 的“Esc 静音”。它只是“停当前动作”，不是“终止任务”；
硬中断才设置静音。

## 3. 设计

### 3.1 中断令牌

新增一个与 run signal 分离的 `InterruptControl`：

```rust
pub struct InterruptControl {
    /// 当前正在执行的工具或流的 token（由 loop 安装/清除）。
    current: Mutex<Option<CancellationToken>>,
    /// 最近一次中断是否为软中断。
    soft: AtomicBool,
}
```

- `soft_interrupt()`：`soft = true`；取消 `current` 的 token。
- `install(token)` / `clear(token)`：loop 在每次 stream/tool 前后安装和清除当前 token。
- run signal 仍然是父 token：硬 abort 会取消 run signal，从而取消当前工具/流。

### 3.2 工具调用

- `run_one_tool_call` 现在创建 `config.signal.child_token()`；改为：
  1. `let child = config.signal.child_token();`
  2. `config.interrupt.install(child.clone());`
  3. `tool.execute(..., child, ...)`
  4. `config.interrupt.clear(child);`
- 软中断取消 child；工具返回（bash/proc/MCP/orchestrator 都观察 token），loop 继续。
- 工具的 `Err` 结果照常写成 tool result；LLM 在下一轮看到“被取消”和 steering 消息。

### 3.3 模型流

- `stream_assistant_response` 现在使用 `config.signal` 作为 provider 的 signal；改为
  每次请求创建一个 child token：
  1. `let stream_token = config.signal.child_token();`
  2. `config.interrupt.install(stream_token.clone());`
  3. `opts.signal = stream_token.clone();`
  4. 结束后 `config.interrupt.clear(stream_token);`
- 软中断取消 stream token；provider 按约定发出 `Error { reason: Aborted }`。
- loop 在 `StopReason::Aborted` 分支增加判断：
  - 如果 `config.signal.is_cancelled()` → 硬 abort，保持现状；
  - 否则如果 `config.interrupt.take_soft()` → **丢弃这条被软中断的 assistant 消息**
    （从 `context.messages` 和 `new_messages` 移除），drain steering；有排队消息就
    继续下一轮，没有就让 LLM 基于取消结果决定（继续/结束）。
- 丢弃被中断的 assistant 消息，避免上下文里留下半条空回复。

### 3.4 Harness / Lane

- `AgentLane` 增加：
  ```rust
  async fn interrupt(&self) -> HarnessResult<InterruptResult>;
  ```
- 活动 run 注册 `Arc<InterruptControl>`；`interrupt()` 路由到当前 run，没有 run 时
  返回 `NoActiveRun`。
- `abort()` 保持不变（硬中断）。
- `InterruptResult`：`Soft` / `NoActiveRun` / `NothingToInterrupt`，供 TUI 显示。

### 3.5 TUI Esc

当前 INTERRUPT 分支改成：

1. 计算 `now - last_escape_time`：
   - 若在 500ms 内且当前 Working → **硬中断**：`abort_run_restoring_queue`；
   - 否则 → **软中断**：`lane.interrupt().await`，不还原队列、不 abort。
2. 软中断不做 `record_abort()`；硬中断保持 `record_abort()`。
3. 更新 `last_escape_time`。
4. Idle 时保持现有 double-escape action。

UI：
- 软中断后显示一行 dim note：“Interrupted the current tool; queued input continues.”
- 如果 `NothingToInterrupt`（例如正在等 provider 连接），退化为硬中断或提示。

## 4. 不变量

- **I1 软中断不终止 run**：run signal 未被取消；loop 继续。
- **I2 硬中断仍然终止 run**：双 Esc 与现有行为一致，排队消息回到编辑器。
- **I3 steering 不丢**：软中断后 loop 在安全点 drain；没有安全点之前消息留在队列。
- **I4 不硬杀原子变更**：只取消工具 token；不观察 token 的 write/edit 完成后再注入。
- **I5 不静音自动续跑**：软中断不设置 signal/goal 的 mute；硬中断才设置。
- **I6 目标/信号预算不变**：软中断不是新的唤醒源。

## 5. 分期

| 阶段 | 内容 | 状态 |
|---|---|---|
| 1 | 工具软中断：`InterruptControl` + `AgentLane::interrupt` + 单/双 Esc | **已落地** `f7013cb` + `4902ab7` |
| 2 | 流软中断：provider token、丢弃半条消息、有排队则续流 | **已落地** 本卡（见 §5.2） |
| 3 | 审计日志、`InterruptResult` 细化 | **已落地** `cf7e9c5` + `7e9377f`（见 §5.3） |

阶段 1 先解决最痛的长工具（bash/dispatch）；阶段 2 覆盖“模型正在生成时按 Esc”。

### 5.1 阶段 1 落地记录

| 位置 | 实现 |
|---|---|
| `crates/kiqi-agent/src/interrupt.rs` | `InterruptControl`：`install` 发单调 `InstallId`、`clear(id)` 只在 id 仍匹配时清槽（并行工具下 A 后完成不会清掉 B）、`soft_interrupt` 只 cancel 已安装的 child token、`take_soft` 一次性 |
| `crates/kiqi-agent/src/agent_loop.rs` | `run_one_tool_call` 在 `execute` 前后 install/clear；**clear 后仅当该动作自己的 token 被取消时 `take_soft()`** |
| `crates/kiqi-harness/src/agent_harness.rs` | `ActiveRun.interrupt`；`AgentLane::interrupt() -> HarnessResult<InterruptResult>`；无 run 返 `Ok(NoActiveRun)`（尽力而为，不像 `abort()` 返错） |
| `crates/kiqi-cli/src/interactive_tui/state.rs` | `esc_action` / `sync_escape_window` 纯函数：Working + 窗口外 → `SoftInterrupt`，Working + 500ms 内 → `HardAbort`，非 Working → `PassThrough` |
| `crates/kiqi-cli/src/interactive_tui/events.rs` | `soft_interrupt_run`：不置 `Aborting`、不 `record_abort()`、不还原队列，只落一行 dim 提示 |

两条设计上的判断（评审确认过，后续阶段不要回退）：

1. **`clear` 用 `InstallId` 而非 token 比较**。tokio-util 0.7 的 `CancellationToken`
   没有 `PartialEq`/`Hash`/指针访问器，`TreeNode` 私有，token 身份在 crate 外不可观测。
2. **`NothingToInterrupt` 会“上膛”**：该 arm 仍然置 soft 标志，于是**下一个**安装的动作
   一开始就是已取消状态；因此 CLI 的提示写成 “Nothing in flight right now; the next step
   will be stopped.”，不能说“没有东西可中断”。阶段 2 把流也 install 之后，这个窗口会
   收窄到“两个动作之间”，但语义不变。

### 5.2 阶段 2 落地记录

| 位置 | 实现 |
|---|---|
| `crates/kiqi-agent/src/agent_loop.rs` | `stream_assistant_response` 返回 `StreamedTurn::{Message, SoftInterrupted}`；每次请求用 `config.signal.child_token()` 作为 `opts.signal`，并 `install`/`clear`（`clear` 在折叠与分类之间，每个过了 `install` 的路径恰好 clear 一次） |
| 同上 | 软中断判定需**四条同时成立**：`stop_reason == Aborted`、`stream_token.is_cancelled()`、`!config.signal.is_cancelled()`、`take_soft()` |
| 同上 | 命中时：`added_partial` 则 `context.messages.pop()`，并 emit `MessageEnd` 收尾 TUI 的流式块；无 `Start`（连接期就被取消，provider 直接回 `Error { reason: Aborted }`）则既不 pop 也不 emit——没有块可收尾 |
| 同上 | `settle_partial` **只在非软路径**调用（分类之后）。它在无 `Start` 时会 push 最终消息，若在折叠里就执行，软分支（只 pop `Start` 推的那条）来不及撤回，被中断的半条 assistant 会留在 `context.messages`，下一次请求就看得见 |
| 同上 | 有排队消息 → `drain_steering` 后 `continue`（注入并重新请求）；无排队 → `break` 出内层循环，交给既有 follow-up 检查，最终 `LoopOutcome::Completed` |

阶段 2 有两处**偏离方案原文**的判断（后续不要回退）：

1. **无排队消息时不重发请求，而是结束本 turn。** 方案 §3.3/§6.1 写的是“继续，让 LLM
   决定”；这对工具调用成立（模型能看到被取消的 tool result），对流不成立——流被中断时
   没有任何 tool result 可看，立刻重发只会让模型把同一段话重说一遍，用户会以为 Esc
   没生效。结束本 turn 仍然是 `Completed`（不是 `Aborted`），run signal 未取消（I1）、
   也不设静音（I5）。
2. **不新增 `AgentEvent` 变体。** `AgentEvent` 是插件契约的一部分（`kiqi-extensions` 的
   `EventTag` + `plugin-abi.md`），为一条丢弃消息新增变体得不偿失。改用
   “emit `MessageEnd` + 从 `new_messages` 移除”。**已知副作用**：TUI 当场能看到被中断
   的那一段（靠 `MessageEnd` 收尾流式块），但重新加载会话后它不在了——因为会话文件
   里确实没有它。
3. **`settle_partial` 延后到分类之后。** 阶段 2 初版在折叠函数返回前就落句，于是
   “连接期被取消、没有 `Start`”的形状（`added_partial == false`）会把 Aborted 的
   assistant 先 push 进上下文；软分支只 pop 有 `Start` 的那种，于是“Esc 打断的那条
   回复”泄漏进下一次请求。现在折叠只发 `Start`/`MessageUpdate`，落句由
   `stream_assistant_response` 在非软路径完成；软路径的两种形状分别是
   “pop + `MessageEnd`”与“什么都不做”。回归用例：
   `a_stream_soft_interrupt_without_a_start_leaves_no_assistant_behind`
   （断言第二次请求的上下文是 `["user","user"]`，且 assistant 的
   `MessageStart`/`MessageEnd` 成对）。

### 5.3 阶段 3 落地记录

| 位置 | 实现 |
|---|---|
| `crates/kiqi-agent/src/interrupt.rs` | `ActionKind { Tool, Stream }`；`install(token, kind)` 把它存进 `Current`；`current_kind()` 读取，`has_current()` 定义在它之上——「有没有动作」与「是什么动作」共用一次加锁，不会互相矛盾 |
| `crates/kiqi-agent/src/agent_loop.rs` | 流请求 install `Stream`、工具调用 install `Tool` |
| `crates/kiqi-harness/src/agent_harness.rs` | `InterruptResult::Soft { action: ActionKind }`；`interrupt()` 用 `current_kind()` 取代 `has_current()` 并把种类报给 UI |
| `crates/kiqi-cli/src/interactive_tui/events.rs` | 两条文案 + 纯函数 `soft_interrupt_note(ActionKind)`：「Interrupted the current tool call; …」/「Interrupted the reply; …」 |
| `crates/kiqi-agent/src/interrupt_trace.rs` | `KI_DEBUG_INTERRUPT=1`（或路径）→ `<temp>/kiqi-interrupt.log`，记 `press installed=<bool> kind=<..>`、`latched-cancelled kind=<..>`、`decision=<soft\|nothing\|no-run> kind=<..>` |

两条不可回退的判断：

1. **第 3 轮那条 `!contains("tool")` 断言必须保持反转后的方向。** 它当时写成「不得出现 tool」是因为
   `Soft` 不携带信息、一条文案要同时覆盖工具与流；现在 `Soft` 带 `ActionKind`，两条文案各自点名自己的
   目标。把断言改回去等于退回信息缺失。
2. **trace 一律在释放锁之后写。** `soft_interrupt` 与 `install` 都在 `InterruptControl` 自己的 mutex 下，
   而 `install` 每个 run 的每次工具调用与每次流请求都会走一次；在锁内写文件会让一个慢磁盘变成整个
   agent loop 的停顿。三个调用点都是「先把 kind/installed 拷进局部变量 → drop guard → 再写日志」，
   模块头与调用点注释都写了这条理由——它是后续改动最容易在「仍能编译」的情况下破坏的约束。

trace 是纯诊断：无人回读、不参与任何返回值或时序、所有 IO 错误被吞掉、env 未设时只花一次 `OnceLock` 查找。

## 6. 开放问题

1. 软中断后**没有**排队消息时：run 继续让 LLM 自行决定，还是直接结束本轮？
   已定（阶段 2）：**工具调用**继续（LLM 应看到被取消的 tool result 再决定）；
   **模型流**结束本 turn（没有 tool result 可看，重发只会重说一遍）。两者都是
   `Completed` 而非 `Aborted`，run signal 不取消。
2. 单 Esc 在“流式生成”和“工具执行”中语义一致（软中断）；双 Esc 始终硬中断。
3. 双 Esc 与现有 `double_escape_action`（默认 tree）的窗口冲突：Working 中双 Esc
   优先硬中断；Idle 中保持 tree。
4. 软中断是否算一次“用户介入”来重置 goal/signal 链？本方案：不算；硬中断才算。
5. wasm 工具：`proc.cancel-requested` 已能观察工具 token；其他 wasm 长调用
   （ask_user）是否也应轮询该函数，留到后续批次。

## 7. 测试

- `kiqi-agent`：用一个等待 token 的 faux 工具，软中断后工具返回、loop drain steering、
  run 继续；run signal 取消时仍走 `LoopOutcome::Aborted`。
- `kiqi-agent`：一个会发 Aborted 的 faux stream，软中断后丢弃半条 assistant 消息并
  继续 drain steering。
- `kiqi-harness`：`interrupt()` 路由到活动 run；无 run 时返回 `NoActiveRun`；硬 abort
  不受影响。
- `kiqi-cli`：Esc 单/双的键处理（单软、双硬、Idle 保持 tree）；排队消息在软中断后
  仍被消费。
- wasm orchestrator：已有 `a_cancelled_wasm_dispatch_returns_immediately`；再补一个
  宿主取消工具 token 的端到端用例（可选）。
