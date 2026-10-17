# Zig skeletons — the two concurrency contracts worth porting first

Companion to [`../porting-traps.md`](../porting-traps.md). These files are **shape
references, not shippable code**: they encode the invariants that were expensive to
get right in `ki` (Rust), so the Zig rewrite can start from a working control-flow
skeleton instead of rediscovering the traps.

**Verification status.** All tests below pass on the toolchain installed on this
machine, **Zig 0.16.0** (`zig version`):

```bash
zig test turn_dispatch.zig     # All 2 tests passed
zig test process_tree.zig      # All 1 tests passed
```

The rewrite itself **targets Zig 0.17** (user, 2026-10-17). The skeletons deliberately
stay on the narrow, long-stable process/signal layer so the 0.16 → 0.17 move only
touches the plumbing described at the end of this file — the invariants do not move.

## Files

| File | Contract | Traps covered |
|---|---|---|
| `turn_dispatch.zig` | ACP turn: synchronous in-flight claim → spawn worker → responder hand-off; cancel is a no-op when idle and **never lost** when it races the start | porting-traps §1–3 |
| `process_tree.zig` | MCP stdio server / subagent: `setpgid` at spawn; TERM the group, grace poll, KILL the group, then reap the direct child | porting-traps §8 |
| `skeletons.zig` | earlier combined draft of both (superseded by the two files above; kept only until it is deleted) | — |

## Invariants the skeletons pin

`turn_dispatch.zig`

- `TurnState.begin()` swaps `in_flight = true` **synchronously in the handler**, then
  clears `cancelled`. A cancel that arrived before the turn was registered is thus
  either seen by this clear (turn starts, then aborts at the first step) or lands
  after the swap (worker sees it) — there is no window where it vanishes. This is the
  `NoActiveRun` window that forced a 100 ms re-abort tick in the Rust ACP layer.
- `cancel()` on an idle state does nothing and **must not** poison the next turn
  (test: `idle cancel does not poison the next turn`).
- The worker checks `isCancelled()` before the first step and between steps; the
  outcome is delivered exactly once; `release()` happens in the worker's `defer`.
- The prompt handler never runs the turn inline — the official ACP connection
  dispatches serially, so inline work blocks `cancel` handling.

`process_tree.zig`

- Both parent and child call `setpgid` (child `setpgid(0,0)`, parent `setpgid(pid,pid)`)
  to close the race with a fast `exec`.
- `groupAlive()` is the `kill(-pgid, 0)` probe; `ProcessNotFound` means empty,
  anything else (e.g. `PermissionDenied`) is treated as alive.
- Kill order: `TERM` to the whole group → poll up to `grace_ms` → `KILL` to the group →
  `waitpid` the direct child. The group id is still valid because the direct child is
  unreaped (its pid cannot be recycled), which is what makes the grandchild kill safe.
- The child gets an **explicit minimal environment** (`PATH=/usr/bin:/bin` in the
  test). Never inherit provider keys or proxy variables into an MCP server.

## 0.16 → 0.17 adaptation points (observed, not guessed)

Everything here was hit while getting these files to compile on 0.16.0; expect the
same or a further move under `std.Io` in 0.17.

| Wanted | 0.16 reality | What the skeletons use |
|---|---|---|
| `std.posix.fork` | gone | `std.c.fork()` (returns `pid_t`, `-1` on error) |
| `std.posix.setpgid` | gone | `std.c.setpgid(pid, pgid) c_int` |
| `std.posix.execveZ` | gone | `std.c.execve(path, argv, envp) c_int` |
| `std.posix.waitpid` | gone | `std.c.waitpid(pid, ?*c_int, options) pid_t`; `std.c.W.NOHANG` |
| `std.posix.exit` | gone | `std.c._exit(code) noreturn` |
| `std.Thread.sleep` | gone | `std.posix.poll(&[_]std.posix.pollfd{}, timeout_ms)` |
| `std.posix.kill` / `std.posix.SIG` | **present** | kept — typed `KillError` (`ProcessNotFound`) is what `groupAlive` needs |
| `std.fs` `realpathAlloc` / `readFileAlloc` | absent in this shape | not used; keep fs work out of the control-plane skeletons |
| `std.c.environ` | `[*:null]?[*:0]u8` | never relied on — `spawn` takes `envp` explicitly |

Rule of thumb for the rewrite: **the process-control layer is libc bindings plus
`std.posix.kill`/`poll`; everything fd-shaped (`pipe`, `Child`, clocks, sleeps) lives
under `std.Io`.** Keep the fork/exec layer thin enough that an `std.Io` API move
cannot strand it.
