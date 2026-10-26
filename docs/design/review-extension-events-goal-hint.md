# 审查记录：extensions 事件面修复 + goal 推进提示

审查对象（`main` 分支，两笔均已提交）：

| commit | 主题 |
|---|---|
| `4ad87ef` | fix(extensions): a dylib-free session still delivers extension events |
| `acf14ed` | feat(goal): an active goal points the model at the orchestrator |

结论：两笔方向正确，未发现逻辑错误。`4ad87ef` 可直接接受；`acf14ed` 有 1 处测试盲区
（已用突变验证证伪其 e2e 声称的语义）、1 处注释错位，建议修掉再算完成。

> 本记录已在**本机 walgit**（`http://127.0.0.1:8081/gqf2008/pi-rust`）开成 issue
> 线程，以签名 `issue` 条目落 `refs/collab/inbox/ki-agent/*`（principal `ki-agent`）：
>
> | thread id | 内容 |
> |---|---|
> | `review-4ad87ef-acf14ed` | 本文的 R1–R8 汇总 |
> | `ask-user-guest-call-panic` | 文末「`ask_user` 插件故障」待查项 |
>
> 查法：`walgit collab thread <id>` / `walgit collab board`（本 checkout 已 `collab join`）。
> GitHub 侧（`origin` / `fork`）不是跟踪载体：`fork`（`gqf2008/pi-rust`）issues 已禁用，
> 且按 AGENTS.md 它只作镜像，不作为变更来源或对齐目标。

---

## R1 [应修] e2e 反向用例没有钉住「工具不在 ⇒ 不加提示」

`crates/kiqi-extension-host/tests/e2e.rs:3151` 反向分支用的是 `goal_caps(...)`，
即 `session: None`（该 helper 见 `tests/e2e.rs` 的 `fn goal_caps`，`session: None`）。
于是它只证明「**没有** session ⇒ 空工具面 ⇒ 不加提示」，并没有证明「工具面存在、
但没装 orchestrator ⇒ 不加提示」——而后者才是提交动机里的现场（快模型 + bash 惯性
的安装不该被点名 `plan`/`dispatch`/`subagent`）。

**可复现判据**（把三工具合取弱化成「工具面非空」）：

```bash
# 把 extensions/wasm/kiqi-goal/src/lib.rs 里的判据改为：
#   let present = |_name: &str| !tools.is_empty();
cargo build --manifest-path extensions/wasm/kiqi-goal/Cargo.toml \
  --target wasm32-wasip2 --release --target-dir extensions/wasm/target
cargo test -p kiqi-extension-host --test e2e --locked goal_injection_carries
# → 仍然 ok（1 passed），即该测试对这条突变无感
```

**建议**：反向分支改成「有工具面但无 orchestrator」，例如
`ToolFaceSession { tools: vec!["bash".into(), "read".into(), "edit".into()] }`，
`session: Some(&bare_tools)`。改完上面那条突变必须变红。

## R2 [应修] 文档注释被插入的常量截断

`extensions/wasm/kiqi-goal/src/lib.rs:396-403`：`append_goal_custom_message` 原有的
三行 doc（`/// Append the goal's current state as a generic custom message. …`）被新插入的
`ORCHESTRATION_HINT` / `fn orchestration_hint()` 从中间截开，现在成了**常量**的 doc 首段；
真正的 `append_goal_custom_message`（`:411`）本身没有任何文档。

**建议**：把 `const ORCHESTRATION_HINT` 与 `fn orchestration_hint` 整体挪到那三行 doc 之前，
或把三行 doc 重新挂到 `:411` 上方。

## R3 [建议] `goal_status` 匹配了两次，将来加状态要改两处

`extensions/wasm/kiqi-goal/src/lib.rs` 的 `append_goal_custom_message` 内，
`:412` 的 `match goal.status.as_str()` 取 `tag`，`:424` 又用
`match (goal.status.as_str(), orchestration_hint())` 决定是否追加提示。合并成一次
`match` 返回 `(tag, 是否可加提示)` 更稳。附带：`:418` 的 `let hint = goal.budget_hint();`
与 `:425` 臂内绑定的 `Some(hint)` 同名不同物，读起来串味，建议改名。

## R4 [建议] `paused` / `complete` 不加提示这条无覆盖

R1 修好之后，仍建议补一个 `goal pause` 后注入**不含**提示的断言——当前 e2e 只走了
`start`（active）。否则将来把 `("active", …)` 的守卫删掉，测试全绿。

## R5 [建议] `--no-extensions` 的语义漂移没写进 CHANGELOG

`4ad87ef` 让无 cdylib 会话也带空注册表后，`crates/rpi-cli/src/session.rs:356` 的
`if args.no_extensions { ExtensionSession::none() }` 分支同样会把事件面交给
compiled/wasm 层——也就是说 `--no-extensions` 现在只关 cdylib 层，goal 计费、
todo/plan-mode 恢复照旧。

