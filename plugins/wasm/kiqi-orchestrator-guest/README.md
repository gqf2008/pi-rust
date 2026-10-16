# kiqi-orchestrator-guest

`rpi-orchestrator` ported to the kiqi wasm ABI (0.7.1): the same three tools
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

Mid-wave reshape (add/remove/cancel while a wave is in flight) is therefore
also gone — reshape between waves works as before.

## Grant it needs

```bash
ki insmod --build plugins/wasm/kiqi-orchestrator-guest --id rpi-orchestrator --yes
ki plugin grant rpi-orchestrator proc --command ki --cwd project --timeout 1800
ki plugin grant rpi-orchestrator exec --command sh --command test --cwd project --timeout 300
ki plugin grant rpi-orchestrator fs --cwd project
```

- `proc --command ki`: the subagent/dispatch children (`ki` resolved from PATH).
- `exec` `sh`/`test`: only for nodes declaring `verify`/`expectFile`. Nodes
  without them need neither.
- `fs --cwd project`: reads `.ki/goals.json` to gate the guidance (below).
  `insmod --yes` grants a newly declared `fs` at the default scope (the plugin's
  own data directory), which this guest cannot read the project file from — the
  explicit `--cwd project` grant above is what scopes it, so run it too.

## Differences from the compile-time plugin

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
