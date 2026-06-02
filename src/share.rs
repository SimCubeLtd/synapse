//! Share the indexed graph via an OCI registry (`synapse push` / `pull`).
//!
//! This is the ONLY module that depends on `oci-client`, `tokio` and
//! `docker_credential`; everything else in the crate stays synchronous. The
//! public surface is sync — network methods spin up a short-lived current-thread
//! tokio runtime internally (see `block_on`) so callers never touch async.
//!
//! The graph is shipped as a single-layer OCI artifact: the raw `synapse.lbug`
//! bytes as one layer, a tiny JSON config blob, and the git/version/blake3
//! metadata stamped into manifest annotations so identity is verifiable and
//! staleness detectable without downloading the (multi-MB) blob.

use crate::config::ShareConfig;
use crate::errors::SynapseError;
use anyhow::{Result, anyhow};
use oci_client::client::{ClientConfig, ClientProtocol, ImageLayer};
use oci_client::manifest::OciImageManifest;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference, client::Config as OciConfig};
use std::collections::BTreeMap;
use std::future::Future;

// --- artifact media types ---------------------------------------------------

/// Media type of the graph layer (the raw `.lbug` bytes).
pub const GRAPH_LAYER_MEDIA_TYPE: &str = "application/vnd.simcube.synapse.graph.v1+lbug";
/// Media type of the small JSON config blob.
pub const GRAPH_CONFIG_MEDIA_TYPE: &str = "application/vnd.simcube.synapse.graph.config.v1+json";

// --- annotation keys ---------------------------------------------------------

/// Full commit SHA the graph was indexed at (OCI standard key).
pub const ANNOT_REVISION: &str = "org.opencontainers.image.revision";
/// RFC3339 creation timestamp (OCI standard key).
pub const ANNOT_CREATED: &str = "org.opencontainers.image.created";
/// synapse version that produced the graph (OCI standard key).
pub const ANNOT_VERSION: &str = "org.opencontainers.image.version";
/// Repo name / artifact title (OCI standard key).
pub const ANNOT_TITLE: &str = "org.opencontainers.image.title";
/// Branch the graph was indexed on (synapse namespace).
pub const ANNOT_BRANCH: &str = "com.simcube.synapse.branch";
/// blake3 of the graph blob (synapse namespace).
pub const ANNOT_BLAKE3: &str = "com.simcube.synapse.blake3";
/// Full commit SHA, duplicated in our namespace (robust to tooling that drops
/// the standard `revision` key).
pub const ANNOT_COMMIT: &str = "com.simcube.synapse.commit";

/// Metadata describing a shared graph artifact — carried in manifest
/// annotations (and mirrored into the config blob). All fields optional so a
/// partially-annotated artifact still parses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphArtifactMeta {
    /// Full commit SHA the graph was indexed at.
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub repo_name: Option<String>,
    pub synapse_version: Option<String>,
    /// blake3 hex of the graph blob, for integrity verification on pull.
    pub blob_blake3: Option<String>,
    pub created_at: Option<String>,
}

impl GraphArtifactMeta {
    /// Render to the OCI manifest annotation map (omitting empty fields).
    pub fn to_annotations(&self) -> BTreeMap<String, String> {
        let mut a = BTreeMap::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(val) = v.as_ref().filter(|s| !s.is_empty()) {
                a.insert(k.to_string(), val.clone());
            }
        };
        put(ANNOT_REVISION, &self.commit);
        put(ANNOT_COMMIT, &self.commit);
        put(ANNOT_CREATED, &self.created_at);
        put(ANNOT_VERSION, &self.synapse_version);
        put(ANNOT_TITLE, &self.repo_name);
        put(ANNOT_BRANCH, &self.branch);
        put(ANNOT_BLAKE3, &self.blob_blake3);
        a
    }

    /// Parse from an OCI manifest annotation map. Prefers our namespaced commit
    /// key, falling back to the OCI standard `revision`.
    pub fn from_annotations(a: &BTreeMap<String, String>) -> Self {
        let get = |k: &str| a.get(k).filter(|s| !s.is_empty()).cloned();
        GraphArtifactMeta {
            commit: get(ANNOT_COMMIT).or_else(|| get(ANNOT_REVISION)),
            branch: get(ANNOT_BRANCH),
            repo_name: get(ANNOT_TITLE),
            synapse_version: get(ANNOT_VERSION),
            blob_blake3: get(ANNOT_BLAKE3),
            created_at: get(ANNOT_CREATED),
        }
    }

    /// The self-describing JSON config blob bytes for this artifact.
    pub fn to_config_blob(&self) -> Vec<u8> {
        let obj = serde_json::json!({
            "schemaVersion": 1,
            "tool": "synapse",
            "synapseVersion": self.synapse_version,
            "commit": self.commit,
            "branch": self.branch,
            "repo": self.repo_name,
            "blake3": self.blob_blake3,
            "createdAt": self.created_at,
        });
        serde_json::to_vec(&obj).unwrap_or_else(|_| b"{}".to_vec())
    }
}

