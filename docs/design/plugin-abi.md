# 插件 ABI：从 cdylib 到 wasm 组件

状态：**推迟为第一方契约；作为第三方（不受信）层推进验证** · 目标版本 `kiqi:plugin@0.6.0`

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
> `plugins/kiqi-plugin.wit` 已升到 parse-validated 的 `0.6.0`（0.1.0 连 `wasm-tools component wit`
> 都过不了：variant case 带命名/多字段是非法的）：导出面与 `trait Plugin` **一一对应**、四个可授权能力
> （`net-http`/`fs`/`exec`/`net-sockets`）**全部真实现且授权自带策略**、调用一律 `result<_, capability-error>`
> （`denied` 授权拒 / `failed` 操作失败）、进度用 import sink。

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
| **执行** | 为全部 import 提供实现；未授予的调用在**调用时**回 `denied`（运行时授权检查） | 宿主（纯机制） |

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
- 存储：`~/.ki/agent/plugins/<id>/grants.json`（`KI_AGENT_DIR` 可覆盖根）；
- CLI：`ki plugin grant|revoke|list`；交互 TUI 在缺授权时提示。

### 形状上的硬要求

能力操作**必须可失败**（`result<_, denied>`）：未授权调用返回 `denied` 而不是加载失败，
插件可优雅降级、用户中途授权无需重载。

**为什么不能靠“不挂 interface”来实现未授权**：组件模型要求组件**声明的** import 在实例化时
**全部**被满足（实测：构建出的组件会**剪掉未使用的 import**——conformance fixture 只 import 了
`types`/`log`/`ui`/`progress`，world 里另七个 interface 根本没进组件），所以能“选择性不挂”的
只是那些未被使用的 interface，**用到的**一个都不能缺。于是 wasm 层的实际做法是：

- 宿主为**全部** import 提供实现——已授予的走真实实现，未授予的调用在**调用时**回 `denied`；
- “缺能力 => 不可表达”这个强保证**只属于编译期 `trait Plugin` 的 `HostCaps: None`**。

所以 wasm 层的实际形态是“**总是可实例化，但未授予的调用必定 `denied`**”，单层。
~~manifest 声明 required 能力、宿主实例化前拒载~~——写过这个设想但从未实现，相关措辞已
在 2026-10-15 随 §6.30 一并更正（WIT 顶部注释与 §6.9 曾同样误述）。fail-closed 由运行时
检查承担：缺授权的调用拿不到能力，而不是加载失败。

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
`mcp-adapter`、`acp`、`server`、`run-stats`、`subagents`、`codegraph`、~~`lens`~~
（起子进程 / 长连 / 后台线程）。

> 2026-10-15（§6.30）：`lens` 移出此名单并已移植（`plugins/wasm/kiqi-rpi-lens-guest`）。
> 当初排除它是因为 exec 还没真实现、cwd 只能关在插件 data 目录；exec 授权带上 cwd 范围
> 与时间预算后，“起子进程”不再是 ABI 验证之外的活。
>
> 2026-10-15（第二批）：`rpi-orchestrator` 从 `rpi-package` 收编为编译期插件
> （`plugins/rpi-orchestrator`，默认 feature），但**不迁 wasm**——它是长生命周期编排：
> 子代理是可活到 1800s 的 `ki -p` 子进程（exec 预算上限 300s）、`dispatch` 的后台节点
> worker 要在工具调用之间持续存活（wasm guest 调用间无线程）、子进程要整组收杀
> （wasm 没有进程组概念）。wasm 化它需要宿主先长出「会话级长生命周期子进程句柄」这类
> 新能力，属未来的设计活；exec cwd scope（§6.30）解决的是 lens 那种有界短任务。

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

### 6.6 已落地：宿主绑定（crate `kiqi-plugin-host`）

`crates/kiqi-plugin-host` 用 wasmtime 49 的 `bindgen!` 绑 `plugins/kiqi-plugin.wit`
（world `plugin`），`cargo check` 通过。实测两条 wasmtime 49 的要点，写下来避免踩坑：

- **`bindgen!` 不再有 `async: true` 选项**（写了会报 unknown key）。异步由 wasmtime 的 cargo
  feature `async`（+ `component-model`）开启，生成 `instantiate_async` / `TypedFunc::call_async`；
- 让 **WIT 函数** async（宿主 import 真异步、guest 直接 `await`）需要 feature
  `component-model-async`。0.2.0 走 WASI 0.2 语义（WIT 函数同步、guest 用 `wasi:io/poll`
  阻塞），宿主的 `net-http.fetch` 这类 import 实现同步（内部可 `spawn_blocking`）；
  要改 0.3 异步语义是后续单独的决定。

生成形状（“宿主侧 SDK 成熟度”的直接证据）：

- 每个能力一个 `Host` trait，方法 snake_case：`log::Host::write`、`progress::Host::emit`、
  `ui::Host::{dialog,set_status,set_editor_text}`、`session_read::Host::{info,active_tools,…}`、
  `session_control::Host::…`、`inject::Host::…`、`net_http::Host::fetch`、`fs::Host::{read,write,read_dir}`、
  `net_sockets::Host::{connect,send,recv,close}`、`exec::Host::run`、`timer::Host::request_tick`；
- 导出面：`plugin.kiqi_plugin_plugin_api()` 返回 `Guest`，其上
  `func_call_tool: TypedFunc<(&str, &str), Result<String, PluginError>>`、
  `func_on_event: TypedFunc<(&Event,), ()>`、`func_on_tick`、`func_before_agent_start`、
  `func_probe`、`func_list_tools/commands/flags/shortcuts`。

未做（后续增量）：`WasmPlugin` 实现 `kiqi_plugin::Plugin`（sync 接缝，内部
`instantiate_async` / `call_async`）、host import 实现（`progress` 转发到 `ProgressSink`，
其余先挂 deny 桩）、guest SDK + conformance fixture、§6.3 的门禁测试。

### 6.7 已落地：guest SDK + conformance fixture

`plugins/wasm/` 是**guest 侧独立 workspace**（这些 crate 必须编到 `wasm32-wasip2`，
不能用宿主 target 构建）：

- `kiqi-plugin-guest`：包 `wit_bindgen::generate!`（world `plugin`，ABI 0.2.0）并开
  `pub_export_macro`，插件 crate 实现生成的 `Guest` 再用 `bindings::export!`；
- `kiqi-conformance-guest`：conformance fixture（工具里 `log.write` + 两个 `progress` 块
  + 一个必定 `denied` 的 `ui.set-status`）。

`cargo build --manifest-path plugins/wasm/kiqi-conformance-guest/Cargo.toml --target wasm32-wasip2 --release`
产出 84 KB 的 **component**，导出 `kiqi:plugin/plugin-api@0.2.0`。

