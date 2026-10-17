# Changelog

All notable changes to the `kiqi-*` crate family are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The nine published crates share one workspace version and are released together
in dependency order. `git tag vX.Y.Z` marks the commit that a release was cut
from; the matching [GitHub Release](../../releases) carries the same notes.

## [Unreleased]

### Changed

- **The `rpi-*` → `kiqi-*` rename is finished.** Every crate, plugin and wasm
  guest directory, package name, feature flag (`plugin-rpi-*` →
  `plugin-kiqi-*`), Rust path (`rpi_ai` → `kiqi_ai`), plugin id and file name
  now uses the `kiqi-` prefix the fork's own `kiqi-plugin` /
  `kiqi-plugin-host` / `kiqi-*-guest` already used; `ki` stays the command. The
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
  the ported plugins came from) and `@juicesharp/rpiv-todo`.

### Added
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
  replay: one live session per process, per-delta streaming of message/thought
  chunks and tool-call progress, honest stop reasons (a failed run surfaces as
  a JSON-RPC error; a client cancel always answers `cancelled`), restart-stable
  session ids a later load restores with the full transcript streamed back
  before the response, and stderr-only diagnostics (`KI_ACP_DEBUG` traces
  method flow). Permission dialogs bridge ki's plugin UI prompts to
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
- **`fs` grants carry a scope (grant-model change; the ABI stays 0.7.1 — no WIT change)**: `ki plugin grant <id> fs --cwd project`
  opts a plugin into reading and writing the project tree (same scope as
  `exec`/`proc`); the default stays the plugin's own data directory, and legacy
  `"fs": true` files keep working unchanged.
- **`kiqi-orchestrator` ported to wasm** (`plugins/wasm/kiqi-orchestrator-guest`):
  the same three tools and guidance, with waves that run to completion inside
  the dispatch call (children in parallel on `proc`; per-node timeouts enforced
  in the guest). Semantic reductions, documented in the guest README: no
  background waves (`waitSeconds: 0` refused), no mid-wave reshape, and the
  child turn ceiling is not enforced (no env equivalent). It leaves the default
  build too — which now compiles in only `kiqi-permissions`.
- **`kiqi-codegraph` ported to wasm** (`plugins/wasm/kiqi-codegraph-guest`):
  the same eight tools, with the index probe/`init`/`sync` on `exec` and the
  per-call MCP session on `proc`. `proc.read-line` gained a `timeout-ms`
  parameter so the port keeps the 20-second session timeout and its
  "try `codegraph unlock`" message (the contract gap the first real consumer
  found). It leaves the default build, which now compiles in only
  `kiqi-orchestrator` and `kiqi-permissions`. Windows stays on the compiled
  plugin: resolving npm/Scoop shims needs PowerShell, and `exec`/`proc` take
  bare command names.