// --- staleness ---------------------------------------------------------------

/// Result of comparing a shared graph's indexed commit against local HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphFreshness {
    /// The graph was indexed at the same commit as local HEAD.
    Match,
    /// The graph's commit differs from local HEAD — results may be stale.
    Mismatch {
        graph_commit: String,
        head_commit: String,
    },
    /// Can't tell (not a git repo, or the artifact has no commit annotation).
    Unknown,
}

/// Compare a graph's indexed commit against local HEAD. Prefix-tolerant so a
/// short SHA on either side still matches a full SHA (e.g. a per-commit tag vs
/// the full annotation). `Unknown` when either side is absent.
pub fn compare_commit(graph_commit: Option<&str>, head_commit: Option<&str>) -> GraphFreshness {
    match (graph_commit, head_commit) {
        (Some(g), Some(h)) if !g.is_empty() && !h.is_empty() => {
            let n = g.len().min(h.len());
            if g[..n].eq_ignore_ascii_case(&h[..n]) {
                GraphFreshness::Match
            } else {
                GraphFreshness::Mismatch {
                    graph_commit: g.to_string(),
                    head_commit: h.to_string(),
                }
            }
        }
        _ => GraphFreshness::Unknown,
    }
}

// --- tag resolution ----------------------------------------------------------

/// Resolve which tag a `pull` should fetch:
/// 1. an explicit `--tag` override (e.g. a specific commit SHA);
/// 2. otherwise the configured moving tag (e.g. "latest").
///
/// The default deliberately does NOT auto-pick the local HEAD commit tag: a
/// teammate is usually on a *different* commit than the pushed graph, so that
/// tag typically wouldn't exist in the registry and the pull would hard-fail
/// with "manifest unknown". Pulling the moving tag and then warning on commit
/// mismatch (see `compare_commit`) is the robust, predictable behaviour — fetch
/// the current shared graph, then surface staleness. Use `--tag <sha>` to pull
/// the exact graph for a specific commit.
pub fn resolve_pull_tag(explicit: Option<&str>, moving_tag: &str) -> String {
    explicit
        .filter(|s| !s.is_empty())
        .map(|t| t.to_string())
        .unwrap_or_else(|| moving_tag.to_string())
}

/// The two tags a `push` writes: the immutable per-commit tag (short SHA) and
/// the moving tag. An explicit `--tag` overrides the per-commit tag.
pub fn push_tags(
    explicit: Option<&str>,
    head_short: Option<&str>,
    moving_tag: &str,
) -> Vec<String> {
    let mut tags = Vec::new();
    match explicit.filter(|s| !s.is_empty()) {
        Some(t) => tags.push(t.to_string()),
        None => {
            if let Some(sha) = head_short.filter(|s| !s.is_empty()) {
                tags.push(sha.to_string());
            }
        }
    }
    if !moving_tag.is_empty() && !tags.iter().any(|t| t == moving_tag) {
        tags.push(moving_tag.to_string());
    }
    tags
}

// --- network layer (the only async / oci-client surface) --------------------

/// Resolved registry coordinates for a single push/pull.
#[derive(Debug, Clone)]
pub struct ShareTarget {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

impl ShareTarget {
    fn reference(&self) -> Reference {
        Reference::with_tag(
            self.registry.clone(),
            self.repository.clone(),
            self.tag.clone(),
        )
    }

    /// `registry/repository:tag` for display.
    pub fn display(&self) -> String {
        format!("{}/{}:{}", self.registry, self.repository, self.tag)
    }
}

/// A pulled graph: the raw blob bytes plus the parsed artifact metadata.
pub struct PulledGraph {
    pub bytes: Vec<u8>,
    pub meta: GraphArtifactMeta,
}

/// Result of a successful push.
pub struct PushOutcome {
    /// The references (tags) the artifact was pushed under.
    pub references: Vec<String>,
    pub digest: String,
}

/// Run a future to completion on a short-lived current-thread tokio runtime.
/// Keeps all async confined to this module so the rest of the CLI stays sync.
fn block_on<F: Future>(fut: F) -> Result<F::Output> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow!("starting async runtime: {e}"))?;
    Ok(rt.block_on(fut))
}