### 6.8 已落地：宿主 `WasmPlugin` + e2e（load / call / unload 全通）

- `kiqi-plugin-host`：`Engine` **同步**（不是 async）；`Linker` 用
  `World::add_to_linker::<HostState, HasSelf<HostState>>` + `wasmtime_wasi::p2::add_to_linker_sync`；
  `HostState { wasi, table }` 实现**全部** `Host` trait：`log` 真写、`progress` 经**线程局部 sink**
  转发到 `ProgressSink`、`session_read` 返空、`session_control`/`inject`/`ui`/`net_http`/`fs`/
  `net_sockets`/`exec`/`timer` 全 **deny 桩**（每个能力调用回 `denied`）。
- `WasmPlugin` 实现 `kiqi_plugin::Plugin`（**sync 接缝**）：`tools`/`call`/`on_event`/`shutdown`；
  `unload()` = 调 guest `shutdown` + 释放实例（`Drop` 也调），供 `/reload` 用。
- **修正 §6.6 的判断：不用 async。** WASI 0.2 是同步语义，`bindgen!` 生成的 guest 调用就是 sync
  （没有 `call_*_async`），所以宿主全程同步即可，`async_support`/`add_to_linker_async`/`block_on`
  都不需要。真需要异步的是**宿主 import 的实现**（将来），那在实现内部 blocking 即可——
  这也正是“异步关在适配器底部”的原意。
- e2e（`crates/kiqi-plugin-host/tests/e2e.rs`）**通过**：构建 fixture → `WasmPlugin::load` →
  `tools` → `call`（断言回显参数、`ui=<denied>`、3 个 progress 块、guest `log` 到达宿主）
  → 未知工具是 typed error → `unload()` 后调用报 “unloaded”。
- CI 的 test job 加 `targets: wasm32-wasip2`。

**这轮暴露的 ABI 缺口，两条已在 0.3.0 补上**：

1. ~~`call-tool` 只返回 string~~ → 已改为结构化 `tool-result`
   （`blocks: list<content-block>` + `is-error` + `details` JSON）。e2e 断言 `details` 往返成功，
   `rpi-todo` 的任务列表从此有地方放。
2. ~~`on-event` 返回 `()`~~ → 已改为返回 `event-action`（`continue`/`claimed`/`replace`）。
   e2e 断言一个 wasm 插件能真 `Claimed` 一个事件。
3. **下一条增量已解决**：`HostCaps` 已接进 host import（见 §6.9）。仍未做的是能力**授权
   （grant）**模型。

### 6.9 已落地：`HostCaps` 接入 + 第一个真 wasm 插件（`rpi-todo`）

- **`HostCaps` 接进 host import**：调用期间把 `HostCaps` 与 `ProgressSink` 一起放进
  线程局部的 `CallContext`（借用期由 call 保证、调用被互斥序列化）；`session-read` 转发到
  `caps.session`，`session-control`/`inject`/`ui` 转发到对应能力（缺能力或 caps 不存在 →
  `denied`）。**当时现状**（ABI 0.6.0 之前的过渡态；`exec`/`net-sockets` 在 §6.26 已真实现，
这行的「仍是 deny 桩」自此过期）：`net-http` 与 `fs` 已真实现（`fs` 关在插件自己的
  `plugins/<id>/data/` 里，越狱写法直接拒）；`exec`/`net-sockets` 仍是 deny 桩，所以
  `ki plugin install` 会对“声明了但本构建无法执行”的能力**明说**（`capability_is_enforced`），
  而不是让用户授权一个永远不会生效的东西；`timer` 已随 `on-tick` 一起删除。
- **`rpi-todo` 编成 wasm 插件**（`plugins/wasm/kiqi-todo-guest`）：list/add/update/toggle/clear
  语义不变；状态走结构化 `tool-result.details`；`SessionStart`/`SessionTree` 时经
  `session-read.session-branch` 恢复（不再是 runtime action）。
- e2e 新增 `todo_guest_restores_from_the_session_branch`：给一个固定 branch 的 `SessionRead`
  → `SessionStart` 触发恢复 → `list` 文本含恢复项、`details.todos` 带状态 → `add` 后 details 更新。
  两个 e2e 全绿。
- 仍未做：能力**授权（grant）**模型；`session-read.info` 的 session-id/run-id 还返空。

### 6.10 已落地：授权（grant）+ `net-http`

- **`Grants`**：每插件一份，从 `$KI_AGENT_DIR/plugins/<id>/grants.json`（默认
  `~/.ki/agent/plugins/<id>/grants.json`）读；**文件缺失 = 什么都不授予**，宿主不替插件拍板。
  `WasmPlugin::load` 自动读，`WasmPlugin::load_with_grants` 供调用方自己管授权（与测试）。
- **`net-http` 实现**：宿主侧 `reqwest` blocking；先按 URL 取 host，再查 grant 的
  `hosts` allow-list（`*` 通配）；不在表里就 `denied`。请求/响应 JSON 形状写进了 WIT 注释：
  `{url, method?, headers?, body?}` → `{status, headers, body}`。
- 域能力（`session`/`control`/`inject`/`ui`）的“授予”仍由调用方构造的 `HostCaps` 表达；
  grants 目前只管 wasm 独有、`HostCaps` 没有对应字段的能力。
- e2e 新增 `net_http_is_gated_by_the_users_grant`：无 grant → `denied`；grant 的 hosts 不匹配
  → `denied`；host 匹配 → 真正打到本地一次性 HTTP server 并拿到响应体。3 个 e2e 全绿。
- 与规格的差异：实现用 `grants.json` 而非规格里的 `grants.toml`（省一个依赖，形状等价）。
- 仍未做：`ki plugin grant/revoke/list` CLI、安装/更新时的提示与“增量声明才重问”、
  `fs`/`exec`/`timer`/`net-sockets` 的实际实现（仍是 deny 桩）、以及能力错误只有
  `denied` 一个分支（网络失败也被叫 `denied`，应当拆出 `failed`）。

### 6.11 已落地：`rpi-langfuse` 代表性子集（事件密度 + HTTP）

`plugins/wasm/kiqi-langfuse-guest`：**不是**完整 langfuse（3 个管理工具与完整 usage/cost
留在编译期插件），而是验证“高频事件 + 出站 HTTP”的最小真实体：

- 从事件流建一轮 trace：`SessionStart` 重置 → `BeforeAgentStart` 建 root span
  （trace name 用宿主传来的 `host.name`，如 `kiqi Turn`）→ `BeforeProviderRequest` 开
  generation span → `MessageEnd` 用输出/usage 收尾 → `SessionShutdown` 关 root 并 flush；
