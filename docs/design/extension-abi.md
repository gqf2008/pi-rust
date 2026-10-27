> 注（2026-10-21）：编译期一方插件通道已退役；文中 `extensions/kiqi-*` 路径与 `extension-kiqi-*` feature 是历史记录，现行实现在 `extensions/wasm/kiqi-*`。

# 插件 ABI：从 cdylib 到 wasm 组件

状态：**推迟为第一方契约；作为第三方（不受信）层推进验证** · 目标版本 `kiqi:extension@0.9.0`

> **0.8.0 兼容性（2026-10-24）**：包版本即 ABI 版本。0.8.0 关闭了两个既有 variant
> （`event` 加 `input`、`event-action` 加 `patch`）、改了两个既有签名（`run-command` 返回
> `command-result`、`session-read.cli-flag` 返回 `option<flag-value>`），world 面新增
> `oauth-http`/`provider-stream`/`provider-http` 导入与 `oauth-api`/`provider-api`/`render-api`
> 导出。因此 **0.7.1 组件无法实例化，必须重建**：仓库内 12 个 guest 同批改完
> （`run-command` 返回类型 + `continue-command` + 三个新导出接口的 Guest 实现）；
> 0.8.0 之前没有已发布的第三方 wasm 生态。

> **本方案不再作为第一方插件的契约。** 实测 `rpi-package` 的 18 个包后改为
> 「Rust `trait Extension` + cargo feature 全编译接入」，见
> [extension-trait.md](extension-trait.md)：16/18 需要分步/部分输出、14/18 会起子进程，
> 前者在 WIT 里别扭，后者让沙箱价值归零。
>
> 本文与 `extensions/kiqi-extension.wit` **保留为 wasm 层（不受信第三方插件）的设计存档**：
> 将来 `WasmExtension` 适配器仍要实现同一个 `trait Extension`。
>
> **本轮新增**：不再只是存档——定下一套**参照移植**（§6.2）来验证 ABI 与宿主侧 SDK 的真实成熟度，
> 判据是 §6.3 的 8 条门禁，而不是「迁了几个插件」。第一方**其余插件留在编译期**，
> 只有明确需要第三方分发/沙箱的才迁。
>
> `extensions/kiqi-extension.wit` 自 0.6.0 起逐版 parse-validated；当前包版本 `0.8.0`（见 §6.39–§6.44，0.1.0 连 `wasm-tools component wit`
> 都过不了：variant case 带命名/多字段是非法的）：导出面与 `trait Extension` **一一对应**、四个可授权能力
> （`net-http`/`fs`/`exec`/`net-sockets`）**全部真实现且授权自带策略**、调用一律 `result<_, capability-error>`
> （`denied` 授权拒 / `failed` 操作失败）、进度用 import sink。

## 1. 为什么换

现在的 `kiqi-extension-sdk` 是一套手搓 C ABI：`cdylib` + `libloading` + `StbString`
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
| 总称 | **插件 / extension** |
| 旧实现（cdylib） | **原生插件**（native extension） |
| 新实现（wasm 组件） | **wasm 插件 / wasm 组件** |
| 强调隔离 | 沙箱插件 |

不用「模块」：wasm 里 core module 与 component 是两个不同的东西，且中文语境下
「模块」更偏代码组织单元（Rust `mod`、ES module）。

## 3. 契约

契约在 [`extensions/kiqi-extension.wit`](../../extensions/kiqi-extension.wit)。关键取舍：

- **工具参数用 JSON 字符串**，不在 WIT 里建模整套 JSON Schema —— 否则 ABI 会
  被 JSON Schema 的每个构造拖住。
- **错误是 typed variant**（`unknown-tool` / `invalid-arguments` / `denied` /
  `failed`），运行时要能区分「模型用错了」和「插件坏了」。
- **`event` 有 `other(other-event)` 逃逸分支**（`kind` + `payload` 包成一个 record——WIT 的 variant
  case 只能带一个类型）：新增事件类型不会破坏老插件。
- **包版本即 ABI 版本**，宿主加载时做闭区间校验，取代 `abi_version: 4`。

## 4. 宿主侧 `Extension` trait（草案）

```rust
pub trait Extension: Send + Sync {
    /// 稳定标识，用于日志前缀与配置寻址（如 "kiqi-goal"）。
    fn id(&self) -> &str;

    /// 一次性注册；对应 WIT `tools.list-tools`。
    fn tools(&self) -> Vec<ToolSpec>;

    /// 调用；对应 WIT `tools.call-tool`。参数/结果都是 JSON 字节。
    fn call(&self, tool: &str, arguments: &[u8]) -> Result<Vec<u8>, ExtensionError>;

    /// 事件钩子；对应 WIT `lifecycle.on-event`。默认空实现。
    fn on_event(&self, event: &Event) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// 关闭；对应 WIT `lifecycle.shutdown`。
    fn shutdown(&self) {}
}
```

**两种实现共用这一个 trait**：

- `NativeExtension`：把现在的 `libloading` 加载路径包成 `Extension`（P1，现有扩展零改动）；
- `WasmExtension`：`wasmtime::component::bindgen!` 加载 `.wasm`（P2）。

上层（工具注册、事件分发、`~/.ki/agent/extensions/` 扫描）只认 `Extension`，不再知道
底层是 cdylib 还是 wasm。

> **注：这是早期草案，签名已过时。** 实际契约以 `crates/kiqi-extension` 的
> [`trait Extension`](../../crates/kiqi-extension/src/lib.rs) 为准（见 [extension-trait.md](extension-trait.md) §6）：
> 它是 sync、带 `HostCaps` / `ProgressSink` / `Event`，方法集比本节宽。
> `WasmExtension` 适配的是那一份，不是本节签名。

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
- 存储：`~/.ki/agent/extensions/<id>/grants.json`（`KI_AGENT_DIR` 可覆盖根）；
- CLI：`ki extension grant|revoke|list`；交互 TUI 在缺授权时提示。

### 形状上的硬要求

能力操作**必须可失败**（`result<_, denied>`）：未授权调用返回 `denied` 而不是加载失败，
插件可优雅降级、用户中途授权无需重载。

**为什么不能靠“不挂 interface”来实现未授权**：组件模型要求组件**声明的** import 在实例化时
**全部**被满足（实测：构建出的组件会**剪掉未使用的 import**——conformance fixture 只 import 了
`types`/`log`/`ui`/`progress`，world 里另七个 interface 根本没进组件），所以能“选择性不挂”的
只是那些未被使用的 interface，**用到的**一个都不能缺。于是 wasm 层的实际做法是：

- 宿主为**全部** import 提供实现——已授予的走真实实现，未授予的调用在**调用时**回 `denied`；
- “缺能力 => 不可表达”这个强保证**只属于编译期 `trait Extension` 的 `HostCaps: None`**。

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
2. **sync/async 接缝**：宿主 `trait Extension` 是 sync，wasmtime async component 调用是 async。
   适配器 park（`block_in_place`）还是把接缝改 async，必须先定；
3. **宿主侧 wasm 插件 crate**：wasmtime `bindgen!` + 为**全部** import 提供实现
   （已授予的走真实实现，未授予的挂 `deny` 桩；组件模型要求在实例化时满足全部 import）；
4. **`ki extension test` mock 宿主**：语言无关，一次投入所有 guest 语言共用；
5. **授权模型**（§5）：声明/manifest + grants + 安装/更新提示 + 无头默认拒。

### 6.2 参照集（用最小集合覆盖 ABI 面，不按难易挑）

真实移植：

| 插件 | 覆盖的 ABI 面 | 理由 |
|---|---|---|
| `kiqi-todo` | tools + events + 状态/details + 分支恢复（SessionRead）+ command | 最简；P2 基线 |
| `kiqi-langfuse` | 14 个事件的密集流 + **outgoing HTTP** + UI status + 3 个 HTTP 工具 | 真实压力（事件多、分步 flush、网络） |
| `kiqi-ask-user` | **UI dialog（阻塞式宿主往返）** + command | 专打 sync/async 最硬的点 |

**conformance fixture**（不是真插件）：为实现「每个 import interface + 每个 export 形状」而写的
最小 guest，是 **SDK 成熟度判据**——只用公开 WIT + 生成 binding + 文档，不碰宿主内部，
过同一套一致性测试。

**明确不迁**（选它们是在验证 host 的 exec/会话驱动，不是验证 ABI）：`im-message`、`voice`、
`mcp-adapter`、~~`acp`~~（2026-10-16 实核确认：它要独占宿主进程 stdin/stdout 并在
SessionStart→Shutdown 间维持服务线程——见 `rpi-package/packages/kiqi-acp/src/lib.rs:4-8,67-70`。
正确形态是**宿主级功能**：让 `ki` 本体当 ACP server，而不是插件。
✅ 2026-10-22 销账：`ki acp serve` 已落地（批次 1/2 完成，批次 3 为 abb 对接与本文档销账），
见 `ki-acp-serve.md`；「不迁 wasm」的裁定不变，落地形态就是宿主功能。）、`server`、`run-stats`、
`subagents`、`codegraph`、~~`lens`~~
（起子进程 / 长连 / 后台线程）。

> 2026-10-15（§6.30）：`lens` 移出此名单并已移植（`extensions/wasm/kiqi-lens`）。
> 当初排除它是因为 exec 还没真实现、cwd 只能关在插件 data 目录；exec 授权带上 cwd 范围
> 与时间预算后，“起子进程”不再是 ABI 验证之外的活。
>
> 2026-10-15（第二批）：`kiqi-orchestrator` 从 `rpi-package` 收编为编译期插件
> （`extensions/kiqi-orchestrator`，默认 feature），但**不迁 wasm**——它是长生命周期编排：
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
3. 迁 `kiqi-todo`（打通 tools/事件/状态）→ `kiqi-langfuse`（事件密度 + HTTP）→
   `kiqi-ask-user`（UI/阻塞）；
4. conformance fixture + 跑 §6.3 门禁；
5. 全绿后才谈第二门语言；官方其余插件留在编译期。

### 6.6 已落地：宿主绑定（crate `kiqi-extension-host`）

`crates/kiqi-extension-host` 用 wasmtime 49 的 `bindgen!` 绑 `extensions/kiqi-extension.wit`
（world `extension`），`cargo check` 通过。实测两条 wasmtime 49 的要点，写下来避免踩坑：

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
- 导出面：`extension.kiqi_extension_extension_api()` 返回 `Guest`，其上
  `func_call_tool: TypedFunc<(&str, &str), Result<String, ExtensionError>>`、
  `func_on_event: TypedFunc<(&Event,), ()>`、`func_on_tick`、`func_before_agent_start`、
  `func_probe`、`func_list_tools/commands/flags/shortcuts`。

未做（后续增量）：`WasmExtension` 实现 `kiqi_extension::Extension`（sync 接缝，内部
`instantiate_async` / `call_async`）、host import 实现（`progress` 转发到 `ProgressSink`，
其余先挂 deny 桩）、guest SDK + conformance fixture、§6.3 的门禁测试。

### 6.7 已落地：guest SDK + conformance fixture

`extensions/wasm/` 是**guest 侧独立 workspace**（这些 crate 必须编到 `wasm32-wasip2`，
不能用宿主 target 构建）：

- `kiqi-wasm-bindings`：包 `wit_bindgen::generate!`（world `extension`，ABI 0.2.0）并开
  `pub_export_macro`，插件 crate 实现生成的 `Guest` 再用 `bindings::export!`；
- `kiqi-conformance`：conformance fixture（工具里 `log.write` + 两个 `progress` 块
  + 一个必定 `denied` 的 `ui.set-status`）。

`cargo build --manifest-path extensions/wasm/kiqi-conformance/Cargo.toml --target wasm32-wasip2 --release`
产出 84 KB 的 **component**，导出 `kiqi:extension/extension-api@0.2.0`。

### 6.8 已落地：宿主 `WasmExtension` + e2e（load / call / unload 全通）

- `kiqi-extension-host`：`Engine` **同步**（不是 async）；`Linker` 用
  `World::add_to_linker::<HostState, HasSelf<HostState>>` + `wasmtime_wasi::p2::add_to_linker_sync`；
  `HostState { wasi, table }` 实现**全部** `Host` trait：`log` 真写、`progress` 经**线程局部 sink**
  转发到 `ProgressSink`、`session_read` 返空、`session_control`/`inject`/`ui`/`net_http`/`fs`/
  `net_sockets`/`exec`/`timer` 全 **deny 桩**（每个能力调用回 `denied`）。
- `WasmExtension` 实现 `kiqi_extension::Extension`（**sync 接缝**）：`tools`/`call`/`on_event`/`shutdown`；
  `unload()` = 调 guest `shutdown` + 释放实例（`Drop` 也调），供 `/reload` 用。
- **修正 §6.6 的判断：不用 async。** WASI 0.2 是同步语义，`bindgen!` 生成的 guest 调用就是 sync
  （没有 `call_*_async`），所以宿主全程同步即可，`async_support`/`add_to_linker_async`/`block_on`
  都不需要。真需要异步的是**宿主 import 的实现**（将来），那在实现内部 blocking 即可——
  这也正是“异步关在适配器底部”的原意。
- e2e（`crates/kiqi-extension-host/tests/e2e.rs`）**通过**：构建 fixture → `WasmExtension::load` →
  `tools` → `call`（断言回显参数、`ui=<denied>`、3 个 progress 块、guest `log` 到达宿主）
  → 未知工具是 typed error → `unload()` 后调用报 “unloaded”。
