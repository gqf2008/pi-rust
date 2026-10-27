> 注（2026-10-21）：编译期一方插件通道已退役；文中 `extensions/kiqi-*` 路径与 `extension-kiqi-*` feature 是历史记录，现行实现在 `extensions/wasm/kiqi-*`。

# wasm 插件层 — 交接文档

> 状态：**可用（ABI 0.9.0）**。契约 `kiqi:extension@0.9.0`；12 个 guest 已按新导出面重建，
> 十个授权 key 都进入 `ENFORCED_CAPABILITIES`（授权、策略校验与 CLI 齐全），OAuth / provider /
> render / resources / 类型化命令面均已落地。provider 由专用 `ProviderHost` 与 CLI 的
> `WasmProvider` 执行，详见 `extension-abi.md` §6.41。设计细节见 `docs/design/extension-abi.md`
> （§6.x 是逐轮记录，0.8.0 的权威条款是 §6.39–§6.44）与 `docs/design/extension-trait.md`（历史存档）。

## 1. 这是什么

给 `ki` 加的插件通道：**插件编译成 wasm 组件**，跑在 wasmtime 沙箱里，按用户授权拿能力。
2026-10-21 起一方插件也只走 wasm；2026-10-26 起 **cdylib 通道整体退役**（`ki dev`/`ki install`/`kiqi-extension-sdk` 已删除），wasm 是唯一通道。

一句话：**`extensions/kiqi-extension.wit` 是 `trait Extension` 的 wasm 版**；0.8.0 起导出面多出三个
贡献面（OAuth / provider / render），不再与 trait 一一对应。

## 2. 契约

### 导出面（插件实现什么）

> 0.8.0 起导出面**不再与 `trait Extension` 一一对应**：world 多出三个贡献面
> （`extensions/kiqi-extension.wit:641-644`），`run-command` 换成返回 `command-result`，并新增
> `continue-command`（`extensions/kiqi-extension.wit:596-607`）。组件模型要求 world 里每个导出都在
> `Guest` trait 上实现——不贡献这些面的 guest 用
> `kiqi_wasm_bindings::deny_contribution_exports!(…)` 填桩
> （`extensions/wasm/kiqi-wasm-bindings/src/lib.rs:89-108`）。

| `trait Extension` | WIT `extension-api` |
|---|---|
| `id()` | `id()` |
| `tools()` / `call()` | `list-tools()` / `call-tool(name, arguments)` |
| `commands()` / `run_command()` | `list-commands()` / `run-command(name, arguments) -> command-result` / `continue-command(name, flow-id, reply)`（§6.44） |
| `flags()` / `shortcuts()` | `list-flags()` / `list-shortcuts()` |
| `on_event()` | `on-event(event) -> event-action`（`continue` / `claimed` / `patch`） |
| `before_agent_start()` | `before-agent-start(prompt)` |
| `probe_points()` / `probe()` | `list-probe-points()` / `probe(point, payload)` |
| `shutdown()` | `shutdown()` |
| —（贡献面，trait 无对应方法） | `oauth-api`：`list-providers()` / `action(provider-id, action)`（§6.40） |
| —（贡献面，trait 无对应方法） | `provider-api`：`list-providers()` / `request(request-json)`（§6.41） |
| —（贡献面，trait 无对应方法） | `render-api`：`transform-markdown(markdown)`（§6.42） |

### 导入面（插件能调用什么）—— 与 `HostCaps` + 能力对应

| `trait` | WIT import |
|---|---|
| `Log` | `log.write` |
| `SessionRead`（6，`session_id` 在 `info()` 里） | `session-read` |
| `SessionControl`（10） | `session-control` |
| `InjectApi`（4） | `inject`（含 `append-custom-entry`） |
| `UiApi`（4，含 `component`） | `ui` |
| `ProgressSink` | `progress.emit` |
| —（沙箱专有） | `net-http` `fs` `net-sockets` `exec` `proc` |
| —（0.8.0，映射 `oauth` grant） | `oauth-http`：`token-request` / `revoke`（token 不进沙箱，§6.40） |
| —（0.8.0，映射 `provider` grant） | `provider-stream`（delta sink）/ `provider-http`（受控流式 HTTP，§6.41） |