- `flush` 走 **`net-http`** POST 到 `{baseUrl}/api/public/otel/v1/traces`，带 `Basic` 认证头，
  序列化成 OTLP/JSON（root 最后发：Langfuse 在 root 到达时冻结 trace header）；
- 配置经 **`langfuse_config` 工具**传入（**不从进程 env 读**——guest 是沙箱，不该被塞宿主环境）。

e2e `langfuse_subset_builds_a_trace_and_flushes_it_over_net_http`：grant 允许 `127.0.0.1`、
配置指向本地 server，派发一轮事件后断言 server 收到的 POST body 含 `Conversational Turn`、
`kiqi Turn`、`LLM Call`、`sess-1`、`test-model`。**4 个 e2e 全绿**。

这也把 §6.10 记的“能力错误只有 `denied`”从纸面变成现实痛点：flush 的网络失败也会被报成
`denied`，语义不对（应拆 `denied` / `failed`，列为 0.4.0 待办）。

### 6.12 门禁状态（§6.3 逐条）

| 门禁 | 状态 |
|---|---|
| 1 跨平台同一份 `.wasm` | ✅ 同一 fixture 在本机 macOS 与 CI Linux 都构建+加载+跑通（与编译期版本的 golden diff 仍未做） |
| 2 面覆盖断言 | ✅ `conformance_surface_is_reachable` 断言契约层被真实触达；**会话层也已接线**（§6.19）：tools / events（原有）+ commands / shortcuts / before_agent_start / probe（新增）；`flags` 的声明由 `ki plugin show` 消费（§6.21） |
| 3 流式进度 | ✅ conformance e2e 断言 3 个 progress 块按序到达 |
| 4 UI 往返 | ✅ `ui.dialog` 成功往返 + 取消 + 超时都断言（`rpi-ask-user` wasm 移植，§6.15） |
| 5 拒绝双态 | ✅ 无 UI → `set_status` denied；授予 UI → granted；net-http 无 grant / 错 host → denied，匹配 → 真请求 |
| 6 用户授权闭环 | ✅ `Grants`（缺失=拒）+ `ki plugin grant/revoke/list` + `install`（列声明、**只对增量重问**、无头默认拒）+ `uninstall`（移到备份）（§6.14/§6.17） |
| 7 隔离 | ✅ 燃料计量的死循环 → trap（不挂）；`panic!` → trap；宿主存活 |
| 8 性能基线 | ✅ 同机对比 `todo list`：wasm 33µs/次 vs 编译期 43µs/次（0.8x），断言比值而非绝对阈值（§6.18） |

参照集（§6.2）：`rpi-todo` ✅（真插件）、`rpi-langfuse` 子集 ✅（真实压力面）、
`rpi-ask-user` ✅（§6.15）、conformance fixture ✅。

### 6.13 ABI 0.4.0：能力错误拆 `denied` / `failed`

- `types` 里 `record denied { reason }` 换成 `variant capability-error { denied(string), failed(string) }`；
  所有能力调用（session-control / inject / ui / net-http / fs / net-sockets / exec / timer）
  改签名为 `result<_, capability-error>`。
- 宿主：**授权拒绝 → `denied`**（含 allow-list 不匹配）；**已授予但操作失败 → `failed`**
  （HTTP 发送失败、URL 非法、client 构建失败等）。`forward` 经 `capability_error(PluginError)` 区分
  （`PluginError::Denied` → denied，其余 → failed）。
- guest：conformance 把错误渲染成 `cap:<reason>` / `cap:failed:<reason>`；langfuse 的 flush 失败
  现在记为 `rejected`，不再是“not granted”。
- 语义收益：**超时的 flush 不再被误报成“没授权”**——正是 §6.11 记下的痛点。
- 这是 0.4.0 breaking：WIT + host + 3 个 guest 一起改，6 个 e2e 全绿。
- 仍未做（重记为需新接缝）：`session-read.info` 的 session-id/run-id 仍返空，需要一个
  `SessionRead::session_id` 接口（不拿进程序 env 顶替）。

### 6.14 已落地：授权落盘 + `ki plugin grant/revoke/list`

- `Grants` 从 `kiqi-plugin-host` 移到契约 crate **`kiqi-plugin::grants`**（加 `serde` 序列化）：
  `load_for` / `save_for` / `allows_host` / `granted` / `grant` / `revoke` / `installed_plugins`。
  这样**宿主（强制）与 CLI（编辑）共用一份类型，且 `rpi-cli` 不会引入 wasmtime**。
- `ki plugin list | show <id> | grant <id> <cap> [--host H]... | revoke <id> <cap>`：读写
  `$KI_AGENT_DIR/plugins/<id>/grants.json`（默认 `~/.ki/agent/plugins/<id>/grants.json`）。
  实测：grant → 落盘 → `list`/`show` 可见 → revoke → `{}`；未知能力/缺参 → exit 2。
- `kiqi_plugin::SessionRead` 新增 `session_id()`（默认 `None`，不破坏既有实现）；宿主
  `session-read.info` 的 `session-id` 走它。实现方（`ActionCaps`）暂未覆盖，所以仍可能为空，
  但**接缝在了，且没有拿 env 顶替**。
- 仍未做：`ki plugin install/uninstall` 本身、以及“安装/更新时列出声明 + 增量重问”的提示
  （要有 install 才有落点）。

### 6.15 已落地：`rpi-ask-user` 忠实移植（UI 往返 + 取消 + 超时）

`plugins/wasm/kiqi-ask-user-guest`：参数归一化与编译期插件一致
（`question`/`options`/`allow*`/`displayMode`/`timeout`/`confirm`/`questions`），交互则不同——
C ABI 不能阻塞，原生版才需要 start/poll/cancel 状态机；wasm 里 `call_tool` 直接阻塞，
所以是直筒的 **open → poll → answer**。

- 每个问题：`ui.dialog({op:"open",...})`，然后循环 `{op:"poll"}`：`answered` 收答案、
  `cancelled` 报错、`timeout` 到点先发 `{op:"cancel"}` 再报 “ask timed out”。
- e2e 4 个：问答往返（答案进 `details.answers`）、confirm 映射 `Confirmed`/`Declined`、
  **取消**、**超时**（并断言 guest 真发了 `cancel`）。
- 门禁 4 由此收绿；**e2e 共 9 个全绿**。

### 6.16 已落地：wasm 后端接进 `ki`

- `ki` 的 `load_hosts()` 除编译期插件外，还扫描 `$KI_AGENT_DIR/plugins/<id>/plugin.wasm`
  （`plugin-wasm` feature，默认开），用 `WasmPlugin::load`（顺带读同目录 `grants.json`）加载，
  包成同一个 `PluginHost`——**没有第二套注册/派发路径**：工具经 `merge_compiled_plugin_tools`、
  事件经 `install_compiled_event_sink`，与编译期插件完全同路。