- CI 的 test job 加 `targets: wasm32-wasip2`。

**这轮暴露的 ABI 缺口，两条已在 0.3.0 补上**：

1. ~~`call-tool` 只返回 string~~ → 已改为结构化 `tool-result`
   （`blocks: list<content-block>` + `is-error` + `details` JSON）。e2e 断言 `details` 往返成功，
   `kiqi-todo` 的任务列表从此有地方放。
2. ~~`on-event` 返回 `()`~~ → 已改为返回 `event-action`（`continue`/`claimed`/`replace`）。
   e2e 断言一个 wasm 插件能真 `Claimed` 一个事件。
3. **下一条增量已解决**：`HostCaps` 已接进 host import（见 §6.9）。仍未做的是能力**授权
   （grant）**模型。

### 6.9 已落地：`HostCaps` 接入 + 第一个真 wasm 插件（`kiqi-todo`）

- **`HostCaps` 接进 host import**：调用期间把 `HostCaps` 与 `ProgressSink` 一起放进
  线程局部的 `CallContext`（借用期由 call 保证、调用被互斥序列化）；`session-read` 转发到
  `caps.session`，`session-control`/`inject`/`ui` 转发到对应能力（缺能力或 caps 不存在 →
  `denied`）。**当时现状**（ABI 0.6.0 之前的过渡态；`exec`/`net-sockets` 在 §6.26 已真实现，
这行的「仍是 deny 桩」自此过期）：`net-http` 与 `fs` 已真实现（`fs` 关在插件自己的
  `extensions/<id>/data/` 里，越狱写法直接拒）；`exec`/`net-sockets` 仍是 deny 桩，所以
  `ki extension install` 会对“声明了但本构建无法执行”的能力**明说**（`capability_is_enforced`），
  而不是让用户授权一个永远不会生效的东西；`timer` 已随 `on-tick` 一起删除。
- **`kiqi-todo` 编成 wasm 插件**（`extensions/wasm/kiqi-todo`）：list/add/update/toggle/clear
  语义不变；状态走结构化 `tool-result.details`；`SessionStart`/`SessionTree` 时经
  `session-read.session-branch` 恢复（不再是 runtime action）。
- e2e 新增 `todo_guest_restores_from_the_session_branch`：给一个固定 branch 的 `SessionRead`
  → `SessionStart` 触发恢复 → `list` 文本含恢复项、`details.todos` 带状态 → `add` 后 details 更新。
  两个 e2e 全绿。
- 仍未做：能力**授权（grant）**模型；`session-read.info` 的 session-id/run-id 还返空。

### 6.10 已落地：授权（grant）+ `net-http`

- **`Grants`**：每插件一份，从 `$KI_AGENT_DIR/extensions/<id>/grants.json`（默认
  `~/.ki/agent/extensions/<id>/grants.json`）读；**文件缺失 = 什么都不授予**，宿主不替插件拍板。
  `WasmExtension::load` 自动读，`WasmExtension::load_with_grants` 供调用方自己管授权（与测试）。
- **`net-http` 实现**：宿主侧 `reqwest` blocking；先按 URL 取 host，再查 grant 的
  `hosts` allow-list（`*` 通配）；不在表里就 `denied`。请求/响应 JSON 形状写进了 WIT 注释：
  `{url, method?, headers?, body?}` → `{status, headers, body}`。
- 域能力（`session`/`control`/`inject`/`ui`）的“授予”仍由调用方构造的 `HostCaps` 表达；
  grants 目前只管 wasm 独有、`HostCaps` 没有对应字段的能力。
- e2e 新增 `net_http_is_gated_by_the_users_grant`：无 grant → `denied`；grant 的 hosts 不匹配
  → `denied`；host 匹配 → 真正打到本地一次性 HTTP server 并拿到响应体。3 个 e2e 全绿。
- 与规格的差异：实现用 `grants.json` 而非规格里的 `grants.toml`（省一个依赖，形状等价）。
- 仍未做：`ki extension grant/revoke/list` CLI、安装/更新时的提示与“增量声明才重问”、
  `fs`/`exec`/`timer`/`net-sockets` 的实际实现（仍是 deny 桩）、以及能力错误只有
  `denied` 一个分支（网络失败也被叫 `denied`，应当拆出 `failed`）。

### 6.11 已落地：`kiqi-langfuse` 代表性子集（事件密度 + HTTP）

`extensions/wasm/kiqi-langfuse`：**不是**完整 langfuse（3 个管理工具与完整 usage/cost
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
| 2 面覆盖断言 | ✅ `conformance_surface_is_reachable` 断言契约层被真实触达；**会话层也已接线**（§6.19）：tools / events（原有）+ commands / shortcuts / before_agent_start / probe（新增）；`flags` 的声明由 `ki extension show` 消费（§6.21） |
| 3 流式进度 | ✅ conformance e2e 断言 3 个 progress 块按序到达 |
| 4 UI 往返 | ✅ `ui.dialog` 成功往返 + 取消 + 超时都断言（`kiqi-ask-user` wasm 移植，§6.15） |
| 5 拒绝双态 | ✅ 无 UI → `set_status` denied；授予 UI → granted；net-http 无 grant / 错 host → denied，匹配 → 真请求 |
| 6 用户授权闭环 | ✅ `Grants`（缺失=拒）+ `ki extension grant/revoke/list` + `install`（列声明、**只对增量重问**、无头默认拒）+ `uninstall`（移到备份）（§6.14/§6.17） |
| 7 隔离 | ✅ 燃料计量的死循环 → trap（不挂）；`panic!` → trap；宿主存活 |
| 8 性能基线 | ✅ 同机对比 `todo list`：wasm 33µs/次 vs 编译期 43µs/次（0.8x），断言比值而非绝对阈值（§6.18） |

参照集（§6.2）：`kiqi-todo` ✅（真插件）、`kiqi-langfuse` 子集 ✅（真实压力面）、
`kiqi-ask-user` ✅（§6.15）、conformance fixture ✅。

### 6.13 ABI 0.4.0：能力错误拆 `denied` / `failed`

- `types` 里 `record denied { reason }` 换成 `variant capability-error { denied(string), failed(string) }`；
  所有能力调用（session-control / inject / ui / net-http / fs / net-sockets / exec / timer）
  改签名为 `result<_, capability-error>`。
- 宿主：**授权拒绝 → `denied`**（含 allow-list 不匹配）；**已授予但操作失败 → `failed`**
  （HTTP 发送失败、URL 非法、client 构建失败等）。`forward` 经 `capability_error(ExtensionError)` 区分
  （`ExtensionError::Denied` → denied，其余 → failed）。
- guest：conformance 把错误渲染成 `cap:<reason>` / `cap:failed:<reason>`；langfuse 的 flush 失败
  现在记为 `rejected`，不再是“not granted”。
- 语义收益：**超时的 flush 不再被误报成“没授权”**——正是 §6.11 记下的痛点。
- 这是 0.4.0 breaking：WIT + host + 3 个 guest 一起改，6 个 e2e 全绿。
- 仍未做（重记为需新接缝）：`session-read.info` 的 session-id/run-id 仍返空，需要一个
  `SessionRead::session_id` 接口（不拿进程序 env 顶替）。

### 6.14 已落地：授权落盘 + `ki extension grant/revoke/list`

- `Grants` 从 `kiqi-extension-host` 移到契约 crate **`kiqi-extension::grants`**（加 `serde` 序列化）：
  `load_for` / `save_for` / `allows_host` / `granted` / `grant` / `revoke` / `installed_extensions`。
  这样**宿主（强制）与 CLI（编辑）共用一份类型，且 `kiqi-cli` 不会引入 wasmtime**。
- `ki extension list | show <id> | grant <id> <cap> [--host H]... | revoke <id> <cap>`：读写
  `$KI_AGENT_DIR/extensions/<id>/grants.json`（默认 `~/.ki/agent/extensions/<id>/grants.json`）。
  实测：grant → 落盘 → `list`/`show` 可见 → revoke → `{}`；未知能力/缺参 → exit 2。
- `kiqi_extension::SessionRead` 新增 `session_id()`（默认 `None`，不破坏既有实现）；宿主
  `session-read.info` 的 `session-id` 走它。实现方（`ActionCaps`）暂未覆盖，所以仍可能为空，
  但**接缝在了，且没有拿 env 顶替**。
- 仍未做：`ki extension install/uninstall` 本身、以及“安装/更新时列出声明 + 增量重问”的提示
  （要有 install 才有落点）。

### 6.15 已落地：`kiqi-ask-user` 忠实移植（UI 往返 + 取消 + 超时）

`extensions/wasm/kiqi-ask-user`：参数归一化与编译期插件一致
（`question`/`options`/`allow*`/`displayMode`/`timeout`/`confirm`/`questions`），交互则不同——
C ABI 不能阻塞，原生版才需要 start/poll/cancel 状态机；wasm 里 `call_tool` 直接阻塞，
所以是直筒的 **open → poll → answer**。

- 每个问题：`ui.dialog({op:"open",...})`，然后循环 `{op:"poll"}`：`answered` 收答案、
  `cancelled` 报错、`timeout` 到点先发 `{op:"cancel"}` 再报 “ask timed out”。
- e2e 4 个：问答往返（答案进 `details.answers`）、confirm 映射 `Confirmed`/`Declined`、
  **取消**、**超时**（并断言 guest 真发了 `cancel`）。
- 门禁 4 由此收绿；**e2e 共 9 个全绿**。

### 6.16 已落地：wasm 后端接进 `ki`

- `ki` 的 `load_hosts()` 除编译期插件外，还扫描 `$KI_AGENT_DIR/extensions/<id>/extension.wasm`
  （`extension-wasm` feature，默认开），用 `WasmExtension::load`（顺带读同目录 `grants.json`）加载，
  包成同一个 `ExtensionHost`——**没有第二套注册/派发路径**：工具经 `merge_compiled_extension_tools`、
  事件经 `install_compiled_event_sink`，与编译期插件完全同路。
- 加载失败（坏组件 / 缺 WASI import）只打印并跳过，不拖垮会话。
- 新增 kiqi-cli 测试：把 conformance fixture 放到临时 `extensions/conformance/extension.wasm`，
  断言被发现、工具可见、且调用跑通（经 `HostCaps::observer` 拿到 `ui=` 的拒绝报告）。
- 代价：`ki` 现依赖 `kiqi-extension-host` → **wasmtime 进了主二进制**（体积/编译时间上升），
  所以给了 `extension-wasm` feature（默认开）；要精简二进制可关掉。

### 6.17 已落地：`ki extension install/uninstall`（声明 + 增量重问 + 无头默认拒）

- `ki extension install <path.wasm> [--id ID] [--yes] [--host H]...`：
  - 把组件复制到 `extensions/<id>/extension.wasm`；
  - **读组件声明的能力**（`kiqi_extension_host::declared_capabilities`：wasmparser 扫 `kiqi:extension/*`
    import，只取可授权的 `net-http`/`fs`/`exec`/`timer`/`net-sockets`；`log`/`progress`/`ui` 等域接口不参与授权）；
  - **只对“声明了但还没授权”的增量征求意见**：`--yes` 全批；有 tty 则一次 `[y/N]`；
    **无 tty 且没 `--yes` → 什么都不授**（无头默认拒）；
  - `net-http` 的 host allow-list 走 `--host`（缺则该项不授并提示）。
- `ki extension uninstall <id>`：把 `extensions/<id>` **移到** `extensions/<id>.removed-<ts>`（可恢复），不删文件。
- 测试：host `declared_capabilities`（conformance fixture 只声明 `net-http`）；kiqi-cli
  `install --yes --host 127.0.0.1` → 复制 + `grants.granted()==["net-http"]` + host 匹配 →
  `uninstall` → 目录被移走。**10 个 host e2e + 4 个 CLI 测试绿。**
- 门禁 6 由此收绿；§6.3 只剩门禁 8（性能基线）。

### 6.18 已落地：性能基线（门禁 8）

`crates/kiqi-cli` 里一个同进程对比测试：同一个 `todo` list 调用，编译期插件 vs wasm 插件，warm 后
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
`before_agent_start` / `probe` 在 trait 层实现、也在 `ExtensionHost` 层测过，但会话从不主动问。现在五个面全部接上：

- **commands**：`register_compiled_commands` 把声明注册进 TUI 命令表（内置先注，重复跳过，不能遮蔽
  `/help`）；`CompiledCommand::execute` 调 `run_command` 把返回文本记进 transcript。
- **shortcuts**：`dispatch_key_event` 对声明了该键的插件派发 `Input` 事件；`Claimed`/`Replace` 即消费按键，
  与原生 `CLAIMED` 同义。只问 `shortcut_hosts` 筛出来的插件。
- **before_agent_start**：`compiled_prompt_hooks` 链在原生 `ExtensionProviderHooks` 之后，原生先算出提示词、
  编译期/wasm 插件再有机会替换；**按 prompt 时间戳去重**（一次工具循环的多次 provider 调用只算一次）。
  `kiqi_extensions::before_agent_start_input` 让两半共用同一份载荷形状。
- **probe**：`probe_tool_call` 在 harness 的 `BeforeToolCall` 钩子里跑 `before-tool-call` 闸门。它本地同步，
  所以否决在原生的审批对话框弹开**之前**就生效；`Replace` 会带进原生派发，两边看到同一份 args。