CHANGELOG 该条只写了 "dylib-free install" / "a wasm-only `ki`"。若这是预期语义
（该 tier 属宿主而非 cdylib），补一句「`--no-extensions` 只关 cdylib 层」；否则
以后会被当 bug 报。

## R6 [观察] 无 cdylib 安装的事件开销从零变成「每次 translate」

修复后，以前不进漏斗的 `AgentEvent` 现在都要构造 `StableExtensionEvent`（工具载荷会做
JSON 序列化，见 `crates/rpi-extensions/src/compiled.rs` 的 `event_payload`），才在空
handler 处提前返回。这是修 bug 的必然代价，不算缺陷；但 TUI 大工具输出场景值得知情，
`crates/rpi-cli/src/interactive_tui/state.rs:1114` 那句「Cheap pre-check」现在只剩一次
`handlers_for(...).is_empty()` 的实际作用。

## R7 [nit] `snapshot()` 的文档没跟着写明不变量

`4ad87ef` 给 `snapshot_arc()`（`crates/rpi-extensions/src/loader.rs:417` 附近）写明了
「本模块构造的会话恒为 `Some`」，但 `snapshot()`（`loader.rs:412`）的旧描述「`None` when
no extensions loaded successfully」仍在。而 `Option` 对内部构造路径事实上已成死角
（`loader.rs:527` 的 `loaded.is_empty()` 也返回 `none()`）。两处说法应对齐。

## R8 [测试基建，影响可复现性] `build_guest` 硬编码产物路径

`crates/kiqi-extension-host/tests/e2e.rs:34-49` 的 `build_guest` 把产物路径写死成
`<root>/extensions/wasm/target/wasm32-wasip2/release/<artifact>`，但仓库里的
`.cargo/config.toml`（本机配置，不进 git）把 `build.target-dir` 指到了数据盘。于是本机跑
任何 guest e2e 都直接失败：

```
copy the guest next to its component dir: Os { code: 2, kind: NotFound, ... }
```

本次审查是显式补编之后才跑通的：

```bash
cargo build --manifest-path extensions/wasm/kiqi-goal/Cargo.toml \
  --target wasm32-wasip2 --release --target-dir extensions/wasm/target
cargo test -p kiqi-extension-host --test e2e --locked goal_injection_carries
# → 1 passed
```

**建议**：`build_guest` 显式传 `--target-dir`，或读 `CARGO_TARGET_DIR`。否则在设了
`build.target-dir` 的机器上，「e2e 已验证」无法复现。

---

## 已核实的正向结论（带判据）

- WIT 侧确有该能力：`extensions/kiqi-extension.wit:179` `active-tools: func() -> list<string>;`
- host 侧读的是**模型可见**的工具集：`crates/rpi-cli/src/extensions_actions.rs:362`
  → `lane.get_active_tools()`（已过 `--tools`/`--exclude-tools` 过滤），
  故 R1 的条件语义在实现上是成立的，缺的只是测试。
- 两条漏斗确实在「空 handler 提前 return」**之前**扇到 compiled：
  `crates/rpi-extensions/src/translate.rs:345`（`dispatch_to_handlers` 内先
  `compiled::dispatch`，`:341` 注释原文 "a `ki` with no dylibs still traces"）与
  `translate.rs:730`（`dispatch_lifecycle_event` 内同序，`:725-729` 注释）。这条是
  `4ad87ef` 的前提，成立。
- 新空快照不会被 staleness 守卫拦掉：`ExtensionRegistry::new()` 的
  `active` 初值为 `true`（`crates/rpi-extensions/src/registry.rs:371`），
  `assert_active`（`registry.rs:948`）只读该标志。
- 回归实跑：`cargo test -p rpi-extensions --locked` → **104 passed / 0 failed**；
  `cargo test -p kiqi-extension-host --test e2e --locked goal_injection_carries` → 1 passed
  （前置条件见 R8）。

## 附带记录：`ask_user` 插件故障

本次审查中调用 `ask_user` 工具（`kiqi_ask_user`）时宿主报
`extension kiqi_ask_user panicked during call`，工具调用直接失败，无法向用户提问。
按 `4ad87ef` 的修法，wasm guest 现在能收到事件面了；此处失败发生在 `call` 路径，
与事件漏斗不同源。

**已定位并修复（`c42f95e`，发生在本记录之后）**：`CompiledToolAdapter::execute` 在
带 tokio 上下文的线程上同步调 guest，而 guest 的对话框轮询用 WASI 时钟
（`std::thread::sleep`），wasmtime 的同步等待走 `Handle::block_on`——在 tokio 线程上
（含 `spawn_blocking`）直接 panic：`Cannot start a runtime from within a runtime`，
外层 `ExtensionHost` 的 `catch_unwind` 把它包成上面那句
`panicked during call`。判据：`crates/kiqi-cli/src/extensions.rs:536` 的
`spawn_extension_call` 把调用移到普通 `std::thread`，回归测试
`extension_calls_run_outside_the_tokio_context`（`extensions.rs:1884`）断言驱动线程无 tokio
上下文；`cargo test -p kiqi-cli --lib extensions::tests::extension_calls_run_outside_the_tokio_context`
→ 1 passed。故这一项**不是待查**，本记录写的时候修复尚未落地。