`oauth` 不需要 `net-http` 授权：端点由 grant 给出，HTTP 由宿主发出。

### 0.8.0 章节索引（`extension-abi.md`）

| 节 | 内容 | 主要代码 |
|---|---|---|
| §6.39 | 授权模型：五个新 key（`oauth`/`provider`/`render`/`resources`/`provider-request`），全部 deny-by-default、授权自带策略、`--yes` 不授予 | `crates/kiqi-host/src/grants.rs`、`crates/kiqi-cli/src/extension_cli.rs` |
| §6.40 | 宿主归因 OAuth：token 不进沙箱，URL 不由 guest 提供 | `crates/kiqi-host/src/wasm/oauth.rs`、`crates/kiqi-cli/src/wasm_oauth.rs` |
| §6.41 | provider：grant 是唯一书目、专用实例、`provider-http` 请求过滤、带索引 delta | `crates/kiqi-host/src/wasm/mod.rs` |
| §6.42 | render：deny-all 实例、仅定稿转换、输出上限与超时 | `crates/kiqi-cli/src/extensions.rs`（`WasmRenderTransformer`） |
| §6.43 | resources：`resources.json` 包约定、安装期摘要、first-wins 顺序 | `crates/kiqi-cli/src/extensions.rs` |
| §6.44 | 类型化命令面、`event.input`、`event-action.patch` | `crates/kiqi-cli/src/interactive_tui/extension_commands.rs` |

## 3. 能力与授权

`grants.json` 在 `$KI_AGENT_DIR/extensions/<id>/grants.json`（默认 `~/.ki/agent/extensions/<id>/`）。
**文件缺失 = 什么都不授予**；宿主永不替插件授权；策略随授权一起存（不是裸布尔）；策略缺失是错误。

| 能力 | 授权形状 | 执行约束 |
|---|---|---|
| `net-http` | `{hosts:[…]}` | 逐跳白名单（重定向也查）；响应体上限 **8 MiB** |
| `fs` | `{cwd: extension-data\|project}`（老文件裸 `true` 照读为 data 监禁） | 监禁在 `extensions/<id>/data/`（词法 + 符号链接两层），监禁即策略 |
| `exec` | `{commands:[…], cwd?, timeout-seconds?}` | 裸命令名（无 shell）；cwd 随授权给范围：`extension-data`（默认，关在 data 目录）或 `project`（关在**项目目录** = 宿主进程 cwd，`ki` 从不 chdir），词法 + 符号链接两层与 `fs` 同款；环境清空只回填 `PATH`；输出各 **256 KiB**；超时随授权（默认 **30s**、上限 **300s**，CLI 拒超、宿主兜底 clamp）；子进程独立进程组、结束即收整组 |
| `proc` | `{commands:[…], cwd?, timeout-seconds?, max-open?}` | **调用内**可交互子进程（spawn/write/read-line/wait/kill）；命令白名单、cwd 同 exec 的 scope、`--timeout` 默认 **600s**/上限 1800s、`--max-open` 默认 **100**/上限 256；句柄只活一次调用，返回即杀组收尸 |
| `net-sockets` | `{hosts:[…]}` | host 白名单；**宿主持 socket**，guest 只拿 `u32` 句柄 |

### 0.8.0 的五个 key（全部 deny-by-default，`--yes` 不授予）