- **flags**：**不需要接线**——`get_cli_flag` 本就从“通用解析的未知长选项”里取值，插件的 `cli_flag(name)`
  直接读到；声明只用于可发现性。

可测性：闸门 / 变换 / 按键筛选都收显式 host 列表（`probe_tool_call_with`、`compiled_prompt_hooks_with`、
`shortcut_hosts`），所以测试能注入合成插件；另有一个测试加载**真的 conformance wasm 组件**，证明这套
会话路径同样能到达沙箱插件。命令面由 `interactive_tui::commands` 的两个单测锁住。

### 6.20 信任边界：沙箱插件不得顶掉既有工具名

接线时发现一个真实的越权路径：`merge_compiled_extension_tools` 按名**覆盖**（`Some(slot) => *slot = harness_tool`），
而装载顺序是 内建 → cdylib 扩展 → 编译期 → wasm，所以**后加载者胜**。于是第三方 `extension.wasm` 只要声明一个叫
`bash` 的工具，模型随后调的 `bash` 就是它——沙箱/授权全被绕过（“extension overrides same-named
built-in”那句注释本来是给**受信**的 cdylib 层写的）。

对策：`ExtensionHost` 带上 `untrusted` 标记（wasm 层为 `true`），`merge_compiled_extension_tools`
对 `untrusted` 的 host **遇到已占用名就拒绝 + 告警**，受信层保持原行为。拒绝只针对**冲突名**：
第三方插件贡献新工具照常（有测试分别锁定两件事）。

同时补上了 `session-read` 的 `session_id`：`RuntimeActionHost::session_id`（默认 `None`）→
`HarnessActionHost` 从 `tool_context` 取（`new_session`/`fork`/`switch_session` 会同步更新）→
`ActionCaps::session_id`。此前该能力恒返 `None`，靠事件载荷的 `sessionId` 兼顾；现在两条路一致
（两条通道由 `publish_session_identity` 同时写入，值不会分叉）。

### 6.21 `flags`/`shortcuts` 的消费者，与三套传输的面差

`flags`（以及 `shortcuts`）是**声明**面：运行时路径分别在 `SessionRead::cli_flag`（值来自通用
`--foo=bar` 解析）和“按键走 `Input` 事件”。但原先这个**声明本身没有任何读者**：`snapshot.flags()`
只有测试在调，`get_cli_flag` 也不查 registry——即“挂了名字的装饰”。现在 `ki extension show <id>` 会实例化
插件并把它声明的 tools / commands / flags / shortcuts / probe-points 列出来，声明由此变成可观测、可断言的。

三套传输的面**本来就不同**，这里显式列清，别再让“8 个面”到处飘：

| 面 | cdylib 扩展 | 编译期 kiqi | wasm |
|---|---|---|---|
| tools / events / commands / flags / shortcuts / before_agent_start | ✅ | ✅ | ✅ |
| probe（权限闸门） | ❌ | ✅ | ✅ |
| renderer / provider / resources_discover / oauth | ✅（上游用；本仓无对应 `Extension` 面） | ❌ | ❌ |

`ki` **同时**装载三层并合并进同一张工具表；同名工具的归属见 §6.20（受信层可覆盖，untrusted 拒绝）。

### 6.22 `/reload` 不再弄丢插件的 UI 能力

`/reload` 会换掉 `ActionBridge`，但**不换** UI mailbox（它们存在 `ReloadContext` 里，就是为了跨桥存活）。
原先重建工具适配器时给的是一个 `ui: None` 的 caps，于是重载后编译期/wasm 插件的 `ui.dialog` /
`set_status` / `set_editor_text` 全没了（其他能力还在）。现在 `reload_caps()` 从 reload 上下文取同一组
mailbox，并且**顺带刷新事件 sink 与全局 caps**，让重载后的 tools / events / commands / probes /
before_agent_start 跑在同一套一致的能力上。测试：拿 `reload_caps` 发一个 dialog，断言它落到 TUI 挂着的
那个 mailbox。

### 6.23 `on_event` 的动作语义在编译期/wasm 层闭合（并修掉一次重复投递）

`Extension::on_event` 返回 `EventAction`，但全局 sink 的类型是 `Arc<dyn Fn(u32, Value)>`——**没有返回值**，
调用处 `let _ = host.on_event(..)` 把动作丢掉。于是编译期插件（以及复用同一条路径的 wasm 插件）
永远无法 claim，而原生 handler 的 `CLAIMED` 是被认的。

现在 `CompiledEventSink` 返回 `CompiledVerdict::{Continue, Claimed}`，`dispatch_data_event_claiming`
把编译期侧的结果并入自己的返回值（并接收 `Option<&RegistrySnapshot>`，因为编译期扇出不需要原生 registry）。
两个 observe 型漏斗仍不消费它，与原生一致。

顺带修了一个本轮引入的真 bug：`dispatch_key_event` 既走漏斗（sink 会把 `Input` 扇一遍）又自己再扇一遍，
**编译期插件会收到两次同一个按键**。现在只走漏斗，按键筛选（只问声明了该键的插件）移到 sink 里，
由 `dispatch_to_compiled()` 统一负责，并有测试分别钉住“声明了的键被问到并 claim”与“未声明的键
根本不会被问到”。

### 6.24 ABI 0.5.0：wasm 版就是 `trait Extension` 那一份（面一一对应）

需求一句话：**wasm 要一份和 `trait Extension` 一样的契约**。逐面比对后发现四处对不上，已对齐：

| `trait Extension` | 0.4.0 的 WIT | 0.5.0 |
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
若要后台 tick，正确做法是**两边一起加**（先给 `trait Extension` 加 `on_tick` + `HostCaps` 的 timer），
而不是让 wasm 单方面多出一个面。

### 6.25 能力不再开空头支票：`fs` 真实现，`exec`/`net-sockets` 明说做不到

原先 `fs`/`net-sockets`/`exec` 都是 `deny_capability!` 桩，但 `ki extension install` 会把它们
列给用户授权、写进 `grants.json`——**授权了也永远是 `denied`**，只有 `net-http` 是真的。这是
“声明了但没人执行”那一类问题的安全版：用户以为给了权限，其实什么也没给。

- **`fs` 实现了**，关在插件自己的数据目录 `extensions/<id>/data/`（从 `extension.wasm` 的位置推出来，
  所以临时目录里的测试天然隔离）。绝对路径和任何 `..` 组件直接拒（逐组件解析 + `starts_with` 复核），
  **监禁就是策略**，所以 `fs` 保持布尔授权。
- **`exec`/`net-sockets` 仍是桩**，但 `ki extension install` 现在会对声明了它们、而本构建无法执行的
  能力**明说**——依据是 `kiqi_extension_host::capability_is_enforced()`（单一事实源），一句
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
| `fs` | `true` | 监禁在 `extensions/<id>/data/`（监禁即策略，所以仍是布尔） |

“策略缺失”是**错误**而非“全放开”：`grant("exec", vec![])` 返回 `exec needs at least one --command`。
CLI 侧 `ki extension grant <id> exec --command echo`、`ki extension install … --command echo`（`--host` 同理）。

本次之后 `kiqi_extension_host::ENFORCED_CAPABILITIES` 覆盖全部四个可授权能力，所以 `ki extension install`
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

> 注意：新增 `kiqi-extension-host` 作为 workspace 成员后，`cargo test --workspace` /
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

**移植的两个插件**（都拿真实的 `Extension::*` 语义对照，不是演示）：

- **`kiqi-webfetch` → `extensions/wasm/kiqi-webfetch`**：走 `net-http`。保留了插件自己的 URL 策略
  （绝本地/绝字面私有 IP、绝内嵌凭证），两个差异写在 guest 文档里而不是藏起来：
  ① **不做 DNS 校验**（guest 无法解析，主机名交给用户允许列表管）；② **不再校验最终 URL**
  （宿主回的是 status/headers/body，重定向安全已由宿主逐跳白名单保证）。
- **`kiqi-plan-mode` → `extensions/wasm/kiqi-plan-mode`**：走 `session-read`/`session-control`/`inject`。
  逻辑（保留哪些工具、`/plan` 打印什么、大纲解析）**一份实现**，抽到 `crates/kiqi-plan-mode-core`，
  编译期插件与 guest 共用——两个 `PlanHost` 适配器分别接 `HostCaps` 和 WIT 导入。
  也正因为如此，“WIT 缺少 `append-custom-entry`” 这个缺口无法用“再写一份”绕过去。

e2e：webfetch 验“插件策略先拦 + 没授权就 denied”（全 hermetic，不撞公网）；plan-mode 用记录型
`SessionControl`/`InjectApi` 验“进入时换掉工具集、完成时恢复并写 `plan-mode` 自定义 entry、
`/plan show` 能读回计划”。

### 6.30 `exec` 的授权带 cwd 范围与时间预算（`kiqi-lens` 移植）

handover 里点名的“下一个真实阻塞点”：诊断类插件要在**项目目录**跑 `cargo`/`clippy`，而
`exec` 的 cwd 只能关在插件自己的 data 目录。这轮把它解开，并把 §6.2「明确不迁」名单里的
`lens` 移出来移植掉（排除它的理由——“验证 host 的 exec 驱动”——在 6.26 exec 真实现后
已经过期）。

**授权形状**（照 §5「策略随授权走」）：`exec` 从 `{commands:[...]}` 扩成

```json
{ "commands": ["cargo", "git"], "cwd": "project", "timeout-seconds": 120 }
```

- `cwd`: `extension-data`（默认）| `project`。旧 grants.json 没有这两个字段，serde default
  落到 `extension-data` + 30s——**旧授权语义不变**；老宿主读到新文件里的未知字段直接忽略，
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

**移植 `kiqi-lens` → `extensions/wasm/kiqi-lens`**（走 `exec` + `session-read`）。
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

顺手修掉编译期 `kiqi-lens` 的一个潜伏 panic：输出截断 `text.truncate(40_000)` 在多字节
字符中间下刀会 panic（`String::truncate` 的字符边界语义），改成退到字符边界再截，
带回归测试（先红后绿验证过）；wasm 版不继承这个 bug。

e2e（`kiqi-extension-host`）：cwd 默认 data 目录、相对 cwd 下钻、`..`/绝对路径越狱被拒、
符号链接父目录被拒（unix）、project scope 落在注入的项目目录、超时预算真掐死 `sleep 10`；
lens 移植验“project scope 下 `git diff --check` exit=0 / data 目录下报 Not a git repository /
无授权报 denied 并指给用户该敲什么命令”。

**审查后修正**（这批 diff 过了一轮分维度对抗审查，确认并修掉的）：

- **监狱检查先建目录后验路径**（承重缺陷，fs 老代码带过来的）：`create_dir_all` 在
  canonicalize 之前跑，符号链接中间件会把目录建到监狱外——操作照样被拒，污染却留下了。
  改成先验最深的现存前缀、再建（必然干净的）尾巴；回归测试钉住“拒绝后监狱外零创建”。
- **拒绝文案的破坏性引导**：guest 的 deny 建议只写被拦的那条命令，而 `ki extension grant`
  是整份替换 ExecGrant——照着敲会顺手吊销另一条命令。建议改为列全工具要用的命令。
- CLI 卫生：`--timeout` 在解析期就验界（install 不再把授权错误降级成 note 还 exit 0）；
  install 对不声明 exec 的组件硬拒 `--cwd/--timeout`、对已授权过的 exec 明说“没应用”；
  revoke 拒收 flags。
- project 范围的 cwd 必须**已存在**（写错路径不再往用户项目里 mkdir）；extension-data 范围
  照旧自动建（沙箱本来就是插件的）。
- guest 的 cwd 前缀剥离补上尾部斜杠归一（`/proj/` == `/proj`），并写明词法剥离的
  fail-safe 边界（符号链接拼写误拒时传相对路径）。

### 6.31 并行可行性实核 + 三插件移植（`websearch`/`subagents`/`goal`）+ 两个不迁裁决

剩余五个未移植插件各经一名分析员实核 + 交叉验证员对抗复核（10 agent），按证据裁决：

**移植（3）**：
- **`kiqi-websearch`**：DDG + Wikipedia 全 keyless，`net-http` 完全覆盖（自定义 header
  转发、8 MiB 上限）；需两个 allow-list host，单授 DDG 时 fallback 缺席按原设计降级。
- **`kiqi-subagents`**：现版本是**纯信封工具**（validate + emit，不起子进程），§6.2 当初
  的排除理由（起子进程）对现行代码不再成立；信封字节序用编译期/宿主两端钉同一个字面量。
- **`kiqi-goal`**：`inject`/WASI 时钟/命令/8 action 全通；TurnEnd 走 untyped `other.kind`
  （WIT 无 turn-end tag，人体工学缺口非能力缺口）。**状态取舍（已裁决并写进 README）**：
  fs 监禁进不了项目目录，guest 把 `{projectPath: Goals}` 映射存进插件 data 目录、以
  `info().cwd` 为键——每项目语义保留，但（a）与编译期插件互不可见、**compiled
  orchestrator 的 goal 联动不会因 wasm goal 开启**；（b）键是路径字符串非规范化路径，
  同目录双拼写是两个状态；（c）fs 无 rename，写非原子、撕裂半径比编译期大。若将来要
  忠实落盘项目目录，需给 `fs` 加 `cwd` 式 scope（§6.30 的对称扩展），单独立项。

