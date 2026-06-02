//! Loading and saving of `.synapse/synapse.toml`.
//!
//! This module is the *only* place that reads or writes the TOML config. The
//! struct shape mirrors the documented default config exactly so a freshly
//! `init`-ed file round-trips without surprises.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Directory name for all Synapse state, relative to the repo root.
pub const SYNAPSE_DIR: &str = ".synapse";
/// Config file name within [`SYNAPSE_DIR`].
pub const CONFIG_FILE: &str = "synapse.toml";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseConfig {
    pub repo: RepoConfig,
    pub index: IndexConfig,
    pub graph: GraphConfig,
    pub pack: PackConfig,
    #[serde(default)]
    pub share: ShareConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_root")]
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexConfig {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub languages: LanguageToggles,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanguageToggles {
    #[serde(default = "default_true")]
    pub csharp: bool,
    #[serde(default = "default_true")]
    pub rust: bool,
    #[serde(default = "default_true")]
    pub python: bool,
    #[serde(default = "default_true")]
    pub go: bool,
    #[serde(default = "default_true")]
    pub javascript: bool,
    #[serde(default = "default_true")]
    pub typescript: bool,
    #[serde(default = "default_true")]
    pub svelte: bool,
    #[serde(default = "default_true")]
    pub bash: bool,
    #[serde(default = "default_true")]
    pub yaml: bool,
    #[serde(default = "default_true")]
    pub json: bool,
    #[serde(default = "default_true")]
    pub markdown: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphConfig {
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default = "default_graph_path")]
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackConfig {
    #[serde(default = "default_budget")]
    pub default_budget: usize,
    #[serde(default = "default_format")]
    pub default_format: String,
    #[serde(default = "default_true")]
    pub include_selection_reasons: bool,
}

/// Sharing the indexed graph via an OCI registry (`synapse push` / `pull`).
///
/// Push is OFF by default (`push_enabled = false`) — it must be explicitly
/// enabled, and even then requires interactive confirmation and a clean tree.
/// Credentials are never stored here: they are discovered from the existing
/// docker login (`~/.docker/config.json` + credential helpers).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareConfig {
    /// Registry host[:port], e.g. "ghcr.io". Empty = sharing not configured.
    #[serde(default)]
    pub registry: String,
    /// Repository path within the registry, e.g. "myorg/myrepo-synapse-graph".
    /// Empty = sharing not configured.
    #[serde(default)]
    pub repository: String,
    /// Moving tag updated on every push alongside the per-commit tag.
    #[serde(default = "default_share_moving_tag")]
    pub moving_tag: String,
    /// MUST be true for `synapse push` to be permitted at all.
    #[serde(default)]
    pub push_enabled: bool,
    /// Transport: "https" (default) or "http" (plaintext; dev/local only).
    #[serde(default = "default_share_protocol")]
    pub protocol: String,
    /// Credential strategy: "auto" (docker creds -> env -> anonymous),
    /// "docker", "env", or "anonymous".
    #[serde(default = "default_share_auth")]
    pub auth: String,
}

impl Default for ShareConfig {
    fn default() -> Self {
        ShareConfig {
            registry: String::new(),
            repository: String::new(),
            moving_tag: default_share_moving_tag(),
            push_enabled: false,
            protocol: default_share_protocol(),
            auth: default_share_auth(),
        }
    }
}

fn default_share_moving_tag() -> String {
    "latest".to_string()
}
fn default_share_protocol() -> String {
    "https".to_string()
}
fn default_share_auth() -> String {
    "auto".to_string()
}

fn default_root() -> String {
    ".".to_string()
}
fn default_true() -> bool {
    true
}
fn default_backend() -> String {
    "ladybug".to_string()
}
fn default_graph_path() -> String {
    ".synapse/graph".to_string()
}
fn default_budget() -> usize {
    40000
}
fn default_format() -> String {
    "markdown".to_string()
}

impl Default for SynapseConfig {
    fn default() -> Self {
        SynapseConfig {
            repo: RepoConfig {
                name: String::new(),
                root: default_root(),
            },
            index: IndexConfig {
                include: default_includes(),
                exclude: default_excludes(),
                languages: LanguageToggles {
                    csharp: true,
                    rust: true,
                    python: true,
                    go: true,
                    javascript: true,
                    typescript: true,
                    svelte: true,
                    bash: true,
                    yaml: true,
                    json: true,
                    markdown: true,
                },
            },
            graph: GraphConfig {
                backend: default_backend(),
                path: default_graph_path(),
            },
            pack: PackConfig {
                default_budget: default_budget(),
                default_format: default_format(),
                include_selection_reasons: true,
            },
            share: ShareConfig::default(),
        }
    }
}

fn default_includes() -> Vec<String> {
    [
        "**/*.cs",
        "**/*.rs",
        "**/*.py",
        "**/*.go",
        "**/*.js",
        "**/*.jsx",
        "**/*.ts",
        "**/*.tsx",
        "**/*.mjs",
        "**/*.cjs",
        "**/*.svelte",
        "**/*.sh",
        "**/*.bash",
        "**/*.zsh",
        "**/*.md",
        "**/*.markdown",
        "**/*.mdx",
        "**/*.sln",
        "**/*.csproj",
        "**/*.props",
        "**/*.targets",
        "**/*.yml",
        "**/*.yaml",
        "**/*.json",
        "**/*.toml",
        "**/package.json",
        "**/pnpm-lock.yaml",
        "**/yarn.lock",
        "**/package-lock.json",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_excludes() -> Vec<String> {
    [
        "**/bin/**",
        "**/obj/**",
        "**/node_modules/**",
        "**/target/**",
        "**/dist/**",
        "**/build/**",
        "**/coverage/**",
        "**/.git/**",
        "**/.next/**",
        "**/.turbo/**",
        "**/.vite/**",
        "**/*.Designer.cs",
        "**/*.g.cs",
        "**/*.generated.cs",
        "**/*.min.js",
        "**/*.bundle.js",
        "**/*.map",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl SynapseConfig {
    /// Load config from a repo root (expects `<root>/.synapse/synapse.toml`).
    pub fn load(root: &Path) -> Result<SynapseConfig> {
        let path = config_path(root);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: SynapseConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// Serialize this config to TOML text.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing config")
    }

    /// Write config to `<root>/.synapse/synapse.toml`.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = config_path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, self.to_toml()?)
            .with_context(|| format!("writing config to {}", path.display()))?;
        Ok(())
    }
}

/// Path to the config file given a repo root.
pub fn config_path(root: &Path) -> PathBuf {
    root.join(SYNAPSE_DIR).join(CONFIG_FILE)
}

/// Absolute path to the `.synapse` directory for a repo root.
pub fn synapse_dir(root: &Path) -> PathBuf {
    root.join(SYNAPSE_DIR)
}