- **`proc` (ABI 0.7.0): a child process the plugin may talk to for the length
  of one tool call** — `spawn` / `write` / `read-line` / `read-stderr` /
  `wait` / `kill`, handle-based like `net-sockets`. Its grant carries the
  policy (`--command`/`--cwd` as `exec`, `--timeout` default 600s / cap
  1800s, `--max-open` default 100). Handles die with the call: the host kills
  each child's process group and reaps it when the calling entry point
  returns, so a guest that forgets `kill` or traps leaves nothing behind
  (asserted on the OS, not on the host's bookkeeping). It exists to migrate
  `kiqi-codegraph`, whose search runs a per-call MCP server over stdio; it
  deliberately does not give plugins a subprocess that outlives a call —
  kiqi-orchestrator's background model needs the host to own the scheduler,
  which is a separate design.

- **`ki insmod <path.wasm>` / `ki rmmod <id>`** — kernel-flavoured top-level
  commands for the wasm tier, identical to `ki plugin install` /
  `ki plugin uninstall` (same flags, same move-aside uninstall). They exist so
  the wasm module verbs do not collide with `ki install`/`ki uninstall`, which
  build and remove Rust cdylib extensions — a different artifact.

### Changed

- **Only the plugins without a wasm port are compiled in by default.** A
  `plugins/kiqi-*` crate that has a `plugins/wasm/kiqi-*-guest` counterpart is
  now opt-in: `--features plugin-kiqi-<name>`, or install the wasm guest with
  `ki insmod --build plugins/wasm/kiqi-<name>-guest`. Default: `kiqi-orchestrator`,
  `kiqi-permissions`, `kiqi-codegraph` (nothing else registered). The crates,
  their features, their cdylib faces and their tests all stay, and CI runs the
  opt-in configuration, so nothing rots. One coupling to know about: the
  orchestrator's goal-linked guidance reads `.ki/goals.json`, which the
  **compiled** `kiqi-goal` writes — with that plugin opt-in, the guidance no
  longer turns itself on (set `KI_ORCHESTRATOR_GUIDE=1` to force it, or build
  `--features plugin-kiqi-goal`). The **wasm** tier closes this gap (below):
  `kiqi-goal-guest` writes the same `.ki/goals.json` and
  `kiqi-orchestrator-guest` gates its guidance on it.
- **The goal→orchestrator coupling is restored in the wasm tier.** `kiqi-goal-guest`
  now keeps its state in `<project>/.ki/goals.json` in the compiled shape
  (`{"active": …|null, "archived": […]}`) through the project-scoped `fs`
  grant, instead of a `{project: Goals}` map in its own data directory — so the
  compiled `kiqi-orchestrator` reads it too. `kiqi-orchestrator-guest` gates its
  "## Subagent orchestration" guidance on that file (an active goal, the compiled
  `active_goal_from` semantics, fail-closed) instead of injecting it
  unconditionally; the compiled `KI_ORCHESTRATOR_GUIDE` override is dropped (a
  guest has no environment). Both guests now need `fs --cwd project`, and the
  old data-directory store is migrated best-effort on first use. The write stays
  non-atomic (`fs` has no `rename`).

### Added

- **`ki insmod --build <guest-crate-dir>`** compiles a wasm guest for
  `wasm32-wasip2` and installs the component cargo reported — the one-command
  way to get a plugin back now that its compiled version is opt-in.

### Changed (previous)

- **`kiqi-langfuse` is no longer in the default build.** The tracing tier needs a
  Langfuse instance and an API key pair, so it is now opt-in:
  `cargo build --features plugin-kiqi-langfuse` (or a `ki` built that way). The
  crate, the wasm port and the feature flag all stay, and CI builds the feature
  explicitly so the compiled path keeps compiling.


### Added

- **`kiqi-orchestrator` is compiled in** behind `plugin-kiqi-orchestrator`
  (default on): bounded DAG subagent orchestration — `subagent` runs one task
  in a child `ki -p --no-session` process, `plan` authors/edits the task graph,
  `dispatch` runs the ready wave, and a goal-coupled guidance section is
  appended to the system prompt when `/goal` is active. Ported from the
  `rpi-package` extension with pre-fork spellings fixed
  (`.ki/` → `.ki/`, `KI_MAX_TURNS_PER_RUN` → `KI_MAX_TURNS_PER_RUN`,
  `KI_ORCHESTRATOR_*` → `KI_ORCHESTRATOR_*`); under `ki` the old names were
  silently inert. The harness's tool-call cancellation token now reaches
  compiled plugins as `HostCaps::cancel`, so aborting a call winds down the
  subagents it spawned instead of leaving them running.


### Fixed

- **Zero budgets are rejected at parse, and proc budgets are capped everywhere**:
  a hand-edited `grants.json` with `timeout-seconds: 0` / `max-open: 0` used to
  parse (now fail-closed, naming the field and bound); `proc`'s `max-open` gained
  the host-side 256 cap it was missing; and `ki plugin grant --timeout` no longer
  applies `exec`'s 300s ceiling to `proc` (600s default / 1800s cap are reachable
  again, and the error names the right bound).
- **Langfuse exports real costs again**: the host reports `UsageCost` as a float
  while the plugin read it as an integer, so every cost came back `None` and the
  `costDetails` block never reached Langfuse; query-string values are now
  percent-encoded too.


### Fixed

- **Plugin events survive a dylib-free install**: the whole plugin event surface
  (`session_start`/`session_shutdown`, turn end, tool events, the session tree,
  UI prompt boundaries) was gated on the cdylib tier being non-empty — the host
  matches on `ExtensionSession::snapshot_arc()` before entering the dispatch
  funnels, and that answered `None` when no cdylibs loaded, so a wasm-only `ki`
  delivered nothing to any plugin (goal's turn budget never charged; todo and
  plan-mode never restored). A dylib-free session now carries an empty registry,
  so the funnels — which fan out to the compiled/wasm sink unconditionally —
  are entered. One consequence worth naming: `--no-extensions` now only shuts
  the cdylib tier. Compiled-in plugins (goal's turn budget, todo and plan-mode
  restoration, Langfuse) and wasm guests still receive events on such a run,
  because that tier belongs to the host rather than to the cdylib loader.


