# kiqi-rpi-lens-guest

`rpi-lens` ported to the kiqi wasm ABI (`kiqi:plugin@0.6.0`): the `code_lens`
tool runs one allowlisted local check — `git diff --check`, `cargo check`,
`cargo fmt -- --check`, or `cargo clippy -- -D warnings` — through the host's
`exec` capability instead of spawning the process itself.

## Grant it needs

The tool cannot run anything until the user grants `exec`, and a diagnostics
plugin wants the *project* cwd scope:

```bash
ki plugin install kiqi_rpi_lens_guest.wasm --id rpi-lens
ki plugin grant rpi-lens exec --command cargo --command git --cwd project --timeout 120
```

- `--command cargo --command git` — the only programs the tool ever runs
  (bare names, matched exactly).
- `--cwd project` — the child starts in the project directory (the session's
  working directory). The default `plugin-data` scope would run the checks in
  the plugin's own data directory, which is not your project.
- `--timeout 120` — `cargo clippy` does not fit in the 30s default; the budget
  is capped at 300s.

Note what the jail is: it bounds where the child *starts*. The child still
runs with your privileges and can path anywhere — the grant's allow-list and
scope are about keeping the plugin's *intent* legible, not sandboxing `cargo`.

## Differences from the compile-time plugin

Stated, not hidden:

1. **No `timeoutSeconds` parameter.** The time budget rides with the exec
   grant (`--timeout`), not with the call — the caller cannot out-bid the
   user's budget. The schema drops the field rather than accept it silently.
2. **`cwd` is scoped.** The compiled plugin accepted any absolute path on the
   machine. Here a cwd is relative (resolved inside the granted scope root) or
   absolute *inside the project directory*; anything else is refused.
3. **Timeouts surface as `… hit the exec time budget`** instead of
   "timed out after {n}s", because the number lives in the grant.

## Build

```bash
cargo build --manifest-path plugins/wasm/kiqi-rpi-lens-guest/Cargo.toml \
    --target wasm32-wasip2 --release
# → plugins/wasm/target/wasm32-wasip2/release/kiqi_rpi_lens_guest.wasm
```
