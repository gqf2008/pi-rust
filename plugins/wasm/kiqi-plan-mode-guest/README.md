# kiqi-plan-mode-guest

`rpi-plan-mode` ported to the kiqi wasm ABI. The plugin swaps the active tool set
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

## Sharing one implementation

The logic (which tools survive, what `/plan` prints, how the outline is parsed)
lives in `rpi-plan-mode-core`, the same crate the compile-time plugin uses. Only
the `PlanHost` adapter differs: `HostCaps` there, the WIT imports here.

## Build

```bash
cargo build --manifest-path plugins/wasm/kiqi-plan-mode-guest/Cargo.toml \
    --target wasm32-wasip2 --release
```