fn build_client(cfg: &ShareConfig) -> Client {
    let protocol = match cfg.protocol.as_str() {
        "http" => ClientProtocol::Http,
        _ => ClientProtocol::Https,
    };
    Client::new(ClientConfig {
        protocol,
        ..Default::default()
    })
}

/// Resolve registry credentials. Order for `auto`: env override -> docker
/// credentials (`~/.docker/config.json` + helpers) -> anonymous. Credentials
/// are never read from synapse's own config.
fn resolve_auth(cfg: &ShareConfig, registry: &str) -> RegistryAuth {
    let from_env = || -> Option<RegistryAuth> {
        if let Ok(token) = std::env::var("SYNAPSE_REGISTRY_TOKEN")
            && !token.is_empty()
        {
            return Some(RegistryAuth::Bearer(token));
        }
        match (
            std::env::var("SYNAPSE_REGISTRY_USER"),
            std::env::var("SYNAPSE_REGISTRY_PASS"),
        ) {
            (Ok(u), Ok(p)) if !u.is_empty() => Some(RegistryAuth::Basic(u, p)),
            _ => None,
        }
    };
    let from_docker = || -> Option<RegistryAuth> {
        match docker_credential::get_credential(registry) {
            Ok(docker_credential::DockerCredential::UsernamePassword(u, p)) => {
                Some(RegistryAuth::Basic(u, p))
            }
            Ok(docker_credential::DockerCredential::IdentityToken(t)) => {
                Some(RegistryAuth::Bearer(t))
            }
            Err(_) => None,
        }
    };

    let resolved = match cfg.auth.as_str() {
        "anonymous" => None,
        "env" => from_env(),
        "docker" => from_docker(),
        // "auto" (default): env override first, then docker, then anonymous.
        _ => from_env().or_else(from_docker),
    };
    resolved.unwrap_or(RegistryAuth::Anonymous)
}

/// Map an oci-client error to a synapse domain error, distinguishing auth
/// failures from generic network errors. Credentials are never included.
fn map_oci_err(e: oci_client::errors::OciDistributionError) -> SynapseError {
    let msg = e.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("auth") || lower.contains("401") || lower.contains("unauthorized") {
        SynapseError::RegistryAuth(msg)
    } else {
        SynapseError::RegistryNetwork(msg)
    }
}

/// Read just the manifest annotations for `target` — cheap, no blob download.
/// Used for the pre-pull staleness check and `status`.
pub fn fetch_meta(cfg: &ShareConfig, target: &ShareTarget) -> Result<GraphArtifactMeta> {
    let client = build_client(cfg);
    let auth = resolve_auth(cfg, &target.registry);
    let reference = target.reference();
    let (manifest, _digest) =
        block_on(async { client.pull_image_manifest(&reference, &auth).await })?
            .map_err(map_oci_err)?;
    Ok(meta_from_manifest(&manifest))
}

fn meta_from_manifest(manifest: &OciImageManifest) -> GraphArtifactMeta {
    manifest
        .annotations
        .as_ref()
        .map(GraphArtifactMeta::from_annotations)
        .unwrap_or_default()
}

/// Pull the graph blob + metadata, verifying the blob's blake3 against the
/// artifact annotation (hard error on mismatch — corruption/tampering).
pub fn pull_graph(cfg: &ShareConfig, target: &ShareTarget) -> Result<PulledGraph> {
    let client = build_client(cfg);
    let auth = resolve_auth(cfg, &target.registry);
    let reference = target.reference();

    let image = block_on(async {
        client
            .pull(&reference, &auth, vec![GRAPH_LAYER_MEDIA_TYPE])
            .await
    })?
    .map_err(map_oci_err)?;

    let meta = image
        .manifest
        .as_ref()
        .map(meta_from_manifest)
        .unwrap_or_default();

    let layer = image
        .layers
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("pulled artifact has no layers"))?;
    let bytes = layer.data.to_vec();

    // Integrity: recompute blake3 and compare to the annotation when present.
    if let Some(expected) = meta.blob_blake3.as_ref() {
        let actual = blake3::hash(&bytes).to_hex().to_string();
        if &actual != expected {
            return Err(SynapseError::IntegrityMismatch.into());
        }
    }

    Ok(PulledGraph { bytes, meta })
}

