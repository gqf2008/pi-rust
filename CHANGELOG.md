# Changelog

All notable changes to the `kiqi-*` crate family are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The nine published crates share one workspace version and are released together
in dependency order. `git tag vX.Y.Z` marks the commit that a release was cut
from; the matching [GitHub Release](../../releases) carries the same notes.

## [Unreleased]

### Added

- **工作视图（`/work`），默认打开。** 在回显区右上角叠加「agent 正在做什么」：一个汇总
  盒子（`todo` 列表、正在运行的工具、LLM 回合与 token，流式时显示已接收字符数）和
  **每个子代理一个独立盒子**（任务名、状态、已用时间/预算进度条、当前工具、最近事件）。
  盒子靠右对齐、自上而下排列，按住正文即可用鼠标拖动，放下位置即最终位置；两者都没
  有内容时整个视图不占屏。`/work` 打开或关闭，Esc 中断后随 run 一起消失。盒子只在
  `--tui-mode fullscreen`（默认）下存在，`regular` 模式改为在工作块里给每个子代理列
  一行。子代理用 `ki -p --mode json` 运行，宿主逐行解析 `tool_execution_*` /
  `message_update` / `result` 事件。

### Added

- **`/mcp-reload`（别名 `/mcp`）。** 重新读取 `mcpServers` 配置并原地重连所有 MCP 服务器，
  然后用新连接重建工具表——改服务器配置不再需要重启 `ki`。管理器与通知 mailbox 保持不变，
  所以资源更新照常送达；命令会逐个列出连不上的服务器及原因，并显示当前的 MCP 工具数。
  实现上是把 `servers` / `configured_names` 换成可替换的 `RwLock`，新增
  `McpManager::reload`（新连接就位后才关旧的，进程树照常回收）。
- **输入历史跨会话保留。** 之前 ↑/↓ 只能翻当前进程里提交过的消息，`ki` 一退出记忆就没了。
  现在每条提交的消息（含多行输入）追加到 `<agent_dir>/input-history.jsonl`，启动时读回
  最近 100 条，连续重复只留一条；↑/↓ 的翻找规则不变（输入框为空或已在翻历史时才取用，
  翻回最新一条恢复草稿）。

### Changed

- **`/agents` 查看与停止后台子代理。** 无参数列出正在跑的后台代理，`/agents kill <id|all>`
  停止它们（会话退出时也会统一杀掉）。
- **`subagent` 改为后台执行。** 之前它是 wasm 扩展里的阻塞调用：一个扩展同一时刻只执行一个
  调用，所以一批 `subagent` 严格串行，主会话被关在一个工具调用里直到最慢的子代理结束。
  现在工具由宿主实现，`execute` 起完子进程就返回（`Started background subagent#N`），
  解析子进程 JSONL 的监督任务把进度写进工作视图，子代理结束时结果作为 follow-up 用户
  消息回到会话（空闲起新轮、忙则排队）。`plan`+`dispatch` 仍是阻塞 wave。
- **工作盒子铺毛玻璃底色。** 盒子（汇总盒子与每个子代理盒子）整块铺主题的 `surface`
  底色与配对的 `surface_text` 前景；正文里强调色的重置之后会重新铺底，所以盒子在回显区
  上读起来是一层浮起的面板。终端没有 alpha 通道，真透出会把两层文字绞在一起，这是
  「半透明感」能做到的程度。
- **排队中的调用会被点名。** 一个 wasm 扩展同一时刻只执行一个调用，并行批里还没进 guest
  的 `dispatch` 没有自己的盒子；汇总盒子现在列出
  `queued · waiting for a free extension call` 与每个排队调用的任务摘要。（`subagent`
  已经不是扩展调用了——它是宿主侧的后台代理，见下一条。）

### Fixed

- **工作盒子不再吞掉文本选择。** 工作视图默认打开后，普通按下的第一站是「移动盒子」，
  汇总盒子又横贯屏幕上沿——最近输出的选择/复制基本必失。现在盒子只在 `Alt`/`Ctrl`
  按住时才移动（汇总盒子标题里写了提示），普通拖动恢复为选择并在松开时自动复制。

- **子代理盒子始终显示它自己说的话。** 之前只有事件流为空时才显示流式文本，跑过一次工具
  之后那块就只剩 `▶ bash`；现在事件与文本并列显示，`regular` 模式的工作块也多一行最近的
  文本——transcript 不再回显进度行之后，盒子是这些内容唯一的实时视图。
- **进度行过滤只在真需要时复制。** `without_node_rows` 返回 `Cow`：没有节点行的结果原样
  借用（bash 每 2 秒刷几十 KB、图片结果是几 MB base64 时不再整块克隆）；解析与吸收共用
  同一个 `node_row`（要求字符串 `id`），工具结束时也先吸收再过滤，只剩节点行的空块直接
  丢弃而不是留一个空行。
- **`/goal <任务>` 真的立刻开跑。** 自动开跑的判定是"命令跑完之后是否出现了一个新
  goal"，但快照取在命令返回之后——`/goal` 在命令内部就把 goals.json 写好了，于是"之前"
  和"之后"永远是同一个 key，判定恒为 false，目标只能等用户再发一句话才动。现在快照在
  命令执行前取（随 `ExtensionCommandDone` 带回来）。pty 实测：`/goal 把测试跑绿` 之后
  立刻产生 provider 请求与 `[ki goal] auto-continue` 日志。
- **子代理进度不再写进 transcript。** 节点行是工作视图的数据，宿主却把原始协议 JSON
  （`{"budgetMs":…,"id":"a",…}`）同时渲染进 `● dispatch` 工具块，同一份信息出现两次，
  而且是不可读的那一份。现在工具块只保留工具自己的文本，进度只以盒子呈现。

- **工作视图的一批审查修复。** 汇总盒子顶边框按字节数算填充，比盒身短 5 格；窄终端下
  子代理盒子被截断右边框；两个盒子重叠时按下命中的是视觉上在下面那个；拖到屏幕边缘外
  再回拉有一段死区；漏掉鼠标抬起后，下一次拖动会串到上一次的盒子；`/work` 即使没有内容
  也常驻浮层，压掉了 `↓ Jump to latest message` 提示；节点 id 复用时继承上一轮的拖动
  位置；`done` 的盒子用时还在继续涨；`dispatch` 被取消后盒子停在 running。
- **节点行的合并语义。** `tool: null` 与 `events: []` 是扩展给出的权威值（工具已结束、
  这是新一轮的事件），之前被当成"这轮没上报"而保留旧值——盒子会一直挂着 `tool bash`。
- **并行的 `subagent` 调用各占一块盒子。** guest 用固定 id `subagent` 上报每次调用，
  宿主按 id 合并——两个同时在跑的调用被并成一个盒子，用时还被后开始的那次清零。现在
  每次调用各自拿 id（`subagent#1`…），并补齐 `task` / `events`。
- **`/goal <任务>` 立刻开始干活。** 之前该命令只写 `.ki/goals.json`，而自动续跑挂在
  `AgentEnd` 上——空会话没有 run 可结束，于是目标建好后必须用户再说一句话才会动。
  现在命令结束后若出现新 goal（或 resume 了旧 goal）且会话空闲，就直接起一次续跑；
  `show`/`list`/`update` 这类记账命令保持被动。显式 `/goal` 同时解除 Esc 中断后的
  续跑静音（`muted_until_input`），并给新目标一份全新的自动续跑预算。
- **自动续跑到达上限时会说话。** 无预算 goal 的 20 次上限原先只写诊断日志，界面上
  表现为「目标莫名停住」。现在会在对话里说明已到上限，并提示 `/goal resume` 继续或
  `/goal stop` 结束旧目标再开新的（新目标有自己的预算）。

- **`plan` / `dispatch` 的进度变成节点行。** `kiqi-orchestrator` 以
  `{"ki":"node","id","state","elapsedMs","budgetMs","tool","text","task","events"}`
  上报每个节点（含 `subagent` 单次调用），宿主合并到同一行并画成工作视图里的盒子；
  工具输出流另发 `▶ id` / `✔ id` 进度行。没有交互 UI 时（headless、单测）上报被忽略，
  wave 行为不变。

### Fixed

- **盒子的标题行是拖动把手。** 在盒子**标题行**上按住拖动 = 移动盒子（不需要修饰键），
  盒子**正文**拖动 = 文本选择；`Alt`/`Ctrl` 按住整块盒子仍可移动。汇总盒子的标题提示改为
  `drag title to move`。
- **点击不再等于选择。** 选择态只由**拖动**进入：左键按下只是预备，画面不画任何选中
  （之前按下就反显一格空字符，看起来像一个黑光标），也没有复制；真正移动后才开始反显，
  松开保留、右键复制并退出选择态。单击、输入、滚轮在任何时候都保持普通输入/滚动行为。
- **右键复制后选中消失。** 左键拖选留着反显，右键把这段再复制一次（并显示 `copied N
  char(s)`），然后清掉高亮——选区用完就不再占屏。左键释放时的自动复制保持原样
  （`fullscreenCopyOnSelect: false` 可关）。Windows 上原本的右键粘贴保留：没有选区时
  仍按粘贴处理。
- **拖选有了可见高亮。** 复制一直是成功的，但画面上没有任何选中效果——拖完看上去像
  「没选中」。现在按下即开始绘制，拖动实时跟随，松开后保留到下一次按键；选中的格子以
  反显呈现（`Alt`/`Ctrl` 拖动移动盒子的手势不受影响）。
- **选择复制覆盖整个屏幕。** 之前全屏模式下拖选只从 chat 容器重新渲染（还固定按 80 列），
  输入框、状态行选不到，列也常常对不上。现在 renderer 每帧把真正画出的网格（去掉光标
  标记）留一份，拖选直接从这份网格按显示列切片——回显区、状态行、输入框都能选，松开
  自动复制；`regular` 模式没有合成层，仍用终端自身的原生选择。
- **库里的诊断不再写进输入区。** TUI 拥有整个屏幕，任何裸 `eprintln!`（MCP 库的
  `[ki mcp] server …: resources/list failed`、provider 适配器告警、panic 信息）都会落在
  光标所在的位置——`/mcp-reload` 之后那条 MCP 诊断就压在了输入框上。现在进入交互 TUI 时
  把进程 stderr 重定向到 `<agent_dir>/logs/ki.log`，退出时恢复；`-p` 无头模式不受影响，
  仍然照常写 stderr。pty 实测：屏幕上 `[ki mcp]` 出现 0 次，同样的行出现在日志里。
- **历史里的「半个工具调用」不会再让请求 400。** `Messages with role 'tool' must be a
  response to a preceding message with 'tool_calls'` 与反方向的 `An assistant message with
  'tool_calls' must be followed by tool messages…` 是同一件事的两面：持久化历史里出现了
  没有轮次的结果、或没有结果的轮次。现在组装 provider 上下文时**双向兜底**（丢弃无主
  结果；给缺结果的轮次补一条标注为「记录前中断」的错误结果），`repair_dangling_tool_calls`
  只看当前分支（此前扫全库：别的分支上同 id 的结果会让真正悬空的调用被漏修，也会把别的
  分支的调用补一条结果到当前分支——孤立结果的另一个来源），settle 在助手消息没落盘时
  也不再提交它的工具结果。运行期重试认这两种措辞。

## [0.4.5] - 2026-10-28

### Fixed

- **`ask_user` 不再被 60 秒 TTL 替人作答。** 宿主曾给所有提问加 60 秒上限：超时后
  guest 返回 `unanswered` + `keep` 并让本轮结束，而 TUI 的选择框还开着，用户随后
  点击的答案被送进已取消的请求并静默丢弃——表现为「选完了 ki 不动，要再说一句话」。
  现在没有显式 `timeout` 的问题会一直等人工回答；请求因超时/取消/abort 失效时 TUI
  主动关窗并在 transcript 说明，迟到的回答不再被吞。同一批修复：`confirm` 形态保留
  自己的 `timeout`、超时前先取一次答案（消除临界竞态）、超大 `timeout` 不再溢出
  trap、超时后回收 mailbox 条目、轮询 25→250ms 退避、RPC abort 能真正取消 guest。
  goal 自动续跑按既有约定不等人工：直接取消提问并说明。
- **ACP 客户端提问随扩展关闭。** 扩展停止等待时取消客户端的
  `request_permission`（协议层自动发 `$/cancel_request`），不再留下一个答案无处可送
  的权限弹窗。

## [0.4.4] - 2026-10-28

### Changed

- **多模态输入不再由 ki 做能力门禁。** Anthropic 与 OpenAI Responses 的用户文本、
  图片及工具图片按原内容发送；删除不支持图片时的占位降级，兼容性错误由
  provider 返回。`input` 仅保留为 models.json 兼容元数据，配置向导不再询问
  图片支持，模型列表不再显示误导性的 images 列。新增测试覆盖工具图片原样
  转发和文本/图片顺序保持。