**不迁（2，均为设计缺口而非一时缺失）**：
- **`kiqi-permissions`**：三重缺口（项目 fs 原子写、home 作用域 + env、projectPath 语义）
  之外是**信任倒置**——它的职责是重写宿主工具门所消费的策略文件，让沙箱 guest 拿到这个
  写权限等于让被监管者写监管规则。需要先设计（专用能力或宿主侧代理），不是补 scope 就能迁。
- **`kiqi-codegraph`**：`search` 要长活子进程的 stdio 流式句柄，与 orchestrator 同属
  「会话级长生命周期句柄」类缓期能力；`init`/`sync` 一次性命令 exec（project scope）
  倒是能跑，但缺 search 的迁移没有意义。

三移植的交接细节（grant 需求、README 差异清单、e2e）见各 guest 目录；e2e +5（含
`KIQI_WEBSEARCH_LIVE=1` 的 detect-and-return live 测试——解析/过滤路径要真网络，
默认跳过并明说，符合「第三态显式化」）。

### 6.32 ABI 0.7.0：`proc` —— 调用内可交互的子进程（为迁 `kiqi-codegraph`）

`exec` 是一次性的（跑完拿输出），表达不了「起一个服务、在它的 stdin/stdout 上做一场会话」。
`kiqi-codegraph` 正是这种：**每次调用内**起 `codegraph serve --mcp`，然后用 JSON-RPC 走
stdin/stdout，调用结束杀掉（`extensions/kiqi-codegraph/src/lib.rs:315`、`:335-346`、`:385-387`）。
方案与取舍见 `docs/design/extension-process-capability.md`。

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

**本能力不解决** `kiqi-orchestrator`：它要「调用之间还在跑」，需要宿主持调度（重新设计），
不是给插件加一层能力能凑出来的。

### 6.33 第一个真实消费者：`kiqi-codegraph` 迁 wasm（并顺手修了契约缺口）

`proc` 落地后立刻有了消费者。移植时发现契约少了一个参数——`read-line` 原本只受子进程预算
（600s）约束，而 codegraph 的会话超时是 20s、且它的超时文案（“try `codegraph unlock`”）
值得保留；同步 guest 又没法自己打断一次阻塞读。**趁 0.7.0 未发布补上
`read-line(handle, max-bytes, timeout-ms)`**（`0` = 等到预算用尽）——第一个消费者暴露的缺口，
如实补契约而不是让 guest 硬扛。

**移植形态**（`extensions/wasm/kiqi-codegraph`）：八个工具与 schema 逐条照搬；
索引探测、`init`、`sync` 走 `exec`（一次性）；MCP 会话走 `proc`（`serve --mcp` 每次调用内起、
会话结束即杀——与编译期版逐条对应）。五条差异写进 guest README，其中两条值得在这里点名：

1. **探测 `.codegraph/` 用 `test` 命令而不是 `stat`**：guest 的 `fs` 监禁在自己的 data 目录，
   看不见项目树。代价是 exec 白名单要多一个 `test`；没有 `test` 二进制的平台（Windows）
   会明确失败而不是猜。
2. **Windows 留在编译期版**：编译期版靠 PowerShell 解析 `codegraph` 的真实路径（npm/Scoop
   shim 不解析），而 `exec`/`proc` 的契约是裸名——这条平台差异不是没做，是契约不允许。

e2e 三条：八工具面与 schema 形状、**探测决策**（有/无索引两种情形，用「下一个需要授权的子命令
是 sync 还是 init」来断言——这条断言在实现里抓到了一个真 bug：探测路径我把 `cwd` 已是 root
这件事算重了）、以及有真 `codegraph` 时的 live 会话（没有就明示跳过）。

### 6.34 `kiqi-orchestrator` 迁 wasm：阻塞波 + `proc.close-stdin`（ABI 0.7.1，纯加法）

> 补记（2026-10-20）：这一节当初**没有落笔**，而 AGENTS.md 与
> `extension-abi-handover.md` 从 `393624c` 起就一直在引用 §6.34。四份文档指向一个不存在的
> 小节。内容据 `c41421d` 补齐，不是事后重写的历史。

业主的裁决是：迁 wasm，接受语义缩减，而不是继续把它编进默认构建。
`extensions/wasm/kiqi-orchestrator` 自带一份三个工具与指引的副本（lens/goal 的先例：
逻辑小到可以复制）。

| 工具 | 迁后的形态 | 代价 |
|---|---|---|
| `plan` | 图、校验、update 语法与渲染沿用编译期逻辑；状态驻留 guest 内存，跨调用保留，`sessionId` 变化时清空 | 无 |
| `dispatch` | 一波在调用内跑完：ready 节点在 `proc` 上并行（宿主按授权 `max-open` 限制），guest 多路复用 stdout 并按节点 `timeoutSeconds` 杀子进程 | **放弃后台波与中途重塑**；`waitSeconds: 0` 被拒（子进程不能活过调用），`waitSeconds: N` 到时杀掉并失败剩余节点。波与波之间仍可重塑 |
| `subagent` | task 走 stdin、EOF、收集 | 子进程是 PATH 上的 `ki`（wasm 里没有 `current_exe()`）；`program` 覆盖是给隔离测试的缝，它收不到 ki 专属 flag，且仍受 allow-list 约束 |
| `expectFile` / `verify` | 走 `exec`（`test -f` / `sh -c`） | 用它们的节点需要把这两个命令放进 exec allow-list |
| 指引注入 | 仍是**同一个门**：只有 `.ki/goals.json` 里有 active goal 时才追加（读该文件需要 `fs` 授权） | `KI_ORCHESTRATOR_GUIDE` env 覆盖没有了——guest 读不到环境变量，只能由文件决定。`sessionId` 变化会清空 `plan` 状态，与编译期版一致 |

契约侧只加了一条：**`proc.close-stdin`**（0.7.1，纯加法）。task 走 stdin，EOF 才是「prompt
写完了」的信号——这正是编译期版一直用的「写完就 drop stdin」协议。

构建面：`kiqi-orchestrator` 离开默认构建（默认只剩 `kiqi-permissions`，信任倒置的排除裁决
不变），CI 的 opt-in 配置加上该 feature；注册表清单测试按 feature 门控 orchestrator 条目，
两种配置下都有意义。

### 6.35 `fs` 授权带 scope（**ABI 不变**，仍 0.7.1——只动授权模型与宿主，不动 WIT）（补齐与 exec/proc 的不对称）

`exec`/`proc` 都有 cwd scope（extension-data | project），`fs` 却只有 data 监禁——goal 状态落不进
项目目录、codegraph 探测靠 `test` 命令，根源都是它。本轮补齐：`fs` 授权从裸 `true` 变为
`{ "cwd": "extension-data" | "project" }`，**老文件 `"fs": true` 经自定义反序列化照读为 data 监禁**
（零迁移）；宿主三个 fs 调用点收敛到一个 `fs_checked`（jail + symlink 同一对函数，与 exec/proc
同源）。默认不变（data 监狱）；`--cwd project` 后 guest 可读写项目树内的文件——
这是「编译期插件能做到、wasm 层此前做不到」清单上的又一项消除。

e2e：project scope 下 fixture 的 fs 往返落在项目树内；data-dir 默认行为不变；
CLI `grant fs --cwd project` 的 JSON 往返有单测钉住。

至此默认构建只剩两个编译期插件：`kiqi-orchestrator`（调度模型错位）与 `kiqi-permissions`
（信任倒置）——都不是缺能力，是 wasm 层当前形状表达不了或有设计缺口。

## 7. 待定

> 2026-10-17（销账一半）：已实现 **SHA-256 sidecar**——`ki insmod`/`ki extension install` 写
> `extension.wasm.sha256`，加载前校验（不匹配/不可读 → 跳过并报错；缺失 → 加载 + 汇总警告），
> `ki extension show` 展示状态。**定性为完整性/损坏检测，不是防篡改**（能改 wasm 者同样能改
> sidecar）。**detached 签名（provenance）仍待定**：等有第三方分发渠道再做，届时才需要信任根。


- ~~wasmtime 依赖很重：独立 crate 还是 feature 开关？~~ → 已定：**独立 crate `kiqi-extension-host` +
  `extension-wasm` feature（默认开）**；`ki` 因此带上 wasmtime，要精简就关 feature。
- `.wasm` 是否需要签名/摘要校验（防替换；「免签名分发」是卖点，但「防篡改」另说）？
- 插件配置来源（`~/.ki/agent/extensions/<id>.json` 还是走 WIT）？
- `wasm32-wasip2` 工具链版本锁定策略（生产要钉版本）。

### 6.36 goal→orchestrator 联动在 wasm 层恢复（2026-10-16，option B）

背景：§6.31 把 goal 状态搬进沙箱 data 目录，代价是编译期 orchestrator 的 goal 联动失效
（它读 `<cwd>/.ki/goals.json`）；wasm orchestrator guest 只好改为**无条件**注入
「## Subagent orchestration」指南——装上就唠叨，而不是有目标才提醒。业主裁定恢复原语义。

做法（零宿主改动，用 §6.35 的 `fs --cwd project`）：goal guest 的状态源搬回
`<cwd>/.ki/goals.json`（编译期同款形状 `{"active":…,"archived":[…]}`）；orchestrator
guest 以同授权读该文件并门控（编译期 `active_goal_from` 语义：active 非空且非 paused；
缺失/损坏 fail-closed）。编译期的 `KI_ORCHESTRATOR_GUIDE` env 覆盖项如实去掉（guest 无环境）。

如实记录的差异：① `fs` 接口只有 read/write/read-dir，**没有 rename**——写为原地截断而
非编译期的原子 tmp+rename（爆炸半径收敛为单个项目）；② 迁移分支在发货布局下**不可达**
（fs 单一 scope：新授权指向项目树，旧数据目录在 `~/.ki` 之下），升级即从空开始，注释与
README 均如实陈述；③ 旧包升级不会自动改 fs scope——升级后需
`ki extension grant <id> fs --cwd project`（`--yes` 只授"已声明未授"的能力）。

e2e（`crates/kiqi-extension-host/tests/e2e.rs`）新增端到端联动测试：把 goal guest 的**真实
输出**喂给 orchestrator guest，断言指南开关跟随 goal 状态。

### 6.37 观察事件不再等待等待用户的 guest（2026-10-18）

现场：一个长达 13MB 的 TUI 会话在模型调用 `ask_user` 后停在 `Working…` 36 分钟，
Esc 无响应。进程采样显示按键线程永久阻塞在一个 std mutex 上；会话文件最后一条是
`tool_started(ask_user)`，之后没有任何记录。

根因（实核）：`ask_user` 的 wasm guest 在 `call_tool` 里 open→poll 等待用户，整个
调用期间持有 `WasmExtension.inner`；而 TUI 的按键线程在打开选择框**之前**先调用
`dispatch_ui_prompt_event(UiPromptStart)`（`open_selector_with_view` 第一步），
它经 `dispatch_to_handlers` → `compiled::dispatch` → `dispatch_to_compiled` 走到
**每一个** wasm host 的 `on_event`，后者 `self.inner.lock()` 阻塞——按钮线程被自己
要问的那个用户卡住，对话框永远不会出现。

修法：把分发拆成两种语义。观察面（`dispatch_to_handlers`、lifecycle）用
`Extension::try_on_event`——默认退回 `on_event`，wasm host 覆盖为 `try_lock`，忙碌即
跳过（观察事件是建议性的，丢一条优于挂死 UI）；决定控制流的 claiming 面
（`dispatch_data_event_claiming`）继续用阻塞的 `on_event`，不允许 fail-open。

残留限制：claiming 面仍可能等一个长期 wasm 调用结束（工具审批场景没有 `ask_user`
这种无限等待，属于可接受的有界等待）。回归测试：
`kiqi-extension-host/tests/e2e.rs::observe_events_skip_a_guest_that_is_waiting_on_the_ui`
（真实 ask_user guest，断言 observe 2 秒内返回 Continue），以及 kiqi-cli 的
`observe_dispatch_does_not_wait_for_a_busy_extension`（模式选择）。

### 6.38 `proc.cancel-requested`：wasm 工具调用的取消查询（**纯加法，包版本仍 0.7.1**）

wasm 工具调用是同步的：宿主在 `Extension::call` 里安装 `HostCaps`（含 `cancel` 探针），
guest 却没有任何接口能看见“用户按了 Esc”。长调用（典型是 orchestrator 的
`dispatch` 等 `waitSeconds`）因此无法提前收尾。

新增一个 host function：

```wit
interface proc {
    /// Whether the current tool call has been cancelled by the host (Esc /
    /// abort). Long-running guests should poll this and wind down.
    cancel-requested: func() -> bool;
}
```

- **宿主**：`with_caps_context` 读当前调用的 `caps.cancel`，无调用上下文时返回
  `false`。探针由 kiqi-cli 的 `HarnessCancel`（工具执行的 `CancellationToken`）
  提供。
- **guest**：`kiqi-orchestrator` 的 `dispatch` 在启动前和每轮 wave 轮询时
  检查；`subagent` 在 spawn 前、以及每个读 slice（`POLL_SLICE_MS = 100`）前检查。
  命中后杀掉仍存的子进程并返回 `cancelled by the host`（`dispatch` 走
  `fail_remaining`）。宿主在调用返回时仍会统一收尸，这条只是让长等待提前结束 —
  单次 `subagent` 因此不再等满 `timeoutSeconds`（默认 300 s）而占着插件的调用锁。
