# 插件契约：trait + cargo feature（编译期接入）

状态：**落地中**（9 个插件已移植，见 §6）· 契约代码 `crates/kiqi-plugin`

## 1. 决定

| 项 | 取值 |
|---|---|
| 契约 | Rust `trait Plugin`（`kiqi-plugin`） |
| 接入 | **cargo feature，18 个包全部编译进 `ki` 二进制**（默认开） |
| 语言 | 只支持 Rust |
| 动态通道 | **保留 cdylib（`rpi-plugin-sdk`）专门伺候 dev**：`ki dev` + watch + `/reload` |
| wasm/WIT | **推迟**，不删：同一个 `Plugin` trait 就是将来 `WasmPlugin` 适配器要实现的接缝，只用于「跑不受信的第三方插件」 |

**为什么放弃 WIT**：WIT 的那套约束（跨语言 guest、无稳定 ABI、canonical ABI 表达不了递归）对"只有 Rust 实现"的契约一条都不成立。于是工具的部分输出、可认领的事件、23 个带类型的运行时动作，在 WIT 里全都别扭，在这里全是直路。

**能力模型（借 wit-bus 的哲学，用 Rust 类型表达）**：不做全局 API，`HostCaps` **每次调用传入**，只装该插件被允许的那几项。`None` 不只是空——插件**无法表达**对它的调用。这就是 wasm 那边"没被链接的 import 不可达"的等价物。

## 2. `RuntimeActionId`（23 个）→ 5 个能力 trait

| trait | 承接的 action |
|---|---|
| `SessionRead` | `GetActiveTools` `GetThinkingLevel` `GetSystemPrompt` `GetCliFlag` `GetSessionBranch` |
| `SessionControl` | `SetSessionName` `SetActiveTools` `SetModel` `SetThinkingLevel` `Compact` `NewSession` `Fork` `NavigateTree` `SwitchSession` `Reload` |
| `InjectApi` | `SendMessage` `SendUserMessage` `AppendEntry` |
| `UiApi` | `UiDialog` `SetStatus` `SetEditorText` `UiComponent` |
| `Log` | —（唯一不被拒的能力） |
| *未归类* | `StdioLifecycle`（属 dev/传输层，不进插件面） |

分层的理由与 wit-bus 把 `bus-facts` / `bus-control` 分居同源：**能力边界 = trait 边界**，而不是一个万能的 `runtime_action(id, json)`。

## 3. 现有 18 个包实测扫描

（对 `rpi-package/packages/*/src` 实测，非印象）

| 包 | 工具 | 事件 | claim | 分步/部分输出 | 对话框 | 用到的 runtime action | 起子进程 |
|---|---|---|---|---|---|---|---|
| rpi-acp | | ✓ | | | ✓ | GetCliFlag, StdioLifecycle | 3 |
| rpi-ask-user | ✓ | | | ✓ | | UiDialog | 1 |
| rpi-codegraph | ✓ | | | ✓ | | — | 1 |
| rpi-goal | ✓ | ✓ | | ✓ | | AppendEntry | 1 |
| rpi-im-message | ✓ | ✓ | | ✓ | | GetCliFlag, SendUserMessage | 1 |
| rpi-langfuse | ✓ | ✓ | | ✓ | | SetStatus | 1 |
| rpi-lens | ✓ | | | ✓ | | — | 1 |
| rpi-mcp-adapter | ✓ | ✓ | | ✓ | | — | 2 |
| rpi-permission-gate | | ✓ | | | ✓ | GetCliFlag, UiDialog | 1 |
| rpi-permissions | ✓ | | | ✓ | | — | 1 |
| rpi-plan-mode | ✓ | ✓ | | ✓ | | AppendEntry, GetActiveTools, GetSessionBranch | |
| rpi-run-stats | | ✓ | ✓ | ✓ | | UiComponent | |
| rpi-server | ✓ | ✓ | | ✓ | | GetCliFlag | 1 |
| rpi-subagents | ✓ | | | ✓ | | — | |
| rpi-todo | ✓ | ✓ | | ✓ | | GetSessionBranch | |
| rpi-voice | | ✓ | ✓ | ✓ | | SendUserMessage, SetEditorText, SetStatus | 1 |
| rpi-webfetch | ✓ | | | ✓ | | — | |
| rpi-websearch | ✓ | | | ✓ | | — | |

