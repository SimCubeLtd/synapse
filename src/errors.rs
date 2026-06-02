//! Error types for SimCube Synapse.
//!
//! Most internal functions return [`anyhow::Result`] at module boundaries. This
//! enum captures the small set of domain errors that callers may want to match
//! on (e.g. "not initialised") and produces clear, non-panicking messages.

use std::path::PathBuf;
use thiserror::Error;

/// Domain-level errors surfaced by Synapse.
#[derive(Debug, Error)]
pub enum SynapseError {
    /// No `.synapse` directory was found walking up from the working directory.
    #[error(
        "no Synapse workspace found (looked for `.synapse/synapse.toml`); run `synapse init` first"
    )]
    NotInitialized,

    /// The config file already exists and `--force` was not supplied.
    #[error("`{0}` already exists; use --force to overwrite")]
    ConfigExists(PathBuf),

    /// A required graph operation is not yet supported by the active backend.
    #[error("graph operation not supported by the active backend: {0}")]
    Unsupported(String),

    /// The graph backend reported a failure.
    #[error("graph backend error: {0}")]
    Backend(String),

    /// `synapse push` was invoked but `[share].push_enabled` is false.
    #[error(
        "push is disabled; set `push_enabled = true` in the [share] section of .synapse/synapse.toml to allow it"
    )]
    PushDisabled,

    /// The registry/repository for sharing is not configured.
    #[error(
        "share target not configured; set `registry` and `repository` in the [share] section (or pass --registry/--repository)"
    )]
    ShareNotConfigured,

    /// Push refused because the working tree has uncommitted changes.
    #[error(
        "working tree has uncommitted changes ({0} file(s)); commit/stash or pass --allow-dirty (the graph is tagged by commit)"
    )]
    DirtyTree(usize),

    /// Push was not confirmed (interactive prompt declined or non-interactive
    /// without `--yes`).
    #[error("push not confirmed (interactive confirmation required, or pass --yes)")]
    PushNotConfirmed,

    /// A registry network/transport call failed.
    #[error("registry network error: {0}")]
    RegistryNetwork(String),

    /// Registry authentication failed.
    #[error("registry authentication failed: {0}")]
    RegistryAuth(String),

    /// A pulled graph failed its blake3 integrity check.
    #[error("pulled graph failed integrity check (blake3 mismatch); the artifact may be corrupt")]
    IntegrityMismatch,
}