- **兼容性**：这是宿主提供的**新增导入**，旧 guest 不引用它，仍按 0.7.1 实例化；
  因此包版本不抬，避免让所有已安装 guest 一起失效。
- **构建**：`extensions/wasm/kiqi-wasm-bindings/build.rs` 增加
  `rerun-if-changed=../../kiqi-extension.wit`，否则 Cargo 看不到 WIT 变更，缓存里的
  旧绑定会让 guest 报 `cancel_requested` 不存在。

验证：`crates/kiqi-cli/src/extensions.rs::a_cancelled_wasm_dispatch_returns_immediately`
（构造一个已取消的探针，断言 wasm `dispatch` 在 spawn 前返回 `cancelled by the host`）。

### 6.39 授权模型：`oauth` / `provider` / `render` / `resources` / `provider-request`（0.8.0）

0.8.0 新增五个授权 key，全部 **deny-by-default**（key 缺失即能力不存在），且全部**授权自带策略**
——沿用 `net-http` 确立的规则：「granted」与「to what」是同一个决定。五个 key 都不接受
`exec`/`proc` 的 `--cwd`/`--max-open`；通用 `Grants::grant()` 会拒绝新 key 并指向专用构造函数
（`crates/kiqi-extension/src/grants.rs:1529-1570`）。

| key | serde 形状（kebab-case） | 默认值 | 校验 / 夹取 |
|---|---|---|---|
| `oauth` | `{providers: [{id, display-name?, scopes?, flows, authorization-endpoint, token-endpoint, device-authorization-endpoint?, revocation-endpoint?, client-id, supports-pkce?}], timeout-seconds?, max-requests?}` | `timeout-seconds` 600；`max-requests` 16 | providers 非空；id 非空且唯一；flows 非空；两个必需端点及可选端点都必须是**精确主机的 `https://` URL**（无通配、无 userinfo、无 fragment）；授予 device-code 时必须有 `device-authorization-endpoint`；timeout 1..=900；max-requests 1..=64（`grants.rs:353-363,395-437,444-517`） |
| `provider` | `{providers: [{id, display-name?, models: [{id, api, base-url?, reasoning?, input?}], endpoint: {scheme, host, path-prefix, methods?}, credential?, auth?, timeout-seconds?, max-response-bytes?, max-handles?}]}` | `timeout-seconds` 600；`methods` `["POST"]`；响应 8 MiB；句柄 8 | providers/models 非空；id 与 model id 唯一；`api` 只接受 10 个 typed wire API（`faux` 与未知值拒绝）；endpoint scheme 必须 `https`、host 精确无通配、path 拒绝编码分隔符/dot/空 segment、methods 仅 `GET`/`POST`；带 credential 时 `path-prefix` 不得为 `/`；timeout 1..=900；响应 64 KiB..=32 MiB；句柄 1..=64 |
| `render` | `true` 或 `{kinds?, timeout-ms?, max-output-bytes?}` | kinds `["markdown"]`；2000 ms；1 MiB | kind 仅 `markdown`；timeout 1..=10000；output 1..=8 MiB（`grants.rs:799-896`） |
| `resources` | `true` 或 `{kinds?, max-items?, max-bytes?}` | kinds `["skills","prompts"]`；64 项；1 MiB | kind 仅 `skills`/`prompts`；max-items 1..=256；max-bytes 1..=16 MiB（`grants.rs:899-1001`） |
| `provider-request` | `{observe?, headers?, messages?, patch?, patch-timeout-ms?}` | observe `none`；headers `names`；messages `none`；patch `false`；2000 ms | `patch-timeout-ms` 1..=min(10000, provider 最紧预算)；`patch: true` 才接受补丁；observe 只有 `none`/`requests`（`grants.rs:1048-1150`） |

`render`/`resources` 的裸 `true` 走与 `fs` 同款的 bool-or-default 反序列化，等于「按默认值授予」
（`grants.rs:258-282`）。`provider-request` 的 `patch` 是**布尔开关**而不是字段白名单：一旦打开，
可改的键与每个键的夹取由宿主固定表决定（§6.44），插件不能自定义补丁面。

两个容易看错的地方：

- `oauth.providers` 与 `provider.providers` 是**列表**，不是以 id 为键的 map；id 的唯一性是校验项，
  而不是 JSON 对象天然保证的。
- `provider` 的 `models` 是**授权书目**（id / api / base-url / reasoning / input）；`provider-api`
  导出的 `model-spec` 多出 `name`/`context-window`/`max-tokens`，但授权是唯一目录权威，
  导出不能扩权，CLI 运行时目录也不依赖 guest 自报（§6.41）。

**手改文件 fail-closed。** `Grants::load_for_at` 在读完文件后统一跑 `validated()`：JSON 解析失败
按「什么都没授予」处理；某个 key 的策略校验失败时**只清掉该 key**，其余合法授权保留
（`crates/kiqi-extension/src/grants.rs:1263-1275,1600-1638`）。宿主使用策略时还会再夹一次
（例如 render 把越界的 `timeout-ms`/`max-output-bytes` 收敛到上限并打印一句，
`crates/kiqi-extension-host/src/lib.rs:3571-3577`），所以手改文件买不到 CLI 会拒绝的策略。

**接口 → grant key 映射。** 导入名、导出名与授权 key 不是同名关系，`grant_key_for_interface`
是唯一映射（`crates/kiqi-extension/src/grants.rs:1214-1235`）：

| ABI 面 | grant key |
|---|---|
| `provider-http`、`provider-stream`（导入）与 `provider-api`（导出） | `provider` |
| `oauth-http`（导入）与 `oauth-api`（导出） | `oauth` |
| `render-api`（导出） | `render` |
| `resources.json` + `resources/`（包约定，无 WIT 面） | `resources` |
| `provider-request`（`on-event` 的 `before-provider-request` 通道） | `provider-request` |

裸接口名与 `kiqi:extension/<name>@<version>` 两种拼写都接受。`GRANTABLE`/`ENFORCED_CAPABILITIES`
（`crates/kiqi-extension-host/src/lib.rs:222-305`）与 `Grants` 的 grant/revoke/granted 分支必须同时更新；
install 的声明扫描也因此分成导入与导出两半（`lib.rs:252-290`）。

**`--yes` 豁免清单。** 安装时 `--yes` 从不自动授予这五个 key：
`AUTO_GRANT_EXEMPT = [oauth, provider, render, resources, provider-request]`。原因是它们的策略
无法从 import 名推导：oauth 要精确 IdP profile，provider 要书目/端点/凭据句柄，
render/resources 要 kind 与预算，provider-request 要 observe/patch 开关。用户必须显式执行
`ki extension grant <id> <capability> ...`；`--yes` 对 `net-http`/`fs`/`exec`/`proc`/`net-sockets`
的行为不变。install 的 `oauth`/`provider` 发现调用在 deny-all 实例上做，有界
（条目/字节/时间/燃料上限），任意超限即放弃发现并安装零授权。

**`resources` 没有安装期隐式同意。** `resources.json` 和 `resources/` 只是包声明；安装会复制并
校验资源树，但不会写入 `resources` 授权，即使带 `--yes` 也一样。必须另跑
`ki extension grant <id> resources --kind skills|prompts`。重装保留现有策略：既不会扩宽用户收窄的
kinds，也不会复活已 revoke 的授权（`extension_cli.rs` 的 reinstall 回归测试固定这两条）。

**`auth.json` 的变更串行化。** 所有 auth-store 读改写都由进程级 `AUTH_MUTATION_MUTEX` 与稳定的
sidecar 文件锁 `auth.json.lock` 串行化；锁内重新读取当前文件，再原子 rename 发布新快照。前者协调
线程，后者协调不同 CLI 进程，避免并发 login/refresh/logout/revoke/uninstall 相互覆盖
（`crates/kiqi-cli/src/config.rs` 的 `with_auth_mutation_lock`）。

**仍然不对称的同意面（业主决定，0.8.0 不修）。** 本轮给 `before-provider-request` 的观察加了
`provider-request` 授权，但 `probe`/`verdict.replace`、`before-agent-start` 的系统提示替换、
以及 `session-read`/`session-control`/`inject`/`ui` 仍不需要 grant。这是明知的遗留差异，
记录在此以免被误读成「所有敏感面都已授权」。

**O17 残余风险与已落地的授权告警。** host attribution 只保证**没有 token 跨过 WASM ABI**，
不保证插件整体拿不到凭据：同时持有 `exec`/`fs`/`net-sockets` 的插件仍可从别的通道读取或外泄
`auth.json`。O17 的裁决是**授权时警告而非拒绝**；`ki extension grant` 的
`credential_capability_warning` 已实现这条告警：授予 `oauth` 或 `provider` 时若插件已持有上述
任一能力，授权仍保存，但 stderr 明确列出能力、`auth.json` 风险以及 revoke/logout 清理命令。
警告不改变爆炸半径——这里记录的是残余风险，而不是声称「token 永远不会离开宿主」。

### 6.40 宿主归因 OAuth：token 不进沙箱，URL 不由 guest 提供

`oauth` 的基本取舍：**guest 是流程控制器，宿主拥有端点、HTTP、凭据存储与展示**。
读 token 的请求一律由宿主发出，guest 只推进流程步骤。原因不是洁癖：如果让 guest 自己用
`net-http` 访问 IdP，token 就落在沙箱里，而外泄边界会变成插件的**另一份** `net-http` 授权
（可能是 `*`），与 OAuth 授权本身无关。

WIT 面因此没有 `oauth-credential` 类型，只有 `oauth-credential-status`
（`expires-at: s64`，`0` 表示无已知过期时间；`extensions/kiqi-extension.wit:210-215`）。
token 材料没有任何跨边界的类型可走。

**guest 不提供 URL。** `oauth-action.begin(string)` 的字符串是授权码流的 PKCE `code_challenge`
（恰好 43 个 base64url 字符），设备码流传空串（`extensions/kiqi-extension.wit:218-224`）。
`oauth-interaction.url` 由宿主依据 grant profile 构造；guest 返回的 `url` 被显式忽略
（`crates/kiqi-cli/src/wasm_oauth.rs:172-176`）。设备码流的 `verification_uri`/`user_code`
也由宿主从自己发出的 device-authorization 响应里解析并展示，`device_code` 从不进入沙箱。

| 流 | 步骤（`ki auth login`，`wasm_oauth.rs:157-245`） |
|---|---|
| authorization-code | 宿主生成 PKCE verifier/challenge → `action(begin(challenge))` 取 guest 的文案提示 → 宿主用 grant 的 `client-id`/scope/`S256` 与固定 OOB redirect 构造授权 URL → 用户粘贴授权码 → 宿主在 token 端点交换（`crates/kiqi-extension-host/src/oauth.rs:603-632`）→ 写入命名空间 key |
| device-code | 宿主 `action(begin(""))` → 宿主向 grant 的 device-authorization 端点请求（`oauth.rs:461-527`）→ 打印 `verification_uri`/`user_code` → 宿主轮询 token 端点，遵守 `interval`（夹取 1..=60 秒）与 `expires_in`（`oauth.rs:536-590`） |

**`oauth-http` 只有两个函数**：`token-request`（授权码兑换；表单白名单保留 `device_code`
以完整表达标准 OAuth 响应，但设备码轮询在 `ki auth` 里由宿主完成，guest 拿不到 device code）
与 `revoke`（`extensions/kiqi-extension.wit:523-542`）。这是相对早期草案的收窄：
草案里的 `device-authorization` 导入被删掉，因为设备授权与轮询整体移到宿主后，guest 再拿到
`device_code` 只会重开残余秘密面。`token-request` 的 `form` 是字符串 JSON，键只允许
`grant_type`、`code`、`code_verifier`、`redirect_uri`、`scope`、`device_code`；
`client_id` 从 grant 取，`client_secret` 直接拒绝。`enforce_token_form_grant` 还在计费与 HTTP
之前核对 `grant_type` 对应的 flow 是否在 `profile.flows` 中、请求的每个 scope 是否都已授予；
guest 只能收窄 scope，省略时由宿主填 profile 的 scopes，越权请求不会消耗请求预算。返回只有
`oauth-token-outcome::{stored(oauth-credential-status), failed(string)}`，原始 token 响应体从不回传。

**凭据存储与类型化删除。** 插件 OAuth token 写在 `extension:<install-id>:<provider-id>`；
命名空间隔离使它碰不到裸的 `anthropic` 条目。写入走 `upsert_extension_oauth_token`：同一插件重新
登录/刷新可以覆盖原有的 OAuth 项，但同名空间位置若已是 `ApiKey` 则拒绝替换。`ki auth refresh`
与 `ki auth logout` 是纯宿主操作：refresh token 从 `auth.json` 直达 IdP；logout 先 revoke，
成功后才删除命名空间 key，失败则保留本地凭据。`delete_extension_oauth_token` 与卸载/撤销所用的
`delete_extension_credentials_at` 都只删除归属该插件且类型确为 OAuth 的项，**`ApiKey` 永远不会被
这些路径删除**。