| key | 授权形状（摘要） | 执行约束 | CLI 拼写 |
|---|---|---|---|
| `oauth` | `{providers:[{id, flows, authorization-endpoint, token-endpoint, device-authorization-endpoint?, revocation-endpoint?, client-id, supports-pkce?}], timeout-seconds?, max-requests?}` | 端点必须是**精确主机的 `https://` URL**（无通配、无 userinfo、无 fragment）；timeout 1..=900；max-requests 1..=64 | `ki extension grant <id> oauth --provider <pid> --profile <file.json>`（可重复成对） |
| `provider` | `{providers:[{id, models:[{id, api, base-url?, reasoning?, input?}], endpoint:{scheme, host, path-prefix, methods?}, credential?, timeout-seconds?}]}` | scheme 必须 `https`、host 精确无通配、methods 仅 `GET`/`POST`；`api` 只收 typed wire API（`faux` 与未知值拒绝） | `ki extension grant <id> provider --model-json <file.json> --endpoint <scheme://host/prefix> [--method GET\|POST] --credential-from auth-json:<pid> [--timeout <s>]` |
| `render` | `true` 或 `{kinds?, timeout-ms?, max-output-bytes?}` | kind 仅 `markdown`；2000 ms（上限 10 s）；1 MiB（上限 8 MiB） | `ki extension grant <id> render --kind markdown` |
| `resources` | `true` 或 `{kinds?, max-items?, max-bytes?}` | kind 仅 `skills`/`prompts`；64 项（上限 256）；1 MiB（上限 16 MiB） | `ki extension grant <id> resources --kind skills\|prompts` |
| `provider-request` | `{observe?, headers?, messages?, patch?, patch-timeout-ms?}` | 补丁走宿主固定表（§6.44）；observe 只有 `none`/`requests` | `ki extension grant <id> provider-request --observe [--headers names\|values] [--messages none\|full] [--patch]` |

`--yes` 只对 `net-http`/`fs`/`exec`/`proc`/`net-sockets` 生效；上面五个必须显式授予
（`AUTO_GRANT_EXEMPT`）。无 tty 且无 `--yes` 时什么都不授。`resources.json` 只是包声明，
安装/重装不会隐式授予或复活 `resources`；必须显式 `ki extension grant`。**手改 `grants.json`
fail-closed**：解析失败按零授权，某个 key 校验失败只清该 key。

> ⚠️ exec 的子进程**不被文件系统沙箱化**：cwd jail 约束的是“从哪起跑”，不是“能碰哪”——
> 拿到 `sh`/`cargo` 授权的插件本来就能 `cd` 到任何地方（甚至改自己的 `grants.json` 等重载
> 生效）。`cwd: project` 没有引入新风险类别，它加的是人体工学；授权的重量始终在 `exec`
> 本身，批 `exec` 给第三方插件前请当成“授予本机用户权限”来对待。

隔离：fuel（可选）、**guest 内存上限 256 MiB**（`Options::max_memory_bytes`）、spin/panic 均 trap 不拖垮宿主。
`ENFORCED_CAPABILITIES` = 上表十个（五旧 + 五新，可授权集合 == 可实现集合，
`crates/kiqi-host/src/wasm/mod.rs:294-305`）；`declared_capabilities()` 读组件声明的
import **与 export**（`oauth-api`/`provider-api`/`render-api` 折叠成 grant key，
`lib.rs:222-290`）。

## 4. 代码地图

| 角色 | 路径 |
|---|---|
| **契约** | `extensions/kiqi-extension.wit` |
| 契约 Rust 侧（`trait Extension`、`Event`/`Verdict`、`grants`） | `crates/kiqi-host` |
| 宿主（wasmtime，把组件包成 `trait Extension`） | `crates/kiqi-host/src/wasm/` |
| 插件 SDK（WIT → Rust trait） | `extensions/wasm/kiqi-wasm-bindings` |
| 已移植插件（12） | `extensions/wasm/kiqi-{conformance,todo,langfuse,ask-user,webfetch,plan-mode,lens,websearch,subagents,goal,codegraph,orchestrator}`（bindings crate：`kiqi-wasm-bindings`） |
| 计划模式逻辑（纯逻辑 + 原生单测） | `extensions/wasm/kiqi-plan-mode/src/plan.rs` |
| CLI | `crates/kiqi-cli/src/extension_cli.rs`（`ki extension …`） |
| 会话接线（发现/注册/事件 sink/各贡献面） | `crates/kiqi-cli/src/extensions.rs` |
| 宿主归因 OAuth（端点、表单白名单、设备码轮询） | `crates/kiqi-host/src/wasm/oauth.rs` |
| provider 请求补丁固定表 | `crates/kiqi-host/src/wasm/provider_patch.rs` |
| `ki auth login/refresh/logout` 的 wasm 路径 | `crates/kiqi-cli/src/wasm_oauth.rs` |
| provider 运行时适配器（惰性 `ProviderHost`、delta 流、取消与每次请求重验 grant） | `crates/kiqi-cli/src/wasm_provider.rs` |
| 设计文档 | `docs/design/extension-abi.md`、`docs/design/extension-trait.md` |