## [0.4.3] - 2026-10-28

### Fixed

- **A minimal custom-model configuration now works end to end.** With no saved
  model choice, the first authenticated `models.json` model wins over unrelated
  built-in provider credentials, so a configured provider/model/key needs no
  `defaultProvider`, `defaultModel`, or extra environment variables.
  `contextWindow` and `maxTokens` remain optional: Anthropic Messages fills its
  required `max_tokens` with a usable default instead of the previous one-token
  floor (which produced empty replies), while OpenAI Responses omits an unset
  `max_output_tokens` and lets the server choose.
- **Goal `doneWhenCmd` uses the real shell backend on Windows.** It now shares
  the bash tool's Git-Bash discovery and legacy-WSL transport instead of trying
  to execute `/bin/bash` or `/bin/sh`. The CLI's Windows-only path assumptions
  in the extension-collision and permissions tests are fixed as well.

## [0.4.2] - 2026-10-27

### Changed

- **`/goal` 直接启动。** 删除 `/goal start` 子命令：`/goal 任务：把卡清干净`
  或 `/goal 把卡清干净` 立即建立目标；`pause` / `resume` / `stop` 保留。
  模型侧 `goal` 工具的 `start` action 不变。新增原生解析测试和宿主 e2e，
  覆盖直接启动、标题剥离和完整生命周期。

- **The extension rename is complete (breaking).** Every surface now says extension. The WIT
  package is `kiqi:extension@0.9.0` (`world extension`, `extension-api`), the
  repo tree is `extensions/` (`extensions/wasm/kiqi-*`), the Rust API
  speaks `Extension` throughout (`StableExtensionEvent`, `ExtensionApi`,
  `ExtensionError`, `WasmExtension`, bindings crate `kiqi-wasm-bindings`), the
  CLI is `ki extension …`, the install directory is
  `$KI_AGENT_DIR/extensions/<id>/` and the component file is `extension.wasm`.
  Host identity and the environment-variable constants moved to
  `kiqi-agent::env` (the crate every consumer already depends on), so
  `kiqi-tools` and `kiqi-mcp` no longer depend on `kiqi-host` at all.
  The guest directories and packages drop their `-guest` suffix too
  (`extensions/wasm/kiqi-todo/`, crate `kiqi-todo`); the shared bindings crate
  is `kiqi-wasm-bindings`. Machine-readable values that carried the old word
  changed with it — notably `ki auth check --json` now reports
  `"source": "extension-oauth"`.
  No compatibility shims: installed guests must be re-installed with
  `ki insmod`. Notes in `docs/design/extension-abi.md` §6.48.
- **The host-side crates are merged.** The three former host crates (contract,
  wasm backend, dispatch spine) are now the single `kiqi-host` crate
  (`docs/design/kiqi-host-merge.md`), breaking for Rust consumers. The contract
  trait is `Extension`; the wasmtime dependencies sit behind the crate's `wasm`
  feature, which only `kiqi-cli` enables, so `kiqi-tools` / `kiqi-mcp` /
  `kiqi-acp` stay wasmtime-free. Code paths: `kiqi_host::` for the contract and
  spine, `kiqi_host::wasm::` for the backend.
- **Extension events are per-session.** The compiled/wasm event sink now lives on
  the session's `RegistrySnapshot`: `install_compiled_event_sink` returns the
  sink it installed, `ExtensionSession::none_with_compiled_sink` mounts it, and
  every `translate` funnel prefers `snapshot.compiled_sink()`. Two sessions in
  one process — the ACP multi-session server — therefore dispatch through their
  own session's caps instead of the last-installed session's; the process-global
  sink stays as the fallback for call sites without a snapshot. Honest boundary:
  the wasm **host instances are still process-global**, so this isolates which
  caps a dispatch uses, not which guests can observe an event. Notes in
  `docs/design/extension-abi.md` §6.50.
- **Session capabilities are per-session too (additive for `kiqi-host`; the old constructors stay).**
  `RegistrySnapshot` now carries `Option<Arc<dyn SessionCaps>>` alongside the
  sink (`with_session_caps` / `session_caps`), and `ExtensionSession::none_with_compiled_sink_and_caps`
  mounts both. The CLI's owned bundle implements the new `SessionCaps` trait, so
  `kiqi-host` never learns the concrete type. Probes, extension commands and
  prompt transforms resolve the **current session's** caps from the snapshot
  instead of the process-global last-writer-wins slot; that slot remains only as
  the fallback for call sites with no session. Consequence: in the ACP
  multi-session server, a guest in session A can no longer cancel, inject into
  or set the status/editor text of session B. Single-session behaviour is
  unchanged; a poisoned session cell is recovered rather than reaching another
  session's caps. Notes in `docs/design/extension-abi.md` §6.51.

- **`EditorChange` now reaches wasm guests.** It used to be short-circuited by
  the native-handler pre-check (always empty in a wasm-only install), so guests
  never saw it; it is now delivered on every changed draft — an intentional
  behaviour change with one guest observe-call per keystroke.

- **Skill bodies load on demand.** Discovery now parses frontmatter only
  (`Skill::content` is empty for discovered skills), so a large skills directory
  costs no body reads at startup or `/reload`. `AgentLane::skill` and
  `format_skill_invocation` share `kiqi_harness::skills::load_skill_body`; `/skill`
  and the `skill` tool do the equivalent fresh read through their filesystem
  environment. The formatter is now fallible, and an unreadable discovered file
  is an error instead of an empty invocation. Nonempty in-memory bodies remain
  authoritative. Notes in `docs/design/extension-abi.md` §6.53.

### Removed

- **The C-ABI registration machine is gone; the event bus is pure serde
  (breaking).** The unreachable native (cdylib) registry layer left behind by
  the channel retirement was removed: `HostApi` and its thread-local, every
  `trampoline_register_*`, `ExtensionToolAdapter`/`ExtensionToolHandle`,
  `PluggableProvider`, native OAuth/prompt-transform/resources handlers, and the
  whole `kiqi_host::abi` module (`StbString`, `StableExtensionEvent`,
  `EventView`, `ExtensionApi`, handler constants). Events now cross the
  compiled/wasm sink as the plain serde `Event` (tag + JSON payload), with the
  dispatch funnels building it directly. `RegistrySnapshot` keeps the session
  staleness flag (no longer consulted by any dispatch path — see §6.50) and
  gains the per-session sink; `EventTag`/`Event`/`EventAction`, the compiled sink,
  `translate` funnels, mailboxes, `ToolCallContext` and `stdio` remain. Notes in
  `docs/design/extension-abi.md` §6.49.
- **The cdylib extension channel is retired (breaking).** wasm guests
  (`extensions/wasm/kiqi-*`, `ki insmod`) are the only extension channel now.
  Gone: the `kiqi-extension-sdk` crate (C ABI types), `examples/extension-stub`, the
  `libloading` loader inside `kiqi-extensions`, `ki dev` / `ki dev-local` (hot
  reload), `ki install` / `ki uninstall` (crates.io cdylib extensions) and
  `ki package link|unlink`. The native-package update checks went with them.
  Anything declaring `#[repr(C)]` handler vtables no longer loads; port the
  extension to a wasm guest against `extensions/kiqi-extension.wit`. `kiqi-extensions`
  survives as the host-side dispatch spine (registry, `translate` funnels,
  compiled/wasm sink, action mailboxes); its C-ABI bus types moved to
  `kiqi-extensions::abi` and the shared contract constants live in
  `kiqi-extension`.
- **A component built against another ABI version gets a clear error.** When a
  guest's imports say `kiqi:extension@X` and the host speaks
  `kiqi:extension@0.9.0`, loading now fails with both versions named and a
  rebuild hint instead of a raw component-model linker error.
- **`ki extension rename <old> <new>`.** Moving an install to a new id no
  longer means reinstalling and copying grants by hand: the command renames the
  install directory (grants live inside it, so they travel), re-keys the
  extension's stored OAuth tokens, and refuses an occupied target.
- **`kiqi-plan-mode-core` is gone — its logic lives in the guest.** The crate
  existed only to make plan-mode logic unit-testable, on the premise that wasm
  guests cannot carry tests; that premise is false (wit-bindgen generates host
  stubs, so `cargo test -p kiqi-plan-mode` runs natively). The logic and
  its six tests now live in `extensions/wasm/kiqi-plan-mode/src/plan.rs`,
  with the WIT adapter as `src/lib.rs`.
- **`kiqi-acp` is a crate now.** The ACP server moved out of `kiqi-cli`
  (`crates/kiqi-cli/src/acp/`) into `crates/kiqi-acp`; the host keeps the
  assembly roots (`SessionFactory`, `session/load` pre-check) in
  `crates/kiqi-cli/src/acp.rs` and injects them through `RunOptions`. The MCP
  boundary glue moved to `kiqi-mcp::boundary` (shared by both).

### Added

- **Progressive skill loading.** `/skill <name>` loads one skill's instructions
  into the conversation (`/skill` alone lists them; trailing text becomes
  additional instructions), and the new `skill` tool gives the model the same
  on-demand load. The system prompt still carries only name/description/location;
  both paths re-read the body from disk, and the tool's reply appends a one-level
  listing of the skill directory so referenced files (scripts, references) are
  read by name. Skills with `disableModelInvocation` are refused to the model and
  reachable only through `/skill`.

- **Session-scoped scheduled jobs.** `.ki/cron.json` in the session working directory can
  hold `prompt` or `command` jobs on a 5-field cron schedule (plus `@hourly`/`@daily`/
  `@weekly`/`@monthly`). They tick only while an interactive session is open and are
  delivered to the agent as an injected user message (`⏰ cron:<name> …`); missed minutes
  are skipped rather than backfilled, and a command job never overlaps itself. Commands
  run through the platform shell in the session cwd with merged stdout/stderr, a 60 s
  default timeout (max 600 s) and a 64 KiB output cap. Host-side only: no extension ABI or
  WIT change.
- **Spawned MCP servers are told the session id.** A stdio server used to be
  started with a scrubbed environment, so it could never answer "whose write was
  this?" — `walgit mcp` reads its own `KI_AGENT_ID` to publish
  `_meta.self_session`, and without it every `resources/updated` notification
  looked like a peer's. The id is now forwarded explicitly (through the SDK's
  `AGENT_ID_ENV` constant, not the ambient allowlist); provider keys and
  `KI_SESSION_ID` stay out. An HTTP MCP server has no environment, so it must
  carry the id in its configured `headers`.