### 数据给出的三条结论

1. **16/18 需要分步/部分输出**——这不是边角需求。WIT 草案里的单发 `execute() -> outcome` 会一次打死 16 个包；`trait` 版直接 `&dyn ProgressSink` 就够。这是放弃 WIT 的最硬证据。
2. **14/18 会起子进程**——若走 wasm 沙箱，这 14 个都得开宿主 `process` 能力，**沙箱价值基本归零**。所以"编译期接入"不是图省事，是这批插件的形态本来就与沙箱不兼容。
3. **只有 2/18 用 claim**（`rpi-run-stats`、`rpi-voice`）——用得少但确实在用，`EventAction::Claimed` 保留。

### 覆盖核对

- 4 个包（`rpi-acp` `rpi-permission-gate` `rpi-run-stats` `rpi-voice`）**不提供工具** → `Plugin::tools()` 默认空集即可；
- 6 个包**没有事件处理器** → `on_event` 默认 `Continue`；
- 实际用到的 action 只有 10/23，其余 13 个是**已声明未使用**；`HostCaps` 按需给即可，不预开放。

## 4. 迁移路径

1. 各包 `impl kiqi_plugin::Plugin`，用 `export_plugin!` 之外的一个新宏/工厂暴露；
2. `ki` 的 `Cargo.toml` 加可选依赖 + feature（`default = [18 个全开]`）；
3. `HostCaps` 由宿主按部署策略构造（现阶段=全给，等有第三方插件再收紧）；
4. `catch_unwind` 逐插件隔离保留（对应 wit-bus 的 trap 路径）；
5. cdylib 通道保留，dev 回路不动。

## 5. 两层的边界（依据实测，非印象）

编译期接入**不是**要把所有插件都收进来。cdylib 通道保留，而且**有它该管的一类**。

### 判据（三条，任一命中即留原生）

1. **长连 / 服务生命周期**：插件自己维持一个长连接或服务循环（WebSocket、设备流、server）。
   → 编译期模式下"更新插件 = 重编 `ki` 二进制"，对常驻服务是实打实的坏处。
2. **驱动子进程**：插件需要起并管理外部进程。
   → 进程内模型不禁止（已移植的 `rpi-codegraph` 就在起 `codegraph`），但**与长连叠加**时归属原生。
3. **需要在调用之外行动**：插件要起后台线程 / 定时器，在两次调用之间主动推送
   （`rpi-run-stats` 就是一个 1 秒 tick 的线程，注释写明"即使没有流式事件也保持等待时间与
   TPM 窗口新鲜"）。
   → `HostCaps` 是**每次调用借来**的，后台线程拿不到它；原生 ABI 能这么干，是因为它给的是
   全局 `Runtime`（函数指针 `Copy + Send`）。这是两种模型的真实差异，不是实现疏漏。
4. **宿主 vtable 散射进签名**：把 `runtime_action` / `free_string` 裸函数指针存进一个 context 结构
   并到处传参（`rpi-im-message` 的 `StartupContext` 即如此）。
   → 穿 `HostCaps` 要改的签名面比 goal + plan-mode 加起来还宽，收益只是少一个 dylib。

### 名单

| 插件 | 归属 | 依据 |
|---|---|---|
| `rpi-todo` `rpi-lens` `rpi-webfetch` `rpi-websearch` `rpi-subagents` `rpi-permissions` `rpi-goal` `rpi-plan-mode` `rpi-codegraph` | **编译期**（已移植，见 §6） | 纯进程内，宿主契约窄（0~5 处） |
| `rpi-ask-user` | 编译期（已移植） | 纯进程内 |
| `rpi-langfuse` | 编译期（待办） | 纯进程内（14 个事件，无后台线程） |
| `rpi-run-stats` | **原生** | 1 秒定时线程主动推送（判据 3） |
| `rpi-im-message` | **原生** | 飞书 WebSocket 长连 + 专用服务 runtime + 驱动 `rpi print` 子进程 + `StartupContext` 散射（判据 1+2+3 全中） |
| `rpi-voice` | **原生** | 录音设备流长连（判据 1） |
| `rpi-mcp-adapter` | **原生** | 管理外部 MCP 子进程的 stdio 生命周期（判据 2） |
| `rpi-acp` | **原生** | stdio 生命周期（`StdioLifecycle`）（判据 2） |
| `rpi-server` | **原生** | 服务端（判据 1） |

