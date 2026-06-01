//! Launching Ladybug Explorer (a Docker image) to visualize the indexed graph.
//!
//! This is the only module that shells out to `docker`. It does **not** run a
//! daemon of Synapse's own: it invokes a user-facing dev tool (the Explorer
//! container), much like opening a browser. The container is always run with
//! `--rm` so it cleans up after itself.
//!
//! The graph lives at `<root>/.synapse/graph/synapse.lbug`; Explorer mounts the
//! containing directory at `/database` and is told the file name via the
//! `LBUG_FILE` environment variable.

use crate::config::SynapseConfig;
use crate::errors::SynapseError;
use crate::repo::Repo;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// File name of the LadybugDB graph within the graph directory.
pub const GRAPH_FILE: &str = "synapse.lbug";

/// Options controlling how the Explorer container is launched.
#[derive(Debug, Clone)]
pub struct ExploreOptions {
    pub port: u16,
    pub read_write: bool,
    pub detach: bool,
    pub in_memory: bool,
    pub image: String,
    pub tag: String,
}

/// Build the `docker run` argument vector (excluding the leading `docker`).
///
/// `graph_dir` is the absolute path to the directory containing the `.lbug`
/// file. Pure and deterministic so it can be unit-tested.
pub fn docker_args(opts: &ExploreOptions, graph_dir: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into(), "--rm".into()];

    if opts.detach {
        args.push("-d".into());
    }

    args.push("-p".into());
    args.push(format!("{}:8000", opts.port));

    if opts.in_memory {
        // Ephemeral: no mount; Explorer ignores -v in this mode.
        args.push("-e".into());
        args.push("LBUG_IN_MEMORY=true".into());
    } else {
        args.push("-v".into());
        args.push(format!("{}:/database", graph_dir.display()));
        args.push("-e".into());
        args.push(format!("LBUG_FILE={GRAPH_FILE}"));
        // READ_ONLY is unsupported with in-memory, so only set it when mounting.
        if !opts.read_write {
            args.push("-e".into());
            args.push("MODE=READ_ONLY".into());
        }
    }

    args.push(format!("{}:{}", opts.image, opts.tag));
    args
}

/// Render the full, copy-pasteable `docker run …` command line.
pub fn docker_command_string(opts: &ExploreOptions, graph_dir: &Path) -> String {
    let mut parts = vec!["docker".to_string()];
    for a in docker_args(opts, graph_dir) {
        // Quote args containing characters a shell would split on.
        if a.contains(char::is_whitespace) {
            parts.push(format!("\"{a}\""));
        } else {
            parts.push(a);
        }
    }
    parts.join(" ")
}

/// Resolve the graph directory for a repo, erroring if the index is absent.
pub fn graph_dir(repo: &Repo, config: &SynapseConfig) -> Result<std::path::PathBuf> {
    let dir = repo.root.join(&config.graph.path);
    let file = dir.join(GRAPH_FILE);
    if !file.is_file() {
        return Err(SynapseError::Backend(format!(
            "no graph found at {} — run `synapse index` first",
            file.display()
        ))
        .into());
    }
    Ok(dir)
}

/// True if a usable `docker` CLI is on PATH and its daemon responds.
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Launch the Explorer container, inheriting stdio (foreground) or returning
/// once started (detached). Assumes `docker_available()` was already checked.
pub fn launch(opts: &ExploreOptions, graph_dir: &Path) -> Result<()> {
    let args = docker_args(opts, graph_dir);
    let status = Command::new("docker")
        .args(&args)
        .status()
        .context("failed to spawn `docker`")?;
    if !status.success() {
        return Err(SynapseError::Backend(format!(
            "`docker run` exited with status {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        ))
        .into());
    }
    Ok(())
}