- **`KI_AGENT_ID` is published to shell commands.** Commands run by the
  `bash`/`powershell` tool now see `KI_AGENT_ID`, the same value as
  `KI_SESSION_ID`, so they can name the session they belong to — e.g.
  `walgit collab claim --session "$KI_AGENT_ID"`. `KI_SESSION_ID` itself remains
  host-private and is still stripped from spawned shells (a nested `ki` would
  otherwise adopt an ancestor's session).

### Changed

- **BREAKING (wasm ABI 0.8.0): every installed guest must be rebuilt.** The package
  version *is* the ABI version, so a 0.7.1 component fails instantiation. Two existing
  signatures changed — `run-command` returns `command-result` and
  `session-read.cli-flag` returns `option<flag-value>` (a bare `--foo` is now `bare`,
  no longer a dropped JSON bool) — and two closed variants gained a case
  (`event.input`, `event-action.patch`). All 12 in-repo guests are rebuilt in this
  release; no third-party wasm ecosystem exists to migrate. The world also gains the
  exports `oauth-api`, `provider-api` and `render-api`, which every guest must
  implement (`kiqi_wasm_bindings::deny_contribution_exports!` supplies empty/denied
  stubs), and the imports `oauth-http`, `provider-stream` and `provider-http`.

  Five capabilities arrive **deny-by-default**, each carrying its policy in the grant:
  `oauth`, `provider`, `render`, `resources` and `provider-request`. None is
  auto-granted by `--yes` (`AUTO_GRANT_EXEMPT`): an install lists them and stops, and
  the user grants explicitly with `ki extension grant <id> <capability> …`. A missing
  grants file still grants nothing, and a hand-edited file fails closed — a key that
  fails validation is dropped while the others survive.

  Behaviour changes worth knowing before upgrading: OAuth tokens never cross the wasm
  boundary (the host performs the token-endpoint HTTP and stores
  `extension:<install-id>:<provider-id>`; a guest sees only `oauth-credential-status`).
  Stored tokens are bound to a fingerprint of the provider id, client id and endpoint
  origins, so refresh/logout/revoke refuse a changed issuing profile. Logout requires
  the explicit `--force-local` path to delete locally after a mismatch, and typed
  deletion never removes an `ApiKey`. Auth-store mutations hold a process mutex and
  `auth.json.lock` across their locked re-read and atomic replace.

  The provider runtime is wired end to end. `WasmProvider` joins resolve and `/model`,
  instantiates its dedicated `ProviderHost` lazily on the first request, and revalidates
  the grant on every request, so revocation stops the next call. `provider-http`
  enforces the endpoint before connecting, rejects encoded separators/dots, dot
  segments and redirects, strips guest credential/framing headers, injects only the
  host-held credential after a fresh fingerprint check, and reaps per-call handles.
  `provider-stream` is restricted to provider calls and bounded at 16,384 deltas,
  1,024 blocks and 16 MiB; the CLI's 256-entry live queue cancels on overflow, while
  its `select!` forwards a host timeout as a terminal stream error without waiting for
  an abandoned guest sender.

  Markdown rendering is finalize-only on a deny-all instance. Both instantiation and
  transformation are bounded; transformation uses a detached worker and a wall-clock
  deadline (epoch interruption still covers pure-CPU spins). The
  `before-provider-request` observation is redacted unless `provider-request --observe`
  is granted (`apiKey` becomes `apiKeyPresent`, messages are dropped by default). Even
  with `--headers values`, only `traceparent`, `tracestate`, `x-request-id`,
  `x-correlation-id` and `x-trace` values cross the ABI. The Observe partition applies
  only to this event, and
  each patch gate uses a dedicated instance so a guest parked in a host import cannot
  starve shared probes or tool calls.

  Consent and residual-risk behavior is explicit: a resources manifest never grants
  `resources`, including on reinstall or with `--yes`; use `ki extension grant`. Granting
  `oauth`/`provider` alongside existing `exec`/`fs`/`net-sockets` emits the implemented
  O17 warning but does not refuse the grant.

  Boundaries, stated rather than implied: built-in providers still resolve stored API
  keys only (`stored_api_key` does not automatically consume an OAuth entry), whereas a
  wasm provider can use an OAuth access token through an explicit, fingerprint-checked
  `--credential-from` grant. The cdylib OAuth/provider/renderer/resources registrations
  are superseded but their P6 deletion is still pending. The process-level compiled
  event sink used by legacy extensions also remains last-writer-wins across session builds.

- **MCP notices can suppress a session's own collab echoes.** Collab entries
  now carry the writing session (from `KI_AGENT_ID`), and the client keeps the
  *raw* session list a notification reports (`_meta.sessions`) to compare it at
  decision time against the session `ki` is serving *then* — a server's
  `self_session` is only the spawn-time snapshot it inherited, so comparing
  against that stops suppressing as soon as you switch or fork. When every
  changed entry came from this session, the notice is recorded without spending
  a turn. A `--no-session` run publishes the literal `ephemeral`, which is not
  unique and never counts as an identity; entries written before sessions
  existed, or servers that do not report them, keep the old behavior.

  **Not yet effective end to end (verified 2026-10-21):** the write side works
  (every entry carries `session`), but `walgit mcp` does not yet put
  `sessions` in the `_meta` of the resources it reports — measured as
  `_meta: {"version": "…"}` for both `refs/` and `collab/board/`, so the
  suppression never fires and a session still wakes on its own writes. The
  consumer side is complete and pinned; the remaining change belongs to the
  `walgit` repository. Revisit this entry when that side ships the field.
- **Per-agent collaboration setup.** `scripts/setup-collab-agents.sh [n]`
  creates one worktree per agent and joins it as its own walgit principal
  (`ki-agent-1..n`), so entries are attributable and claim state is isolated;
  `--dry-run` prints the plan. The one-worktree-per-agent layout is also the
  prerequisite for suppressing a session's own notification echoes.
- **Goals can be finished by a host-run command (`doneWhenCmd`), and headless
  modes can auto-continue them.** `doneWhenCmd` is evaluated at the AgentEnd
  boundary with a short timeout under the same shell sandbox as the bash tool;
  exit 0 stops auto-continue (timeout, spawn failure or a non-zero exit does
  not). The goal JSON gains `done_when_cmd` and `goal start` accepts
  `doneWhenCmd`. With an active goal, `--server`/RPC, `-p` and `--mode json`
  now keep running until the done condition or a budget stops them instead of
  exiting after one prompt (unbounded goals are still capped at
  `MAX_GOAL_AUTO_RUNS` host runs).
- **The compiled-in extension channel is retired (2026-10-21).** The 12
  `extensions/kiqi-*` crates, their `extension-kiqi-*` features, the opt-in-feature
  and standalone-cdylib CI steps, and `scripts/check-feature-parity.sh` are
  gone; the website no longer advertises a `cargo build -p kiqi-permissions`
  install for a host-owned policy. First-party extensions are wasm guests only
  (`ki insmod --build extensions/wasm/kiqi-*`). The
  `kiqi_extension::Extension` trait and `ki dev` cdylib hot-load remain for
  third-party extensions.
- **The TUI now defaults to the `light` theme.** With no saved `theme`, ki
  starts in the light palette; `dark`/`monochrome`/`claude` remain one
  `/theme` away, and an unknown/custom name falls back to light instead of
  silently keeping the previous palette.
- **A `claude` theme preset matches Claude Code's palette.** `/theme claude`
  (or `"theme": "claude"` in settings) uses Claude's orange accent
  (`#d97757`), neutral gray borders and blue links. It deliberately leaves
  page, code-block and tool backgrounds at the terminal default, so it adapts
  to both light and dark terminals instead of painting near-black blocks.
- **The declared minimum Rust version is now 1.96** (`rust-version` in
  `[workspace.package]`), up from a 1.78 that the workspace could no longer build
  against. `wasmtime 49` (via `kiqi-extension-host`) pulls
  `cranelift-assembler-x64 0.136.2`, which itself requires 1.96.0; cargo refuses to
  resolve the workspace below that. Measured, not assumed: 1.94 fails with
  "not supported by the following packages", while 1.96 and 1.98 both build the
  whole workspace. CI now has an `msrv` job that reads the declared value and builds
  against exactly that toolchain, so the number cannot drift from reality unnoticed
  again — which is what had happened here.

### Fixed

- **ACP sessions no longer leak the whole harness when they close.** The
  compiled/wasm capabilities the session's `RegistrySnapshot` carries reach the
  `HarnessActionHost` (`snapshot → sink → caps → host`), and the harness reaches
  the snapshot through its `before_tool_call` hook; the host used to hold a
  strong `Arc<AgentHarness>`, closing that chain into an `Arc` cycle. A single
  session only leaked until process exit, but the ACP multi-session server leaked
  one `AgentHarness` (and its session/storage handles) per opened-and-closed
  session. The host now stores a `Weak` and the session owner (the TUI's
  `AgentSession` / the ACP `LiveSession`) keeps the strong `Arc`, so runtime
  actions still resolve the live harness for the session's lifetime and fail
  loudly with the existing "not ready" error afterwards. Notes in
  `docs/design/extension-abi.md` §6.52.

- **A queued MCP notice no longer starves auto-wake.** While a run was active,
  an MCP resource notice was queued as a steering message; if the run ended
  before the loop consumed it, the notice stayed in the queue.
  `queued_messages()` reported every queued entry as a user message (a custom
  notice renders as empty text), so the wake policy read "user messages are
  queued" and skipped every auto-wake — and no run was left to consume the
  queue. One overnight session accumulated 68 such entries and printed the
  skip line for every new signal. Queue snapshots now report only actual user
  turns (custom entries stay queued for the next run), so an empty user queue
  wakes normally; `clear_queue` likewise returns only user text. A unit test
  pins the notice-only queue.
- **The main-screen TUI no longer corrupts itself after a resize or while the
  terminal's scrollback is being browsed.** Main-screen mode (the macOS
  default) hands whole frames to the terminal, and its incremental diff used a
  screen-relative scroll (`CSI T` after `CSI H`) to rewrite rows that had
  already moved above the viewport. In a real terminal those rows live in the
  scrollback and cannot be addressed: the scroll rearranges whatever the user
  is looking at, so the patched tail landed in older rows at arbitrary columns
  (the recurring fragments-at-odd-columns screen). A row wider than the
  terminal — this path had no width guard — shifted every row below it and
  desynchronised the diff for the rest of the session. A resize cleared the
  scrollback (`CSI 3 J`) before rewriting the whole document, so a write that
  did not land left a blank screen with nothing but the dock. The renderer now
  wraps every row to the width before recording it, clamps in-place updates to
  the visible window (scrollback is left alone; it was painted when it was
  visible), and on resize repaints only the visible window without clearing
  scrollback. Two regression tests pin the over-wide row and the
  window-after-streamed-rewrite contract; verified in otty 1.6.2 at 145×50 and
  57×50 with the window, dock and scrollback intact.
- **The signal skip line now says whether the server attributed the change.**
  The self-echo decision compares a notice's `_meta.sessions` against the session
  `ki` is serving, and both ways it can decline to suppress ended at the same
  text (`signal recorded without wake: …`): a server that reports no provenance
  at all, and one whose reported sessions are a peer's. Telling those apart took
  an external MCP probe the first time it mattered, because from inside the log
  the two are identical. The line now carries `attributed=<n>/<total>`, and a
  test pins the three cases (none, all, mixed) so it cannot silently become a
  constant.
- **Tests that read a `KI_*` variable no longer race the code that writes it.**
  The crate-wide env lock was test-only: 33 test sites took
  `config::test_support::env_lock` while `set_active_session` and
  `publish_session_identity` rewrote `KI_SESSION_ID`/`KI_AGENT_ID` from whatever
  thread reached them, so a test that snapshotted the id could observe another
  test's session. Measured on the previous commit: 15 of 40 runs of
  `extensions_actions::tests` failed on 16 threads, 0 of 20 with one thread.
  Worse, the failing assertion panicked *while holding* the lock, so its
  `.lock().unwrap()` call sites turned the one real failure into 93
  `PoisonError`s in unrelated tests. There is now a single
  `crate::env_lock::EnvLock` that the tests and both production writers share;
  it is re-entrant per thread (a test may call locked product code from inside)
  and it recovers from poisoning instead of cascading. Removing the lock from
  `set_active_session` reproduces the flake at 7/20; removing it from
  `publish_session_identity` does not, which is why both are annotated with what
  the mutation showed.
- **Documented examples named extension crates that no longer exist.** Eight files
  (the website, the user-facing authoring and debugging guides, and their
  embedded copies) showed `ki dev --package kiqi-todo`, `ki dev -P kiqi-webfetch`
  and `ki dev --package kiqi-todo -- --model gateway/model`; those crates were
  deleted with the compile-time channel, so the command answers "package not
  found". They now use `my-ext-a` / `my-ext-b` and say the names are
  placeholders. `crates/kiqi-cli/src/extensions.rs` cited
  `extensions/kiqi-goal/src/lib.rs:134-138` for the goal's `current_dir()` write and
  now names the wasm guest that actually implements it;
  `docs/design/extension-abi-handover.md` still called the first-party channel
  "compile-time" and claimed the retired crates, features and cdylib builds were
  "all kept". `scripts/tests/docs-install-path.sh` now rejects a
  `ki dev --package kiqi-<name>` example and any source comment citing
  `extensions/kiqi-<name>/src` (design docs keep theirs as historical record).
- **The Packages page recommended commands that cannot run.** Its hero showed
  `cargo add kiqi-tools` and `ki install kiqi-ask-user` — the first cannot
  resolve (`publish = false`), the second names an extension crate that was deleted
  with the compile-time channel — next to the claim that everything is
  published on crates.io and a count of "9 core crates and 20 installable
  extensions" for a 28-entry catalog. the old static user-guide pages and the authoring guide carried the same
  `ki install kiqi-extension-example` example. The hero now shows
  `cargo install --path crates/kiqi-cli` and
  `ki insmod --build extensions/wasm/kiqi-ask-user`, the counts are gone
  (the card list already computes them), and every page says plainly that
  nothing is published. `scripts/tests/docs-install-path.sh` now covers
  the old static package and user-guide pages in addition to `packages.json`, and requires the Packages page to show
  at least one command that runs.
- **The README described an extension channel that no longer exists.** Its Extensions
  section still read "Extensions are Rust `cdylib` libraries" and its only install
  example was `ki install kiqi-todo  # build from crates.io and install` — a
  crate that was never published and was deleted with the compile-time channel,
  so the first command a reader copies fails before they learn that anything
  runs. The section now leads with the wasm channel
  (`ki insmod --build extensions/wasm/kiqi-<name>`, without an online package catalog) and keeps the cdylib
  channel for third-party extensions with a `--path` install; the same crates.io
  example was dropped from the user guides and the crate README.
  `scripts/tests/docs-install-path.sh` now rejects `ki install kiqi-<name>`
  without `--path` in the install docs and requires the READMEs to show the
  wasm install command.
