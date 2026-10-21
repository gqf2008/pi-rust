//! A spawned shell must not receive the host's private plumbing.
//!
//! Two families of variables are in scope, and both are answered by the same
//! exclusion:
//!
//! * `KI_SESSION_ID` — the host writes it so plugins can name the session they
//!   observe. A bash/powershell child inherits the environment by default, so
//!   without an exclusion it reaches *everything* the shell runs, including a
//!   nested `ki`, which would then report an ancestor's session.
//! * `LANGFUSE_PI_PARENT_*` — an extension publishes these so a *launcher* can
//!   attach a nested `ki` to the turn that spawned it (the extension looks at
//!   `LANGFUSE_PI_PARENT_TRACE_ID` to decide it is a subagent). Inherited by a
//!   plain shell they are a false positive: a nested `ki` adopts its
//!   ancestor's trace instead of opening its own.
//!
//! `KI_AGENT_ID` is the one name that is deliberately **visible**: it carries
//! the same value as `KI_SESSION_ID` to a different audience, the commands the
//! shell runs, so a child can name the session it belongs to
//! (`walgit collab claim --session "$KI_AGENT_ID"`). A nested `ki` still cannot
//! adopt it, because nothing in `ki` reads it.
//!
//! These run against a real shell, because the property under test is about the
//! process environment of a real child — an in-memory fake spawns nothing and
//! could not observe it.

use kiqi_tools::{is_host_private_env, OsExecutionEnv, Shell, ShellExecOptions};

