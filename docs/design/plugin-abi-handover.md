# wasm 插件层 — 交接文档

> 状态：**可用**。契约 `kiqi:plugin@0.6.0`，能力全实现，限额齐全，CI 全绿。
> 分支：`fork/main`；设计细节见 `docs/design/plugin-abi.md`（§6.x 是逐轮记录）与 `docs/design/plugin-trait.md`。

## 1. 这是什么

给 `ki` 加的第二条插件通道：**第三方插件编译成 wasm 组件**，跑在 wasmtime 沙箱里，按用户授权拿能力。
第一方插件仍是编译期（`trait Plugin`，进程内、无沙箱）——这是设计取舍，不是没做完。

一句话：**`plugins/kiqi-plugin.wit` 就是 `trait Plugin` 的 wasm 版，导出面与导入面都一一对应。**

## 2. 契约

### 导出面（插件实现什么）—— 与 `trait Plugin` 一一对应

| `trait Plugin` | WIT `plugin-api` |
|---|---|
| `id()` | `id()` |
| `tools()` / `call()` | `list-tools()` / `call-tool(name, arguments)` |
| `commands()` / `run_command()` | `list-commands()` / `run-command(name, arguments)` |
| `flags()` / `shortcuts()` | `list-flags()` / `list-shortcuts()` |
| `on_event()` | `on-event(event) -> event-action`（`continue` / `claimed`） |
| `before_agent_start()` | `before-agent-start(prompt)` |
| `probe_points()` / `probe()` | `list-probe-points()` / `probe(point, payload)` |
| `shutdown()` | `shutdown()` |

### 导入面（插件能调用什么）—— 与 `HostCaps` + 能力对应

| `trait` | WIT import |
|---|---|
| `Log` | `log.write` |
| `SessionRead`（6，`session_id` 在 `info()` 里） | `session-read` |
| `SessionControl`（10） | `session-control` |
| `InjectApi`（4） | `inject`（含 `append-custom-entry`） |
| `UiApi`（4，含 `component`） | `ui` |
| `ProgressSink` | `progress.emit` |
| —（沙箱专有） | `net-http` `fs` `net-sockets` `exec` |

## 3. 能力与授权

`grants.json` 在 `$KI_AGENT_DIR/plugins/<id>/grants.json`（默认 `~/.ki/agent/plugins/<id>/`）。
**文件缺失 = 什么都不授予**；宿主永不替插件授权；策略随授权一起存（不是裸布尔）；策略缺失是错误。

| 能力 | 授权形状 | 执行约束 |
|---|---|---|
| `net-http` | `{hosts:[…]}` | 逐跳白名单（重定向也查）；响应体上限 **8 MiB** |
| `fs` | `true` | 监禁在 `plugins/<id>/data/`（词法 + 符号链接两层），监禁即策略 |
| `exec` | `{commands:[…], cwd?, timeout-seconds?}` | 裸命令名（无 shell）；cwd 随授权给范围：`plugin-data`（默认，关在 data 目录）或 `project`（关在**项目目录** = 宿主进程 cwd，`ki` 从不 chdir），词法 + 符号链接两层与 `fs` 同款；环境清空只回填 `PATH`；输出各 **256 KiB**；超时随授权（默认 **30s**、上限 **300s**，CLI 拒超、宿主兜底 clamp）；子进程独立进程组、结束即收整组 |
| `net-sockets` | `{hosts:[…]}` | host 白名单；**宿主持 socket**，guest 只拿 `u32` 句柄 |

> ⚠️ exec 的子进程**不被文件系统沙箱化**：cwd jail 约束的是“从哪起跑”，不是“能碰哪”——
> 拿到 `sh`/`cargo` 授权的插件本来就能 `cd` 到任何地方（甚至改自己的 `grants.json` 等重载
> 生效）。`cwd: project` 没有引入新风险类别，它加的是人体工学；授权的重量始终在 `exec`
> 本身，批 `exec` 给第三方插件前请当成“授予本机用户权限”来对待。

隔离：fuel（可选）、**guest 内存上限 256 MiB**（`Options::max_memory_bytes`）、spin/panic 均 trap 不拖垮宿主。
`ENFORCED_CAPABILITIES` = 上表四个（可授权集合 == 可实现集合）；`declared_capabilities()` 读组件声明的 import。

## 4. 代码地图

| 角色 | 路径 |
|---|---|
| **契约** | `plugins/kiqi-plugin.wit` |
| 契约 Rust 侧（`trait Plugin`、`Event`/`Verdict`、`grants`） | `crates/kiqi-plugin` |
| 宿主（wasmtime，把组件包成 `trait Plugin`） | `crates/kiqi-plugin-host` |
| 插件 SDK（WIT → Rust trait） | `plugins/wasm/kiqi-plugin-guest` |
| 已移植插件（10） | `plugins/wasm/kiqi-{conformance,todo,langfuse,ask-user,webfetch,plan-mode,rpi-lens,websearch,subagents,goal}-guest` |
| 共享逻辑（编译期与 wasm 共用） | `crates/rpi-plan-mode-core` |
| CLI | `crates/rpi-cli/src/plugin_cli.rs`（`ki plugin …`） |
| 会话接线（发现/注册/事件 sink/各贡献面） | `crates/rpi-cli/src/plugins.rs` |
| 设计文档 | `docs/design/plugin-abi.md`、`docs/design/plugin-trait.md` |

`ki` 通过 `plugin-wasm` feature（**默认开**）加载 `$KI_AGENT_DIR/plugins/<id>/plugin.wasm`；
读同目录 `grants.json`。组件自报的 `id` **只用于核对**，install 目录才是身份与授权键。