- **Runtime diagnostics no longer paint over the TUI.** MCP notifications
  arriving while a session was muted logged `[ki mcp] signal recorded without
  wake` with `eprintln!`; each raw stderr line landed at the renderer's cursor
  and shifted the transcript, footer and input box, producing a staircase of
  repeated lines. Signal and goal-continuation diagnostics now append to
  `<agent_dir>/logs/ki.log` instead. A source guard keeps `eprintln!` out of
  the interactive TUI runtime paths.
- **A wide transcript row can no longer shift the input area.** Rendering now
  wraps every transcript row to the viewport width at the scroll-view
  boundary (wide CJK graphemes, long unbroken tokens, ANSI styling and OSC-8
  hyperlinks included). Previously a component that under-measured one row let
  the terminal auto-wrap it, which advanced the cursor one extra line and moved
  the docked editor and footer down.
- **On macOS a shell command can no longer rewrite the permission policy after
  it returns.** The lexical guard and the post-call restore could not stop
  `some-daemon &` from changing `.ki/permissions.json` after the last tool call
  of a session. Every `bash` child tree now runs under a `sandbox-exec` profile
  that allows everything by default but denies writes to the policy files and
  their directories; workspace builds, git and ordinary commands are
  unaffected. `KI_SHELL_SANDBOX=0` opts out, and machines without
  `sandbox-exec` fall back to the previous two layers. A startup probe detects
  an already-active sandbox (a nested `sandbox-exec` cannot apply a profile and
  fails with EPERM), so a `ki` started from inside ki's bash inherits the outer
  confinement instead of failing every command. Linux Landlock remains a
  follow-up (`docs/design/policy-sandbox.md`).
- **Goal auto-continue stops when the board is empty.** A goal whose
  `doneWhen` is `"board-open-0"` is checked at the AgentEnd boundary with a
  bounded `walgit collab board` call; zero open cards skips the next
  auto-continue instead of letting the agent invent more work. A failed or
  timed-out board read means "not done", so a broken walgit cannot claim a
  false completion.
- **Transient `walgit collab entry` pushes are retried, bounded.** The
  collaboration loop observed intermittent network/5xx failures that succeeded
  on retry. `scripts/collab-entry-retry.sh` wraps the command with three
  exponential-backoff attempts; schema, validation and signature errors fail on
  the first attempt instead of being masked.
- **The extension catalogue no longer promises unpublished installs.** The 20
  extension cards told visitors to run `ki install <name>`, which resolves
  through crates.io and cannot work while every crate is `publish = false`.
  In-repo wasm guests now show `ki insmod --build extensions/wasm/<name>`,
  the compiled permissions extension shows a source build, and external extensions are
  marked "not published". `docs-install-path.sh` rejects a bare `ki install` in
  the catalogue.
- **A shell command can no longer take over the permission policy.** Before a
  `bash`/`powershell` call the host snapshots every policy file; after the call
  it restores any change and flags the tool result. The lexical shell guard
  remains best-effort (variables, interpreters, scripts and background writers
  can still evade it at the parsing layer), but the restore closes the
  policy-takeover class for commands that return. A sanctioned `permissions`
  edit in the same batch bumps the file generation and wins over an older
  snapshot instead of being silently undone. The restore does not undo the
  command's other effects.
- **Esc no longer poisons the rest of an orchestrator run.** The compile-time
  `kiqi-orchestrator` faces kept the run-wide abort set after a cancelled tool
  call. A soft interrupt lets the turn continue, so the next `dispatch`,
  `subagent` or `plan` call created node tokens that were born cancelled and
  failed without starting anything. The abort is now consumed at the start of
  every tool call (and at run start), latching it onto any still-registered
  background node first so a pending node is killed rather than orphaned. The
  wasm guest's `subagent` also polls `proc.cancel-requested` before spawning and
  while its child runs, so a cancelled call kills the child and returns instead
  of waiting out `timeoutSeconds` (default 300 s) while holding the extension's
  call mutex.
- **The release pipeline no longer targets the unrelated fork.**
  `release.sh`, `refresh-channels.sh`, the Homebrew/Scoop/winget manifests, the
  website and the user docs now point at `gqf2008/pi-rust`;
  `push-taps.sh` refuses to push unless the `gqf2008/*` taps exist. The v0.4.1
  manifests carry digests read from that release. Homebrew, Scoop and winget are
  documented as **unpublished** until those taps and a `gqf2008.ki` winget
  submission exist. `check-install-version-hints.sh` now also gates the release
  workflow's example tag, so a suggestion with no matching release fails the
  local gate.
- **Esc can interrupt a wasm `dispatch` wave.** The wasm ABI gains an additive
  `proc.cancel-requested` host function (same ABI version; old guests are
  unaffected). The orchestrator guest polls it before and during a wave, kills
  the remaining children, and returns instead of waiting out `waitSeconds`.
- **MCP notifications reach an idle session.** The notice mailbox now signals
  a per-session MUI input adapter (`Notify`, not a poll). When idle, the TUI
  main loop drains the mailbox and applies the wake policy; while a run is
  active, the notice is queued as a steering message into that run. Both paths
  end in the same run/agent logic as terminal input, so an idle session no
  longer waits for a user prompt before seeing a signal.
- **MCP runtime watches are bounded and cannot escape the user's opt-in.** The
  watch verb is capped at 32 model subscriptions; a model-started watch is
  record-only unless the same uri is in the server's config `subscribe` list,
  so it cannot consume the configured wake budget; and a model `stop` cannot
  remove a configured watch. `modelWatch: false` disables the verb per server,
  and ACP client-supplied servers always disable it. Permission rules can now
  target the resource `uri` and action (`docs__watch_resource(start)`), closing
  a URI-scoped deny-rule fail-open. During reconnect restore, one rejected
  subscription drops only that uri (the rest of the configured set survives),
  and the whole restore shares one timeout budget instead of one per uri.
  A no-op re-subscribe keeps queued notices, stopping a never-watched uri no
  longer sends an RPC, subscribe/unsubscribe observe the tool's cancellation
  token, and server tools named `read_resource`/`watch_resource` are skipped so
  the synthetic verbs stay available.
- **The user-message echo is readable whatever the terminal palette is.** The
  bubble paints an explicit background (`surface`) but its body inherited the
  terminal's default foreground; on a light terminal with ki's dark theme that
  rendered near-black text on `#343541` — the echoed input was invisible. Each
  preset now carries a paired `surface_text` (dark: light gray, light: dark
  gray, monochrome: white) applied with a foreground-only reset so the bubble
  background survives nested styled spans. The same class was fixed for
  custom messages, whose light-theme text was transcribed as a near-white
  `237` on a near-white bubble (now `#1f2328`, matching the token's own
  comment) and whose dark-theme text inherited the terminal default (now
  `#d4d4d4`). `/theme light` still switches the whole UI to match a light
  terminal.
- **A resumed session can no longer become unsendable.** When a branch carries
  tool results whose assistant message was never persisted — the shape left by
  a crash during `ask_user` — every provider request started with `role: tool`
  messages and failed with HTTP 400 (`Messages with role 'tool' must be a
  response to a preceding message with 'tool_calls'`). The provider-context
  repair now drops tool results with no preceding assistant `tool_calls` (and
  duplicate results) instead of forwarding them; the dropped ids are recorded
  in the repair report and the warning log. The durable session is untouched.
- **A wasm `ask_user` dialog no longer deadlocks the TUI.** The observe event
  fan-out (including `UiPromptStart`, which the key loop dispatches right before
  it opens the dialog) called every wasm host's `on_event`; a guest call such as
  `ask_user` holds that extension's call lock while it waits for the user, so the
  dispatch waited forever — the dialog never opened, the run showed `Working…`
  indefinitely, and Esc stopped responding (observed: 36 minutes). Observe
  dispatches now use a non-blocking `try_on_event` that skips a busy guest; the
  claiming fan-out that decides control flow keeps the blocking call.
  Regression test:
  `observe_events_skip_a_guest_that_is_waiting_on_the_ui` (extension-host e2e).

### Changed

- **An active goal keeps running without a user prompt.** After a run ends,
  the TUI reads the goal extension's `active` state (`.ki/goals.json`, read-only)
  and starts the next run automatically when the goal is still active and has
  budget left. A goal with `maxTurns`/`maxMinutes` is bounded by the extension; an
  unbounded goal is capped at 20 consecutive host continuations. User input,
  Esc, a paused/completed goal, or a signal wake in the same boundary stops or
  takes precedence over the continuation.
- **MCP signal triggers default to acting.** The wake message now tells the
  agent to take the next useful step when it is within its authorized tools and
  the current goal, and to ask the user only when the action needs
  authorization or the intent is genuinely ambiguous. Routine confirmation is
  explicitly discouraged.
- **The `rpi-*` → `kiqi-*` rename is finished.** Every crate, extension and wasm
  guest directory, package name, feature flag (`extension-rpi-*` →
  `extension-kiqi-*`), Rust path (`rpi_ai` → `kiqi_ai`), extension id and file name
  now uses the `kiqi-` prefix the fork's own `kiqi-extension` /
  `kiqi-extension-host` / `kiqi-*` already used; `ki` stays the command. The
  docs, website data, `Taskfile.yml`, CI, packaging manifests (Homebrew
  `Formula/ki.rb`, Scoop `ki.json`, winget `bigfish1913.ki.*`) and release-asset
  names (`kiqi-v<version>-<target>`) follow. The project-local runtime directory
  is `.ki/` (was `.rpi/`), including the tracked `.ki/skills/` skill.
  Deliberately kept — each because dropping it would lose data or silently
  ignore a configured setup, and each now marked as a legacy trace in place:
  the `rpi.value` session marker (`kiqi-harness/src/session/values.rs`), the
  update-cache `#[serde(alias = "rpi")]` (`kiqi-cli/src/updates.rs`), and the
  pre-fork `RPI_*` / `.rpi` fallbacks in `kiqi-langfuse` and
  `kiqi-permissions`. Deliberately **not** renamed, because they name other
  projects rather than this one: `pi-rust/rpi-package` (the extension registry
  the ported extensions came from) and `@juicesharp/rpiv-todo`.

### Added
- **The model can watch MCP resources at runtime**: `{server}__watch_resource`
  (`start`/`stop`/`list`) subscribes through the same manager path as the
  config `subscribe` list, and is only exposed when the server advertises
  `resources.subscribe`. Updates then arrive as regular MCP signals and content
  is read on demand; a model-started watch survives reconnects within the
  session but is not written back to settings.
- **MCP resource updates can wake the session (`signals`, interactive TUI)**:
  an explicit `subscribe` list is the user's opt-in to be told, so its
  `signals.wake` defaults to `auto`. The host records the notice and, within a
  per-server budget (`minIntervalSeconds` 30, `maxRunsPerHour` 6,
  `maxConsecutiveRuns` 2), starts one run with a trigger that frames the change
  as external, untrusted data; the agent decides whether to act, defer, shelve,
  re-orchestrate, ignore, or ask. Wakes happen only at boundaries, user input
  wins, Esc mutes the chain until the next prompt, and no `subscribe` means
  today's behavior is unchanged. `wake: "off"` keeps record-only for an existing
  subscription; print and ACP remain record-only.
- **MCP signal dispositions are durable (`signals` tool, `/signals`, top-level
  `signals` defaults)**: signals that wake a session enter a session-value
  inbox (coalesced by `server`+`uri`, capped at 64). The built-in `signals`
  tool exposes `list`/`status`/`defer`/`resolve`/`dismiss`/`mute`/`unmute`, so
  the agent can settle a signal explicitly instead of relying on prose.
  `/signals [on|off|list]` is the user-facing inspection and mute command; the
  mute is stored as a session value and survives resume. A top-level `signals`
  block supplies defaults for every MCP server, with a trusted project
  overriding field by field (per-server fields still win). `off`/`notify`
  servers do not enter the inbox.
- **Permission policy is enforced by the host**: `permissions.json` (project
  `.ki/`, legacy `.pi/`, then the global agent dir) is evaluated in
  `before_tool_call` before any tool runs — explicit deny rules block, an allow
  list restricts, and the policy file itself cannot be written by the agent.
  Until now the extension only offered a `permissions` tool, so deny rules were
  advisory; `kiqi-permissions` leaves the default build (opt-in feature kept).
- **Installed wasm extensions carry a SHA-256 sidecar**: `ki insmod` writes
  `extension.wasm.sha256`, load verifies it before wasmtime sees the component
  (mismatch skips the extension with the expected and actual digests; a missing
  sidecar still loads with one aggregate warning), and `ki extension show`
  reports the verification state. Integrity/corruption only — signatures are
  deferred until there is a third-party distribution channel.
