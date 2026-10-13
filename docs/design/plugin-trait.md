# 插件契约：trait + cargo feature（编译期接入）

状态：**P1 落地中** · 契约代码 `crates/kiqi-plugin`

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