`ki` 通过 `extension-wasm` feature（**默认开**）加载 `$KI_AGENT_DIR/extensions/<id>/extension.wasm`；
读同目录 `grants.json`。组件自报的 `id` **只用于核对**，install 目录才是身份与授权键。

### cdylib 面 vs wasm 权威路径（0.8.0 → P6 已收口）

OAuth / provider / renderer / resources 在 cdylib 通道里曾有**旧注册槽位**；0.8.0 起
wasm 就是权威路径，**2026-10-26 的 P6 把 cdylib 装载机整体删除**（`kiqi-extension-sdk`、
`libloading` loader、`ki dev`/`ki install`/`ki package`）。现在没有任何装载路径能喂给
这些旧槽位——registry 类型仍在（脊柱保留），但恒为空：

| cdylib 面（已退役） | 现状 | 权威 wasm 路径 |
|---|---|---|
| `register_oauth_provider` | 死面：`ki auth` 的 extension session 恒为 `ExtensionSession::none()`，不再回退到 cdylib registry | `oauth-api` + `oauth-http`（§6.40） |
| `register_provider` | 死面：`ExtensionSession` 恒空，`PluggableProvider` 不再注入模型链 | `provider-api` + `provider-http`/`provider-stream`（§6.41） |
| `register_message_renderer` / `register_markdown_transformer` / `register_entry_renderer` | 死面：renderer tier 对空 registry 恒无命中 | `render-api`（§6.42）。0.8.0 **只有 markdown**，消息/entry renderer 不提供 |
| `register_resources_discover` | 死面：startup/reload 发现对空 registry 恒无输出 | `resources.json` + `resources` grant（§6.43） |

已删除（2026-10-27，§6.49）：C ABI 注册回调机器（`HostApi`/vtable/线程局部
`current_api`）与 `ExtensionToolAdapter` 已随纯 serde 事件总线一批移除，不再随
脊柱编译
（`extension-abi.md` §6.45）。

**事件 sink 已按会话隔离**（2026-10-27，提交 `bf77562`）：
`install_compiled_event_sink` 仍把 sink 写进进程级槽位，但只作为**无快照路径的 fallback**；
它同时返回该 sink，`kiqi-cli` 在 build/reload 里把它挂到会话的
`RegistrySnapshot`（`ExtensionSession::none_with_compiled_sink` → `RegistrySnapshot::compiled_sink`）。
`translate` 的 5 个派发点优先用 `snapshot.compiled_sink()`，所以同一进程内两个会话
（ACP 多会话）用各自的 caps 派发，不再 last-writer-wins；**wasm host 实例仍是
进程级共享**，会话级隔离的是"用哪份 caps 派发"，不是"哪个 guest 收到事件"。
`COMPILED_CAPS`
（工具/命令/probe/prompt transform 读取的进程级能力集）仍是 last-writer-wins，
不在本批范围内。


## 5. 上手

```bash
# 编译一个插件
cargo build --manifest-path extensions/wasm/kiqi-webfetch/Cargo.toml \
    --target wasm32-wasip2 --release
# → extensions/wasm/target/wasm32-wasip2/release/kiqi_webfetch.wasm

# 安装（列出声明能力；--yes 非交互；无 tty 且无 --yes 则什么都不授）
ki insmod --build extensions/wasm/kiqi-webfetch --id kiqi-webfetch --yes --host api.example.com
ki rmmod kiqi-webfetch                                                 # 移到备份，不删
ki extension list | show <id>          # show 会列出声明面（tools/commands/flags/shortcuts/probe）
ki extension grant <id> exec --command echo     # 或 revoke / uninstall（uninstall 移到备份，不删）
# exec 还能带 cwd 范围与时间预算（诊断类插件要在项目目录跑 cargo/clippy 时）：
ki extension grant <id> exec --command cargo --cwd project --timeout 120

# 0.8.0 的五个新能力都必须显式授权（install --yes 不授予）
ki extension grant my-extension render --kind markdown
ki extension grant my-extension provider-request --observe

# 校验契约
wasm-tools component wit extensions/kiqi-extension.wit
```

