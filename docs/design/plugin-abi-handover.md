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
| `exec` | `{commands:[…]}` | 裸命令名（无 shell）；cwd 关在 data 目录；环境清空只回填 `PATH`；输出各 **256 KiB**；**30s** 超时；子进程独立进程组、结束即收整组 |
| `net-sockets` | `{hosts:[…]}` | host 白名单；**宿主持 socket**，guest 只拿 `u32` 句柄 |

隔离：fuel（可选）、**guest 内存上限 256 MiB**（`Options::max_memory_bytes`）、spin/panic 均 trap 不拖垮宿主。
`ENFORCED_CAPABILITIES` = 上表四个（可授权集合 == 可实现集合）；`declared_capabilities()` 读组件声明的 import。

## 4. 代码地图

| 角色 | 路径 |
|---|---|
| **契约** | `plugins/kiqi-plugin.wit` |
| 契约 Rust 侧（`trait Plugin`、`Event`/`Verdict`、`grants`） | `crates/kiqi-plugin` |
| 宿主（wasmtime，把组件包成 `trait Plugin`） | `crates/kiqi-plugin-host` |
| 插件 SDK（WIT → Rust trait） | `plugins/wasm/kiqi-plugin-guest` |
| 已移植插件（6） | `plugins/wasm/kiqi-{conformance,todo,langfuse,ask-user,webfetch,plan-mode}-guest` |
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
ki plugin install <file.wasm> --id rpi-webfetch --yes --host api.example.com
ki plugin list | show <id>          # show 会列出声明面（tools/commands/flags/shortcuts/probe）
ki plugin grant <id> exec --command echo     # 或 revoke / uninstall（uninstall 移到备份，不删）

# 校验契约
wasm-tools component wit plugins/kiqi-plugin.wit
```

## 6. 测试与 CI

```bash
cargo test -p kiqi-plugin-host --lib --test e2e      # 21 e2e + 2 单测
cargo test -p rpi-plan-mode-core                      # 6
cargo test -p rpi-cli --lib                           # 507
cargo test -p rpi-extensions --lib                    # 102
cargo test -p kiqi-plugin --lib                       # 5
```

CI：4 个 job（Linux `cargo test`、Windows `cargo check`、2× Windows installer）全绿。
注意：`cargo test --workspace` 会一并构建 wasmtime（慢、体积大）；rpi-cli 测试会改
`crates/rpi-cli/.ki/goals.json`，跑完 `git checkout --` 还原。

## 7. 未完成 / 已知限制

| 项 | 说明 |
|---|---|
| **`exec` 的 cwd 策略挡住 `rpi-lens`** | 诊断类插件要在**项目目录**跑 `cargo`/`clippy`，现在 cwd 只能关在插件 data 目录。需要给授权加 cwd 范围（如 `--cwd project\|plugin-data`），两边一起改。**这是下一个真实阻塞点。** |
| Windows 不回收后台孙进程 | 用了 unix 进程组；Windows 需 job object，成本高，代码里已注明 |
| `flags` 声明只有 `ki plugin show` 在读 | 取值路径是 `SessionRead::cli_flag`（通用 `--foo=bar`），声明本身是诊断用 |
| `probe`/`shortcuts`/`before_agent_start` 无生产用户 | 会话已接线并有测试，但只有 conformance fixture 在用；移植 plan-mode/webfetch 也没用到它们 |
| 第一方插件仍是编译期 | 设计；wasm 层面向第三方 |
| `wasmtime` 常驻 `ki` | 由 `plugin-wasm`（默认开）控制；要瘦身需单独门控 |
| `.wasm` 无签名/摘要校验 | 见 `plugin-abi.md` §7 待定项 |
| 装完需重启 | `compiled_plugin_hosts()` 是进程级 `OnceLock` |
| 未移植 | `rpi-goal`、`rpi-lens`、`rpi-permissions`、`rpi-websearch`、`rpi-subagents`、`rpi-codegraph` |

## 8. 提交轨迹（近期，`fork/main`）

```
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
