# kiqi-orchestrator

`kiqi-orchestrator` ported to the kiqi wasm ABI (0.8.0): the same three tools
(`subagent` / `plan` / `dispatch`) and the orchestration guidance, with the
process plumbing on `proc` and one-command `init`-style work on `exec` only
where the acceptance checks need it (`test -f`, `sh -c`).

## The one structural change: waves are blocking

A wasm guest exists only during a tool call. So `dispatch` runs the ready wave
**to completion inside the call** — children start in parallel (the host holds
up to the grant's `max-open`), and the guest multiplexes their stdout with
per-read timeouts until every node reaches a terminal state.

- `waitSeconds: 0` (background waves) is **refused**: children die with the
  call, so "return and collect later" would strand them.
- `waitSeconds: N` kills and fails whatever is still running after N seconds.
- Absent: the wave waits until it drains, each node bounded by its own
  `timeoutSeconds`.
- **Esc/abort**: the guest polls `proc.cancel-requested`; a cancelled call
  kills the remaining children and returns without waiting for the wave.
  `dispatch` checks before spawning and once per wave round; `subagent` checks
  before spawning and on every ~100 ms read slice, so a cancelled single
  delegation kills its child instead of running to `timeoutSeconds` (default
  300 s) while holding the extension's call mutex.

Mid-wave reshape (add/remove/cancel while a wave is in flight) is therefore
also gone — reshape between waves works as before.

## Observability: the right rail

`plan` and `dispatch` publish their state through `ui.component` into the
host's `sidebar.right` slot, which the interactive TUI stacks down the right
edge (one text panel per component, in `order`):

- `orchestrator.plan` — the task list (`✔ done`, `▶ running`, `· pending`,
  `✖ failed`, `– skipped`) with the done count.
- `orchestrator.node.<id>` — one bordered box per node: state, elapsed time and
  the tail of the child's stdout (ANSI stripped, at most 8 lines, rewritten at
  most every 400 ms). `subagent` calls get one box titled `subagent`.
- The tool output stream also carries `▶ id` / `✔ id` lines, so a collapsed
  tool block still shows which node started or finished.

Every panel call is best-effort: without an attached UI (headless modes, unit
tests) the host answers `denied` and the wave runs exactly as before. Boxes are
removed on `shutdown`, and `plan` rewrites drop boxes for nodes it no longer
contains.

## Grant it needs

```bash
ki insmod --build extensions/wasm/kiqi-orchestrator --id kiqi-orchestrator --yes
ki extension grant kiqi-orchestrator proc --command ki --cwd project --timeout 1800
ki extension grant kiqi-orchestrator exec --command sh --command test --cwd project --timeout 300
ki extension grant kiqi-orchestrator fs --cwd project
```

- `proc --command ki`: the subagent/dispatch children (`ki` resolved from PATH).
- `exec` `sh`/`test`: only for nodes declaring `verify`/`expectFile`. Nodes
  without them need neither.
- `fs --cwd project`: reads `.ki/goals.json` to gate the guidance (below).
  `insmod --yes` grants a newly declared `fs` at the default scope (the extension's
  own data directory), which this guest cannot read the project file from — the
  explicit `--cwd project` grant above is what scopes it, so run it too.

## Differences from the compile-time extension

1. **Blocking waves** (above); `maxParallel` is capped by the grant's
   `max-open` (default 100 ≥ the tool's 16 cap).
2. **`maxTurns` is not enforced on the child** — the env knob
   (`KI_MAX_TURNS_PER_RUN`) has no `proc` equivalent. Accepted for schema
   parity, recorded, not enforced.
3. **Guidance is goal-gated, like the compiled version.** The
   "## Subagent orchestration" section is appended to the system prompt only
   when `<project>/.ki/goals.json` holds an active goal — the compiled
   `active_goal_from` semantics, fail-closed (any read or parse problem is "no
   goal"), never appended twice. The compiled `KI_ORCHESTRATOR_GUIDE` env
   override has no guest equivalent (there is no environment to read), so it is
   gone; the file alone decides. The `fs --cwd project` grant is what lets the
   guest read the file, so without it the guidance never turns on. The compiled
   gate has a second condition the guest does not reproduce: it skips injection
   inside a subagent (`KI_ORCHESTRATOR_SUBAGENT`). A guest cannot carry that
   marker — `proc::spawn` has no env knob and the host scrubs the child's env
   (only `PATH` survives) — so a delegated child with an active goal still sees
   the guidance even though `--exclude-tools` left it without `plan`/`dispatch`.
4. **Children spawn `ki` from PATH** — there is no `current_exe()` in wasm.
   `program` overrides it (test seam; the exec/proc allow-list still gates
   whatever is named), and an overridden program receives no ki-specific flags.
5. **The session graph clears on session switch**, same as compiled — tracked
   via the event's `sessionId`.
6. **Failure messages name the step** that needed the grant (`codegraph`-style).

## Testing

Hermetic: `subagent` and full waves run with `program: "cat"` (the task echoes
back), the plan's dependency gating is asserted across two dispatches,
`waitSeconds: 0` refusal, session-switch clearing, and the deny path without
the grant. A live run with the real `ki` follows the repo's detect-and-return
pattern.

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_wasm_bindings::deny_contribution_exports!(…)`.
