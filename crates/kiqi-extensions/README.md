# kiqi-extensions

[![crates.io](https://img.shields.io/crates/v/kiqi-extensions.svg)](https://crates.io/crates/kiqi-extensions)
[![docs.rs](https://docs.rs/kiqi-extensions/badge.svg)](https://docs.rs/kiqi-extensions)

The host-side dispatch spine for `ki` plugins: the registry every tier
registers through, the event funnels that translate runtime events into plugin
events, and the compiled/wasm sink the host installs. Plugins now ship as wasm
guests (`kiqi-plugin-host`); the cdylib loader this crate used to carry was
retired on 2026-10-26.

## Install

```toml
[dependencies]
kiqi-extensions = "0.3"
```

Most users never depend on this directly — the `ki` CLI uses it, and the
harness consumes its adapters through injection.

## What it does

- **Registry.** `ExtensionRegistry` holds what a plugin registered: tools,
  providers, handlers, renderers and resource-discovery callbacks, with a
  deterministic snapshot the runtime can read.
- **Lifecycle events.** Dispatch wrappers for data, empty and lifecycle events,
  a veto path that lets a handler refuse startup, and a tee emitter so a plugin
  can observe events without stealing them from the TUI.
- **Provider hooks.** `ExtensionProviderHooks` lets a plugin observe (and patch)
  provider requests; `PluggableProvider` is the retired cdylib provider adapter.
- **C-ABI bus types.** `kiqi-extensions::abi` carries `StbString`,
  `StablePluginEvent`/`EventView` and the handler constants as the host's
  internal wire format (no `libloading` any more).

## Crate dependencies

`kiqi-extensions` depends on `kiqi-plugin`, `kiqi-ai` and `kiqi-agent` only —
never on `kiqi-harness`. The harness receives these adapters as trait objects, so
the dependency graph stays acyclic.

## Installing and developing

wasm guests install with `ki insmod` (see
[`plugins/kiqi-plugin.wit`](../../plugins/kiqi-plugin.wit)); the cdylib
`ki install` / `ki dev` workflow no longer exists.

## Docs

- Crate docs: <https://docs.rs/kiqi-extensions>
- [Extension authoring guide](https://github.com/gqf2008/pi-rust/blob/main/docs/extensions/authoring.md)
- [Plugin contract](https://github.com/gqf2008/pi-rust/blob/main/docs/design/plugin-abi.md)
- [Ready-to-install extensions](https://github.com/pi-rust/rpi-package)

## License

MIT. See [LICENSE](https://github.com/gqf2008/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/gqf2008/pi-rust/blob/main/NOTICE).