- 加载失败（坏组件 / 缺 WASI import）只打印并跳过，不拖垮会话。
- 新增 rpi-cli 测试：把 conformance fixture 放到临时 `plugins/conformance/plugin.wasm`，
  断言被发现、工具可见、且调用跑通（经 `HostCaps::observer` 拿到 `ui=` 的拒绝报告）。
- 代价：`ki` 现依赖 `kiqi-plugin-host` → **wasmtime 进了主二进制**（体积/编译时间上升），
  所以给了 `plugin-wasm` feature（默认开）；要精简二进制可关掉。

### 6.17 已落地：`ki plugin install/uninstall`（声明 + 增量重问 + 无头默认拒）

- `ki plugin install <path.wasm> [--id ID] [--yes] [--host H]...`：
  - 把组件复制到 `plugins/<id>/plugin.wasm`；
  - **读组件声明的能力**（`kiqi_plugin_host::declared_capabilities`：wasmparser 扫 `kiqi:plugin/*`
    import，只取可授权的 `net-http`/`fs`/`exec`/`timer`/`net-sockets`；`log`/`progress`/`ui` 等域接口不参与授权）；
  - **只对“声明了但还没授权”的增量征求意见**：`--yes` 全批；有 tty 则一次 `[y/N]`；
    **无 tty 且没 `--yes` → 什么都不授**（无头默认拒）；
  - `net-http` 的 host allow-list 走 `--host`（缺则该项不授并提示）。
- `ki plugin uninstall <id>`：把 `plugins/<id>` **移到** `plugins/<id>.removed-<ts>`（可恢复），不删文件。
- 测试：host `declared_capabilities`（conformance fixture 只声明 `net-http`）；rpi-cli
  `install --yes --host 127.0.0.1` → 复制 + `grants.granted()==["net-http"]` + host 匹配 →
  `uninstall` → 目录被移走。**10 个 host e2e + 4 个 CLI 测试绿。**
- 门禁 6 由此收绿；§6.3 只剩门禁 8（性能基线）。

### 6.18 已落地：性能基线（门禁 8）

`crates/rpi-cli` 里一个同进程对比测试：同一个 `todo` list 调用，编译期插件 vs wasm 插件，warm 后
各 2000 次：

```
todo list: compiled 43120 ns/call, wasm 32887 ns/call (0.8x)
```

- 两者都在 ~30–40µs/次，由 `serde_json` 主导；**本工作负载下可观测的沙箱税≈0**（wasm 侧甚至略快：
  它直接返回结构化 `tool-result`，编译期侧要先拼 JSON envelope 再重新解析）。
- 断言用**同机比值**（<200x）而非绝对阈值，避免 runner 抖动；它防的是“数量级回归”，不是钉数。
- 注意：这是微基准，不含 wasmtime 实例化（`load` 每插件一次）与真实 I/O；真实成本主要在首次
  `Component` 编译/实例化（一次性），不在每次调用。
- **§6.3 八条门禁全部收绿。**

### 6.19 已落地：非工具贡献面接进会话（门禁 2 从契约层升级到会话层）

原先会话只消费编译期/wasm 插件的 **tools** 与 **on_event**：`commands` / `flags` / `shortcuts` /
`before_agent_start` / `probe` 在 trait 层实现、也在 `PluginHost` 层测过，但会话从不主动问。现在五个面全部接上：

- **commands**：`register_compiled_commands` 把声明注册进 TUI 命令表（内置先注，重复跳过，不能遮蔽
  `/help`）；`CompiledCommand::execute` 调 `run_command` 把返回文本记进 transcript。
- **shortcuts**：`dispatch_key_event` 对声明了该键的插件派发 `Input` 事件；`Claimed`/`Replace` 即消费按键，
  与原生 `CLAIMED` 同义。只问 `shortcut_hosts` 筛出来的插件。
- **before_agent_start**：`compiled_prompt_hooks` 链在原生 `ExtensionProviderHooks` 之后，原生先算出提示词、
  编译期/wasm 插件再有机会替换；**按 prompt 时间戳去重**（一次工具循环的多次 provider 调用只算一次）。
  `rpi_extensions::before_agent_start_input` 让两半共用同一份载荷形状。
- **probe**：`probe_tool_call` 在 harness 的 `BeforeToolCall` 钩子里跑 `before-tool-call` 闸门。它本地同步，
  所以否决在原生的审批对话框弹开**之前**就生效；`Replace` 会带进原生派发，两边看到同一份 args。
- **flags**：**不需要接线**——`get_cli_flag` 本就从“通用解析的未知长选项”里取值，插件的 `cli_flag(name)`
  直接读到；声明只用于可发现性。

可测性：闸门 / 变换 / 按键筛选都收显式 host 列表（`probe_tool_call_with`、`compiled_prompt_hooks_with`、
`shortcut_hosts`），所以测试能注入合成插件；另有一个测试加载**真的 conformance wasm 组件**，证明这套
会话路径同样能到达沙箱插件。命令面由 `interactive_tui::commands` 的两个单测锁住。

### 6.20 信任边界：沙箱插件不得顶掉既有工具名

接线时发现一个真实的越权路径：`merge_compiled_plugin_tools` 按名**覆盖**（`Some(slot) => *slot = harness_tool`），
而装载顺序是 内建 → cdylib 扩展 → 编译期 → wasm，所以**后加载者胜**。于是第三方 `plugin.wasm` 只要声明一个叫
`bash` 的工具，模型随后调的 `bash` 就是它——沙箱/授权全被绕过（“extension overrides same-named
built-in”那句注释本来是给**受信**的 cdylib 层写的）。

对策：`PluginHost` 带上 `untrusted` 标记（wasm 层为 `true`），`merge_compiled_plugin_tools`
对 `untrusted` 的 host **遇到已占用名就拒绝 + 告警**，受信层保持原行为。拒绝只针对**冲突名**：
第三方插件贡献新工具照常（有测试分别锁定两件事）。

同时补上了 `session-read` 的 `session_id`：`RuntimeActionHost::session_id`（默认 `None`）→
`HarnessActionHost` 从 `tool_context` 取（`new_session`/`fork`/`switch_session` 会同步更新）→
`ActionCaps::session_id`。此前该能力恒返 `None`，靠事件载荷的 `sessionId` 兼顾；现在两条路一致
（两条通道由 `publish_session_identity` 同时写入，值不会分叉）。

### 6.21 `flags`/`shortcuts` 的消费者，与三套传输的面差

