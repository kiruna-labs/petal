//! Plugin registry client (plugins/README.md §2.9): fetch the signed static
//! `index.json`, verify it with the registry's minisign public key baked in at
//! build time, and install a plugin by fetching its `bundle.json`, checking
//! size + sha256 against the index entry, verifying the bundle signature, and
//! checking the bundle's own manifest id/version. Pinned by
//! `contracts/plugin-registry/` (LOCKSTEP with shared/plugin-host/registry.ts).
//!
//! No hosted default: a build without `PETAL_PLUGIN_REGISTRY_URL` and
//! `PETAL_PLUGIN_REGISTRY_PUBKEY` has no registry (the Settings UI hides
//! "Get plugins"); sideloading (I-10) never needs one.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::bus::{is_plugin_id, is_release_version};
use super::store::{self, InstalledRecord, InstalledState};

pub const INDEX_MAX_BYTES: usize = 1024 * 1024;
pub const BUNDLE_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const SIGNATURE_MAX_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryConfig {
    pub url: String,
    pub public_key: String,
}

/// Runtime env wins (dev override), then the build-time bake. Both halves are required.
pub fn config() -> Option<RegistryConfig> {
    let runtime = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
    let url = runtime("PETAL_PLUGIN_REGISTRY_URL")
        .or_else(|| option_env!("PETAL_PLUGIN_REGISTRY_URL").map(str::to_string))?;
    let public_key = runtime("PETAL_PLUGIN_REGISTRY_PUBKEY")
        .or_else(|| option_env!("PETAL_PLUGIN_REGISTRY_PUBKEY").map(str::to_string))?;
    parse_config(&url, &public_key).ok()
}

pub fn parse_config(url: &str, public_key: &str) -> Result<RegistryConfig, String> {
    let url = url.trim().trim_end_matches('/').to_string();
    if !(url.starts_with("https://") || url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1")) {
        return Err("registry url must be https (http only for localhost)".into());
    }
    let public_key = public_key.trim().to_string();
    decode_public_key(&public_key)?;
    Ok(RegistryConfig { url, public_key })
}

fn decode_public_key(text: &str) -> Result<minisign_verify::PublicKey, String> {
    let result = if text.contains('\n') {
        minisign_verify::PublicKey::decode(text)
    } else {
        minisign_verify::PublicKey::from_base64(text)
    };
    result.map_err(|e| format!("registry public key is not a minisign key: {e}"))
}

