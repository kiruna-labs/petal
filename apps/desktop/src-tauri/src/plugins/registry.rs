//! Plugin registry client (plugins/README.md §2.9): fetch the signed static
//! `index.json`, verify it with the registry's minisign public key **baked in
//! at build time**, and install a plugin by fetching its `bundle.json`,
//! checking size + sha256 against the index entry, verifying the bundle
//! signature, and checking the bundle's own manifest id/version. Pinned by
//! `contracts/plugin-registry/` (LOCKSTEP with shared/plugin-host/registry.ts;
//! `invalid-index-cases.json` is iterated by both validators).
//!
//! Trust model (review of PR #99):
//! - The public key comes ONLY from `option_env!` (compile time). No runtime
//!   override exists in any build: on macOS an environment variable is a far
//!   lower bar than code signing (LaunchAgent plist, `launchctl setenv`).
//! - The URL may be overridden at runtime in debug builds only, and the
//!   baked key must still verify whatever that URL serves.
//! - No hosted default: a build without both variables has no registry.
//! - Anti-rollback: the newest accepted index (`generatedAt` and the
//!   signature's `timestamp:`) is persisted; an older index is refused.
//! - Permissions in the index are validated like manifest permissions and the
//!   grant is their intersection with the bundle manifest's own list.
//! - Registry HTTP uses its own client: no redirects, no default headers (the
//!   backend client carries a Vercel bypass secret), body streamed and cut
//!   at the size cap before any byte is trusted.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::bus::{is_plugin_id, is_release_version};
use super::sha256_hex;
use super::store::{self, InstalledRecord, InstalledState, RegistrySeen};

pub const INDEX_MAX_BYTES: usize = 1024 * 1024;
pub const BUNDLE_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const SIGNATURE_MAX_BYTES: usize = 4096;
/// No registry existed before this; an older `generatedAt` is a replay or a forgery.
pub const REGISTRY_EPOCH_MS: u64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
/// Tolerance for clock skew between the publisher and this machine.
const FUTURE_SKEW_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryConfig {
    pub url: String,
    pub public_key: String,
}

