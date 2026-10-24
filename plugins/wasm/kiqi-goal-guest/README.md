# kiqi-goal-guest

`kiqi-goal` ported to the kiqi wasm ABI. Codex-style goal mode: one persistent,
context-injected objective per project that the model keeps advancing across
turns. The plugin is three surfaces over one core:

- the `goal` tool — eight actions: `start`, `update`, `pause`, `resume`,
  `complete`, `limit`, `show`, `list`;
- the `/goal` slash command (plain-text rendering of the same operations);
- a `TurnEnd` hook that charges a turn against the budget, auto-pauses an
  exhausted goal, and re-injects the active goal into context.

State lives in `<project>/.ki/goals.json` in the compiled shape
(`{"active": …|null, "archived": […]}`) — the exact file and shape the compiled
`kiqi-orchestrator` reads, so an active goal started here turns the compiled
orchestration guidance on just as the compiled `kiqi-goal` would. (Before `fs`
gained a scope — a grant-model change, not an ABI one —
the guest kept a `{ "<project>": Goals }` map in its own data directory and
this coupling was broken; §"Differences" records the one-time migration.)

## What the port exercises

| capability | use |
|---|---|
| `fs` (`--cwd project`) | the project file `.ki/goals.json` |
| `session-read.info` | the working directory, needed only for the one-time pre-scope migration |
| `inject.append-entry` | the custom `goal` message that reaches the model |

`log`, `session-control`, `ui`, `net-http`, `net-sockets` and `exec` are unused;
the built component does not import them (the component model prunes imports a
plugin never touches).

## Grant it needs

The state file is a real write in the project tree, so the grant must be scoped
there:

```bash
ki insmod --build plugins/wasm/kiqi-goal-guest --id kiqi-goal --yes
ki plugin grant kiqi-goal fs --cwd project
```

The `--cwd project` grant is the load-bearing line, and `insmod` does not
establish it. `insmod` only prompts for a capability the plugin does not already
hold, so an upgrade from the pre-scope build — which already had `fs`, jailed to
its own data directory at the `plugin-data` default — is not re-scoped by the
`insmod` line alone. Skip the grant line and the guest keeps reading and writing
its data directory; the coupling stays off with no warning.

`inject` is a host-owned capability, not a `ki plugin grant` capability: when a
run exists the host hands the plugin its injection surface, and when it does not
the injection is a non-fatal no-op (the goal is still recorded — see the
differences).

## Differences from the compile-time plugin

Stated, not hidden:

1. **`TurnEnd` arrives untyped.** The WIT `event` enum has no turn-end tag, so
   the host delivers `EventTag::TurnEnd` as `event::other` with kind `"TurnEnd"`
   and the guest string-matches `other.kind`. A future host that renamed the tag
   would silently stop the hook firing (the same latent coupling the compile-time
   plugin has with its `EventTag`).
2. **Injection payload.** `inject.append-entry` forwards the parsed entry
   straight to `InjectApi::append_message`, so the guest passes the **bare**
   custom `AgentMessage` JSON — the same value the compile-time `CapsHost` hands
   to `append_message` — not the `{"message": …}` wrapper the C-ABI host builds
   before the `AppendEntry` runtime action.
3. **Writes deny, reads degrade.** Unchanged from the compiled plugin: a missing
   or unreadable store reads as "no goals", while a write with no `fs` grant
   fails the action with `denied: fs is not granted`. Injection failures stay
   non-fatal (`let _ =`), exactly as before, so `/goal` still works without an
   `inject` surface.
4. **The write is not atomic.** The compiled plugin writes `goals.json.tmp-<pid>`
   and renames it over the target. The `fs` capability has no `rename`, so the
   guest truncate-writes the project file in place; a crash mid-write can leave
   it unparseable, which then reads as "no goals". The file now holds one
   project's goals, so the blast radius of a torn write is that one project (the
   pre-scope map put every project in one file).
5. **Pre-scope migration is best-effort.** Before the `fs` grant gained a scope
   the state was a `{ "<project path>": Goals }` map in the plugin's own data
   directory. On first
   use, if the project file does not exist yet and that old store is reachable
   through the current `fs` root, the project's entry is written out to
   `.ki/goals.json`. In the shipped layout the data directory sits outside the
   project tree, so it is not reachable and the guest starts fresh (the e2e
   migration fixture arranges the data directory inside the project root to
   exercise the path). The old store is never written again.
6. **Cross-tier takeover.** A stock `ki` compiles `kiqi-goal` in, and per §6.20
   the compiled plugin owns both the `goal` tool and the `/goal` command —
   installing this guest alongside it refuses the guest's surfaces (the
   designed takeover guard, not a bug). The guest is for installs without the
   compiled plugin.
7. **Tool-result shape follows the plan-mode convention, not the compiled
   face.** The compile-time `kiqi` face passes the whole result envelope as one
   text block with `details` null; the guest splits it — human-readable text
   into the block, the structured envelope into `tool-result.details` — so the
   model-visible text differs between the tiers (the details gain structure).
8. **The active-goal injection carries an orchestration hint, conditionally.**
   With an active goal the injected custom message gains
   「推进方式：用 `plan` 写 DAG、`dispatch` 跑、节点交给 `subagent`。」— but only
   when the session's tool face actually carries all three tools (read through
   `session-read.active-tools`). An install without the orchestrator stays
   silent rather than naming tools the model cannot call.

The budget arithmetic, action dispatch, command grammar, fingerprint dedupe and
rendering are the compile-time logic, kept in the guest.

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_plugin_guest::deny_contribution_exports!(…)`.

## Build

```bash
cargo build --manifest-path plugins/wasm/kiqi-goal-guest/Cargo.toml \
    --target wasm32-wasip2 --release
# → plugins/wasm/target/wasm32-wasip2/release/kiqi_goal_guest.wasm
```

There are no unit tests here: guest crates only link for `wasm32-wasip2`, so the
behaviour is verified through the host's `crates/kiqi-plugin-host/tests/e2e.rs`.
