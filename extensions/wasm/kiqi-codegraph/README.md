# kiqi-codegraph

`kiqi-codegraph` ported to the kiqi wasm ABI (`kiqi:extension@0.9.0`). The same
eight `codegraph_*` tools, with the process plumbing replaced by the two
capabilities that fit it:

| step | the compiled extension | this port |
|---|---|---|
| is there an index? | `stat`s `<root>/.codegraph/` | `test -d .codegraph` through **`exec`** (the guest's `fs` is jailed to its own data dir — it cannot see the project tree) |
| first index | `codegraph init <root>` | `exec` |
| refresh | `codegraph sync <root>` | `exec` |
| the search/analysis session | spawns `codegraph serve --mcp`, talks JSON-RPC over its stdio, kills it at the end of the call | **`proc`** — same flow, and the handle dies with the tool call by construction |

The eight tool schemas, the JSON-RPC sequence (initialize → `tools/call`),
the reply matching by id, the output truncation and the diagnostic redaction
are the compile-time logic, kept in the guest.

## Grant it needs

```bash
ki insmod --build extensions/wasm/kiqi-codegraph --id kiqi-codegraph --yes
ki extension grant kiqi-codegraph exec --command codegraph --command test --cwd project --timeout 300
ki extension grant kiqi-codegraph proc --command codegraph --cwd project --timeout 600
```

- `test` is in the **exec** list because the index probe is a `test -d` run,
  not a `stat` (see the differences).
- `--cwd project` on both: the commands must run in the user's project, which
  is what `exec`/`proc`'s project scope is for.
- Two grants for one program is the designed shape: `exec` bounds a one-shot
  command, `proc` bounds an interactive session, and the user authorizes each
  separately.

## Differences from the compile-time extension

Stated, not hidden:

1. **`.codegraph/` is probed with `test`, not `stat`.** Needs `test` in the exec
   allow-list. A host without a `test` binary (Windows) makes the probe fail
   and the port says so instead of guessing.
2. **The command is a bare name.** The compiled version resolves the real path
   through PowerShell on Windows because npm/Scoop shims do not resolve from
   `Command::new("codegraph")`; `exec`/`proc` take bare names only, so a Windows
   shim cannot be resolved here — that platform stays on the compiled extension.
3. **`projectPath` must be inside the session's directory.** The compiled
   version accepts any absolute path on the machine; the guest resolves it
   lexically against `session-read`'s `info().cwd` and refuses anything outside
   with a message naming both paths (the host's `exec`/`proc` scope could not
   reach it either).
4. **The 20-second session timeout is per read.** `proc.read-line` takes a
   timeout (`0` = the child's whole budget), so a silent server still produces
   the compiled version's message ("…timed out after 20 seconds; try
   `codegraph unlock`") instead of blocking until the grant's budget runs out.
5. **Failure messages now name the subcommand** (`codegraph init: …`,
   `codegraph sync: …`, `test -d .codegraph: …`) so a refusal or a broken index
   says which step needed what. `clientInfo.version` is this crate's version.

## Testing

`crates/kiqi-extension-host/tests/e2e.rs` covers the tool surface, the id-probe
decision (with and without an index, asserted on which subcommand the missing
grant stops) and — when a real `codegraph` is on `PATH` — a live session
(`codegraph_guest_live_session_when_asked`; it says so and returns when the CLI
is absent, rather than pretending the protocol path was covered).

There are no unit tests here: guest crates only link for `wasm32-wasip2`.

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_wasm_bindings::deny_contribution_exports!(…)`.

## Build

```bash
cargo build --manifest-path extensions/wasm/kiqi-codegraph/Cargo.toml \
    --target wasm32-wasip2 --release
```
