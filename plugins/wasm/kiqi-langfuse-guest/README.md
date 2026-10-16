# kiqi-langfuse-guest

A **representative subset** of `kiqi-langfuse` ported to wasm, to validate the
"high-frequency events + outgoing HTTP" half of the ABI. It is deliberately not
the whole plugin — the three management tools (score/prompt/trace) and the full
OTLP/usage modelling stay in the compile-time plugin.

It builds a turn trace (`Conversational Turn` root span + one `LLM Call`
generation) from the event stream and flushes it to Langfuse's OTLP endpoint
through the `net-http` capability. Configuration arrives through the
`langfuse_config` tool (the real host would populate it from settings; the guest
does not read the process environment).

Build:

```sh
cargo build --manifest-path plugins/wasm/kiqi-langfuse-guest/Cargo.toml \
  --target wasm32-wasip2 --release
```