/// Serialises the tests that write `KI_SESSION_ID` into this process.
///
/// Two tests here set and then remove it. Without a lock they interleave, and the
/// dangerous direction is the *quiet* one: `a_host_private_variable_does_not_reach_the_shell`
/// asserts the child sees the variable **empty**, so a neighbour removing it first
/// makes that assertion pass without the exclusion being exercised at all. The
/// test would keep "passing" after the exclusion broke.
///
/// (Verified load-bearing: emptying `HOST_PRIVATE_ENV` still fails it with the lock
/// held — see the mutation noted in the commit message.)
static SESSION_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`SESSION_ENV_LOCK`]; poisoning is ignored because a panicking test says
/// nothing about the environment.
fn lock_session_env() -> std::sync::MutexGuard<'static, ()> {
    SESSION_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Skip when the platform has no usable shell (the env resolves git-bash on
/// Windows, `/bin/bash` on POSIX). Mirrors the guard other shell tests use.
async fn shell_or_skip() -> Option<OsExecutionEnv> {
    let env = OsExecutionEnv::new();
    match env.exec("true", ShellExecOptions::default()).await {
        Ok(_) => Some(env),
        Err(error) => {
            eprintln!("skipping: no usable shell ({error})");
            None
        }
    }
}

#[tokio::test]
async fn a_host_private_variable_does_not_reach_the_shell() {
    let Some(env) = shell_or_skip().await else {
        return;
    };

    let _env = lock_session_env();
    std::env::set_var("KI_SESSION_ID", "01a0f60b-parent-session");
    std::env::set_var(kiqi_plugin_sdk::AGENT_ID_ENV, "01a0f60b-parent-session");

    // A variable the host does NOT mark private still inherits, so this is a
    // targeted exclusion rather than "the shell sees no environment".
    std::env::set_var("KI_TOOLS_INHERIT_PROBE", "visible");

    let out = env
        .exec(
            r#"printf 'session=[%s] agent=[%s] probe=[%s]\n' "${KI_SESSION_ID-unset}" "$KI_AGENT_ID" "$KI_TOOLS_INHERIT_PROBE""#,
            ShellExecOptions::default(),
        )
        .await
        .expect("exec");

    assert_eq!(
        out.stdout.trim(),
        "session=[unset] agent=[01a0f60b-parent-session] probe=[visible]",
        "the host-private variable must not inherit (not even as an empty value), \
         the agent channel must reach the shell, and ordinary ones must inherit"
    );

    std::env::remove_var("KI_TOOLS_INHERIT_PROBE");
    std::env::remove_var(kiqi_plugin_sdk::AGENT_ID_ENV);
    std::env::remove_var("KI_SESSION_ID");
}

#[tokio::test]
async fn an_explicit_private_variable_still_reaches_the_shell() {
    let Some(env) = shell_or_skip().await else {
        return;
    };

    // The exclusion is about *inheriting*, not about forbidding the name: a
    // caller that deliberately passes a value (a test fixture, a wrapper script)
    // must still be able to. Per-call env is applied after the inherit loop.
    let _env = lock_session_env();
    std::env::set_var("KI_SESSION_ID", "01a0f60b-parent-session");

    let mut per_call = std::collections::HashMap::new();
    per_call.insert("KI_SESSION_ID".to_string(), "01a0f60b-explicit".to_string());
    let out = env
        .exec(
            r#"printf 'session=[%s]\n' "$KI_SESSION_ID""#,
            ShellExecOptions {
                env: Some(per_call),
                ..ShellExecOptions::default()
            },
        )
        .await
        .expect("exec");

    assert_eq!(out.stdout.trim(), "session=[01a0f60b-explicit]");

    std::env::remove_var("KI_SESSION_ID");
}

#[test]
fn the_private_list_covers_what_the_contract_declares() {
    // Derived from the SDK's declarations, not restated: the point of moving
    // these names out of literals was that a copy can drift. Iterating the
    // declared slice means a newly added family member is covered (and checked)
    // without anyone remembering to extend this test.
    for name in kiqi_plugin_sdk::SUBAGENT_PARENT_ENV
        .iter()
        .chain(std::iter::once(&kiqi_plugin_sdk::SESSION_ID_ENV))
    {
        assert!(
            is_host_private_env(name),
            "`{name}` tells a nested ki about its ancestor; it must not inherit"
        );
    }

    // The shell-facing twin is the deliberate exception: stripping it would
    // silently defeat `walgit collab claim --session "$KI_AGENT_ID"`, so the
    // tempting symmetry fix (append it to HOST_PRIVATE_ENV) must fail here.
    assert!(
        !is_host_private_env(kiqi_plugin_sdk::AGENT_ID_ENV),
        "KI_AGENT_ID must reach the shell; only KI_SESSION_ID is stripped"
    );

    // Narrow: the user's own configuration must still reach the shell.
    assert!(!is_host_private_env("KI_OFFLINE"));
    assert!(!is_host_private_env("PATH"));
}

/// The subagent channel is host-private in a spawned shell, but a launcher that
/// *deliberately* sets it on a child it starts must still get through — that is
/// the supported way to nest a subagent. This pins both halves so a future
/// widening of the exclusion cannot quietly disable nesting.
#[tokio::test]
async fn the_subagent_channel_is_inherited_from_augment_not_the_ambient_env() {
    let Some(env) = shell_or_skip().await else {
        return;
    };

    // Ambient (inherited) values are excluded…
    std::env::set_var("LANGFUSE_PI_PARENT_TRACE_ID", "ambient-trace");
    let out = env
        .exec(
            "printf '%s' \"${LANGFUSE_PI_PARENT_TRACE_ID:-unset}\"",
            ShellExecOptions::default(),
        )
        .await
        .expect("exec");
    assert_eq!(
        out.stdout.trim(),
        "unset",
        "an inherited subagent marker must not reach the shell"
    );
    std::env::remove_var("LANGFUSE_PI_PARENT_TRACE_ID");

    // …while an explicit per-call value still does (the nesting path).
    let mut env_vars = std::collections::HashMap::new();
    env_vars.insert(
        "LANGFUSE_PI_PARENT_TRACE_ID".to_string(),
        "deliberate-trace".to_string(),
    );
    let out = env
        .exec(
            "printf '%s' \"${LANGFUSE_PI_PARENT_TRACE_ID:-unset}\"",
            ShellExecOptions {
                env: Some(env_vars),
                ..ShellExecOptions::default()
            },
        )
        .await
        .expect("exec");
    assert_eq!(
        out.stdout.trim(),
        "deliberate-trace",
        "a launcher that sets the value on purpose must not be blocked"
    );
}
