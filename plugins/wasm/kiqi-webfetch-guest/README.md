# kiqi-webfetch-guest

`rpi-webfetch` ported to the kiqi wasm ABI. The tool fetches a public HTTP(S) URL
and returns bounded readable text (HTML reduced to text).

## What the port exercises

`net-http`. The compile-time plugin builds its own `reqwest` client; a guest has
no sockets, so the request goes through the host capability and the **user's
allow-list** decides where it may go:

```bash
ki insmod --build plugins/wasm/kiqi-webfetch-guest --id rpi-webfetch \
    --host api.example.com --host docs.rs
```

`--host '*'` grants any host; the plugin's own URL policy still refuses
`localhost`, embedded credentials, non-HTTP schemes and literal private/loopback
IPs.

## Two differences from the compile-time plugin

- **No DNS check.** The compile-time version resolves the hostname and refuses
  one that lands on loopback/RFC1918, on top of the literal-IP check. A guest
  cannot resolve, so only the literal check survives — the allow-list is what
  constrains a hostname here.
- **No final-URL check.** The host answers with status/headers/body rather than
  the URL it ended on. Redirects are still safe: the host refuses a hop to a host
  that is not allow-listed, and hands the 3xx back instead.

## Build

```bash
cargo build --manifest-path plugins/wasm/kiqi-webfetch-guest/Cargo.toml \
    --target wasm32-wasip2 --release
```