**判据里没有"代码行数"**：`rpi-langfuse` 有 4227 行却该搬（纯事件），`rpi-im-message` 2122 行却该留
（结构属性）。行数不预测移植成本，契约宽度才预测。

### 贡献面（`Plugin` 该实现多少）

原生 ABI 有 12 个注册槽，实测**只有 6 个有真实使用者**：

| 槽 | 使用者 | `Plugin` |
|---|---|---|
| `register_tool` | 14 个包 | `tools` |
| `register_event_handler` | 11 个包 | `on_event`（37 tag） |
| `register_command` | 6 个包 | `commands` + `run_command` |
| `register_flag` | 3 个包 | `flags`（**声明**，值经 `SessionRead::cli_flag` 读） |
| `register_shortcut` | 2 个包 | `shortcuts`（**声明**，按键走 `Input` 事件） |
| `register_before_agent_start` | 2 个包 | `before_agent_start`（真钩子，返回替换的系统提示词） |
| `register_provider` / `_message_renderer` / `_markdown_transformer` / `_entry_renderer` / `_resources_discover` / `_oauth_provider` | **无人用** | 不实现（实测，非省事） |

加 `probes`（权限闸门）与 `shutdown`，`Plugin` 共 **8 个贡献面，每个都有真实使用者**。

## 6. 移植进度

已移植并通过 **caps 级验收**（断言宿主被要求做了什么，而非"加载成功"）：

```
rpi-todo · rpi-lens · rpi-webfetch · rpi-websearch ·
rpi-subagents · rpi-permissions · rpi-goal · rpi-plan-mode · rpi-codegraph · rpi-ask-user
```

`rpi-goal` 与 `rpi-plan-mode` 的宿主访问在**工具路径**上，因此用窄 trait 双适配器
（`AbiHost` 走 `call_runtime` / `CapsHost` 走 `HostCaps`）穿参——**不合成 `PluginApi` vtable**，
那会把 C ABI 拖回编译期通道。

### 度量教训

排批次时用过两个**测不到目标**的指标，都已废弃：

- `call_runtime(` 计数 → 对使用包装函数（`invoke_runtime_action`）的包返回 0；
- `RuntimeActionId::` 计数 → 对**故意钉裸数字 id** 的包（`rpi-ask-user` 用 17、`rpi-langfuse` 用 18，
  理由写在注释里：不能依赖会漂移的 SDK 枚举）返回 0。

正确度量是三者**取并集**：`RuntimeActionId::X` + 裸数字 id + 包装函数名。

## 7. rpi-langfuse 端口规格（唯一待搬项，规格已完备）

实测（4227 行 / 94 个函数 / 14 个事件处理器 / 1 处宿主调用 / 无后台线程）。

### 7.1 唯一宿主调用

`fn set_status(text: &str)` 发的是：

```rust
let args = json!({ "key": "langfuse", "value": text }).to_string();   // 裸 id 18
```

**正好就是 `UiApi::set_status` 的形状**（`{key, value}`），所以：

```rust
pub(crate) trait StatusHost {
    fn set_status(&self, text: &str);
}
struct AbiHost;                        // 保留 `RUNTIME_ACTION_SET_STATUS` = 18 的原调用
pub(crate) struct CapsHost<'a>(pub &'a kiqi_plugin::HostCaps<'a>);
// CapsHost: caps.ui.ok_or(..)?.set_status(&json!({"key":"langfuse","value":text}))
```

`set_status` 的调用点在 5 处（1559 / 1561 / 1584 / 2093 / 3256），都在 flush 路径深处，
所以 `StatusHost` 要沿调用链穿到那里——与 `rpi-goal` 的 `GoalHost` 同一手法。