/// Push the graph blob as a single-layer artifact under each tag, stamping
/// `meta` into the manifest annotations and config blob. The caller has already
/// run all push guards (see `cmd_push`); this is pure transport.
pub fn push_graph(
    cfg: &ShareConfig,
    registry: &str,
    repository: &str,
    tags: &[String],
    bytes: Vec<u8>,
    meta: &GraphArtifactMeta,
) -> Result<PushOutcome> {
    let client = build_client(cfg);
    let auth = resolve_auth(cfg, registry);

    let layers = vec![ImageLayer::new(
        bytes,
        GRAPH_LAYER_MEDIA_TYPE.to_string(),
        None,
    )];
    let config = OciConfig::new(
        meta.to_config_blob(),
        GRAPH_CONFIG_MEDIA_TYPE.to_string(),
        None,
    );
    let annotations = meta.to_annotations();

    let mut digest = String::new();
    for tag in tags {
        let reference =
            Reference::with_tag(registry.to_string(), repository.to_string(), tag.clone());
        let mut manifest = OciImageManifest::build(&layers, &config, Some(annotations.clone()));
        manifest.artifact_type = Some(GRAPH_LAYER_MEDIA_TYPE.to_string());
        let resp = block_on(async {
            client
                .push(&reference, &layers, config.clone(), &auth, Some(manifest))
                .await
        })?
        .map_err(map_oci_err)?;
        digest = resp.manifest_url;
    }

    Ok(PushOutcome {
        references: tags.to_vec(),
        digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_commit_matches_equal_and_prefixes() {
        assert_eq!(
            compare_commit(Some("abcdef123456"), Some("abcdef123456")),
            GraphFreshness::Match
        );
        // short tag vs full annotation: prefix match.
        assert_eq!(
            compare_commit(Some("abcdef1"), Some("abcdef123456")),
            GraphFreshness::Match
        );
        assert!(matches!(
            compare_commit(Some("abcdef1"), Some("999999123456")),
            GraphFreshness::Mismatch { .. }
        ));
        assert_eq!(compare_commit(None, Some("abc")), GraphFreshness::Unknown);
        assert_eq!(compare_commit(Some("abc"), None), GraphFreshness::Unknown);
    }

    #[test]
    fn tag_resolution_precedence() {
        // explicit wins.
        assert_eq!(resolve_pull_tag(Some("v1"), "latest"), "v1");
        // otherwise the moving tag (NOT the local HEAD commit — that usually
        // wouldn't exist in the registry for a teammate on a different commit).
        assert_eq!(resolve_pull_tag(None, "latest"), "latest");
        assert_eq!(resolve_pull_tag(Some(""), "latest"), "latest");
    }

    #[test]
    fn push_tags_include_commit_and_moving() {
        let tags = push_tags(None, Some("abc1234"), "latest");
        assert_eq!(tags, vec!["abc1234".to_string(), "latest".to_string()]);
        // explicit replaces the per-commit tag but moving tag still added.
        let tags = push_tags(Some("release"), Some("abc1234"), "latest");
        assert_eq!(tags, vec!["release".to_string(), "latest".to_string()]);
        // no duplicate when explicit == moving tag.
        let tags = push_tags(Some("latest"), Some("abc1234"), "latest");
        assert_eq!(tags, vec!["latest".to_string()]);
    }

    #[test]
    fn annotations_roundtrip() {
        let meta = GraphArtifactMeta {
            commit: Some("abcdef123456".into()),
            branch: Some("main".into()),
            repo_name: Some("synapse".into()),
            synapse_version: Some("0.1.5".into()),
            blob_blake3: Some("deadbeef".into()),
            created_at: Some("2026-06-02T00:00:00+00:00".into()),
        };
        let a = meta.to_annotations();
        // Standard + namespaced commit keys both present.
        assert_eq!(a.get(ANNOT_REVISION).unwrap(), "abcdef123456");
        assert_eq!(a.get(ANNOT_COMMIT).unwrap(), "abcdef123456");
        assert_eq!(a.get(ANNOT_BLAKE3).unwrap(), "deadbeef");
        // Round-trips back to the same metadata.
        assert_eq!(GraphArtifactMeta::from_annotations(&a), meta);
    }

    #[test]
    fn empty_fields_are_omitted_from_annotations() {
        let meta = GraphArtifactMeta {
            commit: Some("abc".into()),
            ..Default::default()
        };
        let a = meta.to_annotations();
        assert!(a.contains_key(ANNOT_COMMIT));
        assert!(!a.contains_key(ANNOT_BRANCH));
        assert!(!a.contains_key(ANNOT_BLAKE3));
    }

    #[test]
    fn share_config_default_is_pull_only() {
        let c = crate::config::ShareConfig::default();
        assert!(!c.push_enabled);
        assert_eq!(c.protocol, "https");
        assert_eq!(c.auth, "auto");
    }
}
