# 插件 ABI：从 cdylib 到 wasm 组件

状态：**推迟为第一方契约；作为第三方（不受信）层推进验证** · 目标版本 `kiqi:plugin@0.2.0`

> **本方案不再作为第一方插件的契约。** 实测 `rpi-package` 的 18 个包后改为
> 「Rust `trait Plugin` + cargo feature 全编译接入」，见
> [plugin-trait.md](plugin-trait.md)：16/18 需要分步/部分输出、14/18 会起子进程，
> 前者在 WIT 里别扭，后者让沙箱价值归零。
>
> 本文与 `plugins/kiqi-plugin.wit` **保留为 wasm 层（不受信第三方插件）的设计存档**：
> 将来 `WasmPlugin` 适配器仍要实现同一个 `trait Plugin`。
>
> **本轮新增**：不再只是存档——定下一套**参照移植**（§6.2）来验证 ABI 与宿主侧 SDK 的真实成熟度，
> 判据是 §6.3 的 8 条门禁，而不是「迁了几个插件」。第一方**其余插件留在编译期**，
> 只有明确需要第三方分发/沙箱的才迁。
>
> `plugins/kiqi-plugin.wit` 已升到 parse-validated 的 `0.2.0` 骨架（0.1.0 连 `wasm-tools component wit`
> 都过不了：variant case 带命名/多字段是非法的）：能力面做全、每个能力独立 interface、
> 调用一律 `result<_, denied>`、进度用 import sink、timer 由宿主驱动。

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
- **`event` 有 `other(other-event)` 逃逸分支**（`kind` + `payload` 包成一个 record——WIT 的 variant
  case 只能带一个类型）：新增事件类型不会破坏老插件。
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

> **注：这是早期草案，签名已过时。** 实际契约以 `crates/kiqi-plugin` 的
> [`trait Plugin`](../../crates/kiqi-plugin/src/lib.rs) 为准（见 [plugin-trait.md](plugin-trait.md) §6）：
> 它是 sync、带 `HostCaps` / `ProgressSink` / `Event`，方法集比本节宽。
> `WasmPlugin` 适配的是那一份，不是本节签名。

## 5. 权限模型：声明归作者、授权归用户、执行归宿主

三层分开，缺一不可：

| 层 | 内容 | 归属 |
|---|---|---|
| **声明** | 插件 import 哪些能力（WIT + manifest） | 插件作者 |
| **授权** | 用户批准了哪些能力、什么 scope | **用户**（唯一决策者） |
| **执行** | 为全部 import 提供实现（未授予的挂 deny 桩）；manifest 声明的 required 缺则拒载 | 宿主（纯机制） |

**宿主永不自动授权。** 没有用户的 grant，对应调用必定 `denied`（或 required 缺时拒绝加载），
不是「默认给」。

### 何时问用户

- **安装/首次加载**：列出声明的能力集，用户批准后落盘；
- **更新时声明集变大（增量）必须重问**——这是静默提权唯一的入口；
- **危险能力可首用时再问**（TCC 式）：`exec`、裸 socket、整机 fs；
- **无用户在场（headless / CI / cron）**：默认拒绝，只认预置 grants 文件，绝不「没人问就全给」；
- **可随时吊销**：吊销即卸载/禁用，运行中不缓存已吊销授权。

### 粒度与落点

- 按能力 + scope：fs 给到具体根、http 给到 host allow-list、exec 给到命令白名单；
- 存储：`~/.ki/agent/plugins/<id>/grants.toml`；
- CLI：`ki plugin grant|revoke|list`；交互 TUI 在缺授权时提示。

### 形状上的硬要求

能力操作**必须可失败**（`result<_, denied>`）：未授权调用返回 `denied` 而不是加载失败，
插件可优雅降级、用户中途授权无需重载。

**为什么不能靠“不挂 interface”来实现未授权**：组件模型要求 world 声明的 import 在实例化时
**全部**被满足，不能选择性不挂。所以 wasm 层的实际做法是：

- 宿主为**全部** import 提供实现——已授予的走真实实现，未授予的挂 **deny 桩**（每次调用回 `denied`）；
- “缺能力 => 加载即拒”靠插件的 **manifest 声明 required 能力**，宿主在实例化前校验（fail-closed）；
- “缺能力 => 不可表达”这个强保证**只属于编译期 `trait Plugin` 的 `HostCaps: None`**。
wasm 层是“总是可实例化，但未授予的调用必定 `denied`” + “required 缺就拒绝加载”两层合起来等效。

每个能力依然是**独立 interface**：deny 桩、审计与以后独立收紧都因此最小化。

### 必须防的坑

1. **提示疲劳**（10 个能力弹 10 次 → 闭眼点 yes）：危险的默认拒、合并展示、人话描述；
2. **静默升级**：只对声明增量重问；
3. **审计**：授权与使用都留痕（谁何时批了 `exec`、插件何时真的 exec 了什么）；
4. **无头缺省**：默认拒绝 + 预置配置，不「默认通过」。