### 7.2 14 个处理器

**只有 4 个真的读载荷**：`on_before_agent_start` · `on_before_provider_request` ·
`on_session_compact` · `on_session_tree`。其余 10 个只看 tag。

改造是机械的：

```rust
// 原生
extern "C" fn on_x(event: StablePluginEvent, _: *mut c_void) -> i32
let data_str = event.data_text().unwrap_or_default();
let data: Value = serde_json::from_str(&data_str).unwrap_or(Value::Null);

// 编译期
pub(crate) fn on_x(event: &kiqi_plugin::Event, host: &dyn StatusHost) -> i32
let data: Value = event.payload.clone();     // 本来就是 JSON
```

**关键点**：langfuse 自己就是 `data_text()` → `from_str` → `data.get(..)`，
所以"载荷走 JSON"的设计与它天然吻合——当初若给每种载荷做类型化访问器，这一步反而更重。

### 7.3 结构性障碍（唯一真正费工的地方）

**13 个处理器写在 `rpi_plugin_sdk::event_handler! { ... }` 宏块里**。编译期路径要把它们
变成普通函数（去掉宏），dispatch 由 `Plugin::on_event` 按 `event.tag` 做 14 路 match。

这一步要手工做，且要留出编译修错的余量——批量替换会改坏定义行（plan-mode 的教训）。

### 7.4 验收

caps 级：一次 `SessionStart` 后状态被重置；一次 `before_agent_start` 后 trace 状态建立；
**且 14 个 tag 的 dispatch 一一对应**（断言每个 tag 都落到预期处理器，而不是 `_ => {}` 静默吞掉）。

### 7.5 实测修订（写 §7 之后量出来的，比原先估计大）

**`set_status` 穿参只有 4 个函数**，不是"94 个里的一串"：

| 调用点 | 所在函数 | 处理 |
|---|---|---|
| 1559 / 1561 | `flush_ended_spans(include_open: bool)` | 加 `host: &dyn StatusHost` |
| 1584 | `report_flush_error(err: &str)` | 加 `host: &dyn StatusHost` |
| 2093 | `on_before_agent_start(event, _user_data)` | 处理器，本来就有 `host` |
| 3256 | `extern "C" fn destroy(h: StepHandle)` | **随 StepHandle 机器一起删除** |

两个辅助函数的调用方在 1589 / 1595（flush tick），一并穿。

**第二块：StepHandle 机器必须整体丢弃（3038–3105，约 70 行）。**

`start(params, free, builder) -> StepHandle`、另有 3 个返回 `StepHandle` 的函数、以及
`poll(h, ...) -> StepResult` / `cancel(h)` / `destroy(h)`。它存在**只因为 C ABI 不能阻塞**——
而 langfuse 的 3 个工具（`langfuse_score` / `langfuse_prompt` / `langfuse_trace`）走的是**阻塞 HTTP**，
所以原生侧必须拆成 start/poll 两段。

编译期路径下这整套没有存在理由：调用可以直接阻塞。做法与 `rpi-ask-user` 一致——
**丢机器、写成同步循环**，而不是照搬（"移植后必须更简单，不能更复杂"）。

净效果：这一块不是"改 70 行"，是**删 70 行 + 3 个工具的调用路径重写**。

### 7.6 因此这需要一整轮预算

按上面的实测，langfuse 的移植是一次完整会话的工作量（8 个宏块 + 14 个处理器 + 4 个函数穿参
+ 删 StepHandle 机器并重写 3 个工具 + kiqi.rs + 接线 + 验收）。
**不要在一轮预算的尾巴上开它**——中途断掉只能把 langfuse 从工作区摘掉，等于白做。

执行顺序建议（每步可编译、可回退）：
1. 拆 8 个宏块 → 14 个处理器变普通 fn（签名 `(&Event, &dyn StatusHost) -> i32`），编译。
2. 删 StepHandle 机器，3 个工具改同步，编译。
3. 加 `StatusHost` trait + `AbiHost`/`CapsHost`，穿 4 个函数，编译。
4. 写 kiqi.rs（14 路 match），接线（feature + 注册表），跑 caps 级验收。
