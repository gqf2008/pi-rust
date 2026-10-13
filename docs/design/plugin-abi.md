# 插件 ABI：从 cdylib 到 wasm 组件

状态：**草案（P1 契约）** · 目标版本 `kiqi:plugin@0.1.0`

## 1. 为什么换

现在的 `rpi-plugin-sdk` 是一套手搓 C ABI：`cdylib` + `libloading` + `StbString`
+ `abi_version: 4` + 裸函数指针。代价：

- 任何签名变更都要抬 ABI 版本，宿主与插件必须同 Rust/SDK 版本编译；
- 插件是**全权限**原生代码，没有沙箱；
- 每个平台一个 `.dylib/.so/.dll`，macOS 还要签名；
- 错误只能靠裸指针 + 约定传递。

换成 **trait + wasm 组件模型**后：ABI 由 WIT 描述（不再手写 vtable），插件跑在
沙箱里，单个 `.wasm` 跨平台分发且**不需要签名**，权限按 WIT import 授予。

## 2. 术语（已定）

| 概念 | 叫法 |
|---|---|
| 总称 | **插件 / plugin** |
| 旧实现（cdylib） | **原生插件**（native plugin） |
| 新实现（wasm 组件） | **wasm 插件 / wasm 组件** |
| 强调隔离 | 沙箱插件 |

不用「模块」：wasm 里 core module 与 component 是两个不同的东西，且中文语境下
「模块」更偏代码组织单元（Rust `mod`、ES module）。

## 3. 契约

契约在 [`plugins/kiqi-plugin.wit`](../../plugins/kiqi-plugin.wit)。关键取舍：

- **工具参数用 JSON 字符串**，不在 WIT 里建模整套 JSON Schema —— 否则 ABI 会
  被 JSON Schema 的每个构造拖住。
- **错误是 typed variant**（`unknown-tool` / `invalid-arguments` / `denied` /
  `failed`），运行时要能区分「模型用错了」和「插件坏了」。
- **`event` 有 `other(kind, payload)` 逃逸分支**：新增事件类型不会破坏老插件。
- **包版本即 ABI 版本**，宿主加载时做闭区间校验，取代 `abi_version: 4`。

## 4. 宿主侧 `Plugin` trait（草案）

```rust
pub trait Plugin: Send + Sync {
    /// 稳定标识，用于日志前缀与配置寻址（如 "rpi-goal"）。
    fn id(&self) -> &str;

    /// 一次性注册；对应 WIT `tools.list-tools`。
    fn tools(&self) -> Vec<ToolSpec>;

    /// 调用；对应 WIT `tools.call-tool`。参数/结果都是 JSON 字节。
    fn call(&self, tool: &str, arguments: &[u8]) -> Result<Vec<u8>, PluginError>;

    /// 事件钩子；对应 WIT `lifecycle.on-event`。默认空实现。
    fn on_event(&self, event: &Event) -> Result<(), PluginError> {
        Ok(())
    }

    /// 关闭；对应 WIT `lifecycle.shutdown`。
    fn shutdown(&self) {}
}
```

**两种实现共用这一个 trait**：

- `NativePlugin`：把现在的 `libloading` 加载路径包成 `Plugin`（P1，现有扩展零改动）；
- `WasmPlugin`：`wasmtime::component::bindgen!` 加载 `.wasm`（P2）。

上层（工具注册、事件分发、`~/.ki/agent/plugins/` 扫描）只认 `Plugin`，不再知道
底层是 cdylib 还是 wasm。

## 5. 权限模型

WIT `world plugin` 里 **import 什么，插件才有什么**。P1 只开 `log` +
`session-info`。后续逐个增加（`fs`、`exec`、`http`…），且每个能力都要：

1. 在 `.wit` 里声明；
2. 宿主侧实现时按调用方身份做限制（路径白名单、命令白名单、配额）；
3. 有明确的拒绝错误（`plugin-error.denied`）。

## 6. 里程碑

| | 内容 | 验收 |
|---|---|---|
| **P1** | 定契约（本文件 + `.wit`）；抽 `Plugin` trait；现有 cdylib 改成其一个实现 | `ki` 行为不变，现装 3 个插件照跑；测试全绿 |
| **P2** | 加 wasmtime 后端；guest SDK crate；把 `rpi-todo` 编成 `.wasm` 跑通 | 同一份 `.wasm` 在 macOS/Linux 都能加载并调用工具 |
| **P3** | 其余插件迁移；下线 C ABI；目录 `plugins/` 与 CLI `--no-plugins` | 删掉 `rpi-plugin-sdk` 的 vtable 层 |

## 7. 待定

- wasmtime 依赖很重（编译时间 + 体积）：单独立 crate `kiqi-plugin-host`，还是
  做特性开关？
- `.wasm` 是否需要签名/摘要校验（防替换）？
- 插件配置从哪来（`~/.ki/agent/plugins/<id>.json`？）还是走 WIT？
- `wasm32-wasip2` 的 Rust 工具链在生产上够稳吗（需要锁版本）。
