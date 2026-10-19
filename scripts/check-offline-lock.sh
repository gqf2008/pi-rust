#!/bin/sh
# Fail when the locked dependency graph does not cover every feature.
#
# `Cargo.lock` is committed, so `--locked --offline` should always work: the graph
# already names every package, for every feature. When it does not, the symptom is
# one-sided and easy to miss — CI is online, so it just fetches the new crate and
# passes; a developer who builds offline (this repo's `.cargo/config.toml` sets
# `offline = true` on the machines that have it, and it is *not* committed) sees a
# resolution error that looks like a local misconfiguration instead of a missing lock
# entry.
#
# The check is deliberately cheap: resolve the graph for `--all-features` without
# touching the network. Whatever `cargo check` would need, it must already be named.
set -eu
exec cargo check --workspace --all-targets --all-features --locked --offline "$@"
