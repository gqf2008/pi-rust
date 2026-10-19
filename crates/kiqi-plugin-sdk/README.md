# kiqi-plugin-sdk

[![crates.io](https://img.shields.io/crates/v/kiqi-plugin-sdk.svg)](https://crates.io/crates/kiqi-plugin-sdk)
[![docs.rs](https://docs.rs/kiqi-plugin-sdk/badge.svg)](https://docs.rs/kiqi-plugin-sdk)

The stable `#[repr(C)]` ABI contract for ki plugins. Depend on this crate, and
nothing else, to build a Rust `cdylib` that extends the `ki` agent with tools,
providers, slash commands, event handlers, renderers and discovered resources.

This crate has **no dependency on the ki runtime** on purpose. A plugin must
not link the host — the host links the plugin. Both sides share only the types
defined here.

## Install

```toml
[package]
name = "my-kiqi-extension"

[lib]
crate-type = ["cdylib"]

[dependencies]
kiqi-plugin-sdk = "0.3"
serde_json = "1"
```

## Minimal plugin

```rust
use kiqi_plugin_sdk::{export_plugin, StbStringRef};

export_plugin!(|api| {
    // Optional: declare a load priority and the platforms this plugin supports.
    if let Some(declare) = api.declare {
        declare(StbStringRef::from_str(r#"{"priority":60,"platforms":["linux"]}"#));
    }

    // ... register tools, providers, commands and handlers through `api` ...

    0 // status code: 0 = success
});
```

The macro emits the `kiqi_plugin_register` symbol. The version lives *inside* the
`PluginApi` struct (`abi_version`, `struct_size`) rather than in the symbol name,
so the entrypoint signature does not change when the struct grows.

## Safe plugin helpers

`handles::ToolHandles<T>` keeps tool state inside the plugin and returns opaque
numeric `StepHandle` tokens. `get` retains an `Arc`, `lock` serializes state
updates, and `cancel` sets a separate atomic flag without waiting for a poll.
`remove` cancels and retires the token; repeated removal and unknown handles are
no-ops. An in-flight callback keeps its state alive until it returns. Workers
can share the flag through `insert_with_cancel`.

`event_handler!` defines an event callback with a tag-checked `EventView` and
panic containment. Its accessors return owned strings, so handlers never read
the ABI union directly or retain host pointers. Event buffers are borrowed
during fan-out and must not be freed by handlers.

```rust
kiqi_plugin_sdk::event_handler! {
    fn observe_message(event, _user_data) {
        if let Some(message) = event.message_text() {
            println!("{message}");
        }
        kiqi_plugin_sdk::EVENT_HANDLER_CONTINUE
    }
}
```

These helpers preserve the C ABI. They require an SDK release containing the
helpers; packages using a local path dependency must publish that SDK before
publishing dependent packages.

`string_handler!` adapts command, prompt and runtime-action callbacks to `&str`
inputs and a lifetime-bound `callbacks::Output`. `Output::write` handles null
slots and reclaims earlier writes without freeing host-owned input. The SDK's
`callbacks::free_string` is the producer-side ABI deallocator to register for
SDK-allocated outputs. `execute_handler!` adapts borrowed tool call IDs, and
`take_step_result!` copies and reclaims a plugin-owned result into the pointer-free
`callbacks::Step` enum. These are ABI boundaries: the host must still honor
buffer validity and producer allocation ownership.

The optional `process` feature provides `process::ProcessTree` for terminating
descendants and closing inherited pipes. Its Win32/libc calls and owned handles
stay in the SDK; packages can enable `forbid(unsafe_code)` while using the
platform-independent interface.

## Registered TUI text components

Use `RuntimeActionId::UiComponent` (22). Each plugin owns its own component IDs.
Register once, then send content updates without repeating the layout:

```json
{"op":"register","id":"monitor","type":"text","slot":"footer.after","order":10,"width":240,"maxHeight":1,"border":false,"visible":false}
{"op":"update","id":"monitor","text":"TPS 252 · TTFT 0.42s"}
{"op":"show","id":"monitor"}
{"op":"configure","id":"monitor","slot":"sidebar.right","width":40,"maxHeight":8}
{"op":"hide","id":"monitor"}
{"op":"remove","id":"monitor"}
```

Registration is idempotent and preserves existing content, visibility and layout.
`update` accepts `text` and optional `title`; layout fields are rejected. `configure`
changes layout while preserving text and visibility. Hidden components retain state.
Updating an unregistered ID returns an error. Plugin unload removes its components.
Use `RuntimeActionId::SetStatus` with `key` and `value` for legacy footer statuses;
the old `panel` payload has been removed.

Slots: `footer.after`, `sidebar.left`, `sidebar.right`, `overlay`.
Components sort by `order` (default 0), then plugin identity and ID.
Layout fields: `anchor`, `offsetX`, `offsetY`, `width`, `maxHeight`,
`minScreenWidth`, `border`. Anchors use the nine conventional positions from
`top-left` to `bottom-right`, with `left-center` and `right-center`.
Width is 4–240 columns, maxHeight is 1–80 rows. Content is limited to 64 lines,
2048 UTF-8 bytes per line and 256 bytes per title. Controls are stripped by the
renderer and Unicode display widths are respected. Each plugin may register 32
components. Invalid changes preserve the previous state.

Footer rows render below usage in both terminal modes. Sidebars reserve space in
fullscreen mode; native scrollback shows their content above the editor. Overlays
use the visible transcript viewport. Passive components do not change input focus.
Headless hosts store components without rendering them. Older hosts reject action 22;
plugins should handle that error. This API supports text components; interactive
questions continue to use `UiDialog`.

## Why a hand-written C ABI

Both sides are Rust, but they are compiled separately and may use different
compiler versions and different versions of this crate. That rules out passing
any Rust type that is not `#[repr(C)]` or that has a `Drop` impl. The design
rules the SDK enforces:

1. **Every crossing type is `#[repr(C)]`**, and enums in unions carry an explicit
   `#[repr(u32)]` so the discriminant width is pinned.
2. **No `Drop` type crosses.** No `Vec`, `String`, `serde_json::Value`, `Option`
   of those, or `Result`. Owned strings cross as `StbString` (pointer + length)
   together with an explicit `free_string` function the producer exports, so each
   allocation is freed exactly once, by the side that received it.
3. **Structured data crosses as JSON** in a `StbString`. `serde_json` must be
   configured consistently on host and plugin (`preserve_order` +
   `arbitrary_precision`), or integers beyond `u64`/`i64` lose precision and
   object key order can change.
4. **Unions hold `Copy` payloads only**, so a variant read is `unsafe` and the
   caller discriminates on the tag.
5. **Unwinding never crosses the boundary.** Every call is `extern "C"`, and both
   sides contain panics with `catch_unwind`; a panic is converted into a status
   code rather than aborting the process.

These are not stylistic preferences — violating any of them is undefined
behaviour at the boundary.

## Compatibility

The host loader only resolves `kiqi_plugin_register`, exported with
`export_plugin!`. The host passes a `PluginApi` containing registration slots,
`runtime_action` and `declare` (priority and platforms).

The SDK checks `abi_version` against `KI_PLUGIN_ABI_VERSION_UNIFIED` and
requires `struct_size` to cover the `PluginApi` expected by the plugin before
calling its registration body. A mismatch returns a nonzero status and the
host reports a load failure. Extensions using retired entrypoints must be
updated to `export_plugin!` and rebuilt with the current SDK.

## Versioning

A plugin depends on `kiqi-plugin-sdk` only, so its version is decoupled from the
rest of the family. ABI compatibility is checked through `abi_version` and
`struct_size`; each optional capability must also be checked before use.
Rebuild extensions when the SDK contract they require changes.

## Examples and docs

- [`examples/plugin-stub`](https://github.com/gqf2008/pi-rust/tree/main/examples/plugin-stub) —
  a complete plugin registering an `echo` tool with the full four-function
  lifecycle (`execute` → `poll` → `cancel` → `destroy`), an event handler, and a
  resource-discovery handler.
- [Extension authoring guide](https://github.com/gqf2008/pi-rust/blob/main/docs/extensions/authoring.md) —
  templates, safety rules, testing and release checklist.
- Host-side loader: [`kiqi-extensions`](https://crates.io/crates/kiqi-extensions).
- Ready-to-install extensions:
  [`pi-rust/rpi-package`](https://github.com/pi-rust/rpi-package).

## License

MIT. See [LICENSE](https://github.com/gqf2008/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/gqf2008/pi-rust/blob/main/NOTICE).

## Exclusive stdio servers

During registration, `declare {"ownsStdio":true}` requests exclusive ownership
of the process's stdin/stdout. Each plugin has its own runtime context; two
owners cause a startup error, including when their file stems are identical.
Failed registration releases its claim. The host leaves stdio untouched and
runs the normal session lifecycle without rendering a TUI.

Use `RuntimeActionId::StdioLifecycle` (21) with `{"op":"status"}` to check
support and with `{"op":"finish","exitCode":0}` to signal EOF or server
completion. Only the owner can finish, and early completion is retained. The
host dispatches `SessionShutdown` and returns the code. Stop and join plugin
workers in that handler; do not terminate the process from a plugin.

The optional `stdio` feature provides `stdio::StdinLines::poll_line`. It polls
pipe/file input without a background stdin task that can block shutdown.
Wait between `InputLine::Pending` results using the server's own cancellation
mechanism. The caller must exclusively own stdin. Windows console input is
unsupported by this pipe/file server helper.

## Dialog purpose

`UiDialog` (17) keeps its open/poll/cancel protocol. A presentation `kind` such
as `selector` does not imply permission. Omitted `ui.purpose` means `question`;
`purpose: "permission"` requires a nonempty `toolCallId`, a flat single-choice
`confirm`/`selector` and explicit decision metadata on every option:

```json
{
  "op": "open",
  "requestId": "approval-1",
  "toolCallId": "call-1",
  "ui": {
    "purpose": "permission",
    "kind": "confirm",
    "question": "Allow this tool?",
    "options": [
      {"title": "Allow once", "permission": "allow_once"},
      {"title": "Reject", "permission": "reject_once"}
    ]
  }
}
```

Decisions are `allow_once`, `allow_always`, `reject_once`, or `reject_always`.
Titles are nonempty, unique and have no surrounding whitespace. Permission
questions do not accept nested questions, multiple answers or freeform input.
`dialogs::validate` and `dialogs::permission_options` share these checks between
hosts and adapters. Answers retain the selected original title in `values`.
Transport adapters that cannot show ordinary questions cancel them explicitly;
they must not reinterpret them as permission choices.
