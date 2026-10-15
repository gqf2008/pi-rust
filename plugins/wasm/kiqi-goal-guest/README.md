# kiqi-goal-guest

`rpi-goal` ported to the kiqi wasm ABI. Codex-style goal mode: one persistent,
context-injected objective per project that the model keeps advancing across
turns. The plugin is three surfaces over one core:

- the `goal` tool — eight actions: `start`, `update`, `pause`, `resume`,
  `complete`, `limit`, `show`, `list`;
- the `/goal` slash command (plain-text rendering of the same operations);
- a `TurnEnd` hook that charges a turn against the budget, auto-pauses an
  exhausted goal, and re-injects the active goal into context.

## What the port exercises

| capability | use |
|---|---|
| `fs` | the goal store (`goals.json`) in the plugin's own data directory |
| `session-read.info` | the working directory the store is keyed on |
| `inject.append-entry` | the custom `goal` message that reaches the model |

`log`, `session-control`, `ui`, `net-http`, `net-sockets` and `exec` are unused;
the built component does not import them (the component model prunes imports a
plugin never touches).

## Grant it needs

The state store is a real write, so the tool cannot record anything without
`fs`:

```bash
ki insmod --build plugins/wasm/kiqi-goal-guest --id rpi-goal --yes
ki plugin grant rpi-goal fs
```

`inject` is a host-owned capability, not a `ki plugin grant` capability: when a
run exists the host hands the plugin its injection surface, and when it does not
the injection is a non-fatal no-op (the goal is still recorded — see the
differences).

## Differences from the compile-time plugin

Stated, not hidden:

1. **State location + map keying.** The compiled plugin writes
   `<project>/.ki/goals.json`. The wasm `fs` jail only reaches the plugin's own
   data directory (`plugins/<id>/data/`), so the guest keeps a
   `{ "<project path>": Goals }` **map** in `plugins/rpi-goal/data/goals.json`,
   keyed on `session-read`'s `info().cwd`. The per-project semantics are the
   same; a project's `Goals` is stored, read and written exactly as before, only
   nested under its path.
2. **No cross-tree state sharing with the compiled plugin.** The compiled
   `rpi-goal` and this guest read different files, so a goal started in one is
   invisible to the other. In particular the **compiled `rpi-orchestrator`
   reads `.ki/goals.json`** and turns its orchestration guidance on for an
   active goal there — a goal started through this guest does **not** switch
   that guidance on.
3. **Keyed on a path string, not a canonicalized path.** The map key is
   `info().cwd` verbatim, and the `fs` jail does not canonicalize. Two spellings
   of one directory (macOS `/tmp/…` vs `/private/tmp/…`, or any symlinked
   checkout) are two keys and therefore **two separate goal states**.
4. **`TurnEnd` arrives untyped.** The WIT `event` enum has no turn-end tag, so
   the host delivers `EventTag::TurnEnd` as `event::other` with kind `"TurnEnd"`
   and the guest string-matches `other.kind`. A future host that renamed the tag
   would silently stop the hook firing (the same latent coupling the compile-time
   plugin has with its `EventTag`).
5. **Injection payload.** `inject.append-entry` forwards the parsed entry
   straight to `InjectApi::append_message`, so the guest passes the **bare**
   custom `AgentMessage` JSON — the same value the compile-time `CapsHost` hands
   to `append_message` — not the `{"message": …}` wrapper the C-ABI host builds
   before the `AppendEntry` runtime action.
6. **Writes deny, reads degrade.** Unchanged from the compiled plugin: a missing
   or unreadable store reads as "no goals", while a write with no `fs` grant
   fails the action with `denied: fs is not granted`. Injection failures stay
   non-fatal (`let _ =`), exactly as before, so `/goal` still works without an
   `inject` surface.
8. **Cross-tier takeover.** A stock `ki` compiles `rpi-goal` in, and per §6.20
   the compiled plugin owns both the `goal` tool and the `/goal` command —
   installing this guest alongside it refuses the guest's surfaces (the
   designed takeover guard, not a bug). The guest is for installs without the
   compiled plugin.
9. **Tool-result shape follows the plan-mode convention, not the compiled
   face.** The compile-time `kiqi` face passes the whole result envelope as one
   text block with `details` null; the guest splits it — human-readable text
   into the block, the structured envelope into `tool-result.details` — so the
   model-visible text differs between the tiers (the details gain structure).

The budget arithmetic, action dispatch, command grammar, fingerprint dedupe and
rendering are the compile-time logic, kept in the guest. The compiled plugin writes
   `goals.json.tmp-<pid>` and renames it over the target. The `fs` capability has
   no `rename`, so the guest truncate-writes `goals.json` in place; a crash
   mid-write can leave the whole map (every project's goals) unparseable, which
   then reads as "no goals". One file now holds every project, so the blast
   radius of a torn write is wider than in the compiled plugin.

The budget arithmetic, action dispatch, command grammar, fingerprint dedupe and
rendering are the compile-time logic, kept in the guest.

## Build

```bash
cargo build --manifest-path plugins/wasm/kiqi-goal-guest/Cargo.toml \
    --target wasm32-wasip2 --release
# → plugins/wasm/target/wasm32-wasip2/release/kiqi_goal_guest.wasm
```

There are no unit tests here: guest crates only link for `wasm32-wasip2`, so the
behaviour is verified through the host's `crates/kiqi-plugin-host/tests/e2e.rs`.