## 建议动作

最小修复集 = R1 + R2，改完跑：

```bash
cargo test -p kiqi-extension-host --test e2e --locked goal_injection_carries
cargo test -p rpi-extensions --locked
cargo fmt --all -- --check && cargo check --workspace --all-targets --locked
```

R5（CHANGELOG 补一句）、R8（`build_guest` 的 target-dir）建议各自独立小 commit；
R3/R4/R7 可合并成一次「goal 提示的可读性与覆盖」清理。

---

## 处理结果（R1–R8 已闭环）

| # | 状态 | 落点与判据 |
|---|---|---|
| R1 | 已修 | `crates/kiqi-extension-host/tests/e2e.rs:3162` 反向分支改为 `ToolFaceSession { bash, read, edit }` + `session: Some(...)`：工具面非空但无 orchestrator。突变复验：把 guest 的 `present` 改成 `!tools.is_empty()` → 该测试**变红**（`no hint when the orchestration tools are absent`）。 |
| R2 | 已修 | `extensions/wasm/kiqi-goal/src/lib.rs:398-410`：`ORCHESTRATION_HINT` 与 `orchestration_hint()` 整体前移，三行 doc 重新挂在 `append_goal_custom_message`（`:411`）上方。 |
| R3 | 已修 | `lib.rs:414-420`：单次 `match` 返回 `(tag, hint_allowed)`，未知状态兜底为「中性 tag + 不加提示」；预算提示改名 `budget`，提示绑定改名 `nudge`。 |
| R4 | 已修 | e2e 在 `start` 后追加 `goal pause`，断言第二条注入含 `Paused goal` 且**不含**提示。突变复验：`"paused" => (..., true)` → 测试**变红**。 |
| R5 | 已修 | `CHANGELOG.md` 的 Unreleased/Fixed 条目补「`--no-extensions` 现在只关 cdylib 层」。判据：`crates/kiqi-cli/src/session.rs:374` 的 `install_compiled_event_sink(...)` 在 `no_extensions` 分支之后仍无条件执行。 |
| R6 | 不改 | 观察项：无 cdylib 安装的事件开销从零变为「构造 `StableExtensionEvent` 后空 handler 返回」，是修 bug 的必然代价。 |
| R7 | 已修 | `crates/kiqi-extensions/src/loader.rs:411-417`：`snapshot()` 的文档与 `snapshot_arc()` 对齐（本模块构造的会话恒为 `Some`，含 dylib-free 的 `none()`），理由指向 `snapshot_arc()`。 |
| R8 | 已修 | `crates/kiqi-extension-host/tests/e2e.rs:39-62`：`build_guest` 显式传 `--target-dir extensions/wasm/target`，返回路径由同一变量拼出，不再受本机 `build.target-dir` 影响。 |

`ask_user` 的 `panicked during call` 仍未修，保持待查。

### R8 的扩展：同一缺陷的另外 7 处

`cargo test --workspace --locked` 暴露 kiqi-cli 里 7 处「测试自建 guest」也写死了
`<root>/extensions/wasm/target/...`，默认 feature 下直接失败：

```
extension_cli::tests::{install_rejects_exec_flags_for_a_component_without_exec,
  install_and_uninstall_round_trip, insmod_and_rmmod_round_trip,
  declared_surfaces_include_the_guests_flags_and_shortcuts}
extensions::tests::{wasm_extensions_are_discovered_and_registered,
  wasm_non_tool_surfaces_reach_the_session_wiring,
  wasm_call_overhead_stays_within_the_compiled_paths_ballpark}
→ error: .../extensions/wasm/target/wasm32-wasip2/release/kiqi_conformance.wasm is not a file
```

改法：复用生产版 `crate::extension_cli::build_guest`（它从 `cargo build
--message-format=json` 的 artifact 事件取路径，本来就不猜 target-dir），
`extensions::tests` 加一个按 crate 目录取产物的薄包装。修复后 `cargo test --workspace
--locked` 全绿（kiqi-cli lib：584 passed / 1 ignored）。


门禁实跑（本机，`wasm32-wasip2` 已装）：

```bash
cargo test -p kiqi-extension-host --test e2e --locked goal_injection_carries
# → 1 passed（前置不再需要手工补编 guest，R8 已闭环）
cargo test -p kiqi-extensions --locked
# → 104 passed / 0 failed（+ 5 个集成测试文件全绿）
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
sh scripts/sync-embedded-docs.sh
```
