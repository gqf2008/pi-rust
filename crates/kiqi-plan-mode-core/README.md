# kiqi-plan-mode-core

Plan-mode logic for the `kiqi-plan-mode` wasm guest
(`plugins/wasm/kiqi-plan-mode-guest`).

Why this is a separate crate: wasm guest crates link only for
`wasm32-wasip2` and **cannot carry unit tests**, so the mode's behaviour —
which tools survive a plan, what `/plan` prints, how a plan is persisted and
restored — lives here as a plain library with native tests, and the guest is a
thin shim over the WIT `session-read` / `session-control` / `inject` imports.

`PlanHost` is the seam: five session operations the guest provides, everything
else implemented here. Nothing in the `ki` binary links this crate; it is a
workspace member only so `cargo test --workspace` runs its unit tests.