## 6. 测试与 CI

```bash
cargo test -p kiqi-host --features wasm --lib --test e2e   # wasm e2e + 单测
cargo test -p kiqi-host --lib                              # 契约 + 派发脊柱
cargo test -p kiqi-cli --lib                               # CLI
# guest 单测（宿主目标，需在 guest 工作区内）：
(cd extensions/wasm && cargo test -p kiqi-plan-mode --locked)   # 6
(cd extensions/wasm && cargo test -p kiqi-lens --locked)        # 3
```

CI：4 个 job（Linux `cargo test`、Windows `cargo check`、2× Windows installer）全绿。
注意：`cargo test --workspace` 会一并构建 wasmtime（慢、体量大）；kiqi-cli 的 goal 断言
已在子进程 + tempdir 里跑，不再写源码树，跑完无需手动还原。

## 7. 未完成 / 已知限制

| 项 | 说明 |
|---|---|
| **permissions 已由宿主策略取代** | 2026-10-17：策略存宿主的 `permissions.json`（同一文件/形状/全局-项目优先级），`before_tool_call` 真正强制（此前编译期插件只是一个 `permissions` 工具，**从未自动拦截**）。`extension-kiqi-permissions` 移出默认构建、crate 保留 opt-in。方向：宿主持有宿主策略（原「信任倒置」的正解）。 |
| ~~**`exec` 的 cwd 策略挡住 `kiqi-lens`**~~ | ✅ 已解（§6.30）：exec 授权带 `cwd`（`extension-data` 默认 / `project`）与 `timeout-seconds`（默认 30s、上限 300s）；`kiqi-lens` 已移植为 `kiqi-lens`。 |
| Windows 不回收后台孙进程 | 用了 unix 进程组；Windows 需 job object，成本高，代码里已注明 |
| `flags` 声明只有 `ki extension show` 在读 | 取值路径是 `SessionRead::cli_flag`（通用 `--foo=bar`），声明本身是诊断用 |
| `probe`/`shortcuts`/`before_agent_start` 无生产用户 | 会话已接线并有测试，但只有 conformance fixture 在用；移植 plan-mode/webfetch 也没用到它们 |
| ~~第一方插件仍是编译期~~ | ❌ 已不成立（2026-10-21 退役）：一方插件只走 wasm，见 `extensions/wasm/kiqi-*`；`kiqi_extension::Extension` trait 与 `ki dev` 的 cdylib 热载仅服务第三方扩展。 |
| ~~默认只编入无 wasm 版的插件~~ | ❌ 已不成立（2026-10-21 退役）：`extensions/kiqi-*` crate、`extension-kiqi-*` feature、opt-in feature CI 与独立 cdylib 构建全部删除，编译期通道不再编入任何一方插件；一方插件一律 `ki insmod --build extensions/wasm/kiqi-<name>`。权限策略归宿主 `crates/kiqi-cli/src/permissions.rs`。仍然成立的耦合：两种 goal 都写 `.ki/goals.json` —— wasm guest 在 `fs` 授权 scope 为 project 时直接 `fs::write`（`extensions/wasm/kiqi-goal/src/lib.rs` 的 `write_goals`），§6.35 之后默认就是 project。 |
| `wasmtime` 常驻 `ki` | 由 `extension-wasm`（默认开）控制；要瘦身需单独门控 |
| `.wasm` 无来源签名 | 安装摘要 sidecar 会挡住安装后的字节变更，但摘要不是签名；来源签名仍见 `extension-abi.md` §7 待定项 |
| **内建 provider 不自动消费 OAuth entry** | 内建链路的 `stored_api_key` 仍只匹配 `ApiKey`；但 wasm provider 已能通过显式 `--credential-from` 使用 OAuth access token，并在每次请求重算凭据指纹，不能再概括为「OAuth 没有模型请求消费者」（§6.40/§6.41）。 |
| **O17 残余风险仍在，告警已落地** | 同时持有 `exec`/`fs`/`net-sockets` 与 `oauth`/`provider` 时，`credential_capability_warning` 会在授权后警告并给出 revoke/logout 命令；它不拒绝授权，也不能消除插件经其它能力读取 `auth.json` 的爆炸半径（§6.39）。 |
| **同意面仍不对称** | `probe`/`verdict.replace`、`before-agent-start`、`session-read`/`session-control`/`inject`/`ui` 在 0.8.0 仍不需要 grant（业主决定，§6.39 记录）。 |
| ~~cdylib 的 OAuth/provider/renderer/resources 槽位待删~~ | ✅ 已解（2026-10-26 P6）：装载机与 `kiqi-extension-sdk`/`ki dev`/`ki install`/`ki package` 全部删除，旧槽位不可达；~~进程级 compiled event sink 仍是 last-writer-wins~~ ✅ 已解（2026-10-27）：sink 挂在会话的 `RegistrySnapshot` 上，进程级槽位只作无快照路径的 fallback（见上节）。`COMPILED_CAPS` 仍是进程级 last-writer-wins。 |
| 装完需重启 | `compiled_extension_hosts()` 是进程级 `OnceLock` |
| ~~e2e 的 `exec_does_not_hang_when_the_command_leaves_a_background_child` 满负载偶发超时~~ | ✅ 已在 `dce1216` 缓解：把 `fixture_wasm()` 移到计时窗口**之外**（`e2e.rs:2425-2428` 的注释写明理由），15s 窗口里不再包含 wasmtime 编译时间。阈值未动，因为根因是窗口里混入了无关工作而非窗口太小。实测：e2e 单跑 3 次全绿；再与整个 workspace 测试**并行**跑一次，0 失败 |
| ~~**acp 不迁，宿主级立项**~~ | ✅ 已落地：`ki acp serve`（批次 1/2 于 2026-10-16 完成；批次 3 为 abb 对接与文档销账）。裁定与方案见 `extension-abi.md` §6.2 与 `ki-acp-serve.md`（进程内驱动 AgentHarness、协议层用官方 `agent-client-protocol` crate、abb-agent 做语义层参照） |
| 不迁 | `kiqi-permissions`（信任倒置：沙箱 guest 重写宿主权限策略，需先设计）——唯一保留的编译期插件。~~orchestrator~~ 已按业主要求以**阻塞波**形态迁走（§6.34：放弃后台波与中途重塑，其余语义保留）；~~codegraph~~ 随 `proc` 迁走 |

