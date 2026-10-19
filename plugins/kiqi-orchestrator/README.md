# kiqi-orchestrator

> **Ported from `rpi-package` (2026-10-15).** The compile-time `kiqi` face is
> new; the cdylib face is unchanged. Pre-fork spellings were fixed in the port
> (`.ki/` → `.ki/`, `KI_MAX_TURNS_PER_RUN` → `KI_MAX_TURNS_PER_RUN`,
> `KI_ORCHESTRATOR_*` → `KI_ORCHESTRATOR_*`) — under `ki` the old names were
> silently inert (the turn ceiling never applied, the goal-coupled guidance
> never auto-enabled). DESIGN.md / ARCHITECTURE.md keep the original `ki`
> spellings; they describe the same architecture.

Bounded subagent orchestration for `ki`. A subagent is a child
`ki` process; the orchestrator's fan-out and fan-in already exist in the host
(parallel tool calls, results returned to the context), so this extension only
adds the delegation primitive and, later, the DAG scheduler on top of it.

Design and staged plan: [`DESIGN.md`](DESIGN.md). How it collaborates with the
host, `kiqi-goal` and `kiqi-mcp-adapter`: [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Status

Three tools share one in-memory task graph. It lives for the life of a run and
is cleared when the session is switched; deciding whether to resume from history
or start over is left to the human. `plan`/`dispatch` results still carry the
graph in `details` for inspection.

### `subagent` — one bounded delegation

Run one task in a fresh `ki -p` subprocess and return its final answer.

```json
{"task":"review packages/kiqi-lens for bugs","role":"reviewer","maxTurns":6,"timeoutSeconds":120}
```

| param | required | default | bounds |
| --- | --- | --- | --- |
| `task` | yes | — | 1–20000 bytes, non-empty |
| `role` | no | `worker` | recorded only today |
| `provider` | no | child default | passed to the child as `--provider` |
| `model` | no | child default | passed to the child as `--model` |
| `maxTurns` | no | 8 | 1–32 |
| `timeoutSeconds` | no | 300 | 10–1800 |

### `plan` — author/edit the DAG

| action | params | effect |
| --- | --- | --- |
| `set` | `nodes` | replace the graph (validates ids, deps, acyclicity) |
| `update` | `addNodes` / `removeIds` / `setDeps` / `reset` | edit the graph in place |
| `show` | — | render the graph |

A node is `{id, task, deps[], role?, provider?, model?, maxTurns?, timeoutSeconds?, expectFile?, verify?}`.

**Completion is written only by `dispatch`** — there is no way to hand-mark a node
`done`, so a plan cannot be "completed" by the model. `reset` re-arms a node.

### `dispatch` — schedule the DAG

Starts every pending node whose dependencies are all `done`, up to `maxParallel`
children at once (default 4, cap 16). `waitSeconds` decides how long the call
blocks:

| `waitSeconds` | behaviour |
| --- | --- |
| omitted | wait until the running set drains (one call per wave) |
| `0` | start and return immediately — the graph can be reshaped meanwhile |
| `N` | start, then wait up to `N` seconds |

The call reports the nodes it started, the nodes still running, and the result of
every node that reached a terminal state since the previous call (each result is
reported exactly once). With `waitSeconds=0` the orchestrator can, while a wave is
in flight, `plan` `addNodes` / `setDeps` / `cancel` and then call `dispatch`
again to start newly-ready nodes and collect finished ones.

Guards (all enforced in code, not asked of the model):

- a cycle is rejected by `plan` before anything runs;
- nothing ready *and* nothing running *and* pending nodes left reports "no
  progress" instead of spinning;
- a failed node makes its dependents `skipped` transitively;
- **per-node acceptance**: after a child exits successfully, a node that declared
  `expectFile` must have produced it, and one that declared `verify` must pass
  its shell command — otherwise the node is `failed`. Without this a child that
  hit its turn cap still exits 0 and would be recorded as `done`;
- **non-blocking and cancellable**: each tool's builder runs on a worker thread,
  so `poll` never blocks. `plan update cancel` stops one running child; cancelling
  the whole run kills in-flight children **and their descendants** within ~100 ms
  — the tree is walked because nested shells (ki's own `bash` tool) put
  themselves in a new process group;
- **no session litter**: children run with `--no-session`, so a wave does not
  leave a ~2 MB session file per node in the parent's `.ki/sessions/` directory.

### Provider / model passthrough

The child shares the orchestrator's agent dir, so with no `provider`/`model` it
uses that dir's **default** provider. If the orchestrator was started with an
explicit `--provider`/`--model`, pass the same values here so the child does not
silently use a different (or misconfigured) default.

## How it stays bounded

- **Turn budget** — `maxTurns` is passed as `KI_MAX_TURNS_PER_RUN`, which the
  harness enforces regardless of what the child model wants.
- **Wall-clock budget** — the host kills the child after `timeoutSeconds`.
- **Stall window** — a child that produces nothing on stdout **or** stderr for
  180 s (`SUBAGENT_STALL_SECS`) is killed and reported as
  `subagent stalled: no activity for 180s`. This is not a second timeout: the
  clock is reset by every byte, and the child heartbeats every 15 s
  (`KI_HEARTBEAT_SECS`, ki's print mode), so a long task that is still working
  never trips it. Without the window, two real hangs sat for 17 and 29 minutes
  because `ki -p` prints nothing until the run ends. Cancellation and
  `timeoutSeconds` are both checked first.
- **Debugging a delegation** — `KI_DEBUG_SUBAGENT=1` appends a timestamped stage
  log to `<temp>/kiqi-subagent.log` (`spawn pid=…`, `stdin-written`, `wait`, a
  byte/silence snapshot every 30 s, `reaped status=…`, `readers-joined`).
- **Recursion guard** — the child is launched with `--exclude-tools subagent`,
  so a subagent cannot delegate again.
- **Output bound** — child output is truncated before it re-enters the
  orchestrator's context.

## Task transport

The task is written to the child's **stdin** rather than passed positionally:
`ki -p` treats the next non-flag token as its prompt, so a task beginning with
`-` would otherwise be parsed as a CLI flag. Piped stdin becomes the initial
prompt (`kiqi-cli::app::build_initial_message`).

## Autonomy (goal-implied guidance)

Left alone, a capable orchestrator model just does the work itself (it solved a
test goal with `bash` and never touched `plan`/`dispatch`). To make it actually
orchestrate, this extension appends a short workflow section to the system prompt
whenever `kiqi-goal` has an **active goal** in the project — `/goal` implies
orchestration, so no variable is needed.

`KI_ORCHESTRATOR_GUIDE` only *overrides* that decision: `=0/false/no/off` forces
it off even with a goal, `=1/true/yes/on` forces it on even without one.

The guidance is never injected into a subagent (children carry
`KI_ORCHESTRATOR_SUBAGENT=1` and get no `plan`/`dispatch` anyway).

With it on, an unattended run of “four independent crate summaries plus a
dependent index” self-planned a 5-node graph, ran the four summaries in one
parallel wave, then the index node after them, and finished — no human relaying.

## Recursion guard

The child is launched with `--exclude-tools subagent,plan,dispatch`, so a
subagent cannot orchestrate again.

## Build

Built against the local `kiqi-plugin-sdk` (see the workspace `Cargo.toml`).

```bash
cargo test -p kiqi-orchestrator
```
