# kiqi-lens-guest

`kiqi-lens` ported to the kiqi wasm ABI (`kiqi:plugin@0.8.0`): the `code_lens`
tool runs one allowlisted local check — `git diff --check`, `cargo check`,
`cargo fmt -- --check`, or `cargo clippy -- -D warnings` — through the host's
`exec` capability instead of spawning the process itself.

## Grant it needs

The tool cannot run anything until the user grants `exec`, and a diagnostics
plugin wants the *project* cwd scope:

```bash
ki insmod --build plugins/wasm/kiqi-lens-guest --id kiqi-lens --yes
ki plugin grant kiqi-lens exec --command cargo --command git --cwd project --timeout 120
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
4. **The child runs with a cleared environment.** The host's exec clears the
   environment and re-adds only `PATH`, so the ported check runs without
   `CARGO_HOME`/`RUSTUP_HOME`/`RUSTFLAGS`/proxy variables the compiled plugin
   inherited. On a stock rustup/cargo setup the defaults still resolve; a
   project that *requires* those variables builds differently — that's a
   deliberate property of the exec capability, not a per-plugin choice.
5. **Malformed JSON arguments are an error.** The compiled plugin parsed
   unparseable arguments as `null` and ran the default `diff` check; the port
   refuses with `invalid arguments` instead of running a check nobody asked
   for.

Known caveats on the `cwd` mapping (all fail-safe: wrong refusal, never a
wrong grant): the guest strips an absolute cwd against the session's
`info().cwd` *lexically*, so a path that reaches the project through a symlink
spelling (macOS `/tmp/…` vs `/private/tmp/…`) is refused — pass a relative
path instead. And when an embedder overrides the host's `Options::project_dir`
to something other than the process cwd, the guest's strip anchor and the
host's jail root diverge; the default `ki` embedding never does this.

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_plugin_guest::deny_contribution_exports!(…)`.

## Build

```bash
cargo build --manifest-path plugins/wasm/kiqi-lens-guest/Cargo.toml \
    --target wasm32-wasip2 --release
# → plugins/wasm/target/wasm32-wasip2/release/kiqi_lens_guest.wasm
```