/// Verify `data` against a minisign signature file (prehashed "ED" or legacy "Ed").
pub fn verify_minisign(public_key_text: &str, signature_text: &str, data: &[u8]) -> Result<String, String> {
    let pk = decode_public_key(public_key_text)?;
    let sig = minisign_verify::Signature::decode(signature_text)
        .map_err(|e| format!("signature file is malformed: {e}"))?;
    pk.verify(data, &sig, true)
        .map_err(|e| format!("signature does not verify: {e}"))?;
    Ok(sig.trusted_comment().to_string())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------- index shape

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryScan {
    pub tool: String,
    pub report_sha256: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryVersion {
    pub version: String,
    pub min_host_version: String,
    pub api_version: u32,
    pub permissions: Vec<String>,
    pub bundle_url: String,
    pub sig_url: String,
    pub sha256: String,
    pub size: usize,
    pub verified: bool,
    #[serde(default)]
    pub scan: Option<RegistryScan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryPlugin {
    pub id: String,
    pub name: String,
    pub description: String,
    pub publisher: String,
    pub latest: String,
    pub versions: Vec<RegistryVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryIndex {
    pub schema_version: u32,
    pub generated_at: String,
    pub plugins: Vec<RegistryPlugin>,
}

fn is_https_or_local(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1")
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Same rules as `parseRegistryIndex` in shared/plugin-host/registry.ts: any bad entry fails the whole index.
pub fn validate_index(index: &RegistryIndex) -> Result<(), String> {
    if index.schema_version != 1 {
        return Err(format!("schemaVersion must be 1, got {}", index.schema_version));
    }
    let mut ids = std::collections::HashSet::new();
    for p in &index.plugins {
        if !is_plugin_id(&p.id) {
            return Err(format!("bad plugin id {:?}", p.id));
        }
        if !ids.insert(p.id.as_str()) {
            return Err(format!("duplicate plugin id {}", p.id));
        }
        if p.name.is_empty() || p.name.chars().count() > 24 {
            return Err(format!("{}: name must be 1..24 chars", p.id));
        }
        if p.description.chars().count() > 140 {
            return Err(format!("{}: description too long", p.id));
        }
        if p.publisher.is_empty() || p.publisher.len() > 64 {
            return Err(format!("{}: publisher required", p.id));
        }
        if !is_release_version(&p.latest) {
            return Err(format!("{}: latest is not a release version", p.id));
        }
        if p.versions.is_empty() {
            return Err(format!("{}: no versions", p.id));
        }
        let mut versions = std::collections::HashSet::new();
        for v in &p.versions {
            if !is_release_version(&v.version) || !versions.insert(v.version.as_str()) {
                return Err(format!("{}: bad or duplicate version {:?}", p.id, v.version));
            }
            if !is_release_version(&v.min_host_version) {
                return Err(format!("{}@{}: bad minHostVersion", p.id, v.version));
            }
            if v.api_version == 0 {
                return Err(format!("{}@{}: bad apiVersion", p.id, v.version));
            }
            if !is_https_or_local(&v.bundle_url) || !is_https_or_local(&v.sig_url) {
                return Err(format!("{}@{}: bundle/sig url must be https", p.id, v.version));
            }
            if !is_sha256_hex(&v.sha256) {
                return Err(format!("{}@{}: bad sha256", p.id, v.version));
            }
            if v.size == 0 || v.size > BUNDLE_MAX_BYTES {
                return Err(format!("{}@{}: bad size", p.id, v.version));
            }
        }
        if !versions.contains(p.latest.as_str()) {
            return Err(format!("{}: latest {} is not among versions", p.id, p.latest));
        }
    }
    Ok(())
}

pub fn parse_index(text: &str) -> Result<RegistryIndex, String> {
    let index: RegistryIndex = serde_json::from_str(text).map_err(|e| format!("index is not valid JSON: {e}"))?;
    validate_index(&index)?;
    Ok(index)
}

/// Steps 2-5 of the verify chain for one downloaded bundle. Returns the bundle's manifest as JSON.
pub fn verify_bundle(
    public_key: &str,
    bundle: &[u8],
    signature_text: &str,
    expected_id: &str,
    entry: &RegistryVersion,
) -> Result<serde_json::Value, String> {
    if bundle.len() != entry.size {
        return Err(format!("bundle is {} bytes, index says {}", bundle.len(), entry.size));
    }
    if sha256_hex(bundle) != entry.sha256 {
        return Err("bundle sha256 does not match the index".into());
    }
    verify_minisign(public_key, signature_text, bundle).map_err(|e| format!("bundle signature: {e}"))?;
    let root: serde_json::Value = serde_json::from_slice(bundle).map_err(|_| "bundle is not JSON".to_string())?;
    let manifest = root
        .get("manifest")
        .filter(|m| m.is_object())
        .ok_or_else(|| "bundle has no manifest".to_string())?;
    let id = manifest.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let version = manifest.get("version").and_then(|v| v.as_str()).unwrap_or("");
    if id != expected_id {
        return Err(format!("bundle is for {id:?}, expected {expected_id}"));
    }
    if version != entry.version {
        return Err(format!("bundle is version {version:?}, expected {}", entry.version));
    }
    let entry_file = manifest.get("entry").and_then(|v| v.as_str()).unwrap_or("");
    let has_entry = root
        .get("files")
        .and_then(|f| f.get(entry_file))
        .and_then(|s| s.as_str())
        .is_some_and(|s| !s.is_empty());
    if !has_entry {
        return Err(format!("bundle lacks its entry file {entry_file:?}"));
    }
    Ok(manifest.clone())
}

// ---------------------------------------------------------------- network

async fn fetch_text(url: &str, max_bytes: usize) -> Result<String, String> {
    let bytes = fetch_bytes(url, max_bytes).await?;
    String::from_utf8(bytes).map_err(|_| format!("{url}: not UTF-8"))
}

async fn fetch_bytes(url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    if !is_https_or_local(url) {
        return Err(format!("refusing non-https registry url {url}"));
    }
    let response = crate::transport::backend_http::send_with_retry(
        crate::transport::backend_http::client().get(url),
    )
    .await
    .map_err(|e| format!("{url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: HTTP {}", response.status().as_u16()));
    }
    if let Some(len) = response.content_length() {
        if len as usize > max_bytes {
            return Err(format!("{url}: {len} bytes exceeds the {max_bytes} byte limit"));
        }
    }
    let bytes = response.bytes().await.map_err(|e| format!("{url}: {e}"))?;
    if bytes.len() > max_bytes {
        return Err(format!("{url}: {} bytes exceeds the {max_bytes} byte limit", bytes.len()));
    }
    Ok(bytes.to_vec())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedIndex {
    pub index: RegistryIndex,
    pub trusted_comment: String,
    pub registry_url: String,
}

pub async fn fetch_index(config: &RegistryConfig) -> Result<VerifiedIndex, String> {
    let index_text = fetch_text(&format!("{}/index.json", config.url), INDEX_MAX_BYTES).await?;
    let sig_text = fetch_text(&format!("{}/index.json.minisig", config.url), SIGNATURE_MAX_BYTES).await?;
    let trusted_comment = verify_minisign(&config.public_key, &sig_text, index_text.as_bytes())
        .map_err(|e| format!("registry index signature: {e}"))?;
    let index = parse_index(&index_text)?;
    Ok(VerifiedIndex {
        index,
        trusted_comment,
        registry_url: config.url.clone(),
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub async fn install(config: &RegistryConfig, id: &str, version: &str) -> Result<InstalledRecord, String> {
    if !is_plugin_id(id) {
        return Err(format!("invalid plugin id: {id}"));
    }
    if !is_release_version(version) {
        return Err(format!("invalid version: {version}"));
    }
    // Always against a fresh, verified index: the entry (urls, sha, permissions) is the trust anchor.
    let verified = fetch_index(config).await?;
    let plugin = verified
        .index
        .plugins
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("{id} is not in the registry"))?;
    let entry = plugin
        .versions
        .iter()
        .find(|v| v.version == version)
        .ok_or_else(|| format!("{id}@{version} is not in the registry"))?;
    if !entry.verified {
        return Err(format!("{id}@{version} has not been verified by the registry yet"));
    }
    let bundle = fetch_bytes(&entry.bundle_url, entry.size.min(BUNDLE_MAX_BYTES)).await?;
    let sig_text = fetch_text(&entry.sig_url, SIGNATURE_MAX_BYTES).await?;
    verify_bundle(&config.public_key, &bundle, &sig_text, id, entry)?;
    let record = InstalledRecord {
        version: version.to_string(),
        enabled: true,
        source: "registry".to_string(),
        granted_permissions: entry.permissions.clone(),
        installed_at_ms: now_ms(),
        sha256: Some(entry.sha256.clone()),
    };
    store::install(id, record.clone(), &bundle)?;
    log::info!("plugins::registry: installed {id}@{version} from {}", config.url);
    Ok(record)
}

// ---------------------------------------------------------------- commands

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryStatus {
    pub configured: bool,
    pub url: Option<String>,
}

#[tauri::command]
pub fn plugin_registry_status() -> RegistryStatus {
    match config() {
        Some(c) => RegistryStatus {
            configured: true,
            url: Some(c.url),
        },
        None => RegistryStatus {
            configured: false,
            url: None,
        },
    }
}

#[tauri::command]
pub async fn plugin_registry_index() -> Result<VerifiedIndex, String> {
    let config = config().ok_or_else(|| "no plugin registry is configured in this build".to_string())?;
    fetch_index(&config).await
}

#[tauri::command]
pub async fn plugin_install_from_registry(plugin_id: String, version: String) -> Result<InstalledRecord, String> {
    let config = config().ok_or_else(|| "no plugin registry is configured in this build".to_string())?;
    install(&config, &plugin_id, &version).await
}

#[tauri::command]
pub fn plugin_list_installed() -> Result<InstalledState, String> {
    store::list()
}

#[tauri::command]
pub fn plugin_set_installed_enabled(plugin_id: String, enabled: bool) -> Result<(), String> {
    store::set_enabled(&plugin_id, enabled)
}

#[tauri::command]
pub fn plugin_uninstall(plugin_id: String) -> Result<(), String> {
    store::uninstall(&plugin_id)
}

#[tauri::command]
pub fn plugin_read_bundle(plugin_id: String) -> Result<String, String> {
    store::read_bundle(&plugin_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUB: &str = include_str!("../../../../../contracts/plugin-registry/test.pub");
    const INDEX: &str = include_str!("../../../../../contracts/plugin-registry/index.json");
    const INDEX_SIG: &str = include_str!("../../../../../contracts/plugin-registry/index.json.minisig");
    const BUNDLE: &str = include_str!("../../../../../contracts/plugin-registry/plugins/petal.test-hello/1.0.0/bundle.json");
    const BUNDLE_SIG: &str = include_str!("../../../../../contracts/plugin-registry/plugins/petal.test-hello/1.0.0/bundle.json.minisig");

    #[test]
    fn fixture_index_verifies_and_parses() {
        let comment = verify_minisign(PUB, INDEX_SIG, INDEX.as_bytes()).unwrap();
        assert!(comment.contains("file:index.json"));
        let index = parse_index(INDEX).unwrap();
        assert_eq!(index.plugins.len(), 2);
        assert_eq!(index.plugins[0].id, "petal.test-hello");
        assert!(index.plugins[0].versions[0].verified);
        assert!(!index.plugins[1].versions[0].verified);
    }

    #[test]
    fn tampering_with_index_or_trusted_comment_fails() {
        let tampered = INDEX.replace("\"verified\": false", "\"verified\": true");
        assert!(verify_minisign(PUB, INDEX_SIG, tampered.as_bytes()).is_err());
        let altered_sig = INDEX_SIG.replace("trusted comment: timestamp", "trusted comment: timestamq");
        assert!(verify_minisign(PUB, &altered_sig, INDEX.as_bytes()).is_err());
        assert!(verify_minisign(
            "untrusted comment: x\nRWTAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
            INDEX_SIG,
            INDEX.as_bytes()
        )
        .is_err());
    }

    #[test]
    fn public_key_accepts_bare_base64_and_two_line_file() {
        let bare = PUB.lines().find(|l| !l.starts_with("untrusted")).unwrap();
        assert!(decode_public_key(bare).is_ok());
        assert!(decode_public_key(PUB).is_ok());
        assert!(parse_config("https://plugins.example.test/", bare).unwrap().url == "https://plugins.example.test");
        assert!(parse_config("http://plugins.example.test", bare).is_err(), "http refused");
        assert!(parse_config("http://localhost:8787", bare).is_ok());
        assert!(parse_config("https://x.test", "not-a-key").is_err());
    }

    #[test]
    fn fixture_bundle_passes_the_full_chain_and_each_broken_link_is_named() {
        let index = parse_index(INDEX).unwrap();
        let entry = &index.plugins[0].versions[0];
        let bytes = BUNDLE.as_bytes();
        assert_eq!(sha256_hex(bytes), entry.sha256);
        assert_eq!(bytes.len(), entry.size);
        let manifest = verify_bundle(PUB, bytes, BUNDLE_SIG, "petal.test-hello", entry).unwrap();
        assert_eq!(manifest["id"], "petal.test-hello");

        let mut flipped = bytes.to_vec();
        let n = flipped.len();
        flipped[n - 3] ^= 1;
        assert!(verify_bundle(PUB, &flipped, BUNDLE_SIG, "petal.test-hello", entry)
            .unwrap_err()
            .contains("sha256"));
        assert!(verify_bundle(PUB, bytes, BUNDLE_SIG, "acme.other", entry)
            .unwrap_err()
            .contains("expected acme.other"));
        let mut wrong_version = entry.clone();
        wrong_version.version = "2.0.0".into();
        assert!(verify_bundle(PUB, bytes, BUNDLE_SIG, "petal.test-hello", &wrong_version)
            .unwrap_err()
            .contains("version"));
        let mut wrong_size = entry.clone();
        wrong_size.size += 1;
        assert!(verify_bundle(PUB, bytes, BUNDLE_SIG, "petal.test-hello", &wrong_size).is_err());
    }

    #[test]
    fn validate_index_refuses_bad_registries_as_a_whole() {
        let mut index = parse_index(INDEX).unwrap();
        index.plugins[0].versions[0].bundle_url = "http://plugins.example.test/x".into();
        assert!(validate_index(&index).is_err());
        let mut index = parse_index(INDEX).unwrap();
        index.plugins[1].id = index.plugins[0].id.clone();
        assert!(validate_index(&index).is_err());
        let mut index = parse_index(INDEX).unwrap();
        index.plugins[0].latest = "9.9.9".into();
        assert!(validate_index(&index).is_err());
        let mut index = parse_index(INDEX).unwrap();
        index.plugins[0].versions[0].sha256 = "XYZ".into();
        assert!(validate_index(&index).is_err());
        assert!(parse_index("nope").is_err());
    }
}