- **MCP over HTTP/Streamable works**: HTTP `mcpServers` connect through rmcp's
  session/SSE worker with a repo-owned reqwest client (one reqwest major in the
  tree), bounded responses (16 MiB), same-origin redirects only, and resource
  subscriptions over the standalone GET stream (restored after a 404 session
  expiry). `autoRead` lists let chosen uris deliver their content with the
  notice; ACP clients may supply HTTP servers at `session/new`.
- **An active goal points the model at the orchestrator**: the goal guest's
  context injection now appends 「用 `plan` 写 DAG、`dispatch` 跑、节点交给
  `subagent`。」 when those tools are on the session's tool face (and stays
  silent otherwise).

- **`ki acp serve` — a host-level Agent Client Protocol server**: point any ACP
  client (Zed, ABB) at `ki` as an external coding agent over stdio JSON-RPC
  (official `agent-client-protocol` crate, pinned 3.3.0). Covers `initialize`
  / `session/new` / `session/prompt` / `session/cancel` and `session/load`
  replay: multiple live sessions per process routed by `sessionId` (one active
  turn per session), `session/close` cleanup, per-delta streaming of
  message/thought chunks and tool-call progress, honest stop reasons (a failed
  run surfaces as a JSON-RPC error; a client cancel always answers `cancelled`), restart-stable
  session ids a later load restores with the full transcript streamed back
  before the response, and stderr-only diagnostics (`KI_ACP_DEBUG` traces
  method flow). Permission dialogs bridge ki's extension UI prompts to
  `session/request_permission` (question-shaped dialogs are cancelled on their
  original request id, never reinterpreted). `ki acp install-zed` / `ki acp print-zed` register ki in Zed's
  settings.json with a comment-preserving JSONC splice (a `.ki-bak` backup is
  kept on first change), and `ki dev acp serve` refuses loudly instead of
  pretending the dev extension session applies.
- **MCP is a host-level capability now (stdio)**: external MCP servers configured
  under `mcpServers` in ki's `settings.json` (global `~/.ki/agent/settings.json`
  and project `<cwd>/.ki/settings.json`, the same shape the retired adapter
  used, so existing config files port) are connected with the official `rmcp`
  client, and every discovered tool is exposed as a native `{server}__{tool}`
  tool (no `mcp_call` meta-tool: the model sees real tools). Connections live
  with the session and every teardown path reaps the server's whole process
  tree; a server never inherits ki's provider keys (`env_clear` + allowlist),
  every spawn is logged, and a broken server is skipped loudly without killing
  the session. Resources are in too: a server advertising the MCP resources
  capability gets a native `{server}__read_resource(uri)` tool (20k-char bound),
  and `resources/subscribe` is host/config-driven — when the server pushes
  `notifications/resources/updated`, a light notice (server + uri) reaches the
  model, which reads on demand; subscription lifetime follows upstream semantics
  (idempotent subscribe, reconnect restore, unsubscribe-failure transport
  retirement). ACP clients can supply `mcpServers` at
  `session/new` and `session/load` too: stdio entries are accepted directly
  (the editor's request is the user's authorization) and each acceptance is
  logged loudly, HTTP/SSE variants are refused until that transport lands, and
  resource updates reach notice-capable clients as user-facing
  `SessionUpdate::Notice`s. HTTP transport and prompts/sampling remain later
  batches.
- **`fs` grants carry a scope (grant-model change; the ABI stays 0.7.1 — no WIT change)**: `ki extension grant <id> fs --cwd project`
  opts an extension into reading and writing the project tree (same scope as
  `exec`/`proc`); the default stays the extension's own data directory, and legacy
  `"fs": true` files keep working unchanged.
- **`kiqi-orchestrator` ported to wasm** (`extensions/wasm/kiqi-orchestrator`):
  the same three tools and guidance, with waves that run to completion inside
  the dispatch call (children in parallel on `proc`; per-node timeouts enforced
  in the guest). Semantic reductions, documented in the guest README: no
  background waves (`waitSeconds: 0` refused), no mid-wave reshape, and the
  child turn ceiling is not enforced (no env equivalent). It leaves the default
  build too — which now compiles in only `kiqi-permissions`.
- **`kiqi-codegraph` ported to wasm** (`extensions/wasm/kiqi-codegraph`):
  the same eight tools, with the index probe/`init`/`sync` on `exec` and the
  per-call MCP session on `proc`. `proc.read-line` gained a `timeout-ms`
  parameter so the port keeps the 20-second session timeout and its
  "try `codegraph unlock`" message (the contract gap the first real consumer
  found). It leaves the default build, which now compiles in only
  `kiqi-orchestrator` and `kiqi-permissions`. Windows stays on the compiled
  extension: resolving npm/Scoop shims needs PowerShell, and `exec`/`proc` take
  bare command names.
- **`proc` (ABI 0.7.0): a child process the extension may talk to for the length
  of one tool call** — `spawn` / `write` / `read-line` / `read-stderr` /
  `wait` / `kill`, handle-based like `net-sockets`. Its grant carries the
  policy (`--command`/`--cwd` as `exec`, `--timeout` default 600s / cap
  1800s, `--max-open` default 100). Handles die with the call: the host kills
  each child's process group and reaps it when the calling entry point
  returns, so a guest that forgets `kill` or traps leaves nothing behind
  (asserted on the OS, not on the host's bookkeeping). It exists to migrate
  `kiqi-codegraph`, whose search runs a per-call MCP server over stdio; it
  deliberately does not give extensions a subprocess that outlives a call —
  kiqi-orchestrator's background model needs the host to own the scheduler,
  which is a separate design.

- **`ki insmod <path.wasm>` / `ki rmmod <id>`** — kernel-flavoured top-level
  commands for the wasm tier, identical to `ki extension install` /
  `ki extension uninstall` (same flags, same move-aside uninstall). They exist so
  the wasm module verbs do not collide with `ki install`/`ki uninstall`, which
  build and remove Rust cdylib extensions — a different artifact.

### Changed

- **Only the extensions without a wasm port are compiled in by default.** A
  `extensions/kiqi-*` crate that has a `extensions/wasm/kiqi-*` counterpart is
  now opt-in: `--features extension-kiqi-<name>`, or install the wasm guest with
  `ki insmod --build extensions/wasm/kiqi-<name>`. Default: `kiqi-orchestrator`,
  `kiqi-permissions`, `kiqi-codegraph` (nothing else registered). The crates,
  their features, their cdylib faces and their tests all stay, and CI runs the
  opt-in configuration, so nothing rots. One coupling to know about: the
  orchestrator's goal-linked guidance reads `.ki/goals.json`, which the
  **compiled** `kiqi-goal` writes — with that extension opt-in, the guidance no
  longer turns itself on (set `KI_ORCHESTRATOR_GUIDE=1` to force it, or build
  `--features extension-kiqi-goal`). The **wasm** tier closes this gap (below):
  `kiqi-goal` writes the same `.ki/goals.json` and
  `kiqi-orchestrator` gates its guidance on it.
- **The goal→orchestrator coupling is restored in the wasm tier.** `kiqi-goal`
  now keeps its state in `<project>/.ki/goals.json` in the compiled shape
  (`{"active": …|null, "archived": […]}`) through the project-scoped `fs`
  grant, instead of a `{project: Goals}` map in its own data directory — so the
  compiled `kiqi-orchestrator` reads it too. `kiqi-orchestrator` gates its
  "## Subagent orchestration" guidance on that file (an active goal, the compiled
  `active_goal_from` semantics, fail-closed) instead of injecting it
  unconditionally; the compiled `KI_ORCHESTRATOR_GUIDE` override is dropped (a
  guest has no environment). Both guests now need `fs --cwd project`, and the
  old data-directory store is migrated best-effort on first use. The write stays
  non-atomic (`fs` has no `rename`).

### Added

- **`ki insmod --build <guest-crate-dir>`** compiles a wasm guest for
  `wasm32-wasip2` and installs the component cargo reported — the one-command
  way to get an extension back now that its compiled version is opt-in.

### Changed (previous)

- **`kiqi-langfuse` is no longer in the default build.** The tracing tier needs a
  Langfuse instance and an API key pair, so it is now opt-in:
  `cargo build --features extension-kiqi-langfuse` (or a `ki` built that way). The
  crate, the wasm port and the feature flag all stay, and CI builds the feature
  explicitly so the compiled path keeps compiling.


### Added

- **`kiqi-orchestrator` is compiled in** behind `extension-kiqi-orchestrator`
  (default on): bounded DAG subagent orchestration — `subagent` runs one task
  in a child `ki -p --no-session` process, `plan` authors/edits the task graph,
  `dispatch` runs the ready wave, and a goal-coupled guidance section is
  appended to the system prompt when `/goal` is active. Ported from the
  `rpi-package` extension with pre-fork spellings fixed
  (`.ki/` → `.ki/`, `KI_MAX_TURNS_PER_RUN` → `KI_MAX_TURNS_PER_RUN`,
  `KI_ORCHESTRATOR_*` → `KI_ORCHESTRATOR_*`); under `ki` the old names were
  silently inert. The harness's tool-call cancellation token now reaches
  compiled extensions as `HostCaps::cancel`, so aborting a call winds down the
  subagents it spawned instead of leaving them running.


### Fixed

- **Zero budgets are rejected at parse, and proc budgets are capped everywhere**:
  a hand-edited `grants.json` with `timeout-seconds: 0` / `max-open: 0` used to
  parse (now fail-closed, naming the field and bound); `proc`'s `max-open` gained
  the host-side 256 cap it was missing; and `ki extension grant --timeout` no longer
  applies `exec`'s 300s ceiling to `proc` (600s default / 1800s cap are reachable
  again, and the error names the right bound).
- **Langfuse exports real costs again**: the host reports `UsageCost` as a float
  while the extension read it as an integer, so every cost came back `None` and the
  `costDetails` block never reached Langfuse; query-string values are now
  percent-encoded too.


### Fixed

- **Extension events survive a dylib-free install**: the whole extension event surface
  (`session_start`/`session_shutdown`, turn end, tool events, the session tree,
  UI prompt boundaries) was gated on the cdylib tier being non-empty — the host
  matches on `ExtensionSession::snapshot_arc()` before entering the dispatch
  funnels, and that answered `None` when no cdylibs loaded, so a wasm-only `ki`
  delivered nothing to any extension (goal's turn budget never charged; todo and
  plan-mode never restored). A dylib-free session now carries an empty registry,
  so the funnels — which fan out to the compiled/wasm sink unconditionally —
  are entered. One consequence worth naming: `--no-extensions` now only shuts
  the cdylib tier. Compiled-in extensions (goal's turn budget, todo and plan-mode
  restoration, Langfuse) and wasm guests still receive events on such a run,
  because that tier belongs to the host rather than to the cdylib loader.


## [0.4.1] - 2026-10-14

### Added

- **Compile-time extensions now receive events.** The host fans `SessionStart`,
  `BeforeAgentStart`, provider requests, messages and tool calls out to every
  compiled-in extension's `on_event`, so a `ki` with no dynamic libraries still
  traces a run and restores session state. `kiqi-extensions` owns the dispatch
  sink; `ki` installs it per session with that session's capabilities.
- **`kiqi-langfuse` is compiled in** behind `extension-kiqi-langfuse` (default on):
  automatic Langfuse tracing over OTLP, plus the `langfuse_score`,
  `langfuse_prompt` and `langfuse_trace` tools. Its 14 event handlers and three
  tools left the cdylib ABI for the `Extension` trait, and `set_status` now goes
  through the host capability instead of a raw runtime-action id.

### Fixed

- The `kiqi-goal` test target compiles again (its unknown-action test predated
  the host-aware `goal()` signature).

## [0.4.0] - 2026-10-13

### Changed

- **Fork release.** The project is now `kiqi` and the command is `ki` (was
  `rpi`): the executable, the startup lockup and window title, the config
  directory (`~/.ki`, was `~/.rpi`), the project-local directory (`.ki`) and
  every environment variable (`KI_*`, was `RPI_*`) were renamed. Existing
  configs keep working after copying `~/.rpi` to `~/.ki`.
- Crates keep the `kiqi-*` names but are no longer published to crates.io
  (`publish = false`); releases are cut from this fork only.


## [0.3.21] - 2026-10-11

### Added

- Show successfully loaded system prompt and project instruction files in the
  TUI startup Context section. `/context` and the startup listing refresh after
  a successful `/reload`, including project/global precedence and removed files.

### Fixed

- Batch confirmed Windows console text pastes instead of dispatching each
  character separately, substantially reducing the delay before large pasted
  text folds. Preserve Unicode, tabs, line endings, shortcuts and trailing Enter.
- Track ki release notes independently from imported Pi settings so updates
  appear once on the next new interactive session. `/changelog` defaults to the
  latest release; `/changelog all` shows the complete history.
- Read scroll position and follow-output state from the primary scroll view,
  and ignore the jump-to-latest click while another UI owns terminal rendering.
