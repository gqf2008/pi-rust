# kiqi-plan-mode

`kiqi-plan-mode` ported to the kiqi wasm ABI. The extension swaps the active tool set
so the model plans before it edits, persists the finished plan as a custom
session entry, and restores it on session start / tree change.

## What the port exercises

Four session capabilities, and no network or filesystem:

| capability | use |
|---|---|
| `session-read.active-tools` | remember what to restore |
| `session-read.session-branch` | re-read the plan after a session or tree change |
| `session-control.set-active-tools` | enter/leave the read-only planning set |
| `inject.append-custom-entry` | persist `{active, plan}` as a `plan-mode` entry |
| `inject.send-user-message` | submit `/plan <request>` as a planning prompt |

## What the port found

The WIT had `inject.append-entry`, which is `append_message` — but plan mode
persists a **custom-typed** entry, and the compile-time `InjectApi` has a
separate `append_custom_entry` for it. There was no wasm equivalent, so the port
added `inject.append-custom-entry` to the ABI (0.6.0).

## One implementation, inside this crate

The logic (which tools survive, what `/plan` prints, how the outline is parsed)
lives in `src/plan.rs`; `src/lib.rs` is only the `PlanHost` adapter over the WIT
imports. There used to be a separate `kiqi-plan-mode-core` crate so the logic
could carry native unit tests — that turned out to be unnecessary: wit-bindgen
generates host-target stubs, so `cargo test -p kiqi-plan-mode` (from
`extensions/wasm`) runs the pure-logic tests on the host while the
`wasm32-wasip2` build remains the shipped artifact.

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_wasm_bindings::deny_contribution_exports!(…)`.

## Build

```bash
cargo build --manifest-path extensions/wasm/kiqi-plan-mode/Cargo.toml \
    --target wasm32-wasip2 --release
```