## 5. 上手

```bash
# 编译一个插件
cargo build --manifest-path plugins/wasm/kiqi-webfetch-guest/Cargo.toml \
    --target wasm32-wasip2 --release
# → plugins/wasm/target/wasm32-wasip2/release/kiqi_webfetch_guest.wasm

# 安装（列出声明能力；--yes 非交互；无 tty 且无 --yes 则什么都不授）
ki insmod --build plugins/wasm/kiqi-webfetch-guest --id rpi-webfetch --yes --host api.example.com
ki rmmod rpi-webfetch                                                 # 移到备份，不删
ki plugin list | show <id>          # show 会列出声明面（tools/commands/flags/shortcuts/probe）
ki plugin grant <id> exec --command echo     # 或 revoke / uninstall（uninstall 移到备份，不删）
# exec 还能带 cwd 范围与时间预算（诊断类插件要在项目目录跑 cargo/clippy 时）：
ki plugin grant <id> exec --command cargo --cwd project --timeout 120

# 校验契约
wasm-tools component wit plugins/kiqi-plugin.wit
```

## 6. 测试与 CI

```bash
cargo test -p kiqi-plugin-host --lib --test e2e      # 32 e2e + 5 单测
cargo test -p rpi-plan-mode-core                      # 6
cargo test -p rpi-lens                                # 3
cargo test -p kiqi-plugin --lib                       # 7
cargo test -p rpi-cli --lib                           # 510
cargo test -p rpi-extensions --lib                    # 102
```

CI：4 个 job（Linux `cargo test`、Windows `cargo check`、2× Windows installer）全绿。
注意：`cargo test --workspace` 会一并构建 wasmtime（慢、体积大）；rpi-cli 测试会改
`crates/rpi-cli/.ki/goals.json`，跑完 `git checkout --` 还原。

## 7. 未完成 / 已知限制

| 项 | 说明 |
|---|---|
| ~~**`exec` 的 cwd 策略挡住 `rpi-lens`**~~ | ✅ 已解（§6.30）：exec 授权带 `cwd`（`plugin-data` 默认 / `project`）与 `timeout-seconds`（默认 30s、上限 300s）；`rpi-lens` 已移植为 `kiqi-rpi-lens-guest`。 |
| Windows 不回收后台孙进程 | 用了 unix 进程组；Windows 需 job object，成本高，代码里已注明 |
| `flags` 声明只有 `ki plugin show` 在读 | 取值路径是 `SessionRead::cli_flag`（通用 `--foo=bar`），声明本身是诊断用 |
| `probe`/`shortcuts`/`before_agent_start` 无生产用户 | 会话已接线并有测试，但只有 conformance fixture 在用；移植 plan-mode/webfetch 也没用到它们 |
| 第一方插件仍是编译期 | 设计；wasm 层面向第三方 |
| **默认只编入无 wasm 版的插件** | `rpi-orchestrator` / `rpi-permissions` / `rpi-codegraph` 三个；有 guest 的那些（todo/lens/webfetch/websearch/subagents/goal/plan-mode/ask-user/langfuse）改为 `--features plugin-rpi-<name>` 或 `ki insmod --build`。crate/feature/cdylib 面/测试全保留，CI 跑 opt-in 配置防腐烂。注意耦合：orchestrator 的 goal 联动读 `.ki/goals.json`，只有**编译期** goal 会写 |
| `wasmtime` 常驻 `ki` | 由 `plugin-wasm`（默认开）控制；要瘦身需单独门控 |
| `.wasm` 无签名/摘要校验 | 见 `plugin-abi.md` §7 待定项 |
| 装完需重启 | `compiled_plugin_hosts()` 是进程级 `OnceLock` |
| e2e 的 `exec_does_not_hang_when_the_command_leaves_a_background_child` 在满负载并行跑时偶发超时 | 15s 的 `recv_timeout` 窗口里含 wasmtime 编译 fixture 的时间，并行构建时可能不够；单跑必过。改阈值属 `RULE_阈值变更`，未动 |
| 不迁（裁决见 §6.31） | `rpi-permissions`（信任倒置：沙箱 guest 重写宿主权限策略，需先设计）、`rpi-codegraph`（search 要长活 stdio 句柄，同 orchestrator 类）、`rpi-orchestrator`（长生命周期编排） |

## 8. 提交轨迹（近期，`fork/main`）

```
本批（exec cwd scope + rpi-lens 移植；含一轮分维度对抗审查的修正）：
- feat(plugins): exec grant carries a cwd scope and a time budget
- feat(plugins): port rpi-lens to wasm (kiqi-rpi-lens-guest)
- fix(plugins): rpi-lens truncation no longer panics mid-codepoint
- docs(plugin-abi): round 6.30 + correct three stale claims
- fix(host): the jail check must not create directories outside the root
- fix(plugins): the lens guest's deny advice must list every command it needs
- fix(cli): exec flag hygiene in `ki plugin`
- fix(host): a project-scoped exec cwd must exist
- test(host): pin the exec timeout clamp and the project-dir fallback
- fix(plugins): lens guest cwd mapping edge cases + documented parity gaps
- docs(plugin-abi): correct the phantom manifest gate and stale states
- fix(cli): announce when re-granting exec resets its scope/budget

dce1216 移植 rpi-webfetch + rpi-plan-mode（ABI 0.6.0，补导入面两缺口）
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
639d935 ki plugin install/uninstall
9eb8b12 wasm 插件接入会话
```
