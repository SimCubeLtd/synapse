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
}