## 8. 提交轨迹（近期，`fork/main`）

```
本批（exec cwd scope + kiqi-lens 移植；含一轮分维度对抗审查的修正）：
- feat(extensions): exec grant carries a cwd scope and a time budget
- feat(extensions): port kiqi-lens to wasm (kiqi-lens)
- fix(extensions): kiqi-lens truncation no longer panics mid-codepoint
- docs(extension-abi): round 6.30 + correct three stale claims
- fix(host): the jail check must not create directories outside the root
- fix(extensions): the lens guest's deny advice must list every command it needs
- fix(cli): exec flag hygiene in `ki extension`
- fix(host): a project-scoped exec cwd must exist
- test(host): pin the exec timeout clamp and the project-dir fallback
- fix(extensions): lens guest cwd mapping edge cases + documented parity gaps
- docs(extension-abi): correct the phantom manifest gate and stale states
- fix(cli): announce when re-granting exec resets its scope/budget

dce1216 移植 kiqi-webfetch + kiqi-plan-mode（ABI 0.6.0，补导入面两缺口）
087f73b docs: intro 里已成假的现状描述
d1990cf exec 收整进程组（修“后台子进程挂死宿主”）
9f1c2a9 net-http 响应体上限 8 MiB
aa2da88 guest 内存上限
78e2238 修两个绕过：net-http 重定向、fs 符号链接
39aa036 exec/net-sockets 真实现（授权带策略）
90397d3 fs 真实现（监禁）+ install 明说做不到的能力
4b5e25a ABI 0.5.0：wasm 版 = trait 那一份（导出面一一对应）
4d7544d 编译期插件的 on_event 动作到达路由分发器
9a43be8 /reload 不再丢 UI 能力
dcba367 三方插件不得顶掉既有工具名
8dbe54f 其余贡献面接进会话（commands/shortcuts/before_agent_start/probe）
639d935 ki extension install/uninstall
9eb8b12 wasm 插件接入会话
```