- Keep context-file opt-outs consistent between startup and reload.

## [0.3.20] - 2026-10-11

- Add extension-owned text components: register layout once, update content separately,
  configure positions, show/hide, and automatically clean up on unload or reload.
- Keep legacy footer status text and remove the experimental panel payload.
- Improve footer/sidebar rendering, scroll navigation, Unicode line updates,
  and terminal stdio lifecycle support for extensions.


## [0.3.19] - 2026-10-10

### Fixed

- Restore IME text input in iTerm2 after the 0.3.18 keyboard regression by
  using Pi's Kitty flags 7 instead of all-key reporting (flags 15). Preserve
  UTF-8 text commits, modified keys and functional-key repeat/release events.

## [0.3.18] - 2026-10-10

### Added

- New interactive sessions show bundled release notes once after an upgrade.
  Fresh installs record a baseline without a notice, resumed sessions defer it,
  and `collapseChangelog` offers a compact notice linking to `/changelog`.

### Fixed

- macOS TUI keyboard setup stays on the active screen buffer and requests
  printable-key release events where supported. Local Terminal.app recognizes
  Shift+Enter through native modifier state; suspension restores terminal modes.
- Extension shortcuts no longer intercept modal dialog, selector or search input.
  Passive panels render at their allocated width, stay clear of the editor/footer,
  clamp offsets and avoid each other while space permits.
- `/changelog` displays the real packaged changelog instead of a static summary,
  including offline installs and Keep a Changelog version headings.

## [0.3.17] - 2026-10-08

### Added

- TUI extension panels support positioned transcript, sidebar, and popup layouts,
  including pet companion and live run statistics panels. Task lists and plans
  retain their current state across tool updates and restored sessions.
- TUI prompts accept multiple pasted image paths, clipboard screenshots, and
  copied image files on Windows and macOS. Image-only prompts, steering, and
  follow-up messages retain their attachments; dequeuing restores them to the
  draft, and clearing the draft removes pending images.
- User and tool images are normalized for model input with Pi's default
  2000-pixel dimensions and 4.5 MiB base64 limit, including orientation handling,
  format conversion, and explicit notices when an image cannot be processed.

### Fixed

- Empty assistant starts and whitespace-only streaming updates no longer insert
  a blank row before the reply. Message spacing appears only with visible content.
- The footer displays the current thinking level when starting or restoring a
  session. Tool output hints consistently identify Ctrl+O for expansion and
  Ctrl+T for showing or hiding thinking blocks.
- Image visibility settings now apply to previews and restored history. Terminal
  image detection includes Ghostty, WezTerm, and Warp; Kitty rendering preserves
  aspect ratio, reserves layout rows, and removes obsolete placements.
- Supporting Unix terminals enable enhanced keyboard reporting so Shift+Enter
  can be distinguished from Enter and inserts a newline rather than submitting.
- The self-update tests resolve fixture paths consistently with production
  validation, avoiding macOS ancestor-link path mismatches and exercising the
  stale staging cleanup branch.
- Documentation topic descriptions reference the unified C ABI rather than
  obsolete ABI v2/v3 entrypoints.

## [0.3.16] - 2026-10-06

## [0.3.15] - 2026-10-04

## [0.3.13] - 2026-10-02

### Added

