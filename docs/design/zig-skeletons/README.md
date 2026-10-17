# Zig skeletons — the two concurrency contracts worth porting first

Companion to [`../porting-traps.md`](../porting-traps.md). These files are **shape
references, not shippable code**: they encode the invariants that were expensive to
get right in `ki` (Rust), so the Zig rewrite can start from a working control-flow
skeleton instead of rediscovering the traps.

**Verification status.** Both files compile and their tests pass on the toolchain
installed on this machine, **Zig 0.17.0** (`zig version`) — the same version the
rewrite targets (user, 2026-10-17):

```bash
zig test turn_dispatch.zig                             # All 2 tests passed
zig test process_tree.zig                              # All 1 tests passed
zig fmt --check turn_dispatch.zig process_tree.zig     # clean
```

The skeletons deliberately stay on the narrow process/signal layer, so the
invariants do not move with `std.Io`; only the plumbing names below do.

## Files

| File | Contract | Traps covered |
|---|---|---|
| `HANDOFF.md` | 交接说明：阅读顺序、不变量、0.17 实测面、架构立场、语义真源 | — |
| `turn_dispatch.zig` | ACP turn: synchronous in-flight claim → spawn worker → responder hand-off; cancel is a no-op when idle and **never lost** when it races the start | porting-traps §1–3 |
| `process_tree.zig` | MCP stdio server / subagent: `setpgid` at spawn; TERM the group, grace poll, KILL the group, then reap the direct child | porting-traps §8 |

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

## 0.17 surface actually used (observed, not guessed)

Every row came from real `zig test` output while porting the skeletons from
0.16.0 to 0.17.0 — these are the APIs a rewrite reaches for first.

| Wanted | 0.17 reality | What the skeletons use |
|---|---|---|
| `std.posix.fork` | gone | `std.c.fork()` (returns `pid_t`, `-1` on error) |
| `std.posix.setpgid` | gone | `std.c.setpgid(pid, pgid) c_int` |
| `std.posix.execveZ` | gone | `std.c.execve(path, argv, envp) c_int` |
| `std.posix.waitpid` | gone | `std.c.waitpid(pid, ?*c_int, options) pid_t`; `std.c.W.NOHANG` |
| `std.posix.exit` | gone | `std.c._exit(code) noreturn` |
| `std.Thread.sleep` | gone (sleeps belong to `std.Io`) | `std.posix.poll(&[_]std.posix.pollfd{}, timeout_ms)` |
| `std.posix.kill` / `std.posix.SIG` | **present** | kept — the typed `KillError` (`ProcessNotFound`) is what `groupAlive` needs |
| array repeat `T{v} ** N` | **gone** (parses as a binary `*`) | `@splat(&noopStep)` for a comptime array fill; a loop when the element is not comptime-known |
| `std.fs` `realpathAlloc` / `readFileAlloc` | absent in this shape | not used; keep fs work out of the control-plane skeletons |
| `std.c.environ` | `[*:null]?[*:0]u8` | never relied on — `spawn` takes `envp` explicitly |

Rule of thumb for the rewrite: **the process-control layer is libc bindings plus
`std.posix.kill`/`poll`; everything fd-shaped (`pipe`, `Child`, clocks, sleeps) lives
under `std.Io`.** Keep the fork/exec layer thin enough that an `std.Io` API move
cannot strand it.