## [0.4.1] - 2026-10-14

### Added

- **Compile-time plugins now receive events.** The host fans `SessionStart`,
  `BeforeAgentStart`, provider requests, messages and tool calls out to every
  compiled-in plugin's `on_event`, so a `ki` with no dynamic libraries still
  traces a run and restores session state. `kiqi-extensions` owns the dispatch
  sink; `ki` installs it per session with that session's capabilities.
- **`kiqi-langfuse` is compiled in** behind `plugin-kiqi-langfuse` (default on):
  automatic Langfuse tracing over OTLP, plus the `langfuse_score`,
  `langfuse_prompt` and `langfuse_trace` tools. Its 14 event handlers and three
  tools left the cdylib ABI for the `Plugin` trait, and `set_status` now goes
  through the host capability instead of a raw runtime-action id.

### Fixed

- The `kiqi-goal` test target compiles again (its unknown-action test predated
  the host-aware `goal()` signature).

## [0.4.0] - 2026-10-13

### Changed

- **Fork release.** The project is now `kiqi` and the command is `ki` (was
  `ki`): the executable, the startup lockup and window title, the config
  directory (`~/.ki`, was `~/.ki`), the project-local directory (`.ki`) and
  every environment variable (`KI_*`, was `KI_*`) were renamed. Existing
  configs keep working after copying `~/.ki` to `~/.ki`.
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

- Add plugin-owned text components: register layout once, update content separately,
  configure positions, show/hide, and automatically clean up on unload or reload.
- Keep legacy footer status text and remove the experimental panel payload.
- Improve footer/sidebar rendering, scroll navigation, Unicode line updates,
  and terminal stdio lifecycle support for plugins.


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
- Plugin shortcuts no longer intercept modal dialog, selector or search input.
  Passive panels render at their allocated width, stay clear of the editor/footer,
  clamp offsets and avoid each other while space permits.
- `/changelog` displays the real packaged changelog instead of a static summary,
  including offline installs and Keep a Changelog version headings.

## [0.3.17] - 2026-10-08

### Added

- TUI plugin panels support positioned transcript, sidebar, and popup layouts,
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

- Plugin `message_update` events now carry native Pi's `assistantMessageEvent`
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