**签发 profile 指纹绑定。** 登录与刷新把 `oauth_profile_fingerprint(profile)` 存进凭据的
`profile_fingerprint`：SHA-256 材料包含 provider-id、client-id，以及 authorization/token/
device-authorization/revocation 各端点的 **origin**；路径、scopes 与 flow 偏好有意不参与。
refresh 在发 HTTP 前调用 `ensure_oauth_profile_matches`，logout 在 revoke 前检查，
`revoke_stored_token` 自身也再检查一次；指纹缺失或不匹配都拒绝，防止旧 token 被发往新 IdP。
profile 已变化时 logout 默认保留本地凭据并提示确认命令；只有显式重跑
`ki auth logout --provider <id> --force-local` 才跳过远端 revoke、只删除本地 OAuth 项，该路径
不发出任何 IdP 请求。

**残余秘密与诚实的边界。** 在 `ki auth` 的实现里 verifier 与 `device_code` 都在宿主，
guest 只收到 challenge；如果某个 guest 选择自己调 `token-request` 完成 `exchange`，授权码会
经过 guest，但 token 仍不回传。内建 provider 的自动凭据解析仍有边界：`stored_api_key` 只匹配
`ApiKey`，不会把 OAuth entry 自动当成内建模型调用的 API key。另一方面，wasm provider 的
`provider-http` 已能在用户显式 `--credential-from auth-json:<pid>` 授权后读取 OAuth access token
（与 ApiKey 一样做使用期指纹校验）；因此不能再笼统地说「OAuth token 没有模型请求消费者」。

### 6.41 provider：grant 是唯一书目，专用实例，受控 HTTP 与带索引的流

**书目权威与 CLI 接线。** `provider` grant 的 `models` 是运行时目录的唯一权威：
`granted_extension_models` 直接从有效 grant 生成 catalog，`WasmProvider` 在 resolve 与 `/model`
重载后作为真正的 `kiqi_ai::Provider` 注册。`provider-api.list-providers` 只是建议性元数据，用于
安装期的 deny-all 有界发现，以及 `ProviderHost::list_specs` 的交集收窄；guest 多报的 model 不会
进入 CLI catalog。`provider-api.request` 的 model id 必须在 grant 里，宿主在终态 assistant message
上用 grant 覆盖 `provider`/`model`/`api`，guest 不能冒充别的模型。`provider-api` 与
`provider-stream`/`provider-http` 都映射到同一个 `provider` key（§6.39），不分别授权。

**专用实例。** provider 调用跑在独立的 `ProviderHost` 上：同一 Component、独立的
`Engine`/`Store`，deny-all grants 上只安装当前 provider profile 的调用状态，没有工具、UI、session、
`fs`/`exec`/`net-http`/`oauth` 能力。组件 start 的实例化也走 `instantiate_bounded` 的 fuel、epoch
与外墙时钟。一次长生成不会占着工具调用、`on_event` 与快捷键查询共用的实例锁。请求期间
`CallContext.kind == ProviderRequest`，取消经 `CancelProbe` 同时到达 guest 与 HTTP read。

**`provider-http` 是请求过滤器，不是通用 HTTP 客户端。** 接口是
`open(request-json) -> handle`、`read(handle, max-bytes) -> option<list<u8>>`（`none` = EOF）与
`close(handle)`。三个导入在非 `ProviderRequest` 调用栈上都直接 `denied`；`open` 必须同时匹配
grant 的 `https`、精确 host、path-prefix 与 methods，任何一项不匹配都不建立连接。实现不做
「先归一化再匹配」：它在 URL parser 消 dot segment 之前检查原始路径，拒绝 `%2F`/`%5C`/`%2E`
（大小写不敏感）、`.`/`..` segment 与重复 `/`，随后再查前缀；带凭据的 grant 也不得把
path-prefix 设为整台主机的 `/`。重定向不跟随，3xx 在 `open` 处拒绝。

发出前剥离 guest 提供的 `Host`、`Content-Length`、`Transfer-Encoding`、`Connection`、
`Authorization`、`x-api-key`、`x-goog-api-key`、`proxy-authorization`、`cookie`、`api-key`、
`x-auth-token`、`cf-aig-authorization`，凭据头只由宿主注入。请求体上限 16 MiB；单块 read 最大
1 MiB，响应累计受 grant 的 `max-response-bytes` 约束。句柄表按调用编号隔离，受
`max-handles` 限制；每次调用开始清理上一调用的遗留项，退出与 unload/Drop 再回收，所以旧句柄
不能跨调用复用。

**宿主持凭据，使用期指纹检查已经生效。** grant 里只有不透明句柄（`pc-…`），值不在
`grants.json`。`ki extension grant <id> provider --credential-from auth-json:<pid>` 把
`{source, provider, display, fingerprint}` 写入 `extensions/<id>/credentials.json`（0600，在 fs jail
之外），提示与 `ki extension show` 只显示来源和指纹前缀。每次 provider 请求在 guest 运行前重新读取
sidecar 与 `auth.json`，解析 ApiKey 或 OAuth access token，并重算 SHA-256 与授权时指纹比较；
来源丢失、条目不可用或值已改变都拒绝，不会拿旧句柄继续发请求。

**`provider-stream` 有 call-kind 门与有界聚合。** `emit`/`emit-batch` 只在
`CallContext.kind == ProviderRequest` 时接受 delta，否则是 no-op 加一条诊断。delta 为
`provider-delta {kind: text|thinking|tool-call, phase: start|delta|end, content-index, text}`；
`content-index` 区分并发 tool call 及 thinking/text 块。guest 必须显式发 start，宿主校验
start→delta→end 顺序、重复/越界事件、index 与 kind 的绑定及未闭合块；tool-call 聚合文本必须是
含字符串 `{id, name, arguments}` 的 JSON。单次调用最多 **16,384 个 delta**（含 start/end）、
**1,024 个独立块**、**16 MiB 累计 UTF-8 文本**；超预算即失败并禁用该实例。

**`WasmProvider` 惰性构建且每次请求重验授权。** resolve 只保存 wasm 路径、agent root 与 grant
书目，不读组件、不编译 Engine；第一次 `stream_simple` 才在 blocking thread 建立 `ProviderHost`，
每个 resolve 有自己的 slot。缓存命中前也会重新加载并验证 `grants.json`：只有当前 profile 与缓存
完全一致才复用；revoke 后下一次请求在发送前拒绝，收窄或替换 profile 则丢弃旧实例并按当前授权
重建，trap/deadline 禁用的实例同样不会复用。

**CLI 流也有背压与终止边界。** guest worker 用 `try_send` 向容量 **256** 的 tokio delta 队列
投递；队列满或关闭会取消并失败，而不是无界缓冲或让 guest 等消费端。CLI 的 `tokio::select!`
同时等待宿主结果与 delta 到达（优先宿主结果），不以 guest sender 关闭作为完成条件。ProviderHost
的墙钟等待覆盖 epoch 无法打断的 WASI/host-import 阻塞；超时后即使遗留 worker 仍持有 sender，
CLI 也立即终止该流并发出错误，不等到 guest 自己返回。

### 6.42 render：deny-all 实例、仅定稿转换、输出上限与超时语义

**只保留一个函数。** `render-api.transform-markdown(markdown) -> result<string, extension-error>`：
纯文本到文本，没有主题、没有消息/entry renderer 导出（`extensions/kiqi-extension.wit:579-584`）。
宿主负责主题、清洗、调度、缓存，以及运行它的实例。

**deny-all 实例。** `RenderHost` 不是 `WasmExtension`：它有自己的 `Engine`（打开 epoch 中断）、
自己的 `Store`（`Grants::default()`），从不调用 `install_context`，调用跑在刚 spawn 的
`std::thread` 上，`CURRENT` 线程局部为空，并有 debug 断言钉住
（`crates/kiqi-extension-host/src/lib.rs` 的 `RenderHost`）。因此 `session-read`/`ui`/`inject`/
`exec` 等即使没有 grant 门，也在结构上不可达——它们没有 `HostCaps`。epoch 是 Engine 级开关，
deadline 是 Store 级；所有 Engine 都经 `wasmtime_config` 构造，保证 macOS Mach-port 设置进程一致。
进程级 ticker 每 `RENDER_TICK_MS = 10` ms 推进所有活跃 render Engine 的 epoch。组件 start
也不是无限执行：`instantiate_bounded` 把 `World::instantiate` 放到独立 worker，套用 discovery
fuel、epoch deadline 与外层墙钟；成功前不会发布 `RenderHost`。

**仅定稿转换。** `AssistantMessageComponent::transform_markdown` 在 `is_streaming` 为真时直接
返回原文，**根本不咨询 transformer**；`finalize(blocks)` 先把 streaming 置 false，再重建一次；
`set_streaming(false)` 在 true→false 的边沿也只重建一次
（`crates/kiqi-tui/src/assistant_message.rs:78-152,391-415`）。CLI 侧还有一层
`(extension id, 文本 hash)` 缓存（上限 128 条），同一条定稿文本的重复重建不会二次调用 guest
（`crates/kiqi-cli/src/extensions.rs:1586-1695`）。定稿调用点统一走 `finalize`
（`crates/kiqi-cli/src/interactive_tui/run.rs:135`、`crates/kiqi-cli/src/transcript_view.rs:201,288`）。
这直接回答了流式路径的 O(n²) 成本：**delta 期间 0 次 guest 调用，每条定稿消息恰好 1 次**。

**输出上限与清洗。** grant 的 `max-output-bytes` 默认 1 MiB、上限 8 MiB
（`crates/kiqi-extension/src/grants.rs:808-811`）。宿主先剥掉 CSI/OSC/两字节 ANSI 转义，
再把仍存在的 C0/C1（换行/制表除外）当作硬错误，最后在 UTF-8 字符边界截断
（`crates/kiqi-extension-host/src/lib.rs` 的 `sanitize_render_output`）。
任何失败（guest Err、trap、超时、控制字符）都保留原文并记一条诊断；trap 或墙钟超时会禁用
本会话的 renderer（`RenderHost::transform_markdown` 与 CLI 的 `WasmRenderTransformer`）。

**超时语义与实现值。** 一个 transform 的 deadline 默认 **2000 ms**、上限 **10 s**。
调用方用 `recv_timeout(deadline)` 等独立 worker；worker 的 `JoinHandle` 被主动 drop（detach），
超时路径**不 join、不等 guest 返回**，而是禁用该 renderer、保留原文并记诊断。因此 guest 即使
停在 WASI host import 里也不能永久卡住 TUI。epoch deadline 仍保留：毫秒数向上取整成至少一个
10 ms tick，负责打断纯 CPU 自旋；墙钟是 host-import 阻塞的外层兜底。这个取值与早期 spec 草案的
**50..2000 ms** 不同，理由记录如下：

- 早期范围是按「每个 delta 可能调用一次」估的，50 ms 是给流式路径的延迟预算；
  定稿制让每消息只调用一次，per-delta 预算不再有意义。
- 真实 markdown renderer（语法高亮、树解析）在长消息上远超 50 ms；2 s 默认值把超时定位为
  **存活兜底**而不是性能预算。上限 10 s 给大消息留余量，同时仍能保证 TUI 不会永久卡住。
- 早期草案里的 `max-input-bytes` 与 `debounce-ms` 没有落地：输入大小由会话消息本身约束，
  定稿一次调用也使去抖失去对象。0.8.0 的 `render` grant 只有
  `kinds`/`timeout-ms`/`max-output-bytes` 三个字段。

### 6.43 resources：`resources.json` 包约定、安装期摘要与 first-wins 顺序

`resources` 没有 WIT 面，也没有运行时导出。它是**包约定**：guest crate 在
`extension.wasm` 旁带 `resources.json`、`resources/<kind>/…`，`ki insmod` 复制它们并写
`resources.sha256`（`crates/kiqi-cli/src/extensions.rs:1705-1740,1930-1990`）。
`resources.json` 的形状就是 `ResourcesGrant`（kinds / max-items / max-bytes）：
manifest 是**包声明**，grant 是**用户授权**，实际加载取两者交集
（`extensions.rs:1811-1840,2100-2145`）。

安装期校验：

- manifest 必须是普通文件（symlink 拒绝），未知 kind 或越界 cap 是**安装错误**，不是静默收窄；
- `resources/` 树逐项拒绝 symlink、绝对路径与 `..`；
- `resources.sha256` 记录每个文件的摘要，**manifest 自己也在内**——安装后手改 kinds/caps 会像
  手改 skill 一样失败；
- 文件数/总字节同时受 manifest 与 grant 的 cap 约束，取两者较小值；`resources.sha256` 列出
  包外文件即安装错误。

解析与顺序（`crates/kiqi-cli/src/session.rs:619-700`、`crates/kiqi-cli/src/extensions.rs:2165-2200`）：

- 只处理 `extensions/<id>/extension.wasm` 存在、且持有 `resources` 授权的包；没有 grants 的目录跳过；
- grant 的 `kinds` 决定哪一类被交给 loader（只授 `skills` 时不会加载 prompts）；
- 插件目录在启动与 `/reload` 两处都**追加在最后**。`load_skills_with_precedence` 与
  `load_prompt_templates_with_precedence` 按名字 **first-wins** 去重
  （`crates/kiqi-cli/src/resource_dirs.rs:247-284`），所以内建/项目资源永远赢过插件资源，
  未来重排也不能让插件遮蔽内建；冲突仍会产出 `ResourceDiagnostic`。