/// The build-time trust root. The key is never read from the runtime environment.
pub fn config() -> Option<RegistryConfig> {
    let public_key = option_env!("PETAL_PLUGIN_REGISTRY_PUBKEY")?;
    let baked_url = option_env!("PETAL_PLUGIN_REGISTRY_URL").map(str::to_string);
    let url = if cfg!(debug_assertions) {
        // Debug builds may point at a local/staging registry; it must still be
        // signed by the baked key, so this cannot widen trust.
        std::env::var("PETAL_PLUGIN_REGISTRY_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or(baked_url)
    } else {
        baked_url
    }?;
    match parse_config(&url, public_key) {
        Ok(c) => Some(c),
        Err(e) => {
            log::warn!("plugins::registry: registry configuration rejected: {e}");
            None
        }
    }
}

pub fn parse_config(url: &str, public_key: &str) -> Result<RegistryConfig, String> {
    let url = url.trim().trim_end_matches('/').to_string();
    if !is_registry_url(&url) {
        return Err(
            "registry url must be https (http only for localhost / 127.0.0.1 / [::1])".into(),
        );
    }
    let public_key = public_key.trim().to_string();
    decode_public_key(&public_key)?;
    Ok(RegistryConfig { url, public_key })
}

/// https, or http only when the HOST is a loopback name -- parsed, not prefix-matched
/// (`http://localhost.attacker.example` is a registrable public name).
pub fn is_registry_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "https" => parsed.host_str().is_some(),
        "http" => matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
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
/// Returns the trusted comment (which carries the publisher's `timestamp:`).
pub fn verify_minisign(
    public_key_text: &str,
    signature_text: &str,
    data: &[u8],
) -> Result<String, String> {
    let pk = decode_public_key(public_key_text)?;
    let sig = minisign_verify::Signature::decode(signature_text)
        .map_err(|e| format!("signature file is malformed: {e}"))?;
    pk.verify(data, &sig, true)
        .map_err(|e| format!("signature does not verify: {e}"))?;
    Ok(sig.trusted_comment().to_string())
}

/// `timestamp:<unix seconds>` from a minisign trusted comment (tab-separated fields).
pub fn signed_at_from_trusted_comment(comment: &str) -> Option<u64> {
    comment
        .split(['\t', ' '])
        .find_map(|field| field.strip_prefix("timestamp:"))
        .and_then(|v| v.parse::<u64>().ok())
}

// ---------------------------------------------------------------- permissions (mirror of manifest.ts)

const STATIC_PERMISSIONS: &[&str] = &[
    "meeting:read",
    "data:publish",
    "state:write",
    "storage",
    "ui:toolbar-button",
    "ui:header-button",
    "ui:overlay",
    "ui:popover",
    "ui:panel",
    "ui:settings",
    "ui:toast",
    "shares:read",
    "clipboard:write",
    "net:fetch:user-urls",
];

fn is_net_host(host: &str) -> bool {
    // `^(\*\.)?([a-z0-9-]+\.)*[a-z0-9-]+(:\d{1,5})?$`
    let host = host.strip_prefix("*.").unwrap_or(host);
    let name = match host.rsplit_once(':') {
        Some((n, p)) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => n,
        Some(_) => return false,
        None => host,
    };
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Same vocabulary as `isPermission` in shared/plugin-host/manifest.ts. Reserved
/// (`frames:read`) and wildcard (`net:fetch:*`) are not permissions.
pub fn is_permission(value: &str) -> bool {
    if STATIC_PERMISSIONS.contains(&value) {
        return true;
    }
    match value.strip_prefix("net:fetch:") {
        Some(host) => host != "user-urls" && host != "*" && is_net_host(host),
        None => false,
    }
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

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parse `YYYY-MM-DDTHH:MM:SS(.fff)?Z` to unix milliseconds. Strict on purpose: the
/// publisher writes exactly this shape and anything else is a forgery or a bug.
pub fn parse_rfc3339_utc_ms(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    if *bytes.last()? != b'Z' {
        return None;
    }
    let num = |s: &str| -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let year = num(&value[0..4])?;
    let month = num(&value[5..7])?;
    let day = num(&value[8..10])?;
    let hour = num(&value[11..13])?;
    let minute = num(&value[14..16])?;
    let second = num(&value[17..19])?;
    let rest = &value[19..value.len() - 1];
    let millis = match rest.strip_prefix('.') {
        None if rest.is_empty() => 0,
        Some(frac) if (1..=3).contains(&frac.len()) => {
            num(frac)? * 10u64.pow(3 - frac.len() as u32)
        }
        _ => return None,
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let (y, m) = if month <= 2 {
        (year as i64 - 1, month as i64 + 9)
    } else {
        (year as i64, month as i64 - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    if days < 0 {
        return None;
    }
    Some((days as u64 * 86_400 + hour * 3_600 + minute * 60 + second) * 1000 + millis)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Same rules as `parseRegistryIndex` in shared/plugin-host/registry.ts: any bad entry fails the whole index.
pub fn validate_index(index: &RegistryIndex) -> Result<u64, String> {
    if index.schema_version != 1 {
        return Err(format!(
            "schemaVersion must be 1, got {}",
            index.schema_version
        ));
    }
    let generated_at_ms = parse_rfc3339_utc_ms(&index.generated_at)
        .ok_or_else(|| "generatedAt must be an RFC 3339 UTC timestamp".to_string())?;
    if generated_at_ms < REGISTRY_EPOCH_MS {
        return Err("generatedAt predates the registry".into());
    }
    if generated_at_ms > now_ms() + FUTURE_SKEW_MS {
        return Err("generatedAt is in the future".into());
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
                return Err(format!(
                    "{}: bad or duplicate version {:?}",
                    p.id, v.version
                ));
            }
            if !is_release_version(&v.min_host_version) {
                return Err(format!("{}@{}: bad minHostVersion", p.id, v.version));
            }
            if v.api_version == 0 {
                return Err(format!("{}@{}: bad apiVersion", p.id, v.version));
            }
            let mut seen_perms = std::collections::HashSet::new();
            for perm in &v.permissions {
                if !is_permission(perm) || !seen_perms.insert(perm.as_str()) {
                    return Err(format!(
                        "{}@{}: bad or duplicate permission {perm:?}",
                        p.id, v.version
                    ));
                }
            }
            if !is_registry_url(&v.bundle_url) || !is_registry_url(&v.sig_url) {
                return Err(format!(
                    "{}@{}: bundle/sig url must be https",
                    p.id, v.version
                ));
            }
            if !is_sha256_hex(&v.sha256) {
                return Err(format!("{}@{}: bad sha256", p.id, v.version));
            }
            if v.size == 0 || v.size > BUNDLE_MAX_BYTES {
                return Err(format!("{}@{}: bad size", p.id, v.version));
            }
        }
        if !versions.contains(p.latest.as_str()) {
            return Err(format!(
                "{}: latest {} is not among versions",
                p.id, p.latest
            ));
        }
    }
    Ok(generated_at_ms)
}

pub fn parse_index(text: &str) -> Result<(RegistryIndex, u64), String> {
    let index: RegistryIndex =
        serde_json::from_str(text).map_err(|e| format!("index is not valid JSON: {e}"))?;
    let generated_at_ms = validate_index(&index)?;
    Ok((index, generated_at_ms))
}

/// Anti-rollback (review #3): refuse an index older than the last accepted one for this registry.
pub fn check_not_rolled_back(
    previous: Option<&RegistrySeen>,
    url: &str,
    generated_at_ms: u64,
    signed_at_s: u64,
) -> Result<(), String> {
    match previous {
        Some(seen) if seen.url == url => {
            if generated_at_ms < seen.generated_at_ms || signed_at_s < seen.signed_at_s {
                return Err(format!(
                    "registry index is older than the last one accepted (generatedAt {} < {}, signed {} < {}); refusing a rollback",
                    generated_at_ms, seen.generated_at_ms, signed_at_s, seen.signed_at_s
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Steps 2-5 of the verify chain for one downloaded bundle. Returns the manifest's own permission list.
pub fn verify_bundle(
    public_key: &str,
    bundle: &[u8],
    signature_text: &str,
    expected_id: &str,
    entry: &RegistryVersion,
) -> Result<Vec<String>, String> {
    if bundle.len() != entry.size {
        return Err(format!(
            "bundle is {} bytes, index says {}",
            bundle.len(),
            entry.size
        ));
    }
    if sha256_hex(bundle) != entry.sha256 {
        return Err("bundle sha256 does not match the index".into());
    }
    verify_minisign(public_key, signature_text, bundle)
        .map_err(|e| format!("bundle signature: {e}"))?;
    let root: serde_json::Value =
        serde_json::from_slice(bundle).map_err(|_| "bundle is not JSON".to_string())?;
    let manifest = root
        .get("manifest")
        .filter(|m| m.is_object())
        .ok_or_else(|| "bundle has no manifest".to_string())?;
    let id = manifest.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let version = manifest
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if id != expected_id {
        return Err(format!("bundle is for {id:?}, expected {expected_id}"));
    }
    if version != entry.version {
        return Err(format!(
            "bundle is version {version:?}, expected {}",
            entry.version
        ));
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
    let manifest_permissions: Vec<String> = manifest
        .get("permissions")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(manifest_permissions)
}

/// The enforced grant: what the (signed) index lists AND the bundle's own manifest asks for,
/// restricted to known permissions (review #4).
pub fn granted_permissions(
    index_permissions: &[String],
    manifest_permissions: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = index_permissions
        .iter()
        .filter(|p| is_permission(p) && manifest_permissions.iter().any(|m| m == *p))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------- network

/// Registry traffic never shares the backend client: no default headers (the backend
/// client carries a Vercel bypass secret), and no redirects (each hop would otherwise
/// be chosen by the server, including an https->http downgrade).
fn registry_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("petal-plugin-registry/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("registry HTTP client configuration is valid")
}

/// GET `url`, streaming the body and aborting past `max_bytes` (review #6: the cap applies
/// before the bytes are buffered, and errors name the configured registry, not a redirect).
async fn fetch_bytes(
    client: &reqwest::Client,
    registry: &str,
    url: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    if !is_registry_url(url) {
        return Err(format!("registry {registry}: refusing non-https url {url}"));
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("registry {registry}: request failed: {e}"))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(format!(
            "registry {registry}: server redirected ({}); redirects are not followed",
            status.as_u16()
        ));
    }
    if !status.is_success() {
        return Err(format!("registry {registry}: HTTP {}", status.as_u16()));
    }
    if let Some(len) = response.content_length() {
        if len > max_bytes as u64 {
            return Err(format!(
                "registry {registry}: {len} bytes exceeds the {max_bytes} byte limit"
            ));
        }
    }
    let mut response = response;
    let mut out: Vec<u8> =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(max_bytes as u64) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("registry {registry}: read failed: {e}"))?
    {
        if out.len() + chunk.len() > max_bytes {
            return Err(format!(
                "registry {registry}: response exceeds the {max_bytes} byte limit; aborted"
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn fetch_text(
    client: &reqwest::Client,
    registry: &str,
    url: &str,
    max_bytes: usize,
) -> Result<String, String> {
    let bytes = fetch_bytes(client, registry, url, max_bytes).await?;
    String::from_utf8(bytes).map_err(|_| format!("registry {registry}: response is not UTF-8"))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedIndex {
    pub index: RegistryIndex,
    pub trusted_comment: String,
    pub registry_url: String,
}

/// Fetch, signature-verify, validate, and anti-rollback-check the index; records it as the newest seen.
pub async fn fetch_index(config: &RegistryConfig) -> Result<VerifiedIndex, String> {
    let client = registry_client();
    let index_text = fetch_text(
        &client,
        &config.url,
        &format!("{}/index.json", config.url),
        INDEX_MAX_BYTES,
    )
    .await?;
    let sig_text = fetch_text(
        &client,
        &config.url,
        &format!("{}/index.json.minisig", config.url),
        SIGNATURE_MAX_BYTES,
    )
    .await?;
    let trusted_comment = verify_minisign(&config.public_key, &sig_text, index_text.as_bytes())
        .map_err(|e| format!("registry index signature: {e}"))?;
    let signed_at_s = signed_at_from_trusted_comment(&trusted_comment)
        .ok_or_else(|| "registry index signature carries no timestamp".to_string())?;
    let (index, generated_at_ms) = parse_index(&index_text)?;
    check_not_rolled_back(
        store::registry_seen()?.as_ref(),
        &config.url,
        generated_at_ms,
        signed_at_s,
    )?;
    store::record_registry_seen(RegistrySeen {
        url: config.url.clone(),
        generated_at_ms,
        signed_at_s,
    })?;
    Ok(VerifiedIndex {
        index,
        trusted_comment,
        registry_url: config.url.clone(),
    })
}

pub async fn install(
    config: &RegistryConfig,
    id: &str,
    version: &str,
) -> Result<InstalledRecord, String> {
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
        return Err(format!(
            "{id}@{version} has not been verified by the registry yet"
        ));
    }
    let client = registry_client();
    let bundle = fetch_bytes(
        &client,
        &config.url,
        &entry.bundle_url,
        entry.size.min(BUNDLE_MAX_BYTES),
    )
    .await?;
    let sig_text = fetch_text(&client, &config.url, &entry.sig_url, SIGNATURE_MAX_BYTES).await?;
    let manifest_permissions = verify_bundle(&config.public_key, &bundle, &sig_text, id, entry)?;
    let record = InstalledRecord {
        version: version.to_string(),
        enabled: true,
        source: "registry".to_string(),
        granted_permissions: granted_permissions(&entry.permissions, &manifest_permissions),
        installed_at_ms: now_ms(),
        sha256: Some(entry.sha256.clone()),
    };
    // Nothing above touched disk; only a fully verified bundle is stored.
    store::install(id, record.clone(), &bundle)?;
    log::info!(
        "plugins::registry: installed {id}@{version} from {}",
        config.url
    );
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
    let config =
        config().ok_or_else(|| "no plugin registry is configured in this build".to_string())?;
    fetch_index(&config).await
}

#[tauri::command]
pub async fn plugin_install_from_registry(
    plugin_id: String,
    version: String,
) -> Result<InstalledRecord, String> {
    let config =
        config().ok_or_else(|| "no plugin registry is configured in this build".to_string())?;
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
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    const PUB: &str = include_str!("../../../../../contracts/plugin-registry/test.pub");
    const INDEX: &str = include_str!("../../../../../contracts/plugin-registry/index.json");
    const INDEX_SIG: &str =
        include_str!("../../../../../contracts/plugin-registry/index.json.minisig");
    const BUNDLE: &str = include_str!(
        "../../../../../contracts/plugin-registry/plugins/petal.test-hello/1.0.0/bundle.json"
    );
    const BUNDLE_SIG: &str = include_str!("../../../../../contracts/plugin-registry/plugins/petal.test-hello/1.0.0/bundle.json.minisig");
    const INVALID_CASES: &str =
        include_str!("../../../../../contracts/plugin-registry/invalid-index-cases.json");

    fn temp_store() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "petal-registry-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        store::initialize_for_tests(&dir);
        dir
    }

    /// path -> (status, extra headers, body)
    type Files = HashMap<&'static str, (u16, Vec<(&'static str, String)>, Vec<u8>)>;

    /// Runs the async body on a throwaway runtime so a test can hold the store lock around it
    /// (the lock is a plain std Mutex shared with the sync `store` tests).
    fn block_on(f: impl std::future::Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f);
    }

    /// A tiny HTTP/1.1 file server: path -> (status, headers, body). Serves until dropped.
    struct FakeRegistry {
        url: String,
        hits: Arc<Mutex<Vec<String>>>,
    }

    fn spawn_registry(files: Files) -> FakeRegistry {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                recorder.lock().unwrap().push(path.clone());
                let (status, headers, body) = files.get(path.as_str()).cloned().unwrap_or((
                    404,
                    vec![],
                    b"not found".to_vec(),
                ));
                let mut head = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
                let mut has_len = false;
                for (k, v) in &headers {
                    if k.eq_ignore_ascii_case("content-length") {
                        has_len = true;
                    }
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                if !has_len
                    && !headers
                        .iter()
                        .any(|(k, _)| k.eq_ignore_ascii_case("x-no-length"))
                {
                    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        FakeRegistry {
            url: format!("http://127.0.0.1:{}", addr.port()),
            hits,
        }
    }

    /// The contract fixture served at its own paths (bundle bytes and signature swappable per test).
    fn fixture_files(bundle: &[u8], bundle_sig: &str) -> Files {
        let mut files = HashMap::new();
        files.insert("/index.json", (200, vec![], INDEX.as_bytes().to_vec()));
        files.insert(
            "/index.json.minisig",
            (200, vec![], INDEX_SIG.as_bytes().to_vec()),
        );
        files.insert(
            "/plugins/petal.test-hello/1.0.0/bundle.json",
            (200, vec![], bundle.to_vec()),
        );
        files.insert(
            "/plugins/petal.test-hello/1.0.0/bundle.json.minisig",
            (200, vec![], bundle_sig.as_bytes().to_vec()),
        );
        files
    }

    fn config_for(url: &str) -> RegistryConfig {
        RegistryConfig {
            url: url.trim_end_matches('/').to_string(),
            public_key: PUB.to_string(),
        }
    }

    fn fixture_entry() -> RegistryVersion {
        parse_index(INDEX).unwrap().0.plugins[0].versions[0].clone()
    }

    // ---- pure checks ----

    #[test]
    fn fixture_index_verifies_and_parses_with_a_timestamp() {
        let comment = verify_minisign(PUB, INDEX_SIG, INDEX.as_bytes()).unwrap();
        assert_eq!(
            signed_at_from_trusted_comment(&comment),
            Some(1_788_868_800)
        );
        let (index, generated_at_ms) = parse_index(INDEX).unwrap();
        assert_eq!(index.plugins.len(), 2);
        assert_eq!(
            generated_at_ms,
            parse_rfc3339_utc_ms("2026-09-08T12:00:00.000Z").unwrap()
        );
        assert!(index.plugins[0].versions[0].verified);
        assert!(!index.plugins[1].versions[0].verified);
    }

    #[test]
    fn rfc3339_parser_is_strict() {
        assert_eq!(
            parse_rfc3339_utc_ms("2026-01-01T00:00:00.000Z"),
            Some(REGISTRY_EPOCH_MS)
        );
        assert_eq!(
            parse_rfc3339_utc_ms("2026-01-01T00:00:00Z"),
            Some(REGISTRY_EPOCH_MS)
        );
        assert_eq!(
            parse_rfc3339_utc_ms("2026-01-01T00:00:00.5Z"),
            Some(REGISTRY_EPOCH_MS + 500)
        );
        assert_eq!(parse_rfc3339_utc_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_rfc3339_utc_ms("not a date at all"), None);
        assert_eq!(parse_rfc3339_utc_ms("2026-01-01 00:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc_ms("2026-01-01T00:00:00+02:00"), None);
    }

    #[test]
    fn every_shared_invalid_case_is_rejected() {
        let cases: serde_json::Value = serde_json::from_str(INVALID_CASES).unwrap();
        let cases = cases["cases"].as_array().unwrap();
        assert!(cases.len() >= 15);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut doc: serde_json::Value = serde_json::from_str(INDEX).unwrap();
            let path = case["path"].as_str().unwrap();
            if case["delete"].as_bool() == Some(true) {
                let (parent, key) = path.rsplit_once('/').unwrap();
                match doc.pointer_mut(parent).unwrap() {
                    serde_json::Value::Object(map) => {
                        map.remove(key);
                    }
                    serde_json::Value::Array(arr) => {
                        arr.remove(key.parse::<usize>().unwrap());
                    }
                    _ => panic!("bad delete path in {name}"),
                }
            } else {
                *doc.pointer_mut(path)
                    .unwrap_or_else(|| panic!("bad path in {name}")) = case["value"].clone();
            }
            assert!(
                parse_index(&doc.to_string()).is_err(),
                "expected rejection: {name}"
            );
        }
        assert!(parse_index("nope").is_err());
    }

    #[test]
    fn registry_urls_are_parsed_not_prefix_matched() {
        assert!(is_registry_url("https://plugins.example.test/"));
        assert!(is_registry_url("http://localhost:8787/x"));
        assert!(is_registry_url("http://127.0.0.1/x"));
        assert!(!is_registry_url(
            "http://localhost.attacker.example/registry"
        ));
        assert!(!is_registry_url("http://127.0.0.1.attacker.example/x"));
        assert!(!is_registry_url("http://plugins.example.test/"));
        assert!(!is_registry_url("ftp://plugins.example.test/"));
        let bare = PUB.lines().find(|l| !l.starts_with("untrusted")).unwrap();
        assert!(parse_config("https://plugins.example.test/", bare).is_ok());
        assert!(
            parse_config("https://plugins.example.test/", PUB).is_ok(),
            "two-line key file also accepted"
        );
        assert!(parse_config("http://localhost.attacker.example/registry", bare).is_err());
        assert!(parse_config("https://x.test", "not-a-key").is_err());
    }

    #[test]
    fn permissions_follow_the_manifest_vocabulary_and_the_grant_is_an_intersection() {
        assert!(is_permission("meeting:read"));
        assert!(is_permission("net:fetch:hooks.slack.com"));
        assert!(is_permission("net:fetch:*.example.com"));
        assert!(is_permission("net:fetch:localhost:8787"));
        assert!(!is_permission("frames:read"));
        assert!(!is_permission("net:fetch:*"));
        assert!(!is_permission("totally:made:up"));
        assert!(!is_permission("net:fetch:https://x.com"));
        let granted = granted_permissions(
            &[
                "data:publish".into(),
                "frames:read".into(),
                "net:fetch:*".into(),
                "ui:toast".into(),
                "data:publish".into(),
            ],
            &["data:publish".into(), "meeting:read".into()],
        );
        assert_eq!(
            granted,
            vec!["data:publish".to_string()],
            "only known permissions the manifest also asks for"
        );
    }

    #[test]
    fn anti_rollback_refuses_older_indexes_for_the_same_registry_only() {
        let seen = RegistrySeen {
            url: "https://r.test".into(),
            generated_at_ms: 1000,
            signed_at_s: 10,
        };
        assert!(check_not_rolled_back(None, "https://r.test", 1, 1).is_ok());
        assert!(check_not_rolled_back(Some(&seen), "https://r.test", 1000, 10).is_ok());
        assert!(check_not_rolled_back(Some(&seen), "https://r.test", 999, 11).is_err());
        assert!(check_not_rolled_back(Some(&seen), "https://r.test", 1001, 9).is_err());
        assert!(
            check_not_rolled_back(Some(&seen), "https://other.test", 1, 1).is_ok(),
            "a different registry has its own history"
        );
    }

    #[test]
    fn verify_bundle_names_each_broken_link() {
        let entry = fixture_entry();
        let bytes = BUNDLE.as_bytes();
        let perms = verify_bundle(PUB, bytes, BUNDLE_SIG, "petal.test-hello", &entry).unwrap();
        assert!(perms.contains(&"meeting:read".to_string()));
        let mut flipped = bytes.to_vec();
        let n = flipped.len();
        flipped[n - 3] ^= 1;
        assert!(
            verify_bundle(PUB, &flipped, BUNDLE_SIG, "petal.test-hello", &entry)
                .unwrap_err()
                .contains("sha256")
        );
        assert!(verify_bundle(PUB, bytes, BUNDLE_SIG, "acme.other", &entry)
            .unwrap_err()
            .contains("expected acme.other"));
        let mut wrong_version = entry.clone();
        wrong_version.version = "2.0.0".into();
        assert!(
            verify_bundle(PUB, bytes, BUNDLE_SIG, "petal.test-hello", &wrong_version)
                .unwrap_err()
                .contains("version")
        );
        let mut wrong_sig = BUNDLE_SIG.to_string();
        wrong_sig = wrong_sig.replace("trusted comment: timestamp", "trusted comment: timestamq");
        assert!(
            verify_bundle(PUB, bytes, &wrong_sig, "petal.test-hello", &entry)
                .unwrap_err()
                .contains("signature")
        );
    }

    // ---- end to end against a fake registry ----

    /// `install()` with the bundle/sig urls re-homed from the fixture's `plugins.example.test`
    /// onto the fake server. The signed index cannot name the fake origin (the fixture's
    /// secret key is discarded at generation), so this mirrors `install()` step for step; the
    /// only difference is the url resolver.
    async fn install_via(
        config: &RegistryConfig,
        id: &str,
        version: &str,
        resolve: impl Fn(&str) -> String,
    ) -> Result<InstalledRecord, String> {
        let verified = fetch_index(config).await?;
        let plugin = verified
            .index
            .plugins
            .iter()
            .find(|p| p.id == id)
            .ok_or("not in registry")?;
        let entry = plugin
            .versions
            .iter()
            .find(|v| v.version == version)
            .ok_or("version not in registry")?;
        if !entry.verified {
            return Err("not verified".into());
        }
        let client = registry_client();
        let bundle = fetch_bytes(
            &client,
            &config.url,
            &resolve(&entry.bundle_url),
            entry.size.min(BUNDLE_MAX_BYTES),
        )
        .await?;
        let sig_text = fetch_text(
            &client,
            &config.url,
            &resolve(&entry.sig_url),
            SIGNATURE_MAX_BYTES,
        )
        .await?;
        let manifest_permissions =
            verify_bundle(&config.public_key, &bundle, &sig_text, id, entry)?;
        let record = InstalledRecord {
            version: version.to_string(),
            enabled: true,
            source: "registry".to_string(),
            granted_permissions: granted_permissions(&entry.permissions, &manifest_permissions),
            installed_at_ms: now_ms(),
            sha256: Some(entry.sha256.clone()),
        };
        store::install(id, record.clone(), &bundle)?;
        Ok(record)
    }

    fn rehome(base: &str) -> impl Fn(&str) -> String + '_ {
        move |url: &str| url.replace("https://plugins.example.test", base)
    }

    #[test]
    fn end_to_end_install_stores_only_a_fully_verified_bundle() {
        let _guard = store::test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_store();
        block_on(async {
            let server = spawn_registry(fixture_files(BUNDLE.as_bytes(), BUNDLE_SIG));
            let config = config_for(&server.url);
            let record = install_via(&config, "petal.test-hello", "1.0.0", rehome(&server.url))
                .await
                .unwrap();
            assert_eq!(record.version, "1.0.0");
            assert_eq!(
                record.granted_permissions,
                vec![
                    "meeting:read",
                    "state:write",
                    "ui:toast",
                    "ui:toolbar-button"
                ]
            );
            assert!(dir.join("petal.test-hello/1.0.0/bundle.json").exists());
            assert_eq!(store::read_bundle("petal.test-hello").unwrap(), BUNDLE);
            let seen = store::registry_seen().unwrap().unwrap();
            assert_eq!(seen.signed_at_s, 1_788_868_800);
            assert_eq!(
                server.hits.lock().unwrap().len(),
                4,
                "index, index sig, bundle, bundle sig"
            );
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tampered_bundle_is_refused_and_the_store_is_untouched() {
        let _guard = store::test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_store();
        block_on(async {
            let mut evil = BUNDLE.as_bytes().to_vec();
            // Same length, different content: sha256 catches it before the signature is even checked.
            let n = evil.len();
            evil[n - 3] ^= 1;
            let server = spawn_registry(fixture_files(&evil, BUNDLE_SIG));
            let config = config_for(&server.url);
            let err = install_via(&config, "petal.test-hello", "1.0.0", rehome(&server.url))
                .await
                .unwrap_err();
            assert!(err.contains("sha256"), "{err}");
            assert!(
                store::list().unwrap().plugins.is_empty(),
                "nothing recorded"
            );
            assert!(!dir.join("petal.test-hello").exists(), "nothing on disk");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bundle_with_a_wrong_signature_is_refused() {
        let _guard = store::test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_store();
        block_on(async {
            let server = spawn_registry(fixture_files(BUNDLE.as_bytes(), INDEX_SIG)); // index's sig served for the bundle
            let config = config_for(&server.url);
            let err = install_via(&config, "petal.test-hello", "1.0.0", rehome(&server.url))
                .await
                .unwrap_err();
            assert!(err.contains("bundle signature"), "{err}");
            assert!(!dir.join("petal.test-hello").exists());
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tampered_index_is_refused_before_any_bundle_fetch() {
        let _guard = store::test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_store();
        block_on(async {
            let mut files = fixture_files(BUNDLE.as_bytes(), BUNDLE_SIG);
            files.insert(
                "/index.json",
                (
                    200,
                    vec![],
                    INDEX
                        .replace("\"verified\": false", "\"verified\": true")
                        .into_bytes(),
                ),
            );
            let server = spawn_registry(files);
            let err = fetch_index(&config_for(&server.url)).await.unwrap_err();
            assert!(err.contains("index signature"), "{err}");
            assert_eq!(
                server.hits.lock().unwrap().len(),
                2,
                "index + its signature only"
            );
            assert!(
                store::registry_seen().unwrap().is_none(),
                "a refused index is never recorded as seen"
            );
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rolled_back_index_is_refused_once_a_newer_one_was_accepted() {
        let _guard = store::test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_store();
        block_on(async {
            let server = spawn_registry(fixture_files(BUNDLE.as_bytes(), BUNDLE_SIG));
            let config = config_for(&server.url);
            store::record_registry_seen(RegistrySeen {
                url: config.url.clone(),
                generated_at_ms: parse_rfc3339_utc_ms("2026-09-09T00:00:00.000Z").unwrap(),
                signed_at_s: 1_788_868_800,
            })
            .unwrap();
            let err = fetch_index(&config).await.unwrap_err();
            assert!(err.contains("rollback"), "{err}");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn redirects_are_not_followed() {
        let _guard = store::test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_store();
        block_on(async {
            let mut files = HashMap::new();
            files.insert(
                "/index.json",
                (
                    302,
                    vec![("Location", "http://127.0.0.1:1/evil".to_string())],
                    Vec::new(),
                ),
            );
            let server = spawn_registry(files);
            let err = fetch_index(&config_for(&server.url)).await.unwrap_err();
            assert!(err.contains("redirect"), "{err}");
            assert!(
                err.contains(&server.url),
                "error names the configured registry: {err}"
            );
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_bodies_are_cut_off_at_the_cap_not_buffered() {
        let _guard = store::test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_store();
        block_on(async {
            let big = vec![b'x'; 3 * 1024 * 1024];
            let mut files = HashMap::new();
            // Declared length above the cap: refused from the header alone.
            files.insert("/index.json", (200, vec![], big.clone()));
            // No Content-Length: the stream must be aborted once the cap is crossed.
            files.insert(
                "/index.json.minisig",
                (200, vec![("X-No-Length", "1".to_string())], big),
            );
            let server = spawn_registry(files);
            let client = registry_client();
            let err = fetch_bytes(
                &client,
                &server.url,
                &format!("{}/index.json", server.url),
                INDEX_MAX_BYTES,
            )
            .await
            .unwrap_err();
            assert!(err.contains("exceeds"), "{err}");
            let err = fetch_bytes(
                &client,
                &server.url,
                &format!("{}/index.json.minisig", server.url),
                SIGNATURE_MAX_BYTES,
            )
            .await
            .unwrap_err();
            assert!(err.contains("aborted"), "{err}");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_public_key_is_a_compile_time_constant_only() {
        // `config()` reads the key from option_env! only. Guard the property in code review's
        // absence: the runtime environment never contributes a key.
        let src = include_str!("registry.rs");
        let body = src
            .split("pub fn config()")
            .nth(1)
            .unwrap()
            .split("pub fn parse_config")
            .next()
            .unwrap();
        // Positive guard, not a blocklist: a negative match only rejects the spellings its
        // author thought of (`env::var("PETAL_PLUGIN_REGISTRY_PUBKEY")` with `use std::env;`
        // in scope slips past one). Instead require that EVERY mention of the key's name in
        // `config()` is the compile-time read -- any other way of reading it, however spelled,
        // adds a mention that is not inside `option_env!` and fails here.
        let baked = "option_env!(\"PETAL_PLUGIN_REGISTRY_PUBKEY\")";
        let mentions = body.matches("PETAL_PLUGIN_REGISTRY_PUBKEY").count();
        let baked_reads = body.matches(baked).count();
        assert!(baked_reads >= 1, "config() must read the key via {baked}");
        assert_eq!(
            mentions, baked_reads,
            "every PETAL_PLUGIN_REGISTRY_PUBKEY read in config() must be {baked}; \
             found {mentions} mention(s) but only {baked_reads} compile-time read(s) -- \
             a runtime key override must never return, in any spelling"
        );
        assert!(
            body.contains("cfg!(debug_assertions)"),
            "URL override stays debug-only"
        );
    }
}