- Plugins can now replace the system prompt at runtime via
  `register_before_agent_start` — the mechanism pi's extensions use as a
  `before_agent_start` handler returning `systemPrompt`. ki already broadcast
  `BeforeAgentStart` so a plugin could watch a turn start, but the event handler
  ABI is fire-and-forget (`i32` only), so it could not influence one. The new
  slot carries an `out` pointer instead, which is the shape
  `register_resources_discover` already used for the same reason.

  Semantics match pi: the return value **replaces** the prompt (a plugin that
  wants to append reads the envelope's `systemPrompt` and concatenates),
  handlers chain in registration order, a handler that errors or panics is
  skipped without aborting the fan-out, and the result applies to that run only.
  It is computed once per user prompt, so a tool loop's several provider calls
  see the same prompt rather than re-running handlers mid-run.

  The slot is appended at the end of the plugin API struct, which keeps every
  earlier field offset valid — existing plugins keep loading unchanged and a
  plugin checks for the slot before using it, so there is no ABI version bump.

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
- The subagent channel's variable names moved into the plugin contract
  (`kiqi_plugin_sdk::SUBAGENT_PARENT_*`, with `SUBAGENT_PARENT_ENV` as the
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

- The plugin contract now owns the host identity: `kiqi_plugin_sdk::HOST_NAME`
  (`"ki"`) and `HOST_VERSION`, with `kiqi_cli::APP_NAME` / `VERSION` defined as
  aliases of them so the two cannot drift. `BeforeAgentStart`'s payload carries
  `host: {name, version}`, giving an extension a way to label its own output (a
  trace name, a tag, `service.name`) from the host instead of hardcoding a brand
  that a rename silently invalidates. `kiqi-langfuse` had been reporting the
  upstream TypeScript plugin's `"Pi Turn"` / `"pi"` for exactly that reason.
- The host now publishes the session id twice, so an extension that observes a
  run can name the session it is watching. `KI_SESSION_ID` (constant:
  `kiqi_plugin_sdk::SESSION_ID_ENV`) is written whenever a session becomes active,
  and `BeforeAgentStart`'s payload carries `sessionId` next to
  `prompt`/`imageCount`, recomputed each turn. Previously the only channel was
  `__ki.sessionId` on a plugin **tool** call, so a tracing extension reported a
  placeholder session for a pure-chat run and for every turn before the first
  tool call. The payload field is the one that makes the first turn correct; the
  environment variable is a convenience for plugins that never see a tool call.

### Fixed

- Switching sessions no longer leaves plugins naming the previous one. The
  session id reaches a plugin on two channels — the `ToolCallContext` (read per
  plugin **tool** call) and `KI_SESSION_ID` (read per turn by the provider
  hooks) — and only the startup build republished them. An in-process swap
  (`/import`, `/fork`, the `/session` selector, and the plugin
  `NewSession`/`Fork`/`SwitchSession` runtime actions) changed the session the
  harness serves without touching either, so a plugin keyed to the old id went
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

- **Plugin tools now receive the session they run in.** The host merges a
  reserved `__ki` object — `{"cwd", "sessionId"}` — into every plugin tool
  call's arguments, in the tool adapter's `execute` rather than in the agent loop
  or `prepare_arguments`, so the field reaches the plugin and nothing else: not
  the model, not the `ToolExecutionStart`/`Update` events, not the session log.
  A model cannot forge it either — the host's value overrides one that matches.
  Built-in tools are unaffected; they already receive an
  `ExecutionToolContext`. This is what lets a plugin keep per-session state
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

- **`kiqi-plugin-sdk` did not compile on `main`.** Duplicate definitions added
  after 0.3.0 shipped — a `pub type PluginApi = PluginApiVt` alias beside the
  `PluginApi` struct, and a second `register_entrypoint_unified` — produced
  `E0428`, `E0119` and `E0609`, so `cargo test --workspace` failed immediately
  for anyone cloning the repository. The duplicates are removed and the build is
  green again. The published 0.3.0 artifacts were built before they landed and
  were never affected.

### Changed

- Every crate declares `homepage` and `documentation`, and ships its own
  `README.md` instead of the workspace README. Previously all nine crates.io and
  docs.rs pages showed the same generic document, and its relative links
  (`docs/architecture/overview.md`, `LICENSE`, `examples/plugin-stub`) resolved to 404 on
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
  machine's global plugins (reported 103.6 ms / 1.7×); a second bug started the
  cold-start stopwatch after `spawn()`. With both fixed, ki's `--version` (17.5 ms) and
  a cold start (17.7 ms) are effectively equal — the cost is process creation.
- `crates/kiqi-cli/embedded-docs/` — the documentation snapshot compiled into the
  `ki` binary for the `docs` tool — is refreshed and now guarded: CI runs
  `scripts/sync-embedded-docs.sh` and fails if the snapshot drifts from the
  repository documents. It had fallen behind by several releases.

## [0.3.0] - 2026-09-25

### Changed

- **Breaking:** the plugin ABI is unified on a single `kiqi_plugin_register`
  entrypoint (`KI_PLUGIN_ABI_VERSION_UNIFIED`), with the version carried inside
  the `PluginApi` struct instead of the symbol name. `kiqi-extensions` still
  resolves `kiqi_plugin_register_v3` and then `kiqi_plugin_register_v2`, so plugins
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

- **Breaking:** plugin ABI unified — see 0.3.0 for the entrypoint negotiation
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
- Extensions: a plugin panic is now isolated instead of taking down the host
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
- **ABI v3** (`PluginApiVt3Ext.declare`) adds priority and platform declarations.
- Optional JSONL event journal (`KI_EVENT_LOG=1`) with `ki events tail` and
  `ki events path`.

### Docs

- New `docs/remote/user-guide.md` and `docs/README.md`.

## [0.1.23] - 2026-09-21

### Added

- Headless RPC mode behind the `--server` flag.

### Fixed

- `register_entrypoint` is now marked `unsafe fn`, making the host/plugin safety
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
  queues and prompt templates; `kiqi-plugin-sdk` with a stable ABI.

## [0.1.0] – [0.1.5]

Early crates.io publications while the workspace layout, provider layer and
agent loop were being established. See `git log` for details.

[Unreleased]: https://github.com/bigfish1913/pi-rust/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/bigfish1913/pi-rust/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/bigfish1913/pi-rust/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/bigfish1913/pi-rust/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/bigfish1913/pi-rust/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bigfish1913/pi-rust/compare/v0.1.28...v0.2.0