`flags`（以及 `shortcuts`）是**声明**面：运行时路径分别在 `SessionRead::cli_flag`（值来自通用
`--foo=bar` 解析）和“按键走 `Input` 事件”。但原先这个**声明本身没有任何读者**：`snapshot.flags()`
只有测试在调，`get_cli_flag` 也不查 registry——即“挂了名字的装饰”。现在 `ki plugin show <id>` 会实例化
插件并把它声明的 tools / commands / flags / shortcuts / probe-points 列出来，声明由此变成可观测、可断言的。

三套传输的面**本来就不同**，这里显式列清，别再让“8 个面”到处飘：

| 面 | cdylib 扩展 | 编译期 kiqi | wasm |
|---|---|---|---|
| tools / events / commands / flags / shortcuts / before_agent_start | ✅ | ✅ | ✅ |
| probe（权限闸门） | ❌ | ✅ | ✅ |
| renderer / provider / resources_discover / oauth | ✅（上游用；本仓无对应 `Plugin` 面） | ❌ | ❌ |

`ki` **同时**装载三层并合并进同一张工具表；同名工具的归属见 §6.20（受信层可覆盖，untrusted 拒绝）。

### 6.22 `/reload` 不再弄丢插件的 UI 能力

`/reload` 会换掉 `ActionBridge`，但**不换** UI mailbox（它们存在 `ReloadContext` 里，就是为了跨桥存活）。
原先重建工具适配器时给的是一个 `ui: None` 的 caps，于是重载后编译期/wasm 插件的 `ui.dialog` /
`set_status` / `set_editor_text` 全没了（其他能力还在）。现在 `reload_caps()` 从 reload 上下文取同一组
mailbox，并且**顺带刷新事件 sink 与全局 caps**，让重载后的 tools / events / commands / probes /
before_agent_start 跑在同一套一致的能力上。测试：拿 `reload_caps` 发一个 dialog，断言它落到 TUI 挂着的
那个 mailbox。

### 6.23 `on_event` 的动作语义在编译期/wasm 层闭合（并修掉一次重复投递）

`Plugin::on_event` 返回 `EventAction`，但全局 sink 的类型是 `Arc<dyn Fn(u32, Value)>`——**没有返回值**，
调用处 `let _ = host.on_event(..)` 把动作丢掉。于是编译期插件（以及复用同一条路径的 wasm 插件）
永远无法 claim，而原生 handler 的 `CLAIMED` 是被认的。

现在 `CompiledEventSink` 返回 `CompiledVerdict::{Continue, Claimed}`，`dispatch_data_event_claiming`
把编译期侧的结果并入自己的返回值（并接收 `Option<&RegistrySnapshot>`，因为编译期扇出不需要原生 registry）。
两个 observe 型漏斗仍不消费它，与原生一致。

顺带修了一个本轮引入的真 bug：`dispatch_key_event` 既走漏斗（sink 会把 `Input` 扇一遍）又自己再扇一遍，
**编译期插件会收到两次同一个按键**。现在只走漏斗，按键筛选（只问声明了该键的插件）移到 sink 里，
由 `dispatch_to_compiled()` 统一负责，并有测试分别钉住“声明了的键被问到并 claim”与“未声明的键
根本不会被问到”。

### 6.24 ABI 0.5.0：wasm 版就是 `trait Plugin` 那一份（面一一对应）

需求一句话：**wasm 要一份和 `trait Plugin` 一样的契约**。逐面比对后发现四处对不上，已对齐：

| `trait Plugin` | 0.4.0 的 WIT | 0.5.0 |
|---|---|---|
| `id()` | 缺 | **加 `id: func() -> string`**；宿主只在装载时**核对**（install 目录仍是授权键，声明不一致就报一句，不夺权） |
| — | `init()` 多一个 | 删（组件实例化本身就是构造点） |
| — | `on-tick()` 多一个 | 删（宿主从不驱动，4 个 guest 全是空实现） |
| — | `timer` 能力 | 删（只为驱动 `on-tick` 而存在；`Grants.timer` 一并去掉） |
| `EventAction` 三态 | `event-action` 三态 | **`{continue, claimed}`**（`replace` 从未被任何插件构造，且没有投递通道） |

其余一一对应：`tools`↔`list-tools`、`call`↔`call-tool`、`commands`/`run_command`↔
`list-commands`/`run-command`、`flags`↔`list-flags`、`shortcuts`↔`list-shortcuts`、
`before_agent_start`↔`before-agent-start`、`probe_points`/`probe`↔`list-probe-points`/`probe`、
`on_event`↔`on-event`、`shutdown`↔`shutdown`。能力导入（`log`/`session-read`/`session-control`/
`inject`/`ui`/`progress` + `net-http`/`fs`/`net-sockets`/`exec`）是 `HostCaps` 的对应物加沙箱专有项，
不与贡献面冲突。

代价：`timer` 能力被删——它当初为“宿主驱动 tick”设计，但 trait 没有对应面、也没有任何插件在用。
若要后台 tick，正确做法是**两边一起加**（先给 `trait Plugin` 加 `on_tick` + `HostCaps` 的 timer），
而不是让 wasm 单方面多出一个面。

### 6.25 能力不再开空头支票：`fs` 真实现，`exec`/`net-sockets` 明说做不到

原先 `fs`/`net-sockets`/`exec` 都是 `deny_capability!` 桩，但 `ki plugin install` 会把它们
列给用户授权、写进 `grants.json`——**授权了也永远是 `denied`**，只有 `net-http` 是真的。这是
“声明了但没人执行”那一类问题的安全版：用户以为给了权限，其实什么也没给。

- **`fs` 实现了**，关在插件自己的数据目录 `plugins/<id>/data/`（从 `plugin.wasm` 的位置推出来，
  所以临时目录里的测试天然隔离）。绝对路径和任何 `..` 组件直接拒（逐组件解析 + `starts_with` 复核），
  **监禁就是策略**，所以 `fs` 保持布尔授权。
- **`exec`/`net-sockets` 仍是桩**，但 `ki plugin install` 现在会对声明了它们、而本构建无法执行的
  能力**明说**——依据是 `kiqi_plugin_host::capability_is_enforced()`（单一事实源），一句
  “this build cannot enforce …，调用会返回 `failed` 而不是 `denied`”写在 install 输出里，
  并有 e2e 分别钉住“哪些算能用”与“哪些不算”。

conformance guest 因此多了一路：拿 `fs` 写一个文件再读回来，把结果报在工具输出里，e2e 分“授权”
（真读到 `conformance`，且文件落在 `data/conformance/probe.txt`）与“未授权”（报 `denied`）两边验。

### 6.26 能力面没有空头支票了：`exec`/`net-sockets` 也真实现（授权带策略）

接上一节：`exec`/`net-sockets` 也不再是桩。它们的授权从布尔改成**带策略**（照 `net-http` 的样子）：