- Recovering a session that was interrupted mid-tool-call no longer poisons the
  provider context. When an assistant `toolCall` has no matching `toolResult` —
  the crash landed between the two durable writes — OpenAI-compatible endpoints
  rejected the whole request with `No tool output found for function call …`.
  The harness now repairs the in-memory context before conversion: it inserts a
  synthetic error result ("Tool call was interrupted before its result was
  persisted. The tool was not re-executed during session recovery.") directly
  after the call, in call order for parallel calls, and logs how many it
  repaired. The durable session is untouched, no interrupted tool is re-run,
  and normal history round-trips unchanged.

### Fixed

- Extension `message_update` events now carry native Pi's `assistantMessageEvent`
  delta envelope (`text_delta`, `thinking_delta`, `toolcall_delta`, …) instead
  of a growing `AgentMessage` snapshot. A streaming consumer reads each delta's
  real `delta` field once, so it no longer has to diff successive cumulative
  snapshots and stream size stays linear — matching the JSON/RPC contract the
  other Pi runtimes emit.
- `ki update` no longer stages a brand new build of the version that is
  already running. It now asks crates.io first and reports
  `ki is up to date (0.3.13 is the latest release)` instead of compiling and
  scheduling a replacement for the same version — the behaviour that made a
  working update look broken. `--force` keeps an explicit reinstall available,
  and an unverified answer (cached, or a registry that could not be reached) is
  labelled as such rather than asserted. A `pending-*` directory that already
  holds a validated binary counts as "about to happen", so re-running
  `ki update` while an update is queued does not start a second compile.
- `ki update` prunes the debris previous interrupted runs left in
  `~/.ki/agent/self-update`. A Windows update persists its `pending-*` staging
  directory so the PowerShell helper can install from it after the parent exits;
  when the helper died first — a reboot at the wrong moment, a machine-wide
  `taskkill` — the directory stayed forever, because nothing in the update path
  removed it and its still-`preparing` status file meant the next start never
  looked at it either. Staging directories no live status file claims are now
  reclaimed, along with the `preparing`/`waiting` records that named them.
- The status file `ki update` prints is readable by the time you go looking for
  it. A `succeeded` record used to be deleted by the very next ki start, so the
  path in that message was dead on arrival and a finished update read as one
  that never ran. Completed records now survive as the receipt for the installed
  version (aging out after a day), and the `staged` line names the version and
  says what to do: `ki 0.3.13 staged; quit this ki and it will be applied.`

## [0.3.12] - 2026-10-01

### Added

- Extensions can now replace the system prompt at runtime via
  `register_before_agent_start` — the mechanism pi's extensions use as a
  `before_agent_start` handler returning `systemPrompt`. ki already broadcast
  `BeforeAgentStart` so an extension could watch a turn start, but the event handler
  ABI is fire-and-forget (`i32` only), so it could not influence one. The new
  slot carries an `out` pointer instead, which is the shape
  `register_resources_discover` already used for the same reason.

  Semantics match pi: the return value **replaces** the prompt (an extension that
  wants to append reads the envelope's `systemPrompt` and concatenates),
  handlers chain in registration order, a handler that errors or panics is
  skipped without aborting the fan-out, and the result applies to that run only.
  It is computed once per user prompt, so a tool loop's several provider calls
  see the same prompt rather than re-running handlers mid-run.

  The slot is appended at the end of the extension API struct, which keeps every
  earlier field offset valid — existing extensions keep loading unchanged and a
  extension checks for the slot before using it, so there is no ABI version bump.

## [0.3.11] - 2026-10-01

### Fixed

- A spawned shell no longer inherits kiqi-langfuse's subagent-nesting channel
  (`LANGFUSE_PI_PARENT_TRACE_ID` / `_SPAN_ID` / `_SESSION_ID` / `_DEPTH`). The
  extension publishes those variables so a launcher can attach a nested `ki` to
  the turn that spawned it, and decides it is a subagent by reading them back —
  but a bash/powershell child inherits the environment, so a plain `ki` started
  from inside a shell adopted its ancestor's trace: it reused the parent trace
  id, hung its root off the parent's root span, and named it `Subagent Turn`
  instead of opening its own trace. Only `KI_SESSION_ID` was on the
  host-private exclusion list, because the prefix-based check could not see
  these. They are now excluded; a launcher that sets them deliberately still
  gets through, which is the supported nesting path. Two tests pin both halves.
- The subagent channel's variable names moved into the extension contract
  (`kiqi_extension_sdk::SUBAGENT_PARENT_*`, with `SUBAGENT_PARENT_ENV` as the
  family), and `kiqi-tools` now consults that declaration instead of restating
  the strings. Restating them is what made the leak above possible in the first
  place: the exclusion and the extension each held a private copy of the names,
  across two repositories, so a rename on either side left the list matching a
  name nobody sets — no compile error, no failing test, the leak simply came
  back. A newly added family member is now excluded automatically, and
  `kiqi-cli` pins the wire values so changing one is caught as the cross-repo
  change it is.

## [0.3.10] - 2026-10-01

### Added

- The extension contract now owns the host identity: `kiqi_extension_sdk::HOST_NAME`
  (`"ki"`) and `HOST_VERSION`, with `kiqi_cli::APP_NAME` / `VERSION` defined as
  aliases of them so the two cannot drift. `BeforeAgentStart`'s payload carries
  `host: {name, version}`, giving an extension a way to label its own output (a
  trace name, a tag, `service.name`) from the host instead of hardcoding a brand
  that a rename silently invalidates. `kiqi-langfuse` had been reporting the
  upstream TypeScript extension's `"Pi Turn"` / `"pi"` for exactly that reason.
- The host now publishes the session id twice, so an extension that observes a
  run can name the session it is watching. `KI_SESSION_ID` (constant:
  `kiqi_extension_sdk::SESSION_ID_ENV`) is written whenever a session becomes active,
  and `BeforeAgentStart`'s payload carries `sessionId` next to
  `prompt`/`imageCount`, recomputed each turn. Previously the only channel was
  `__ki.sessionId` on an extension **tool** call, so a tracing extension reported a
  placeholder session for a pure-chat run and for every turn before the first
  tool call. The payload field is the one that makes the first turn correct; the
  environment variable is a convenience for extensions that never see a tool call.

### Fixed

- Switching sessions no longer leaves extensions naming the previous one. The
  session id reaches an extension on two channels — the `ToolCallContext` (read per
  extension **tool** call) and `KI_SESSION_ID` (read per turn by the provider
  hooks) — and only the startup build republished them. An in-process swap
  (`/import`, `/fork`, the `/session` selector, and the extension
  `NewSession`/`Fork`/`SwitchSession` runtime actions) changed the session the
  harness serves without touching either, so an extension keyed to the old id went
  on serving it. All of them now route through one funnel that republishes both.
- `KI_SESSION_ID` no longer leaks into the shell that `bash`/`powershell`
  spawn. The shell inherited the whole process environment, so the variable
  reached anything the command ran — including another `ki`, which had no way
  to tell its ancestor's session from its own. A small deny-list now keeps the
  host's own plumbing out of a spawned shell, while explicit per-call values
  still pass through.
- `ki update` no longer refuses to run when the running `ki` was launched
  through a link, junction, or `subst` drive. The path validation demanded that
  `canonicalize()` reproduce the launch path byte for byte, so any link *above*
  the executable — a `C:\tools\current\ki.exe` junction, a `subst`, scoop's
  `current` directories — aborted the update with "current ki executable
  resolves through a link or junction". The message named neither the path nor
  the link, and the check was the wrong shape: the leaf is what matters, so a
  linked *file* is still refused, now with both paths printed, while links
  *above* the executable resolve to the real install directory the update
  should rewrite.
- `ki update` no longer tells you to add the staging directory to `PATH`.
  Cargo's install advice named `<agent>/self-update/pending-*/bin`, which the
  atomic replace consumes, so following it added a path that stopped existing a
  second later; the child `cargo install` now runs `--quiet` (errors still
  print).

### Changed

- `docs/README.md` is marked as a historical 0.1.13 release post,
  and its update-commands section now carries a correction. It still told
  readers to run `ki pi-update` and `ki self-update`, neither of which exists
  any more — the article only documented 0.1.13-era behavior and was never
  revisited when the Pi layer was removed and the alias was dropped.

## [0.3.9] - 2026-10-01

### Added

- `ki update` performs a real self-update again. `a5506dd` had reduced it to a
  bare `cargo install` that ignored its arguments, so `ki update --help`
  reinstalled the crate instead of printing help and `--offline` did nothing.
  Restored: argument validation, `--help`, the `cargo binstall`/`cargo install`
  path, direct binary replacement on Unix, the staged PowerShell replacement on
  Windows (a running `.exe` cannot replace itself), staged-binary version
  validation, rollback, and the status report shown on the next start.
- The startup update check covers the ki release and installed Rust extensions
  again, with the 6-hour cache and per-item fallback that survived a registry
  outage.

### Changed

- The Pi compatibility layer is *not* restored: `ki package`, `pi-package
  update`, `install-pi`, `uninstall-pi` and `--enable-pi-packages` stay removed,
  along with the npm/Git package stores and the Node extension host.

### Fixed

- Removed subcommands no longer fall through to the prompt path. `ki package
  update` used to be sent to the model as the message "package update" and
  appeared to hang; it now fails immediately with a pointer to `ki install` /
  `ki uninstall` / `ki update`. `ki self-update` reports the same way and
  points at `ki update`.

## [0.3.8] - 2026-09-30

## [0.3.6] - 2026-09-30

## [0.3.5] - 2026-09-29

## [0.3.4] - 2026-09-27

### Added

- **Long pastes fold into a `[paste #N …]` marker instead of flooding the
  editor.** A bracketed paste longer than 10 lines or 1000 characters collapses
  to a single `[paste #N +M lines]` / `[paste #N M chars]` token, matching native
  pi, so dropping a stack trace or a file into the prompt still leaves a readable
  draft. The editor keeps the full text beside the marker — submit sends the
  expansion — and the marker is atomic: one backspace or arrow press removes or
  steps over the whole thing, and deleting an earlier marker renumbers the later
  ones so `#N` stays gapless. Pastes under the threshold still insert verbatim,
  and Windows' Ctrl+V clipboard fallback takes the same path.
- **`Ctrl+X` copies the way native pi's `app.message.copy` does.** The editor
  selection is copied when there is one, otherwise the last assistant reply.
  Native pi binds the same action to the same key; this is where selection-copy
  lives now that `Ctrl+C` clears.

### Changed

- **Transcript repaints are now O(1) per unchanged message, closing the
  long-session lag against native pi.** ki re-parsed every finished message's
  markdown/layout on every frame, so a 400-message session cost ~18 ms per
  frame (over the 16 ms budget) and got slower as it grew. It now mirrors
  native pi's caching layers: `Markdown` and `Text` memoize
  `(content, width) -> lines`, `UserMessageComponent` builds its `Box` +
  `Markdown` once instead of per frame, finalized `ToolExecutionComponent` /
  `BashExecutionComponent` panels cache their output, and the layout pass has a
  frame-local `renderCached` map so a component measured for an auto basis is
  not rendered again when painted. A live theme switch bumps a global revision
  that every cache keys on, so `apply_theme_preset` cannot leave stale colours.
  A 400-message transcript drops from ~18 ms to ~0.8 ms per frame (~22×) and
  stays under budget past 3000 messages; `crates/kiqi-tui/examples/bench_transcript.rs`
  reproduces the measurement.
- **The terminal window/tab title now carries the startup mark.** An idle
  session sets `▃ ▅ ▂ ki · 🦀` instead of `🦀π ki`, keeping the activity
  suffix it had before (`⟳` in flight, `↻` retrying), so the tab and the startup
  lockup read as one brand: the title's three eighth-blocks are the lockup's
  3:5:2 bar silhouette flattened into a single row, and a test reads the bar
  heights back out of the lockup so the two cannot drift apart. Each bar is one
  cell wide — the first cut used `██` per bar and read as three fat slabs in a
  tab strip. The title is built by `pi-cli`'s `brand` module, so it drops the
  crab with `KI_NO_EMOJI` exactly like the lockup — and it drops `π`, which is
  East-Asian *Ambiguous* and therefore measured differently under a CJK locale.
  Block elements share `█`'s width class, so the title is still single-width
  everywhere.
- **`Ctrl+C` clears the editor instead of aborting the run, matching native pi's
  `handleCtrlC`.** One press clears the draft — text *and* selection — and a
  second press within 500ms quits. Aborting stays on `Esc` (`app.interrupt`), so
  a stray `Ctrl+C` while a turn is streaming can no longer kill it, which is easy
  to do while a half-typed steering message sits in the editor. Auto-repeat is
  ignored, so holding the key cannot turn one tap into the quit; the quit still
  forces the process out when the async loop is parked inside a run. The footer,
  `/help`, and the remote-shell footer now read `Ctrl+C: Clear/Exit` and
  `Esc: Abort` instead of `Ctrl+C: Abort/Exit`.
- **`Ctrl+D` and the model-cycle keys now match native pi too.** `Ctrl+D` no
  longer aborts a run: with text in the editor it deletes the character forward
  (`tui.editor.deleteCharForward`), and on an empty editor it quits
  (`app.exit`). Aborting is now `Esc` and nothing else. Model cycling moved from
  `Ctrl+M` to native's `Ctrl+P`, joined by a new `Shift+Ctrl+P` (`Alt+P` on
  Windows) for the previous model — every hint, selector description, and doc
  comment that named the old key was updated with it.
- **The last native-only keybindings are wired up.** `Ctrl+Z` suspends to the
  background on Unix (`app.suspend` — native binds no key on Windows, so neither
  does ki): it stops the renderer, hands the terminal back to the shell, and
  re-enters the alternate screen when the process is resumed with `fg`. On
  Windows, `Ctrl+Q` joins `Alt+Enter` for follow-ups and `Alt+V` joins `Ctrl+V`
  for image paste — native's Windows keys, added alongside ki's rather than
  replacing them, because the Windows clipboard-*text* fallback still rides
  `Ctrl+V`. The registry now matches native's defaults for every app-level
  action ki implements.

### Fixed

- **Cancelling a run hands the messages you queued while it was in flight back
  to the editor.** Esc / Ctrl+C / Ctrl+D aborted the lane, which drains its
  steering and follow-up queues — and the helper meant to pull them into the
  editor first existed but was never called, so anything staged during a run
  vanished on cancel. The three cancel paths now drain the queue into the draft
  (queued text first, then whatever you had typed, caret at the end of the last
  line) *before* the abort, mirroring native pi's
  `restoreQueuedMessagesToEditor({ abort: true })`. Dequeuing with Alt+Q / Alt+Up
  reports `Restored N queued message(s) to editor` (or `No queued messages to
  restore`) like native `handleDequeue`, and the restored draft's caret lands on
  the last line at a byte offset — the old `(0, char_count)` call parked it at
  the end of the first line and split multi-byte characters.

## [0.3.3] - 2026-09-26

### Added

- **Extension tools now receive the session they run in.** The host merges a
  reserved `__ki` object — `{"cwd", "sessionId"}` — into every extension tool
  call's arguments, in the tool adapter's `execute` rather than in the agent loop
  or `prepare_arguments`, so the field reaches the extension and nothing else: not
  the model, not the `ToolExecutionStart`/`Update` events, not the session log.
  A model cannot forge it either — the host's value overrides one that matches.
  Built-in tools are unaffected; they already receive an
  `ExecutionToolContext`. This is what lets an extension keep per-session state
  without asking the model to guess an identity. `docs/extensions/authoring.md`
  documents the contract, including the one failure mode worth naming: a
  `#[serde(deny_unknown_fields)]` parameter struct will reject the injected key.
- `cargo binstall kiqi-cli` works: `[package.metadata.binstall]` points at the
  release assets, which are named after the binary (`ki`) rather than the crate,
  so the URL template uses `{ bin }`.

### Changed

- A tool result can ask to be rendered as markdown by returning
  `details.markdown = true`. The TUI renders those bodies with its markdown
  component — real table borders, list markers, inline code — instead of printing
  the source literally; every other tool is untouched. Markdown rows are not
  re-wrapped (the panel already wraps) and drop the `│` gutter so table borders
  stay clean.

### Fixed

- **A session log written by two processes at once can be opened again.** Two
  writers diverged their sequence counters: a mutation landed with an
  already-consumed seq, a later writer's seqs jumped forward, and an entry
  chained to a leaf the local state never reached — at which point the v4 loader
  rejected the whole session, so it could not be opened at all. Loading now drops
  a stale-seq append, resyncs the counter across a divergent writer's forward gap
  (raw seqs untouched, so later appends stay consistent), and re-chains an entry
  onto the lane leaf; when it dropped anything the file is republished
  atomically.
- A session already open in another `ki` process is refused with `session … is
  already open in another ki process` instead of being written to by both. The
  host holds an exclusive OS advisory lock per session (`<path>.lock`) for its
  lifetime.
- `Could not open the saved session:` was printed twice when a session could not
  be opened.

## [0.3.2] - 2026-09-25

### Added

- Session pickers (`ki -r` and `/session`) show readable rows instead of a wall
  of near-identical timestamped file names. Each row comes from a cheap
  head/tail summary of the session file — display name, first user prompt
  (single-lined and truncated to a label), and on-disk size — so saved sessions
  can be told apart without parsing logs that can run to tens of megabytes.
  Search matches the label, the short id and the file name, and the primary
  column is wide enough that a long label is not clipped.
- `--name` / `-n` records a session's display name durably, for a fresh session
  as well as a restored one; an empty value clears a name set earlier.

### Changed

- The interactive header now shows the three-bar brand mark from the site
  favicon, labelled with the crab in place of the language name, instead of the
  previous crab-and-π logo.
  The old mark mixed an emoji with a box-drawing glyph, whose east-asian width is
  ambiguous, so the header rendered inconsistently depending on the terminal.
  The mark follows the active theme's accent colour for its middle bar.
  `KI_NO_EMOJI=1` renders the lockup without the crab (and without the
  separator that would otherwise dangle).
- The workspace is now `rustfmt`-clean. The sweep is listed in
  `.git-blame-ignore-revs` so `git blame` skips it.
- `kiqi-ai`, `kiqi-agent`, `kiqi-tools`, `kiqi-harness` and `kiqi-cli` now carry the
  crates.io `artificial-intelligence` category, so they are reachable by
  browsing that category and not only by keyword search.

### Fixed

- A superseded runtime-context update could publish its `activeTools` list after
  a newer one had already been applied. Concurrent `set_runtime_context` calls
  run on separate threads, and the thread holding the older response could reach
  the recording step last, resurrecting a tool list the newer context had
  replaced. The result is now discarded unless its revision is still the newest,
  matching the rule Node already applies to the request itself.
- `-c` / `--continue` and `-r` / `/session` no longer offer sessions that hold
  only their header, the leftovers an abandoned launch leaves behind. Resuming
  one of them previously opened an empty transcript; `-c` now picks the newest
  session that has content, falling back to the newest overall only when every
  session is empty.

## [0.3.1] - 2026-09-25

No functional change to the agent, tools or CLI. This release repairs the build
on `main` and fixes how every crate presents itself on crates.io and docs.rs.

### Fixed

- **`kiqi-extension-sdk` did not compile on `main`.** Duplicate definitions added
  after 0.3.0 shipped — a `pub type ExtensionApi = ExtensionApiVt` alias beside the
  `ExtensionApi` struct, and a second `register_entrypoint_unified` — produced
  `E0428`, `E0119` and `E0609`, so `cargo test --workspace` failed immediately
  for anyone cloning the repository. The duplicates are removed and the build is
  green again. The published 0.3.0 artifacts were built before they landed and
  were never affected.

### Changed

- Every crate declares `homepage` and `documentation`, and ships its own
  `README.md` instead of the workspace README. Previously all nine crates.io and
  docs.rs pages showed the same generic document, and its relative links
  (`docs/architecture/overview.md`, `LICENSE`, `examples/extension-stub`) resolved to 404 on
  those sites.
- docs.rs now builds with `all-features` for every crate except `kiqi-cli`, whose
  clipboard feature needs platform libraries that are not present there.

### Docs

- Added `docs/performance/benchmark-vs-pi.md` and `scripts/bench-vs-pi.mjs` — a reproducible,
  same-machine comparison against native Pi over the same JSONL command channel,
  with isolated config directories and both tools offline. ki is 9.7× faster to
  start, 10.7× faster to cold start, 7.6× smaller in memory and ~18× smaller
  installed. **Corrected after release:** an earlier run set only pi's `PI_*`
  isolation variables, so ki read the real `~/.ki/agent` and loaded the
  machine's global extensions (reported 103.6 ms / 1.7×); a second bug started the
  cold-start stopwatch after `spawn()`. With both fixed, ki's `--version` (17.5 ms) and
  a cold start (17.7 ms) are effectively equal — the cost is process creation.
- `crates/kiqi-cli/embedded-docs/` — the documentation snapshot compiled into the
  `ki` binary for the `docs` tool — is refreshed and now guarded: CI runs
  `scripts/sync-embedded-docs.sh` and fails if the snapshot drifts from the
  repository documents. It had fallen behind by several releases.

## [0.3.0] - 2026-09-25

### Changed

- **Breaking:** the extension ABI is unified on a single `kiqi_extension_register`
  entrypoint (`KI_EXTENSION_ABI_VERSION_UNIFIED`), with the version carried inside
  the `ExtensionApi` struct instead of the symbol name. `kiqi-extensions` still
  resolves `kiqi_extension_register_v3` and then `kiqi_extension_register_v2`, so extensions
  built against an older ABI keep loading; extension authors should rebuild
  against the current SDK when convenient.
- Workspace dependencies (`kiqi-*` internal crates) bumped to `0.2.0`, then to
  `0.3.0` for the whole family.

### Added

- A model-visible system prompt section that makes the agent aware of reasoning
  replay, developed alongside `docs/debugging/llm-repetition-forensics.md`.

### Docs

- `docs/debugging/llm-repetition-forensics.md` updated with the latest evidence and the
  corrections to earlier conclusions in that document.

## [0.2.0] - 2026-09-25

### Changed

- **Breaking:** extension ABI unified — see 0.3.0 for the entrypoint negotiation
  order.

## [0.1.28] - 2026-09-25

### Added

- **Crash recovery, end to end.** Frame-level progress (`LaneRecord::AssistantFrame`),
  commit-on-settle for assistant messages carrying tool calls, startup
  continuation from three entry points (before the first request, during tool
  execution, while waiting to retry), and retry-intent records so a crash during
  backoff retries rather than salvaging a failed attempt as history.
- **Durable queues.** Steering, follow-up and `nextRun` messages are recorded on
  enqueue and rebuilt on start, so a message typed while the agent works survives
  a restart.
- **Deferred (long-poll) provider responses.** New `DeferredProvider` capability
  and a defaulted `Provider::deferred()` in `kiqi-ai`; `run_agent_loop_from_assistant`
  in `kiqi-agent` executes parked tool calls exactly once; `resume_deferred` in
  `kiqi-harness` polls, records, and re-suspends as needed.
- **Settings parity.** `retry`, `compaction`, `steeringMode` and `followUpMode`
  now come from `settings.json` instead of hardcoded defaults. `sessionDir`,
  `shellCommandPrefix` and `httpProxy` are wired, and native Pi's key names
  `enabledModels` / `httpIdleTimeoutMs` are accepted.

### Fixed

- A run that kept dying can no longer restart the same work forever: every
  continuation point is bounded by a resume budget.

### Docs

- `docs/debugging/llm-repetition-forensics.md` §11 documents the per-state comparison
  against native Pi's durable state machine.
- `docs/native-pi-missing-features.md` §7/§15 corrected against verified
  behaviour.

## [0.1.27] - 2026-09-24

### Added

- CBOR binary protocol (framing + codec) in the CLI.
- Image generation API (`openrouter-images`) in `kiqi-ai`.
- Remote model catalog refresh (`pi.dev`).
- TUI: rich footer status bar with live git branch refresh; code-block syntax
  highlighting.
- CLI: file-watch auto reload (`fs-watch`); HTTP proxy and pooled idle timeout;
  startup timing / experimental switches, session cwd restore, structured
  resource diagnostics, and cache-waste statistics.
- Custom provider, user-agent, guidance and attribution support, plus
  `--changelog`.

### Fixed

- Anthropic request failures no longer write to stderr, which previously
  corrupted fullscreen TUI rendering.
- Extensions: an extension panic is now isolated instead of taking down the host
  process.
- `kiqi-ai`: corrected a `constrained.rs` doc example that referenced a
  non-existent `ToolDefinition`.

## [0.1.26] - 2026-09-23

### Fixed

- `clipboard`: gate the `arboard` dependency and clipboard functions on
  non-Android targets so `cargo install` works on Android/Termux.

## [0.1.25] - 2026-09-23

### Added

- Extension UI prompt events and fullscreen copy-on-select.
- Transcript search (`Ctrl+Shift+F`).
- Paste-content detection to fix multi-line pasting on Windows; bracketed paste
  mode is enabled explicitly.
- PowerShell tool registered on Windows, plus a `default_tools` setting.
- `kiqi-agent`: updated agent construction API and restructured documentation.

### Fixed

- Bash tool throttled-flush behaviour.
- `@` symbol is preserved when accepting file autocomplete.
- `arboard` gated to non-Android targets.

## [0.1.24] - 2026-09-22

### Added

- **Remote mode.** `ki --mode rpc` is a real JSONL command loop, `ki --server`
  runs the agent headless over TCP and prints a token at startup, and
  `ki --connect <host:port>` attaches a terminal client that holds no local
  provider, tools, extensions or session files. Token auth is connection-level
  (`-32001` on missing/invalid token); `--no-token` disables it, and
  `KI_SERVER_TOKEN` is honoured. Shared wire types (`RemoteEvent` /
  `RemoteCommand` / `RemoteResponse`) live in one module so the two ends cannot
  drift.
- **Extension lifecycle events (P0–P5) with veto support** — a handler can
  refuse startup by returning `EVENT_HANDLER_ABORT` (new `BeforeTuiStart` hook,
  exit code `3`).
- **ABI v3** (`ExtensionApiVt3Ext.declare`) adds priority and platform declarations.
- Optional JSONL event journal (`KI_EVENT_LOG=1`) with `ki events tail` and
  `ki events path`.

### Docs

- New `docs/remote/user-guide.md` and `docs/README.md`.

## [0.1.23] - 2026-09-21

### Added

- Headless RPC mode behind the `--server` flag.

### Fixed

- `register_entrypoint` is now marked `unsafe fn`, making the host/extension safety
  boundary explicit (breaking change for extension authors).
- Restored the render scheduler and reload error reporting.

## [0.1.22] - 2026-09-19

### Added

- `ki dev-local` degrades gracefully to skills-only mode when no Cargo `cdylib`
  is present.

### Fixed

- Restore the editor state on abort so error text can no longer leak into the
  input box.
- Render Markdown content in the `ask_user` dialog.
- Correct error handling for local development mode without a Cargo `cdylib`.

## [0.1.21] - 2026-09-19

### Changed

- CI: removed a redundant `release.yml` that conflicted with the `ci.yml`
  release job; the crates.io publish now runs on pushes to `main` and supports
  trusted publishing.

### Docs

- Added repository status badges and improved repository discoverability.

## [0.1.20] - 2026-09-19

### Fixed

- Preserve steering messages in the active transcript when a blocking tool is
  cancelled, so queued input is consumed during tool-batch cleanup instead of
  waiting indefinitely for a future run.
- Wait for Windows process-tree termination and stop stdout/stderr readers on
  cancellation, preventing listener commands from hanging the TUI.

## [0.1.19] - 2026-09-16

### Fixed

- Soft-wrap long editor drafts inside the bordered input area.
- Preserve row/column after multiline file or command autocomplete replaces text.
- Clear stale autocomplete suggestions when selectors close.
- Keep regular-mode 405, authentication and network diagnostics visible.
- Sanitize provider error bodies so terminal control sequences and oversized
  responses cannot corrupt the layout.

## [0.1.18] - 2026-09-15

### Fixed

- Redraw only the actual changed line range, so status updates no longer rewrite
  the input editor and footer.
- Avoid all terminal output for unchanged frames, stopping background render
  ticks from pulling the native viewport back to the bottom.
- Track terminal height and cursor position precisely across renders and resizes.
- Clear removed tail rows without adding artificial scrollback lines.

## [0.1.17] - 2026-09-15

### Changed

- macOS default interactive sessions use regular/main-screen mode so the
  terminal owns both text selection and scrollback; Windows and Linux keep
  fullscreen as the default. `--tui-mode regular|fullscreen` overrides on every
  platform.

## [0.1.16] - 2026-09-15

### Fixed

- Reconcile the live assistant component from the harness's authoritative final
  message when prompt completion wins the race with asynchronous event delivery,
  preventing truncated response tails.
- Keep `/copy` output consistent with the rendered transcript.

## [0.1.15] - 2026-09-15

### Added

- `--timeout <seconds>` / `--timeout=<seconds>` CLI override with strict
  positive-integer validation.

### Changed

- Consistent 600-second default timeout for Anthropic Messages, OpenAI Chat
  Completions and OpenAI Responses, including streamed bodies.
- Mouse tracking is off by default on macOS so native text selection works;
  wheel behaviour on Windows and Linux is unchanged.

## [0.1.14] - 2026-09-14

### Added

- Retry progress in the TUI (retry number, budget, live backoff countdown) and
  structured `retry_scheduled` events in JSON mode.
- Retry transient provider failures up to 10 times per request by default;
  authentication, parameter, quota and billing failures stay terminal.

### Fixed

- Parse standard YAML block sequences in skill frontmatter, so lists such as
  `triggers:` are loaded instead of discarded.
- Preserve provider and abort diagnostics when a request ultimately fails.
- Removed the duplicate `Working...` footer status.

## [0.1.13] - 2026-09-14

### Added

- `ki dev-local` and `ki dev --local-only` load only resources from the current
  project and the active development extension, including project-local
  `.ki/skills`, `.pi/skills`, configured project resource paths, and the
  extension's own resources. `/reload` matches the local-only scope.

## [0.1.12] - 2026-09-14

### Added

- Pi-compatible managed npm and Git package stores: structured `npmCommand`
  argv, npm aliases, trusted project settings, bounded subprocesses and
  conservative provenance checks.
- Independent ki and package update checks at startup, with `PI_OFFLINE`
  support across startup and manual update commands.
- Resumable, clean-tree release workflow for the nine published crates.

### Changed

- `models.json` is read as an ordered provider/model catalog and native Pi's
  authenticated default-selection rules are applied without leaking first-party
  API keys into custom compatible providers.
- Git package updates are staged before atomically replacing the installed
  checkout; build or activation failures preserve the previous package.
- Self-updates are staged and validated before replacing the running executable
  on Unix and Windows.
- Install, update and uninstall of Rust-native extension artifacts share one
  mutation lock with rollback for ordinary filesystem and registry failures.

### Known limitation

- Rust-native multi-file artifact transactions recover from reported errors but
  keep no crash journal. Termination in the narrow interval between filesystem
  renames can require manual repair of the extension registry or artifact
  directory.

## [0.1.11] - 2026-09-13

### Added

- Image attachments and clipboard paste, native keybinding configuration, model
  and thinking controls, settings panels, external-editor drafts, session
  navigation and improved tool rendering in the TUI.
- CLI parity: `--list-models`, `--offline`, project trust controls, streamed JSON
  lifecycle events, and HTML/JSONL session export.

### Changed

- Default provider and model selection follows native Pi's precedence (saved
  defaults first, then known provider defaults) while preserving declaration
  order of custom providers.
