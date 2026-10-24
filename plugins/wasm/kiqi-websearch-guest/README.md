# kiqi-websearch-guest

`kiqi-websearch` ported to the kiqi wasm ABI. The `websearch` tool searches public
web references with DuckDuckGo and falls back to Wikipedia.

## What the port exercises

`net-http`, against two keyless HTTPS endpoints:

| endpoint | use |
|---|---|
| `https://api.duckduckgo.com/` | primary — the Instant Answer API |
| `https://en.wikipedia.org/w/api.php` | fallback — Wikipedia search, when DDG yields fewer than `limit` hits |

A guest has no sockets, so both GETs go through the host capability and the
**user's allow-list** decides which of them it may reach:

```bash
ki insmod --build plugins/wasm/kiqi-websearch-guest --id kiqi-websearch \
    --host api.duckduckgo.com --host en.wikipedia.org
```

`--host '*'` grants any host. Everything that is pure — argument validation, the
hit extraction/filtering, the result envelope — stays in the guest; the host only
transports.

## The two hosts, and what degrades without each

Both hosts are required for the tool to work fully:

- **Only `api.duckduckgo.com` granted.** The primary call works; when the
  fallback is actually attempted (DDG returned fewer than `limit` hits — if DDG
  alone satisfies the limit, Wikipedia is never called and `failed` stays
  empty), it is denied and the result carries `"failed": ["wikipedia"]` while
  still returning DDG's hits. Graceful — you just get narrower coverage.
- **Only `en.wikipedia.org` granted.** This is *not* enough: the DuckDuckGo call
  is the primary source and, as in the compile-time plugin, its failure is fatal
  (the call errors rather than degrading). Grant both.

## Differences from the compile-time plugin

- **Result shape follows the trait face.** The two compile-time faces disagree:
  `src/kiqi.rs` (`kiqi_plugin::Plugin` — the actual kiqi contract) puts the raw
  result JSON `{query, count, results, failed}` in the block text and leaves
  `details` null, while `src/lib.rs` (the cdylib face) renders a human summary
  into the text and stashes the JSON in `details`. This port follows the **trait**
  face, so the block text is the JSON envelope and `details` is `null`.
- **No DNS/IP checks.** The compile-time plugin performs no host policy of its own
  either — the request goes straight out — so for this plugin the host
  allow-list is the *only* gate, exactly as the webfetch port notes. With
  `--host '*'` both endpoints are reachable wherever they resolve.
- **Responses travel through the host.** Where the compile-time plugin reads
  `reqwest`'s status/headers/body directly, the guest receives the capability's
  `{status, headers, body}` JSON. The host forwards the request headers
  (including the `user-agent` this plugin sets) and caps the body at 8 MiB; the
  guest reads only `status` and `body`. Because the status is now explicit, a
  non-2xx answer is treated as a failure up front (the compile-time plugin let
  `.json()` decide), which also lets the Wikipedia fallback distinguish a real
  error page from a result. The guest also sends `accept: application/json`
  alongside its `user-agent`; the host forwards both.
- **Cross-tier takeover.** The compiled `kiqi-websearch` is a default feature of
  a stock `ki`; per §6.20 its `websearch` tool owns the name, so installing this
  guest alongside it refuses the guest's tool (the designed takeover guard,
  not a bug). The guest is for installs without the compiled plugin.

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_plugin_guest::deny_contribution_exports!(…)`.

## Build

```bash
cargo build --manifest-path plugins/wasm/kiqi-websearch-guest/Cargo.toml \
    --target wasm32-wasip2 --release
```