- 主题（themes）被丢弃：它们被收集但没有消费者，`ThemeManager` 是 preset 驱动的；
- `/reload` 会重读 grants 与包 manifest，但**不会重建 wasm host**
  （`compiled_extension_hosts` 是 `OnceLock`），新安装插件的资源目录会被重读，工具仍需重启；
- 绝不扫描 `extensions/<id>/data/`——那是 guest 可写的 fs jail，资源必须来自宿主校验过的包树。

### 6.44 类型化命令面、`event.input` 与 `event-action.patch`

**`command-result` 与 `continue-command`。** `run-command` 不再返回裸字符串，而是
`command-result {flow-id: string, body: command-result-body}`；body 是
`message(string) | selector(selector-spec) | editor(editor-spec) | prompt(prompt-spec)`。
`continue-command(name, flow-id, reply)` 接收
`command-reply {selected(string), submitted(string), cancelled}`
（`extensions/kiqi-extension.wit:322-343,601-607`）。`flow-id` 是防串线的关键：命令调用已被移到
独立线程（TUI 键循环不得阻塞在 guest 上），没有 flow id 时，一个迟到的 selector 回复会打到
下一次命令上。

TUI 侧是一个显式状态机（`crates/kiqi-cli/src/interactive_tui/extension_commands.rs:71-230`，
入口 `commands.rs:884-900`）：

- `execute` 只 spawn，立即返回；pending 槽记录 `{host, name, flow-id, kind}`；
- selector/editor/prompt 返回时槽位生效；回复携带 flow-id，宿主在**调用 guest 之前**校验
  host/name/flow-id/kind，未知或过期一律拒绝；
- 同一命令再次执行会替换旧槽（旧回复随即变成 stale）；
- Esc 发送 `cancelled` 并清槽；message 类结果经现有 TUI 消息通道
  `add_note_message` + `request_render` 显示。

命令返回的每个字符串在渲染或写入编辑器缓冲区前都过宿主清洗：ANSI 剥离、C0/C1（换行/制表除外）
拒绝、selector 最多 64 项、单项/标题/占位符 1 KiB、selector 总量 64 KiB、editor 预填 256 KiB、
message 256 KiB；selector/editor/prompt 的 `flow-id` 为空直接判错
（`crates/kiqi-extension-host/src/lib.rs:3177-3265`）。

**`cli-flag` 带值与声明域。** `session-read.cli-flag(name) -> option<flag-value>`，
`flag-value {bare, text(string)}`（`extensions/kiqi-extension.wit:345-350,365`）。这修掉旧实现把
JSON Bool 丢掉的问题：裸 `--foo` 现在是 `bare`，带值标志是 `text`。读取是**声明域**的：
`HostState` 在实例化后缓存一次 `list-flags`（`extension_id` + `declared_flags`），
`cli-flag` 先检查名字是否在调用插件自己的声明里，不在就返回 `none`
（`crates/kiqi-extension-host/src/lib.rs:682-690,781-795,2399-2420`）。因此一个插件读不到邻居
插件的 flag；这条收紧不需要 grant，也是 0.8.0 的默认（不需要额外选择）。

**`event.input`。** `event` 新增 `input(input-event)`；`input-event {key, kind, ctrl, alt,
shift}`，`kind ∈ {press, repeat, release}`（`extensions/kiqi-extension.wit:154-157,290-303`）。
宿主把 `EventTag::Input`（u32 32，`crates/kiqi-extension/src/lib.rs:466`）映射成这个类型；
缺失字段退化为中性值而不是丢事件（`crates/kiqi-extension-host/src/lib.rs:3356-3384,3412`）。
旧 guest 的 `on_event` 若用 `_`/`if let`/`matches!` 分支则不受影响。

**`event-action.patch`：唯一的改写通道，且只在 provider gate 生效。**
`event-action` 新增 `patch(string)`（`extensions/kiqi-extension.wit:168-177`）。它**只**对
`before-provider-request` 且发生在 provider gate 时生效；其他任何 tag 返回的 `Patch` 都被忽略
（`crates/kiqi-extension-host/src/lib.rs:2513-2521`；gate 见 `:2533-2580`）。这也是对 §6.23 的修订：
那一节写的是「全局 sink 没有返回值，编译期/wasm 插件永远无法 claim，也没有改写通道」——
0.8.0 起 `patch` 是**事件通道上**唯一的改写概念，但 observe/claiming 漏斗仍不消费它，
`Patch` 不会把事件变成 `Claimed`。

补丁是**整包校验**的：任一键/值违规就拒绝整个补丁，请求按未打补丁继续，并记一条诊断。
固定表（`crates/kiqi-extension-host/src/provider_patch.rs:30-70,77-190`）：

| 项 | 限制 |
|---|---|
| 信封 | 只能是 JSON 对象，序列化后 ≤ 32 KiB；顶层只接受下列键，未知键与不可补丁键都整包拒绝 |
| `headers` | ≤ 32 项；名字非空且 ≤ 128 B，小写后命中拒绝表即整包拒绝（插入与删除都命中）；值只接受字符串或 `null`（`null` = 删除），字符串 ≤ 2048 B；序列化后 ≤ 16 KiB |
| `metadata` | ≤ 64 项；键非空且 ≤ 128 B；值只接受字符串或 `null`；字符串 ≤ 2048 B；序列化后 ≤ 16 KiB |
| `timeoutMs` | 整数 1000..=900000，且不超过 provider grant 的最紧预算 |
| `maxTokens` | 整数 1..=262144 |
| `temperature` | 有限数，0.0..=2.0 |
| 不可补丁 | `sessionId`、`cacheRetention`、`maxRetries`、`maxRetryDelay` |

拒绝表（大小写不敏感，插入/删除都算）：`authorization`、`x-api-key`、`x-goog-api-key`、
`api-key`、`x-auth-token`、`cf-aig-authorization`、`cookie`、`proxy-authorization`、`host`、
`content-length`、`transfer-encoding`、`content-type`（`provider_patch.rs:49-63`）。
`null` 表示删除 map 项，与 `SimpleStreamOptionsPatch` 的 inner-`None` 语义一致；
插件不能靠「先删后加」绕过凭据头保护。

**provider-request 的投递面。** `provider-request` 授权决定 `observe` 是否投递脱敏事件
（默认 `none`）以及 `patch: true` 是否接受补丁。观察载荷保留请求元数据与 `headerNames`，
`apiKey` 换成 `apiKeyPresent`；`messages` 只有 `messages: full` 才保留，否则删除并补
`messageCount`。即使授予 `headers: values`，header **值**也只放行明确的非凭据白名单：
`traceparent`、`tracestate`、`x-request-id`、`x-correlation-id`、`x-trace`（大小写不敏感）；
未知或自定义名字一律留在宿主，名字本身仍可见。这不是 deny-list，因为任何厂商自定义头都可能
是凭据。

**Observe 分区只作用于 `BeforeProviderRequest`。** 在该 tag 上，patch-capable host 只走 provider
gate，observe-only host 只走观察漏斗，无授权则不调用，每个请求对每个 host 至多一次；其它
Session/Message/Tool/Input 等 tag 仍按原规则扇出，patch 能力不会让插件丢掉生命周期事件。

**gate 每次使用专用实例。** `WasmExtension::provider_gate` 从已编译的 Engine/Component 新建独立
Store，绝不借用共享工具/事件实例的 `inner`。调用方有 `patch-timeout-ms` 墙钟，实例内 epoch
继续兜底纯 CPU 自旋；返回或超时后该专用实例不再复用。guest 即使停在阻塞 WASI host import，
被放弃的也是专用 worker 与 Store，不会持有共享互斥锁，因此后续 probe、工具调用和下一次 gate
都能继续。

### 6.45 cdylib 通道退役（2026-10-26）

wasm 成为唯一插件通道。删除面（用户已批准清单 ①②③）：

- `crates/kiqi-extension-sdk`（C ABI 类型）、`examples/extension-stub`、
  `crates/kiqi-extensions/tests/{extension_stub_smoke,voice_ptt_smoke}.rs`；
- `ki dev` / `ki dev-local` / `__kiqi_dev_cleanup`（`dev_extension.rs`），
  `ki install` / `ki uninstall`（`install.rs`），`ki package link|unlink`
  （`packages.rs`），以及 `updates` 里的 native 包检查/缓存维度；
- `kiqi-extensions` 的 `libloading` 装载机（`load_one`/`load_dir`/`load_session*`/
  `merge_registries`/`LoadedExtension`/`ExtensionKeepalive`/`ExtensionLoadError`）。

留下的脊柱：`ExtensionSession`（无装载机、恒为空 registry 的会话持有者）、
`registry` 注册面、`translate` 派发漏斗、`compiled` sink（wasm fan-out）、
`actions`/mailboxes/`ToolCallContext`、`stdio`（runtime action 21）。
C ABI 线格式（`StbString`/`StableExtensionEvent`/`EventView`/handler consts）从 sdk
搬进 `kiqi-extensions::abi`，共享常量（`HOST_*`/`*_ENV`/`EVENT_TAG_COUNT`）与
`EventTag`、`dialogs` 归一到 `kiqi-extension`。

未在本次完成、留作后续（现状为不可达的惰性代码）：registry 注册层的 C-ABI
回调机器（`HostApi`/vtable/线程局部 `current_api`）仍随脊柱编译，但已无任何
装载路径能喂给它；把它折叠为纯 serde 事件总线是独立的后续批次（已于 §6.49 完成）。

### 6.46 `kiqi-host`：三 crate 合并 + `Extension → Extension`（2026-10-26）

按 `docs/design/kiqi-host-merge.md` 执行块 2：

- `crates/kiqi-extension`（契约）+ `crates/kiqi-extension-host`（wasmtime 后端，移入
  `src/wasm/`）+ `crates/kiqi-extensions`（派发脊柱，平铺进 crate 根）
  → 合并为 **`crates/kiqi-host`**。
- trait 由 `Extension` 改名为 **`Extension`**（两轮 Jev 评分定名：`Extension` 0.57 /
  `HostExtension` 0.05；理由 = Rust 里 `XxxExt` 约定俗成是给已有类型加方法的扩展
  trait，与「插件实现方契约」语义相反）。`impl Extension for WasmExtension` /
  `dyn Extension`。
- wasmtime 系列依赖挂到 `wasm` feature；契约与派发脊柱恒编译，`kiqi-tools` /
  `kiqi-mcp` / `kiqi-acp` 无条件依赖它，只有 `kiqi-cli`（`extension-wasm` feature）
  打开 `wasm`。
- crate 级消费路径：`kiqi_host::*`（契约/脊柱），`kiqi_host::wasm::*`（后端，
  仅 `wasm` feature）。wasm e2e（`tests/e2e.rs`，5,140 行）整体迁移并以
  `#![cfg(feature = "wasm")]` 门控。

### 6.47 `kiqi-plan-mode-core` 折叠回 guest（2026-10-26）

该 crate 的唯一存在理由是「guest 不能写单测」——实测**不成立**：wit-bindgen 为宿主
目标生成桩，`extensions/wasm` 工作区里 `cargo test -p kiqi-plan-mode` 能在宿主上
跑纯逻辑单测（6 个测试原样通过）。于是：

- `crates/kiqi-plan-mode-core/src/lib.rs` → `extensions/wasm/kiqi-plan-mode/src/plan.rs`
  （`PlanHost` trait + 逻辑 + 单测），guest `lib.rs` 只留 WIT 适配；
- 删 crate、workspace member 与依赖边；`ki` 二进制本来就不链接它；
- **规则修正**：guest 的纯逻辑可以带原生单测；跨 WIT 边界的行为仍由
  `crates/kiqi-host/tests/e2e.rs` 从宿主侧验证（CLAUDE.md / AGENTS.md 同步）。
- CI 增一步 `cargo test --manifest-path extensions/wasm/Cargo.toml -p kiqi-plan-mode`。

### 6.48 词汇统一为 extension（WIT 0.9.0，2026-10-26）

用户裁定：代码与文档的词汇统一为 **extension**，旧词从全仓删除。这是整体重命名，
包版本升到 **0.9.0**（WIT 包名变更是 ABI 变更）：

- **WIT**：契约文件更名为 `extensions/kiqi-extension.wit`；包 `kiqi:extension@0.9.0`；
  `world extension`；`extension-api` 接口。
- **仓库目录**：宿主侧目录统一为 `extensions/`；guest 树 `extensions/wasm/kiqi-<x>/`
  （目录与 Cargo 包名都不再带 `-guest` 后缀，仅组件模型语境保留 host/guest 说法）；
  共享 bindings crate 为 `kiqi-wasm-bindings`。
- **Rust 面**：`StableExtensionEvent`、`ExtensionApi`、`ExtensionError`、
  `WasmExtension`、`extension_cli`、`extensions.rs`、
  `KI_EXTENSION_ABI_VERSION_UNIFIED` 等；trait 为 `Extension`。
- **运行时**：CLI 子命令 `ki extension …`；安装目录 `$KI_AGENT_DIR/extensions/<id>/`；
  组件文件 `extension.wasm`。**无向后兼容**——已装 guest 需重新 `ki insmod`。
- 验证：全仓文本（含文档）中旧词出现次数为 **0**；workspace 2626 + guest 单测、
  wasm 目标构建、宿主侧 e2e（82 项）、fmt/check 与全部脚本门禁通过。

### 6.49 纯 serde 事件总线：删除 C-ABI 注册机（2026-10-27）