## 6. 里程碑

### 6.1 前置（不做就会返工）

1. **流式进度**：现有 `call-tool -> string` 一次打死 16/18 需要分步输出的插件。必须加
   `import progress`（host 侧 sink）或 `stream<string>`；
2. **sync/async 接缝**：宿主 `trait Plugin` 是 sync，wasmtime async component 调用是 async。
   适配器 park（`block_in_place`）还是把接缝改 async，必须先定；
3. **宿主侧 wasm 插件 crate**：wasmtime `bindgen!` + 为**全部** import 提供实现
   （已授予的走真实实现，未授予的挂 `deny` 桩；组件模型要求在实例化时满足全部 import）；
4. **`ki plugin test` mock 宿主**：语言无关，一次投入所有 guest 语言共用；
5. **授权模型**（§5）：声明/manifest + grants + 安装/更新提示 + 无头默认拒。

### 6.2 参照集（用最小集合覆盖 ABI 面，不按难易挑）

真实移植：

| 插件 | 覆盖的 ABI 面 | 理由 |
|---|---|---|
| `rpi-todo` | tools + events + 状态/details + 分支恢复（SessionRead）+ command | 最简；P2 基线 |
| `rpi-langfuse` | 14 个事件的密集流 + **outgoing HTTP** + UI status + 3 个 HTTP 工具 | 真实压力（事件多、分步 flush、网络） |
| `rpi-ask-user` | **UI dialog（阻塞式宿主往返）** + command | 专打 sync/async 最硬的点 |

**conformance fixture**（不是真插件）：为实现「每个 import interface + 每个 export 形状」而写的
最小 guest，是 **SDK 成熟度判据**——只用公开 WIT + 生成 binding + 文档，不碰宿主内部，
过同一套一致性测试。

**明确不迁**（选它们是在验证 host 的 exec/会话驱动，不是验证 ABI）：`im-message`、`voice`、
`mcp-adapter`、`acp`、`server`、`run-stats`、`subagents`、`codegraph`、`lens`
（起子进程 / 长连 / 后台线程）。

### 6.3 验收门禁（「验证通过」的判据）

1. **跨平台同一份 `.wasm`**：一次 `wasm32-wasip2` 构建，macOS + Linux wasmtime 加载，
   工具/事件输出与编译期版本**逐字节对齐**（golden diff）；
2. **面覆盖断言**：conformance fixture 断言每个 import/export 都被真实触达
   （caps 级：断言宿主被要求做了什么，不是「加载成功」）；
3. **流式进度**：至少一个工具调用中途 emit progress 并被按块渲染；
4. **UI 往返**：`ask-user` 打开对话框、等答复、可取消（不卡死）；
5. **能力拒绝双态**：未授予 → 调用 `denied` 且优雅降级；必需 import 未授予 →
   加载 fail-closed（或 deny 桩）——两条都断言；
6. **用户授权闭环**：安装列声明 → 更新新增能力必须重问 → grant/revoke 生效 → 无头默认拒；
7. **隔离**：挂死/panic 的 guest 被 fuel/epoch/超时掐掉，宿主存活（对标 cdylib 的 `catch_unwind`）；
8. **性能基线**：tool 调用/事件分发 wasm vs 编译期的开销出数字（把沙箱税量化，别用猜）。

### 6.4 SDK 语言顺序

不要一期铺四门。**Rust 优先**（宿主同语言、wit-bindgen 最成熟、兼作参照实现）；
随后按真实第三方需求加 **TS**（`jco` + `componentize-js`，注意内嵌 JS 引擎、体积 MB 级）
或 **Go**（`TinyGo` + `wit-bindgen-go`，注意不是标准 Go）二选一；**C/C++ 最后**
（`wit-bindgen c` 较成熟，C++ 生成器相对滞后，现实多是 C 绑定包一层）。
每多一门 = 一套 CI/示例/文档/异步适配/版本跟随，收益要等真实用户。

### 6.5 执行顺序（每步可回退）

1. 定 ABI 0.2.0（含 §6.1 两个 blocker）→ 出 **Rust guest SDK**；
2. 宿主 wasm 后端 + mock；
3. 迁 `rpi-todo`（打通 tools/事件/状态）→ `rpi-langfuse`（事件密度 + HTTP）→
   `rpi-ask-user`（UI/阻塞）；
4. conformance fixture + 跑 §6.3 门禁；
5. 全绿后才谈第二门语言；官方其余插件留在编译期。

## 7. 待定

- wasmtime 依赖很重（编译时间 + 体积）：独立 crate `kiqi-plugin-host` 还是 feature 开关？
- `.wasm` 是否需要签名/摘要校验（防替换；「免签名分发」是卖点，但「防篡改」另说）？
- 插件配置来源（`~/.ki/agent/plugins/<id>.json` 还是走 WIT）？
- `wasm32-wasip2` 工具链版本锁定策略（生产要钉版本）。