- OpenAI Responses streaming handles sparse output indexes, reasoning and text
  phases, function/custom tool calls, terminal failures, incomplete responses,
  usage accounting, replay identifiers and malformed frames.
- Anthropic SSE decoding streams incrementally, preserves split UTF-8, surfaces
  transport failures and responds promptly to cancellation.
- Pi JavaScript/TypeScript packages are opt-in via `--enable-pi-packages`.

### Fixed

- The startup update checker no longer re-reads untrusted project package
  declarations; it uses the same trust-gated package set as the session.
- Rust 1.78 remains the supported minimum, verified with the 1.78 toolchain for
  both source APIs and the locked dependency graph.
- The published `kiqi-cli` crate embeds its documentation so the crates.io tarball
  builds independently of the workspace.

## [0.1.10] - 2026-09-12

### Added

- `ki dev` builds and watches a local Rust extension; `/reload` forces a fresh
  build without replacing a working extension when compilation fails.
- `ki uninstall` and `ki uninstall-pi`.
- Multiplexed Node extension transport allowing concurrent requests with
  out-of-order responses; tool and command cancellation propagates through
  `AbortSignal`.

### Fixed

- Provider context compatibility keeps Pi responses without an explicit `role`
  field usable in side threads.

> **Beta notice:** the Node/TypeScript extension bridge is an experimental
> compatibility feature. Use it for local evaluation only.

## [0.1.9] - 2026-09-10

### Added

- The welcome screen lists every active tool, including tools registered by
  runtime extensions, plus discovered skills. Empty skill sets render as
  `Skills (0) none`.

### Fixed

- Crate metadata now points at the correct GitHub repository.

## [0.1.8] - 2026-09-10

### Changed

- Session history, sharing and exports request entries in chronological order,
  matching the terminal transcript.
- The package catalog distinguishes core Rust libraries (`cargo add kiqi-tools`)
  from loadable `cdylib` extensions (`ki install kiqi-todo`).

## [0.1.7] - 2026-09-09

### Added

- `ki install <crate>` for crates.io `cdylib` extensions, with `--version`,
  `--path`, `--locked` and `--force`.
- Nine published extensions added to the website catalog: `kiqi-mcp-adapter`,
  `kiqi-web-access`, `kiqi-subagents`, `kiqi-background-tasks`, `kiqi-lens`,
  `kiqi-todo`, `kiqi-codegraph`, `kiqi-memory`, `kiqi-token-usage`.

## [0.1.6] - 2026-09-09

### Added

- First public crate family: async streaming agent runtime; Anthropic Messages,
  OpenAI-compatible Chat Completions and faux providers; `read`, `write`, `edit`,
  `bash`, `grep`, `find`, `ls` tools; JSONL sessions, context compaction, hooks,
  queues and prompt templates; `kiqi-extension-sdk` with a stable ABI.

## [0.1.0] – [0.1.5]

Early crates.io publications while the workspace layout, provider layer and
agent loop were being established. See `git log` for details.

[Unreleased]: https://github.com/bigfish1913/pi-rust/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/bigfish1913/pi-rust/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/bigfish1913/pi-rust/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/bigfish1913/pi-rust/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/bigfish1913/pi-rust/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bigfish1913/pi-rust/compare/v0.1.28...v0.2.0
