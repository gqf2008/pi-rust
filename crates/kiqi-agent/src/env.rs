//! Host identity and the environment-variable contract shared with spawned
//! processes.
//!
//! These names are consumed by the process-level crates (`kiqi-cli`,
//! `kiqi-tools`, `kiqi-mcp`), not by the extension ABI — the wasm contract is
//! `extensions/kiqi-extension.wit`. They live here because every consumer
//! already depends on `kiqi-agent`, so sharing them costs no new dependency
//! edge (`docs/design/kiqi-host-merge.md`).

/// The application name of the host that loads extensions: `"ki"`.
///
/// This is the **contract's** copy of the host identity, and the host's
/// `APP_NAME` is defined in terms of it, so the two cannot drift. An extension
/// that labels its own output (a trace name, a tag, a `service.name`) should
/// derive from this rather than hardcode a brand — it has no other way to learn
/// what it is embedded in.
///
/// [`HOST_VERSION`] is the matching version. Both are also handed to extensions at
/// runtime in the `BeforeAgentStart` payload's `host` object, which is what an
/// extension should prefer: that value always describes the process actually
/// running, whereas these constants describe the host the extension was *built*
/// against.
pub const HOST_NAME: &str = "kiqi";

/// Version of the [`HOST_NAME`] host that this SDK ships with.
///
/// Because every workspace crate shares one version, this is the host's version
/// as of this SDK release — good for a user agent or a fallback label, and
/// superseded at runtime by the `host.version` field of the `BeforeAgentStart`
/// payload.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment variable carrying the id of the session this process serves.
///
/// The host **rewrites** it whenever a session becomes active — at startup and
/// on every in-process swap (`/import`, fork, switch) — so the value always
/// names the session currently being served. It is not an embedder override:
/// a value inherited from a parent process would otherwise make a child `ki`
/// report its ancestor's session forever. Embedders that need to pin the id set
/// it after the session is active rather than before.
///
/// The id is a plain UTF-8 string (no NUL), which is what makes writing it with
/// `std::env::set_var` sound.
pub const SESSION_ID_ENV: &str = "KI_SESSION_ID";

/// Environment variable carrying the same session id to **commands a shell runs**.
///
/// Same value as [`SESSION_ID_ENV`], rewritten at the same moments (startup and
/// every in-process swap), but a separate name addressed to a different
/// audience: a command under `bash`/`powershell` can name its session, e.g.
/// `walgit collab claim --id <card> --session "$KI_AGENT_ID"`. [`SESSION_ID_ENV`]
/// must *not* reach a spawned shell (a nested `ki` would adopt it), while this
/// one exists precisely so a child command can say which session it belongs to.
///
/// The host only ever writes it and nothing in `ki` reads it, so a pre-set value
/// is harmless — the next activation overwrites it. It must **not** be added to
/// `kiqi_tools::HOST_PRIVATE_ENV`: that list is stripped from spawned shells and
/// would silently defeat the variable (a test pins this).
///
/// Under `--no-session` the value is the literal `ephemeral`, which is not
/// unique across runs; treat it as "no session" when passing it to a claim.
pub const AGENT_ID_ENV: &str = "KI_AGENT_ID";

/// Environment variable a tracing extension publishes to name the trace a
/// *nested* `ki` should attach to, and reads back to decide it is a subagent.
///
/// The name is `kiqi-langfuse`'s (it owns the `LANGFUSE_` prefix), but the
/// variable is declared here because **three** parties have to agree on it and
/// they live in different crates, two of them in different repositories:
///
/// - the extension writes it (so a launcher can attach a nested run),
/// - the host's `bash`/`powershell` must *not* pass it on to a shell (or a plain
///   `ki` started from one mistakes itself for a subagent), and
/// - a launcher may set it deliberately, which is the supported nesting path.
///
/// Declaring it once here is what keeps those three in step. The host builds its
/// exclusion list from these declarations, so dropping or renaming the family
/// ([`SUBAGENT_PARENT_ENV`]) is a compile error there rather than a silently
/// re-opened leak, and a newly added member is excluded without the host having
/// to be told. What the compiler still cannot check is the string itself, since
/// `kiqi-langfuse` lives in another repository and matches on the *value* — that
/// is pinned by a test. See [`SUBAGENT_PARENT_ENV`] for the whole family.
pub const SUBAGENT_PARENT_TRACE_ID_ENV: &str = "LANGFUSE_PI_PARENT_TRACE_ID";

/// See [`SUBAGENT_PARENT_TRACE_ID_ENV`]. The parent turn's root observation id.
pub const SUBAGENT_PARENT_SPAN_ID_ENV: &str = "LANGFUSE_PI_PARENT_SPAN_ID";

/// See [`SUBAGENT_PARENT_TRACE_ID_ENV`]. The parent's session id.
pub const SUBAGENT_PARENT_SESSION_ID_ENV: &str = "LANGFUSE_PI_PARENT_SESSION_ID";

/// See [`SUBAGENT_PARENT_TRACE_ID_ENV`]. Nesting depth, so a grandchild knows
/// how far down it is.
pub const SUBAGENT_PARENT_DEPTH_ENV: &str = "LANGFUSE_PI_PARENT_DEPTH";

/// Every [`SUBAGENT_PARENT_TRACE_ID_ENV`] sibling, as one slice.
///
/// A host that excludes this channel from a spawned shell should cover the whole
/// slice rather than restating the names, and assert it does — a new entry here
/// that is not excluded is the failure mode this array exists to make testable.
pub const SUBAGENT_PARENT_ENV: &[&str] = &[
    SUBAGENT_PARENT_TRACE_ID_ENV,
    SUBAGENT_PARENT_SPAN_ID_ENV,
    SUBAGENT_PARENT_SESSION_ID_ENV,
    SUBAGENT_PARENT_DEPTH_ENV,
];