| 能力 | 授权形状 | 执行约束 |
|---|---|---|
| `net-http` | `{hosts:[...]}` | 请求 host 命中允许列表 |
| `net-sockets` | `{hosts:[...]}` | 连接的 host 命中允许列表；**宿主持有 socket**，guest 只拿到 `u32` 句柄（正好是 WIT 原本的 `connect->u32` 形状），所以握完手也绕不过去 |
| `exec` | `{commands:[...]}` | **裸命令名**（含 `/`/`\` 直接拒，无 shell）；cwd 关在插件自己的 data 目录（相对路径逐组件检查）；**环境变量清空后只回填 `PATH`**（不继承宿主的 `KI_SESSION_ID` 等）；stdout/stderr 各 256 KiB 上限；30s 墙上时钟超时后 kill；子进程在**独立进程组**里，结束后**连组一起收** |
| `fs` | `true` | 监禁在 `plugins/<id>/data/`（监禁即策略，所以仍是布尔） |

“策略缺失”是**错误**而非“全放开”：`grant("exec", vec![])` 返回 `exec needs at least one --command`。
CLI 侧 `ki plugin grant <id> exec --command echo`、`ki plugin install … --command echo`（`--host` 同理）。

本次之后 `kiqi_plugin_host::ENFORCED_CAPABILITIES` 覆盖全部四个可授权能力，所以 `ki plugin install`
对声明齐全的组件不再有警告；测试钉住这个集合本身，加上三路新 e2e：`exec` 白名单内/外、
`net-sockets` 走真实 loopback 回声（`sent=4 recv=ping`）与 host 不在允许列表时的 `denied`。

### 6.27 修掉两个绕过：`net-http` 重定向、`fs` 符号链接

实现完能力面后回头攻这两个授权面，两个都是真洞：

1. **`net-http` 的重定向能绕过白名单**。原先只检查了初始 URL 的 host，而 reqwest 默认跟随重定向
   ——一个**已授权**的主机只需回一个 302 到任意地址，请求就到了未授权的 host。现在用
   `redirect::Policy::custom`：**每一跳的 host 都要重新过白名单**，不在列表里就 `stop`（把 3xx
   交回插件，而不是自做主张报错）；跳数仍限 10。
2. **`fs` 的监禁只是词法的**。绝对路径和 `..` 被拒了，但数据目录里若存在一个指向外面的符号链接
   （`exec` 或用户留下的），`fs::read("link")` 会跟着跑出去。现在多一道
   `symlink_checked()`：父目录 `canonicalize` 后必须仍在（规范化后的）root 内，且最后一段本身
   不能是链接。中间目录是链接的情况也被拦。

写这一步时自己的 bug 也被测试抳住了：`root.canonicalize().unwrap_or(root)` 在 data 目录尚未
存在时会回退成**未规范化**的 root（macOS 上 `/var` vs `/private/var`），于是所有路径都被判
“escapes”。修法是先 `create_dir_all(root)` 再规范化。

测试：host crate 新增 2 个单测（词法/符号链接监禁、未授权回 `denied` 而非 `failed`）与 1 个真
起 HTTP 服务器验重定向不被跟随的 e2e。

### 6.28 隔离再补一维：guest 内存上限

此前只有 fuel（算力上限）与 panic/trap 包含，**内存没有上限**——一个 guest 可以一路
`memory.grow` 到 wasm32 的 4 GiB 地址空间。现在 `StoreLimits` 默认封顶 **256 MiB**
（`DEFAULT_MAX_MEMORY_BYTES`，可用 `Options::max_memory_bytes` 覆盖），并真的通过
`store.limiter()` 接上（不接就只是一个配置值）。超过上限时 `memory.grow` 被拒，Rust 侧分配失败即
trap，宿主不受影响——与 spin/panic 同一类隔离。

conformance fixture 相应地多了一个 `alloc` 工具，e2e 两边都验：**低于上限要成功**
（1 MiB）、**超过上限要 trap**（128 MiB，未真的分配）。只写“超限要 trap”是不够的——那无法区分
“上限生效”与“分配被恒禁用”。

同类的“无上限读”还差一处，一并补了：`net-http` 的响应体原先用 `response.text()` 整包读入，
一个被授权（或被攻陷）的主机只要不停发就能把宿主内存吃干。现在封顶 **8 MiB**
（`MAX_RESPONSE_BYTES`），超出返回 `failed` 并明说上限；e2e 用一个真发 9 MiB 的 loopback 服务验它
（读端到上限就停，服务侧的 broken pipe 是预期内的）。

`exec` 还修了一个**会挂死宿主**的缺陷：读线程 `join()` 等的是管道 EOF，而任何留下后台进程的命令
（如 `sh -c 'sleep 60 & exit 0'`）都会让孙子进程攥着管道——**正常退出也会卡死**，不只是超时路径。
现在子进程在独立进程组里（`process_group(0)`），子进程结束后无论成功还是超时都 `kill(-pid)`
收掉整组，读线程才能拿到 EOF。e2e 用一个后台子进程的用例钉住，且它跑在单独线程上
——回归时表现为 15s 后断言失败，而不是把测试进程挂死。

> 注意：新增 `kiqi-plugin-host` 作为 workspace 成员后，`cargo test --workspace` /
> `cargo check --workspace --all-targets` 会一并构建 wasmtime（编译时间/体积显著）。§7 里
> “独立 crate 还是 feature 开关”的问题现在只剩后半句：是否把它从默认 workspace 构建/CI 里
> 单独门控。

### 6.29 移植真实插件，并把**导入面**也对齐（ABI 0.6.0）

前面几轮对齐的是**导出面**（插件实现什么）。这轮把两个典型一方插件移到 wasm，一眼就看出
**导入面（能力）**也有缺口：

| 接口 | `trait` 里的方法数 | WIT 0.5.0 | 0.6.0 |
|---|---|---|---|
| `inject` | 4（`send_message`/`send_user_message`/`append_message`/`append_custom_entry`） | 3（WIT 的 `append-entry` 映射到的是 **`append_message`**） | **加 `append-custom-entry`** |
| `ui` | 4（含 `component`，即原生 runtime action 22） | 3 | **加 `component`** |

其余导入一一对应：`log.write`；`session-read` 六个（`session_id` 在 `info()` 里）；
`session-control` 十个；`progress`；以及沙箱专有的 `net-http`/`fs`/`net-sockets`/`exec`。

**移植的两个插件**（都拿真实的 `Plugin::*` 语义对照，不是演示）：

- **`rpi-webfetch` → `plugins/wasm/kiqi-webfetch-guest`**：走 `net-http`。保留了插件自己的 URL 策略
  （绝本地/绝字面私有 IP、绝内嵌凭证），两个差异写在 guest 文档里而不是藏起来：
  ① **不做 DNS 校验**（guest 无法解析，主机名交给用户允许列表管）；② **不再校验最终 URL**
  （宿主回的是 status/headers/body，重定向安全已由宿主逐跳白名单保证）。
- **`rpi-plan-mode` → `plugins/wasm/kiqi-plan-mode-guest`**：走 `session-read`/`session-control`/`inject`。
  逻辑（保留哪些工具、`/plan` 打印什么、大纲解析）**一份实现**，抽到 `crates/rpi-plan-mode-core`，
  编译期插件与 guest 共用——两个 `PlanHost` 适配器分别接 `HostCaps` 和 WIT 导入。
  也正因为如此，“WIT 缺少 `append-custom-entry`” 这个缺口无法用“再写一份”绕过去。

e2e：webfetch 验“插件策略先拦 + 没授权就 denied”（全 hermetic，不撞公网）；plan-mode 用记录型
`SessionControl`/`InjectApi` 验“进入时换掉工具集、完成时恢复并写 `plan-mode` 自定义 entry、
`/plan show` 能读回计划”。

### 6.30 `exec` 的授权带 cwd 范围与时间预算（`rpi-lens` 移植）

handover 里点名的“下一个真实阻塞点”：诊断类插件要在**项目目录**跑 `cargo`/`clippy`，而
`exec` 的 cwd 只能关在插件自己的 data 目录。这轮把它解开，并把 §6.2「明确不迁」名单里的
`lens` 移出来移植掉（排除它的理由——“验证 host 的 exec 驱动”——在 6.26 exec 真实现后
已经过期）。

**授权形状**（照 §5「策略随授权走」）：`exec` 从 `{commands:[...]}` 扩成

```json
{ "commands": ["cargo", "git"], "cwd": "project", "timeout-seconds": 120 }
```

- `cwd`: `plugin-data`（默认）| `project`。旧 grants.json 没有这两个字段，serde default
  落到 `plugin-data` + 30s——**旧授权语义不变**；老宿主读到新文件里的未知字段直接忽略，
  仍然是更严的那个方向。
- `timeout-seconds`：默认 30s（正是原来硬编码的值，不是悄悄放宽），上限 **300s**——CLI
  在 grant 时拒绝超限，宿主兜底 clamp 并在 stderr 留一行说明（手改 grants.json 是唯一能
  绕过 CLI 校验的路）。
- **ABI 不动（仍是 0.6.0）**：WIT 的 `exec.run` 本来就有 `cwd: option<string>`，scope 与
  budget 全部落在授权侧，不需要改契约、不需要重新协商。

**宿主侧的两个支撑点**：

1. **项目目录从哪来**：宿主原本只有 data 目录一个文件系统锚点（从 wasm 路径推导）。
   新增 `Options::project_dir`（load 期旋钮），默认取**宿主进程 cwd**——`ki` 启动后从不
   chdir，session 记录的 cwd 也是从同一个地方初始化的，所以进程 cwd 就是项目目录；
   测试与将来会迁移的嵌入方用旋钮显式给。
2. **cwd 的越狱检查与 `fs` 对齐**：exec 的 cwd 此前只过词法 jail，没过 symlink 检查
   （`fs` 在 6.27 就补上了）。现在两条路共用同一对 `jail_under`/`symlink_checked_under`，
   project 根（用户的树，满是符号链接）尤其需要这一层。

**一句必须写明的诚实表述**：exec 的子进程**不被文件系统沙箱化**（无 namespace/seccomp，
环境清空只回填 `PATH`），cwd jail 约束的是“从哪起跑”，不是“能碰哪”——拿到 `sh`/`cargo`
授权的插件本来就能 `cd` 到任何地方，甚至改自己的 `grants.json` 等下次加载生效。所以
`cwd: project` 没有引入新的风险类别，它增加的是**人体工学**（让诊断插件落在正确的目录）；
真正的授权重量始终在 `exec` 本身。给用户看的授权文案应当这么说。

**移植 `rpi-lens` → `plugins/wasm/kiqi-rpi-lens-guest`**（走 `exec` + `session-read`）。
逻辑小（check 名 → argv、输出截断格式化），照 webfetch 的样子直接随 guest 带一份，
不开 `-core` crate。差异写进 guest README：

1. **砍掉 `timeoutSeconds` 参数**——预算随授权，调用方不能出价盖过用户的预算；
2. **`cwd` 限项目内**——编译期版本接受机器上任意绝对路径；wasm 版只收相对路径或
   项目目录内的绝对路径（用 `session-read` 的 `info().cwd` 做词法前缀剥离，词法意味着
   符号链接拼写（macOS `/tmp` ↔ `/private/tmp`）会被误拒——fail-safe，传相对路径即可）；
3. 超时文案从 “timed out after {n}s” 变成 “hit the exec time budget”（数字在授权里）；
4. **子进程环境清空**（只有 `PATH`）——没有 `CARGO_HOME`/`RUSTUP_HOME`/`RUSTFLAGS`/代理
   变量；标准 rustup/cargo 安装不受影响，依赖这些变量的项目会构建得不一样；
5. **参数 JSON 解析失败直接报错**——编译期版本把坏参数当 `null` 然后跑默认的 diff 检查；
   wasm 版拒绝，而不是跑一个没人点的检查。

顺手修掉编译期 `rpi-lens` 的一个潜伏 panic：输出截断 `text.truncate(40_000)` 在多字节
字符中间下刀会 panic（`String::truncate` 的字符边界语义），改成退到字符边界再截，
带回归测试（先红后绿验证过）；wasm 版不继承这个 bug。

e2e（`kiqi-plugin-host`）：cwd 默认 data 目录、相对 cwd 下钻、`..`/绝对路径越狱被拒、
符号链接父目录被拒（unix）、project scope 落在注入的项目目录、超时预算真掐死 `sleep 10`；
lens 移植验“project scope 下 `git diff --check` exit=0 / data 目录下报 Not a git repository /
无授权报 denied 并指给用户该敲什么命令”。

**审查后修正**（这批 diff 过了一轮分维度对抗审查，确认并修掉的）：

- **监狱检查先建目录后验路径**（承重缺陷，fs 老代码带过来的）：`create_dir_all` 在
  canonicalize 之前跑，符号链接中间件会把目录建到监狱外——操作照样被拒，污染却留下了。
  改成先验最深的现存前缀、再建（必然干净的）尾巴；回归测试钉住“拒绝后监狱外零创建”。
- **拒绝文案的破坏性引导**：guest 的 deny 建议只写被拦的那条命令，而 `ki plugin grant`
  是整份替换 ExecGrant——照着敲会顺手吊销另一条命令。建议改为列全工具要用的命令。
- CLI 卫生：`--timeout` 在解析期就验界（install 不再把授权错误降级成 note 还 exit 0）；
  install 对不声明 exec 的组件硬拒 `--cwd/--timeout`、对已授权过的 exec 明说“没应用”；
  revoke 拒收 flags。
- project 范围的 cwd 必须**已存在**（写错路径不再往用户项目里 mkdir）；plugin-data 范围
  照旧自动建（沙箱本来就是插件的）。
- guest 的 cwd 前缀剥离补上尾部斜杠归一（`/proj/` == `/proj`），并写明词法剥离的
  fail-safe 边界（符号链接拼写误拒时传相对路径）。

### 6.31 并行可行性实核 + 三插件移植（`websearch`/`subagents`/`goal`）+ 两个不迁裁决

剩余五个未移植插件各经一名分析员实核 + 交叉验证员对抗复核（10 agent），按证据裁决：

**移植（3）**：
- **`rpi-websearch`**：DDG + Wikipedia 全 keyless，`net-http` 完全覆盖（自定义 header
  转发、8 MiB 上限）；需两个 allow-list host，单授 DDG 时 fallback 缺席按原设计降级。
- **`rpi-subagents`**：现版本是**纯信封工具**（validate + emit，不起子进程），§6.2 当初
  的排除理由（起子进程）对现行代码不再成立；信封字节序用编译期/宿主两端钉同一个字面量。
- **`rpi-goal`**：`inject`/WASI 时钟/命令/8 action 全通；TurnEnd 走 untyped `other.kind`
  （WIT 无 turn-end tag，人体工学缺口非能力缺口）。**状态取舍（已裁决并写进 README）**：
  fs 监禁进不了项目目录，guest 把 `{projectPath: Goals}` 映射存进插件 data 目录、以
  `info().cwd` 为键——每项目语义保留，但（a）与编译期插件互不可见、**compiled
  orchestrator 的 goal 联动不会因 wasm goal 开启**；（b）键是路径字符串非规范化路径，
  同目录双拼写是两个状态；（c）fs 无 rename，写非原子、撕裂半径比编译期大。若将来要
  忠实落盘项目目录，需给 `fs` 加 `cwd` 式 scope（§6.30 的对称扩展），单独立项。

**不迁（2，均为设计缺口而非一时缺失）**：
- **`rpi-permissions`**：三重缺口（项目 fs 原子写、home 作用域 + env、projectPath 语义）
  之外是**信任倒置**——它的职责是重写宿主工具门所消费的策略文件，让沙箱 guest 拿到这个
  写权限等于让被监管者写监管规则。需要先设计（专用能力或宿主侧代理），不是补 scope 就能迁。
- **`rpi-codegraph`**：`search` 要长活子进程的 stdio 流式句柄，与 orchestrator 同属
  「会话级长生命周期句柄」类缓期能力；`init`/`sync` 一次性命令 exec（project scope）
  倒是能跑，但缺 search 的迁移没有意义。

三移植的交接细节（grant 需求、README 差异清单、e2e）见各 guest 目录；e2e +5（含
`KIQI_WEBSEARCH_LIVE=1` 的 detect-and-return live 测试——解析/过滤路径要真网络，
默认跳过并明说，符合「第三态显式化」）。

### 6.32 ABI 0.7.0：`proc` —— 调用内可交互的子进程（为迁 `rpi-codegraph`）

`exec` 是一次性的（跑完拿输出），表达不了「起一个服务、在它的 stdin/stdout 上做一场会话」。
`rpi-codegraph` 正是这种：**每次调用内**起 `codegraph serve --mcp`，然后用 JSON-RPC 走
stdin/stdout，调用结束杀掉（`plugins/rpi-codegraph/src/lib.rs:315`、`:335-346`、`:385-387`）。
方案与取舍见 `docs/design/plugin-process-capability.md`。

**接口**（新 interface `proc`，包版本 `0.6.0 → 0.7.0`，纯加法）：
`spawn(cmd, args, cwd) -> u32` · `write(handle, bytes) -> u64` · `read-line(handle, max) -> option<string>`
· `read-stderr(handle, max) -> list<u8>` · `wait(handle, timeout-ms) -> option<s32>` · `kill(handle)`。
句柄是 `u32`，形状照抄 `net-sockets`（宿主持句柄，guest 只拿编号）。

**一条硬规则**：句柄只活**一次工具调用**。调用返回（正常/出错/trap/取消）时，宿主对每个句柄
杀**整个进程组**并回收。因此 guest 忘了 `kill` 或半路 trap 都不留残留——「跨调用常驻子进程」
被有意排除在本能力之外（那是宿主拥有调度权的另一件事，见方案 §9）。

**授权与强制**（新可授权能力 `proc`，策略随授权走）：`{commands, cwd, timeout-seconds, max-open}`，
`--command`/`--cwd` 与 `exec` 同一套，`--timeout` **默认 600s、上限 1800s**（刻意不复用 `exec` 的
30s/300s——会话不是有界一次性命令），`--max-open` 默认 100、上限 256。命令裸名 + 白名单、
cwd 走**同一对** `jail_under`/`symlink_checked_under`（与 §6.30 同源，不再各写一份）、
环境清空只回填 `PATH`、子进程独立进程组。三处防阻塞：stdout 行线程 + 有界 channel（聊天止于
他自己的孩子写阻塞，不涨宿主内存）、stderr 有上限缓冲、stdin 由写线程代转（子进程不读时
`write` 到预算即 `failed`，而不是把调用钉死）。

**验收**（e2e 4 条，全绿）：往返（spawn/write/read-line/wait/read-stderr/kill 六函数全触达）、
门禁三态（未授权 / 不在白名单 / cwd 越狱）、`max-open` 上限、以及**收尸**——子进程自报 pid、
guest 故意不杀，调用返回后该 pid 在 **OS 层面**查不到（断言落在 `kill(pid, 0)`，不是宿主自己的账本）。

**本能力不解决** `rpi-orchestrator`：它要「调用之间还在跑」，需要宿主持调度（重新设计），
不是给插件加一层能力能凑出来的。

## 7. 待定

- ~~wasmtime 依赖很重：独立 crate 还是 feature 开关？~~ → 已定：**独立 crate `kiqi-plugin-host` +
  `plugin-wasm` feature（默认开）**；`ki` 因此带上 wasmtime，要精简就关 feature。
- `.wasm` 是否需要签名/摘要校验（防替换；「免签名分发」是卖点，但「防篡改」另说）？
- 插件配置来源（`~/.ki/agent/plugins/<id>.json` 还是走 WIT）？
- `wasm32-wasip2` 工具链版本锁定策略（生产要钉版本）。
