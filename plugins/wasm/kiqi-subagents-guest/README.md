# kiqi-subagents-guest

`kiqi-subagents` ported to the kiqi wasm ABI (`kiqi:plugin@0.7.1`). It registers
one tool, `delegate_task`, which validates its arguments and returns a bounded,
non-recursive **delegation envelope** — a JSON description of the work a host
might schedule. It imports **no capability**: the whole behaviour is argument
validation and JSON shaping, so nothing needs granting.

## Grant it needs

None. This port declares no capability, so `ki plugin install` has nothing to
ask for and the tool is usable immediately:

```bash
cargo build --manifest-path plugins/wasm/kiqi-subagents-guest/Cargo.toml \
    --target wasm32-wasip2 --release
ki insmod --build plugins/wasm/kiqi-subagents-guest --id kiqi-subagents --yes
```

## The envelope is byte-stable

The tool emits exactly the bytes the compile-time plugin emits:

```json
{"kind":"kiqi-delegation","task":"review","role":"worker","maxTurns":32,"timeoutSeconds":1800,"recursive":false}
```

Field order, clamping (`maxTurns` 1–32 default 8, `timeoutSeconds` 10–1800
default 300, `role` default `worker`) and the two error strings are all kept
identical, and the e2e test compares the envelope byte for byte.

## Differences from the compile-time plugin

Stated, not hidden:

1. **No real spawn — and wasm is not where one would go.** The current code
   spawns nothing either: its README says "scheduling remains a host
   responsibility", and the tool only *emits the envelope*. The aspirational
   path (actually launching a child agent) is not available here. `exec` could
   run a bounded one-shot (default 30s, capped at 300s), but the delegation
   contract is a session-level child that outlives a tool call, and a wasm guest
   has no handle that lives between calls (plugin-abi.md §6.2 reaches the same
   conclusion for `kiqi-orchestrator`). A true spawner stays compile-time work.

2. **Cross-tier tool-name takeover (§6.20).** This guest declares the same id
   (`kiqi-subagents`) and the same tool name (`delegate_task`) as the compiled
   plugin. `ki` merges three tiers into one tool table (builtin → cdylib →
   compiled → wasm) and §6.20 makes a collision **asymmetric**: a trusted layer
   may override a name, but an *untrusted* (wasm) host that hits an occupied name
   is refused with a warning. So installed alongside a `ki` that already ships
   the compiled `kiqi-subagents`, the compiled `delegate_task` keeps the name and
   **this guest's tool is the one refused**. That is the designed takeover guard
   (a third-party wasm plugin must not be able to shadow a first-party tool),
   not a bug. Run this guest on a build without the compiled plugin to exercise
   it.

3. **Field order is pinned explicitly, not inherited from `serde_json`.** The
   compile-time workspace enables serde_json's `preserve_order`
   (`Cargo.toml`: `serde_json = { version = "1", features = ["preserve_order", "raw_value"] }`),
   so its `json!` serializes keys in insertion order — `kind, task, role,
   maxTurns, timeoutSeconds, recursive`. This guest workspace does **not**
   declare that feature, and a build resolves serde_json with default features
   (verified: `features: ["default","std"]`), where `json!` would sort the same
   keys — `kind, maxTurns, recursive, role, task, timeoutSeconds` — and change
   the bytes. (The checked-in `plugins/wasm/Cargo.lock` currently lists
   `indexmap` under serde_json, but that is a lock superset and not a guarantee:
   feature resolution is what matters.) Rather than bet the byte contract on an
   ambient flag, the guest writes the envelope with an explicit field order and
   still passes the two string values through `serde_json` for correct escaping.
   The output is identical to the compile-time plugin; the difference is *why*
   it is stable.

## Build

```bash
cargo build --manifest-path plugins/wasm/kiqi-subagents-guest/Cargo.toml \
    --target wasm32-wasip2 --release
```
