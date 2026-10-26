# kiqi-langfuse

A **representative subset** of `kiqi-langfuse` ported to wasm, to validate the
"high-frequency events + outgoing HTTP" half of the ABI. It is deliberately not
the whole extension — the three management tools (score/prompt/trace) and the full
OTLP/usage modelling stay in the compile-time extension.

It builds a turn trace (`Conversational Turn` root span + one `LLM Call`
generation) from the event stream and flushes it to Langfuse's OTLP endpoint
through the `net-http` capability. Configuration arrives through the
`langfuse_config` tool (the real host would populate it from settings; the guest
does not read the process environment).

Build:

```sh
cargo build --manifest-path extensions/wasm/kiqi-langfuse/Cargo.toml \
  --target wasm32-wasip2 --release
```

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_wasm_bindings::deny_contribution_exports!(…)`.