§6.45 留下的「不可达的惰性代码」本批删除。先按证据确认不可达：`HostApi`、
`set_current_api`/`clear_current_api`、vtable 构造与全部 `trampoline_register_*`、
`ExtensionToolAdapter`/`ExtensionToolHandle`/`ExtensionToolError`，以及
`registry.rs` 里依赖函数指针的注册类型，除测试外没有任何生产调用者——唯一的
装载路径（`load_one`）已在 §6.45 删除，wasm 后端走的是 `Extension` trait 与 WIT
导入，不经过这些类型。

删除面：

- `kiqi-host/src/abi.rs` 整个模块：`StbString`/`StbStringRef`/`StableToolSchema`/
  `StepResult*`/`StableExtensionEvent`/`EventView`/`EventPayload`/`ExtensionApi`/
  `KI_EXTENSION_ABI_VERSION_UNIFIED`/`REGISTER_SYMBOL_UNIFIED`/`register_entrypoint`/
  `guard*`/`EVENT_HANDLER_*` 以及所有 handler 函数指针别名。仓库内已无
  `kiqi_host::abi` 引用；`EventTag` 仍在 crate 根（契约），无需迁移。
- `lib.rs` 的 `HostApi`、线程局部 `CURRENT_HOST_API`、全部注册 trampoline、
  `host_free_string`、`stub_runtime_action`；`ExtensionToolError`。
- `tool.rs` 的 `ExtensionToolHandle`/`ExtensionToolAdapter`（`ToolCallContext` 保留，
  wasm 侧仍通过 `SessionRead` 读取会话身份）。
- `registry.rs` 的 `ExtensionRegistry`、`ExtensionTool`、`Registered*` 记录、
  `RegistryEntry`、优先级/平台声明；只保留 `RegistrySnapshot`（共享的 active 标志，
  `/reload` 用它让旧会话停止派发）与 `assert_active`。
- `actions.rs` 的 `RuntimeActionId`/`dispatch`/`trampoline_runtime_action`/
  `run_action`（`ActionBridge`、`RuntimeActionHost`、mailboxes、`ReloadMailbox`
  保留：wasm 的 `SessionRead`/`SessionControl`/`InjectApi`/`UiApi` 与 CLI 的
  stdio/reload 仍在使用）。
- `prompt_transform.rs`、`provider.rs`（`PluggableProvider`）、`oauth.rs`
  （native OAuth 注册）：wasm 的 prompt/provider/OAuth 走 §6.41/§6.44 的独立实现，
  这几条 native 分支已无生产者。`resources.rs` 只保留 `DiscoveredResources` 与
  空实现的 `emit_resources_discover`（资源来自包树 `resources.json`，见 §6.43）。
- `kiqi-cli` 的 native 调用点同步移除：markdown/message/entry 渲染器、扩展命令、
  tool-call 审批、OAuth 登录/刷新/撤销、provider 列表；`kiqi-acp` 无代码改动。

事件总线改为纯 serde：

- `compiled::CompiledEventSink` 的签名从 `(DispatchMode, u32, serde_json::Value)`
  改为 `(DispatchMode, &Event)`；sink 不再反查 tag、不再解析字符串载荷。
- `translate` 的漏斗直接构造 `Event { tag, payload }`：`MessageStart`/`MessageEnd`
  用 `AgentMessage` 的 serde 值，`MessageUpdate` 用既有的 `assistantMessageEvent`
  信封，tool 事件用 camelCase 对象（`toolCallId`/`toolName`/`params`/`result`/
  `isError`），无载荷 tag 为 `Value::Null`。`dispatch_data_event` 对非法 JSON 仍退化为
  `null`，不会丢事件。
- 漏斗签名去掉不再需要的 snapshot 参数；`dispatch_lifecycle_event` 恒返回
  `None`（`EventAction` 没有 abort），`dispatch_tool_call_event` 删除（wasm 的
  tool gate 走 `probe_tool_call`）。`ExtensionProviderHooks` 只保留 provider 事件
  观察与 gate；prompt transform 由 `compiled_prompt_hooks` 链式处理。
- 公开行为不变：事件仍扇出到 wasm sink；`SessionStart`/`SessionTree`/
  `SessionShutdown` 等 lifecycle 仍经 `dispatch_*` 送达；provider gate 的
  `Patch` 语义与 observe 分区未变。

验证：`cargo fmt --all -- --check`、`cargo check --workspace --all-targets`
（±`--all-features`）、`cargo test --workspace`、`cargo test -p kiqi-host
--features wasm`（含构建 guest 的 e2e）。

### 6.50 纯 serde 事件总线 + 会话级 sink（2026-10-27）

- **C-ABI 注册机删除**：`HostApi`/线程局部 `current_api`/trampoline 注册函数/
  `ExtensionToolAdapter`/`PluggableProvider` 与本地的 OAuth/prompt-transform/
  resources handler，以及整个 `kiqi_host::abi` 模块（`StbString`/
  `StableExtensionEvent`/`EventView`/`ExtensionApi`/handler 常量）。事件经漏斗直接
  构造 serde `Event`（tag + JSON payload）。
- **sink 会话化**：`RegistrySnapshot` 持有 `Option<CompiledEventSink>`
  （`with_compiled_sink` / `compiled_sink`），`compiled` 增加
  `dispatch_*_with(sink, …)`，漏斗优先用 `snapshot.compiled_sink()`；进程级槽位保留
  作为无快照路径的 fallback。cli 的 build 在创建 emitter 之前挂载 sink，reload 先建
  caps/sink 再换 cell。
- **边界如实记录**：wasm **host 实例仍是进程级共享**——会话级隔离的是"用哪份 caps
  派发"，不是"哪个 guest 收到事件"。`COMPILED_CAPS`（工具/命令/probe/prompt
  transform）仍进程级。
- **`EditorChange`** 现在随每次草稿变化送达 sink（此前 wasm-only 安装被原生 handler
  预检查短路、从未送达）；有意的行为变化，代价是每次变化一次 guest 观察调用。
  有宿主侧 e2e 钉住（`crates/kiqi-host/tests/e2e.rs` 的
  `editor_change_reaches_the_wasm_guest_through_the_observation_funnel`）。
- **staleness 标志**：`RegistrySnapshot::invalidate` 保留但不再参与派发门控
  （`assert_active` 无调用者）；文档不再声称 `/reload` 靠它停止旧会话派发。
- 遗留**已修复**（见 §6.52）：sink 经 caps 反向持有 action host，而 host 曾强持有
  harness，构成 `harness → … → snapshot → sink → caps → host → harness` 的 Arc 环；
  单会话无影响，ACP 多会话下每次开关会话都泄漏整个 `AgentHarness`。§6.52 把 host 的
  harness 引用改为 `Weak`，环已断开。

### 6.51 会话级 caps：probe / 扩展命令 / prompt transform（2026-10-27）

§6.50 把事件 sink 挂到了 `RegistrySnapshot`，但**非派发**的三条路径仍读进程级
`COMPILED_CAPS`：`before-tool-call` probe、扩展命令（TUI 的 `/…`）、以及 prompt
transform。ACP 多会话下它是 last-writer-wins——会话 A 的 guest 可以 `session_control`
取消/改名会话 B、`inject` 写入 B 的对话、`set_status`/`set_editor_text` 改写 B 的 UI。
本节把这三条路径也改为**会话作用域**。

**承载方式（类型安全，`kiqi-host` 不认识 CLI 的类型）**

- `kiqi-host` 新增 `trait SessionCaps: Send + Sync { fn host_caps(&self) -> HostCaps<'_>; }`。
  它只产出契约里已有的 `HostCaps`，不暴露任何 CLI 类型。
- `RegistrySnapshot` 增加 `session_caps: Option<Arc<dyn SessionCaps>>`
  （`with_session_caps` / `session_caps`），与 `compiled_sink` 并列；`/reload` 换
  snapshot 时 caps 与 sink 一起原子换掉。
- `ExtensionSession::none_with_compiled_sink_and_caps(sink, caps)` 是生产构造器；
  旧的 `none_with_compiled_sink` / `none` 委托它并传 `None`（保持测试与无扩展路径）。
- CLI 的 `OwnedCaps` 实现 `kiqi_host::SessionCaps`（`host_caps()` 即原来的
  `caps()`）。取舍：trait 放在 `kiqi-host` 而不是把 `OwnedCaps` 搬进去，是为了让
  契约层继续零 CLI 依赖；代价是每次调用多一次 `dyn` 间接（本来 probe/命令/transform
  就是 `dyn Extension` 调用，可忽略）。

**消费点**

- **probe**：`extension_tool_call_hook` 捕获会话 cell，调用时
  `session_caps_from_cell` 现取快照上的 caps。之所以不把 caps 捕获进 hook：harness
  的 `before_tool_call` 没有 setter，`/reload` 换不了它；读 cell 才能跟随 reload。
- **扩展命令**：TUI worker 从 `ReloadContext.extension_session` 读当前会话的 caps
  （同为 call-time 解析）。
- **prompt transform**：`compiled_prompt_hooks(inner, caps)` 在 build/reload 时绑定
  该 harness 自己的 caps；reload 本来就会 `set_provider_hooks` 换新实例。
- **进程级槽位**：`COMPILED_CAPS` 降级为无会话路径的 fallback
  （`ki` 子命令、测试、`ExtensionSession::none()`）。生产 build 一定把 caps 挂上
  snapshot，所以两个 ACP 会话不会互相取用。

**失败路径**：`session_caps_from_cell` 对 poisoned mutex 用 `into_inner` 恢复——锁只
保护一次 `mem::replace`，内部值始终是完整会话，这是 fail-closed 的选择（绝不回落到
“最近构建的另一个会话”）。无会话的调用者才回落到进程级槽位，与旧行为一致。单会话
行为不变。

**测试**：`crates/kiqi-cli/src/extensions.rs` 的
`two_sessions_use_their_own_caps_for_probe_command_and_prompt` 在同一进程建两个
`ExtensionSessionCell`（session-a / session-b，各带 `session_id` 不同的 `SessionRead`），
用一个记录型 `Extension` 分别触发 before-tool-call probe、`run_command` 与
`before_agent_start`，断言三条路径各自拿到自己的 session id。`kiqi-host` 侧另测
snapshot 的 caps 随之 clone。

**边界不变**：wasm host 实例仍进程级共享（§6.50 的诚实边界）；本节只保证“用哪份
caps 调用”，不保证“哪个 guest 收到事件”。第三方插件在调用时读进程 cwd 的残余限制
仍需单独的插件路由改造。

### 6.52 断开 sink→caps→host→harness 的 Arc 环（2026-10-27）

§6.50 记录了遗留：harness 经 `before_tool_call` 钩子持有会话 cell，cell 里的
`RegistrySnapshot` 带 sink，sink 持有 `OwnedCaps`，caps 的 `ActionCaps` 持有
`Arc<dyn RuntimeActionHost>`，而 `HarnessActionHost` 又强持有 `Arc<AgentHarness>`
——闭合为 `harness → … → snapshot → sink → caps → host → harness`。单会话只泄漏到
进程退出；ACP 多会话下每开/关一个会话就留下整个 harness（连同其 session/JSONL 句柄）。

**修复**

- `HarnessActionHost.harness` 由 `Arc<OnceLock<Arc<AgentHarness>>>` 改为
  `Arc<OnceLock<Weak<AgentHarness>>>`；`harness()` 用 `Weak::upgrade()`，失败时仍是
  原来的 `"runtime action invoked before harness was built"` 错误（绝不回退到另一个
  会话的 harness）。
- **强引用归会话所有者**：`session::build`/`build_with_selection` 在
  `AgentHarness::create` 成功后立即 `Arc::new`，用**这个** Arc 调 `set_harness`（存
  `Arc::downgrade`），并把 `Arc<AgentHarness>` 作为返回值交给调用方。若只把 cell 改成
  `Weak` 却仍传 `Arc::new(h.clone())`，那个 Arc 没有别的持有者，会在 `build` 返回时即被
  释放，令 Weak 永远 upgrade 失败——这是必须同时把 `build` 的返回类型改成
  `Arc<AgentHarness>` 的唯一原因。
- **生命周期**：TUI 路径由 `app.rs` 的局部 `Arc`（以及共享同一分配的
  `AgentSession::harness`）持有；ACP 路径把同一 Arc 经 `SessionDeps.harness` 移入
  `LiveSession.harness`，会话关闭时随之释放。`reload_extension_resources` 改用
  `&Arc<AgentHarness>`：复用旧宿主的分支不重设（cell 指向的正是这个会话 Arc），
  无旧 bridge 的分支用传入的 Arc 重新填 Weak。
- **可观察断言**：`crates/kiqi-cli/src/extensions_actions.rs` 的
  `dropping_the_session_releases_the_harness_despite_the_caps_chain` 按生产顺序装配
  caps→host、sink→caps、snapshot→sink/caps，并让 harness 通过 `before_tool_call`
  持有 snapshot；随后 drop 所有者 Arc，断言 `Weak<AgentHarness>` 与 session storage 的
  `Weak` 都不再 upgrade（旧实现下 storage 仍被 host 的强 Arc 经 harness 克隆保活，
  测试会红）。

**验证**：`cargo fmt --all -- --check`；`cargo check --workspace --all-targets`
（±`--all-features`）；lean；`cargo test -p kiqi-cli --lib`；
`cargo test -p kiqi-host --features wasm --lib`；ACP e2e。
