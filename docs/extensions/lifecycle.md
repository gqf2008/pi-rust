# 扩展生命周期事件

本文描述当前实现，替代旧的生命周期事件总线设计稿。事件标签与 trait 契约见
[kiqi-host](../../crates/kiqi-host/src/lib.rs)，分发实现见
[translate.rs](../../crates/kiqi-host/src/translate.rs)。（cdylib 通道已于
2026-10-26 退役，其 C-ABI 注册机也已在 2026-10-27 删除——见
[extension-abi.md](../design/extension-abi.md) §6.49；下文「扩展」的权威载体是
wasm guest。）

`BeforeTuiStart` 是 ki 特有的启动事件，在 TUI 初始化前发送。`SessionStart` 和
`SessionShutdown` 处理会话启动与关闭，`SessionTree` 在会话树移动后通知。生命周期
事件使用无 payload 的 `Event`（`payload` 为 JSON `null`），经 `translate` 漏斗送达
编译期/wasm sink：`dispatch_lifecycle_event` → `CompiledEventSink` →
`Extension::on_event`。

事件是**观察性**的：扩展契约的 `EventAction` 只有 `Continue`/`Claimed`/`Patch`,
没有 abort/veto，所以扩展不能否决启动或关闭。需要阻止某个动作时，扩展应在对应
的可路由事件（如 `Input`、`ToolCall`）上返回 `Claimed`，而不是依赖终止回调。

宿主调用 `on_event` 时按 sink 的派发模式决定是否等待忙碌的 guest：observe 漏斗
跳过忙的 guest，claiming 漏斗等待其结论。panic 在宿主侧被隔离，不会展开穿过
wasm 边界。

扩展注册、支持的事件与工具生命周期见 [扩展开发指南](authoring.md)。
