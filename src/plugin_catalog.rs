//! Official plugin catalog metadata, selection, caching, and downloads.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

const REPOSITORY: &str = "nklmilojevic/sofka-plugins";
const COMMIT_URL: &str = "https://api.github.com/repos/nklmilojevic/sofka-plugins/commits/HEAD";
const RAW_ROOT: &str = "https://raw.githubusercontent.com/nklmilojevic/sofka-plugins";
const RELEASE_ROOT: &str = "https://github.com/nklmilojevic/sofka-plugins/releases/download/";
const CATALOG_MAX_BYTES: usize = 10 * 1024 * 1024;
pub const ARTIFACT_MAX_BYTES: usize = 50 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REDIRECT_LIMIT: usize = 5;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema_version: u32,
    pub generated_at: String,
    pub plugins: Vec<CatalogPlugin>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPlugin {
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub publisher: String,
    pub repository: String,
    pub versions: Vec<CatalogVersion>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogVersion {
    pub version: String,
    pub sofka: String,
    pub source_commit: String,
    pub license: String,
    pub readme: String,
    #[serde(default)]
    pub requirements: Vec<RuntimeRequirement>,
    pub command: String,
    #[serde(default = "default_target")]
    pub target: String,
    pub output: String,
    pub mutating: bool,
    #[serde(default)]
    pub confirm: bool,
    #[serde(default)]
    pub dangerous: bool,
    #[serde(default)]
    pub network_load: bool,
    pub status: VersionStatus,
    #[serde(default)]
    pub withdrawal_reason: Option<String>,
    pub artifacts: Vec<Artifact>,
}

fn default_target() -> String {
    "selection".into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequirement {
    pub name: String,
    pub install: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionStatus {
    Active,
    Withdrawn,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub platform: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CachedCatalog {
    schema_version: u32,
    commit: String,
    fetched_at: u64,
    catalog: Catalog,
}

#[derive(Clone, Debug)]
pub struct CatalogSnapshot {
    pub catalog: Catalog,
    pub commit: String,
    pub fetched_at: u64,
    pub offline: bool,
}

#[derive(Clone, Debug)]
pub struct Selection<'a> {
    pub plugin: &'a CatalogPlugin,
    pub version: &'a CatalogVersion,
    pub artifact: &'a Artifact,
}

impl Catalog {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > CATALOG_MAX_BYTES {
            return Err("catalog exceeds 10 MiB".into());
        }
        let catalog: Self =
            serde_json::from_slice(bytes).map_err(|e| format!("invalid catalog JSON: {e}"))?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!(
                "unsupported catalog schema_version {} (expected 1)",
                self.schema_version
            ));
        }
        let mut ids = HashSet::new();
        let mut folded_ids = HashSet::new();
        for plugin in &self.plugins {
            validate_id(&plugin.id)?;
            if !ids.insert(&plugin.id) || !folded_ids.insert(plugin.id.to_ascii_lowercase()) {
                return Err(format!("duplicate plugin ID {:?}", plugin.id));
            }
            if plugin.display_name.trim().is_empty() || plugin.description.trim().is_empty() {
                return Err(format!("plugin {} has empty display metadata", plugin.id));
            }
            if !plugin.repository.starts_with("https://") {
                return Err(format!("plugin {} repository must use HTTPS", plugin.id));
            }
            let mut versions = HashSet::new();
            for release in &plugin.versions {
                let version = Version::parse(&release.version)
                    .map_err(|e| format!("plugin {} version: {e}", plugin.id))?;
                if !versions.insert(version) {
                    return Err(format!(
                        "plugin {} has duplicate version {}",
                        plugin.id, release.version
                    ));
                }
                VersionReq::parse(&release.sofka).map_err(|e| {
                    format!(
                        "plugin {} version {} compatibility: {e}",
                        plugin.id, release.version
                    )
                })?;
                if release.source_commit.len() != 40
                    || !release.source_commit.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(format!(
                        "plugin {} version {} has an invalid source commit",
                        plugin.id, release.version
                    ));
                }
                if release.license.trim().is_empty() || !release.readme.starts_with("https://") {
                    return Err(format!(
                        "plugin {} version {} has invalid license or README metadata",
                        plugin.id, release.version
                    ));
                }
                if release.command.trim().is_empty()
                    || !matches!(release.target.as_str(), "selection" | "context")
                    || !matches!(release.output.as_str(), "popup" | "background" | "report")
                {
                    return Err(format!(
                        "plugin {} version {} has invalid execution metadata",
                        plugin.id, release.version
                    ));
                }
                if release.requirements.iter().any(|requirement| {
                    requirement.name.trim().is_empty() || requirement.install.trim().is_empty()
                }) {
                    return Err(format!(
                        "plugin {} version {} has an invalid runtime requirement",
                        plugin.id, release.version
                    ));
                }
                match release.status {
                    VersionStatus::Withdrawn
                        if release
                            .withdrawal_reason
                            .as_deref()
                            .is_none_or(|reason| reason.trim().is_empty()) =>
                    {
                        return Err(format!(
                            "plugin {} version {} is withdrawn without an explanation",
                            plugin.id, release.version
                        ));
                    }
                    VersionStatus::Active if release.withdrawal_reason.is_some() => {
                        return Err(format!(
                            "plugin {} version {} is active but has a withdrawal explanation",
                            plugin.id, release.version
                        ));
                    }
                    _ => {}
                }
                if release.artifacts.is_empty() {
                    return Err(format!(
                        "plugin {} version {} has no artifacts",
                        plugin.id, release.version
                    ));
                }
                let mut platforms = HashSet::new();
                for artifact in &release.artifacts {
                    if !platforms.insert(&artifact.platform) {
                        return Err(format!(
                            "plugin {} version {} has duplicate platform {}",
                            plugin.id, release.version, artifact.platform
                        ));
                    }
                    validate_artifact(artifact).map_err(|e| {
                        format!("plugin {} version {}: {e}", plugin.id, release.version)
                    })?;
                }
            }
        }
        Ok(())
    }

    pub fn find(&self, id: &str) -> Result<&CatalogPlugin, String> {
        self.plugins
            .iter()
            .find(|plugin| plugin.id == id)
            .ok_or_else(|| format!("unknown plugin {id:?}"))
    }

    pub fn matching(&self, query: &str) -> Vec<&CatalogPlugin> {
        let query = query.to_ascii_lowercase();
        let mut plugins: Vec<_> = self
            .plugins
            .iter()
            .filter(|plugin| {
                query.is_empty()
                    || plugin.id.to_ascii_lowercase().contains(&query)
                    || plugin.display_name.to_ascii_lowercase().contains(&query)
                    || plugin.description.to_ascii_lowercase().contains(&query)
                    || plugin
                        .tags
                        .iter()
                        .any(|tag| tag.to_ascii_lowercase().contains(&query))
            })
            .collect();
        plugins.sort_by(|a, b| a.id.cmp(&b.id));
        plugins
    }

    pub fn select(&self, request: &str) -> Result<Selection<'_>, String> {
        let (id, exact) = parse_request(request)?;
        let plugin = self.find(id)?;
        let current = Version::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|e| format!("invalid sofka build version: {e}"))?;
        let platform = platform()?;
        let release = if let Some(exact) = exact {
            let wanted = Version::parse(exact)
                .map_err(|e| format!("invalid requested version {exact:?}: {e}"))?;
            plugin
                .versions
                .iter()
                .find(|release| Version::parse(&release.version).ok().as_ref() == Some(&wanted))
                .ok_or_else(|| format!("plugin {id} has no version {exact}"))?
        } else {
            plugin
                .versions
                .iter()
                .filter_map(|release| {
                    let version = Version::parse(&release.version).ok()?;
                    let compatible = VersionReq::parse(&release.sofka).ok()?.matches(&current);
                    (version.pre.is_empty()
                        && compatible
                        && matches!(release.status, VersionStatus::Active)
                        && release.artifacts.iter().any(|artifact| {
                            artifact.platform == platform || artifact.platform == "any"
                        }))
                    .then_some((version, release))
                })
                .max_by(|(a, _), (b, _)| a.cmp(b))
                .map(|(_, release)| release)
                .ok_or_else(|| format!("plugin {id} has no compatible stable version"))?
        };
        if matches!(release.status, VersionStatus::Withdrawn) {
            return Err(format!(
                "plugin {id} version {} was withdrawn: {}",
                release.version,
                release
                    .withdrawal_reason
                    .as_deref()
                    .unwrap_or("no reason given")
            ));
        }
        let requirement = VersionReq::parse(&release.sofka).expect("validated compatibility");
        if !requirement.matches(&current) {
            return Err(format!(
                "plugin {id} version {} requires sofka {}, current version is {current}",
                release.version, release.sofka
            ));
        }
        let artifact = release
            .artifacts
            .iter()
            .find(|artifact| artifact.platform == platform)
            .or_else(|| {
                release
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.platform == "any")
            })
            .ok_or_else(|| {
                format!(
                    "plugin {id} version {} has no artifact for {platform}",
                    release.version
                )
            })?;
        Ok(Selection {
            plugin,
            version: release,
            artifact,
        })
    }
}

impl CatalogPlugin {
    pub fn latest_compatible(&self) -> Option<&CatalogVersion> {
        let current = Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
        let platform = platform().ok()?;
        self.versions
            .iter()
            .filter_map(|release| {
                let version = Version::parse(&release.version).ok()?;
                let compatible = VersionReq::parse(&release.sofka).ok()?.matches(&current);
                (version.pre.is_empty()
                    && compatible
                    && matches!(release.status, VersionStatus::Active))
                .then_some(release)
                .filter(|release| {
                    release
                        .artifacts
                        .iter()
                        .any(|artifact| artifact.platform == platform || artifact.platform == "any")
                })
                .map(|release| (version, release))
            })
            .max_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, release)| release)
    }
}

fn validate_id(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && id.as_bytes()[0].is_ascii_alphanumeric();
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid plugin ID {id:?}; use lowercase ASCII letters, digits, and hyphens"
        ))
    }
}

fn validate_artifact(artifact: &Artifact) -> Result<(), String> {
    if artifact.platform != "any"
        && !matches!(
            artifact.platform.as_str(),
            "x86_64-unknown-linux-gnu"
                | "aarch64-unknown-linux-gnu"
                | "x86_64-apple-darwin"
                | "aarch64-apple-darwin"
        )
    {
        return Err(format!("unsupported platform {:?}", artifact.platform));
    }
    if !artifact.url.starts_with(RELEASE_ROOT) {
        return Err(format!(
            "artifact URL must be a release asset of {REPOSITORY}"
        ));
    }
    if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("artifact SHA-256 must contain 64 hexadecimal characters".into());
    }
    if artifact.size == 0 || artifact.size > ARTIFACT_MAX_BYTES as u64 {
        return Err("artifact compressed size must be between 1 byte and 50 MiB".into());
    }
    Ok(())
}

pub fn parse_request(request: &str) -> Result<(&str, Option<&str>), String> {
    let (id, version) = request
        .split_once('@')
        .map_or((request, None), |(id, version)| (id, Some(version)));
    validate_id(id)?;
    if version.is_some_and(str::is_empty) || version.is_some_and(|v| v.contains('@')) {
        return Err(format!("invalid plugin request {request:?}"));
    }
    Ok((id, version))
}

pub fn platform() -> Result<&'static str, String> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        ("x86_64", "macos") => Ok("x86_64-apple-darwin"),
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        (arch, os) => Err(format!("unsupported plugin platform {arch}-{os}")),
    }
}

pub fn cache_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(path).join("sofka").join("plugins");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(home)
            .join(".cache")
            .join("sofka")
            .join("plugins");
    }
    std::env::temp_dir().join("sofka").join("plugins")
}

pub fn config_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|p| !p.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .ok_or_else(|| {
            "cannot determine config directory: set XDG_CONFIG_HOME or HOME".to_string()
        })?;
    Ok(base.join("sofka"))
}

pub async fn load(offline: bool) -> Result<CatalogSnapshot, String> {
    if offline {
        return load_cached(&cache_dir().join("catalog-cache.json"));
    }
    let commit_bytes = get(COMMIT_URL, CATALOG_MAX_BYTES).await?;
    #[derive(Deserialize)]
    struct Commit {
        sha: String,
    }
    let commit: Commit = serde_json::from_slice(&commit_bytes)
        .map_err(|e| format!("invalid GitHub commit response: {e}"))?;
    if commit.sha.len() != 40 || !commit.sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("GitHub returned an invalid catalog commit".into());
    }
    let url = format!("{RAW_ROOT}/{}/index.json", commit.sha);
    let bytes = get(&url, CATALOG_MAX_BYTES).await?;
    let catalog = Catalog::parse(&bytes)?;
    let fetched_at = now();
    let cached = CachedCatalog {
        schema_version: 1,
        commit: commit.sha.clone(),
        fetched_at,
        catalog: catalog.clone(),
    };
    let cache = serde_json::to_string(&cached).map_err(|e| e.to_string())?;
    crate::atomicfile::write(&cache_dir().join("catalog-cache.json"), &cache)
        .map_err(|e| format!("caching catalog: {e}"))?;
    Ok(CatalogSnapshot {
        catalog,
        commit: commit.sha,
        fetched_at,
        offline: false,
    })
}

fn load_cached(path: &Path) -> Result<CatalogSnapshot, String> {
    let bytes = std::fs::read(path).map_err(|e| {
        format!(
            "offline catalog is unavailable at {}: {e}; run without --offline once",
            path.display()
        )
    })?;
    if bytes.len() > CATALOG_MAX_BYTES + 1024 * 1024 {
        return Err("cached catalog exceeds its size limit".into());
    }
    let cached: CachedCatalog =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid cached catalog: {e}"))?;
    if cached.schema_version != 1 {
        return Err("unsupported catalog cache version".into());
    }
    if cached.commit.len() != 40 || !cached.commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("cached catalog has an invalid commit".into());
    }
    cached.catalog.validate()?;
    Ok(CatalogSnapshot {
        catalog: cached.catalog,
        commit: cached.commit,
        fetched_at: cached.fetched_at,
        offline: true,
    })
}

pub fn age(fetched_at: u64) -> String {
    let seconds = now().saturating_sub(fetched_at);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

pub async fn artifact(artifact: &Artifact, offline: bool) -> Result<PathBuf, String> {
    validate_artifact(artifact)?;
    let path = cache_dir()
        .join("artifacts")
        .join(format!("{}.tar.gz", artifact.sha256.to_ascii_lowercase()));
    if path.is_file() {
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        if digest(&bytes) == artifact.sha256.to_ascii_lowercase() {
            return Ok(path);
        }
        std::fs::remove_file(&path)
            .map_err(|e| format!("removing corrupt cached artifact {}: {e}", path.display()))?;
    }
    if offline {
        return Err(format!(
            "artifact {} is not cached; run install without --offline once",
            artifact.sha256
        ));
    }
    let bytes = get(&artifact.url, ARTIFACT_MAX_BYTES).await?;
    if bytes.len() as u64 != artifact.size {
        return Err(format!(
            "artifact size mismatch: catalog says {}, downloaded {}",
            artifact.size,
            bytes.len()
        ));
    }
    let actual = digest(&bytes);
    if actual != artifact.sha256.to_ascii_lowercase() {
        return Err(format!(
            "artifact SHA-256 mismatch: expected {}, received {actual}",
            artifact.sha256
        ));
    }
    write_bytes(&path, &bytes).map_err(|e| format!("caching artifact: {e}"))?;
    Ok(path)
}

pub fn digest(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let value = sha2::Sha256::digest(bytes);
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temp = path.with_extension(format!("tmp-{}-{nonce:x}", std::process::id()));
    let mut created = false;
    let result = (|| {
        let mut file = std::fs::File::options()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        created = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(temp);
    }
    result
}

type HttpClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

fn client(allow_http: bool) -> Result<HttpClient, String> {
    let builder = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .map_err(|e| format!("loading system TLS roots: {e}"))?;
    let https = if allow_http {
        builder.https_or_http().enable_http1().build()
    } else {
        builder.https_only().enable_http1().build()
    };
    Ok(
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(https),
    )
}

async fn get(url: &str, limit: usize) -> Result<Vec<u8>, String> {
    get_with(url, limit, REQUEST_TIMEOUT, false).await
}

async fn get_with(
    url: &str,
    limit: usize,
    timeout: Duration,
    allow_test_http: bool,
) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, get_inner(url, limit, allow_test_http))
        .await
        .map_err(|_| format!("request timed out after {}ms: {url}", timeout.as_millis()))?
}

async fn get_inner(url: &str, limit: usize, allow_test_http: bool) -> Result<Vec<u8>, String> {
    let client = client(allow_test_http)?;
    let mut current = url.to_string();
    for redirects in 0..=REDIRECT_LIMIT {
        let uri: http::Uri = current
            .parse()
            .map_err(|e| format!("invalid catalog URL: {e}"))?;
        validate_http_uri(&uri, allow_test_http)?;
        let request = http::Request::get(uri)
            .header(
                http::header::USER_AGENT,
                format!("sofka/{}", env!("CARGO_PKG_VERSION")),
            )
            .header(http::header::ACCEPT, "application/vnd.github+json")
            .body(Full::new(Bytes::new()))
            .map_err(|e| format!("building request: {e}"))?;
        let response = client
            .request(request)
            .await
            .map_err(|e| format!("requesting {current}: {e}"))?;
        if response.status().is_redirection() {
            if redirects == REDIRECT_LIMIT {
                return Err("too many GitHub download redirects".into());
            }
            let location = response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "GitHub redirect has no valid Location".to_string())?;
            current = location.to_string();
            continue;
        }
        let status = response.status();
        let rate_limited = status == http::StatusCode::FORBIDDEN
            && response.headers().contains_key("x-ratelimit-remaining");
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| format!("reading response: {e}"))?;
            if let Ok(data) = frame.into_data() {
                if bytes.len().saturating_add(data.len()) > limit {
                    return Err(format!("download exceeds {} MiB", limit / 1024 / 1024));
                }
                bytes.extend_from_slice(&data);
            }
        }
        if !status.is_success() {
            if rate_limited {
                return Err("GitHub API rate limit reached; retry later or use --offline".into());
            }
            let detail: String = String::from_utf8_lossy(&bytes)
                .trim()
                .chars()
                .take(200)
                .collect();
            return Err(if detail.is_empty() {
                format!("HTTP {status} from {current}")
            } else {
                format!("HTTP {status} from {current}: {detail}")
            });
        }
        return Ok(bytes);
    }
    unreachable!()
}

fn validate_http_uri(uri: &http::Uri, allow_test_http: bool) -> Result<(), String> {
    let local_test = allow_test_http
        && uri.scheme_str() == Some("http")
        && matches!(uri.host(), Some("127.0.0.1" | "localhost"));
    if uri.scheme_str() != Some("https") && !local_test {
        return Err("catalog downloads require HTTPS".into());
    }
    let host = uri.host().unwrap_or_default();
    if local_test
        || matches!(
            host,
            "api.github.com"
                | "raw.githubusercontent.com"
                | "github.com"
                | "release-assets.githubusercontent.com"
                | "objects.githubusercontent.com"
        )
    {
        Ok(())
    } else {
        Err(format!("refusing download from untrusted host {host:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(response: Vec<u8>, delay: Duration) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            std::thread::sleep(delay);
            let _ = stream.write_all(&response);
        });
        format!("http://{address}/fixture")
    }

    fn catalog() -> Catalog {
        Catalog {
            schema_version: 1,
            generated_at: "2026-09-11T00:00:00Z".into(),
            plugins: vec![CatalogPlugin {
                id: "resource-summary".into(),
                display_name: "Resource summary".into(),
                description: "Summarize a resource".into(),
                tags: vec!["report".into()],
                publisher: "sofka".into(),
                repository: "https://github.com/nklmilojevic/sofka-plugins".into(),
                versions: vec![CatalogVersion {
                    version: "0.1.0".into(),
                    sofka: ">=0.1.0, <1.0.0".into(),
                    source_commit: "0".repeat(40),
                    license: "MIT OR Apache-2.0".into(),
                    readme: "https://example.com/readme".into(),
                    requirements: vec![],
                    command: "./resource-summary".into(),
                    target: "selection".into(),
                    output: "report".into(),
                    mutating: false,
                    confirm: false,
                    dangerous: false,
                    network_load: false,
                    status: VersionStatus::Active,
                    withdrawal_reason: None,
                    artifacts: vec![Artifact {
                        platform: "any".into(),
                        url: format!(
                            "{RELEASE_ROOT}resource-summary-v0.1.0/resource-summary.tar.gz"
                        ),
                        sha256: "0".repeat(64),
                        size: 10,
                    }],
                }],
            }],
        }
    }

    #[test]
    fn validates_and_selects_catalog_entries() {
        let catalog = catalog();
        catalog.validate().unwrap();
        let selected = catalog.select("resource-summary").unwrap();
        assert_eq!(selected.version.version, "0.1.0");
        assert_eq!(selected.artifact.platform, "any");
        assert!(catalog.select("missing").is_err());
    }

    #[test]
    fn search_is_case_insensitive_sorted_and_matches_tags() {
        let mut catalog = catalog();
        let mut second = catalog.plugins[0].clone();
        second.id = "alpha".into();
        second.display_name = "Other".into();
        second.description = "Something else".into();
        catalog.plugins.push(second);
        let ids: Vec<_> = catalog
            .matching("REPORT")
            .into_iter()
            .map(|plugin| plugin.id.as_str())
            .collect();
        assert_eq!(ids, ["alpha", "resource-summary"]);
        assert!(catalog.matching("absent").is_empty());
    }

    #[test]
    fn rejects_unsafe_ids_urls_hashes_and_withdrawals() {
        for id in ["../bad", "Bad", ".", "bad/name"] {
            let mut catalog = catalog();
            catalog.plugins[0].id = id.into();
            assert!(catalog.validate().is_err(), "accepted {id}");
        }
        let mut invalid_url = catalog();
        invalid_url.plugins[0].versions[0].artifacts[0].url = "https://example.com/a".into();
        assert!(invalid_url.validate().is_err());
        let mut withdrawn = catalog();
        withdrawn.plugins[0].versions[0].status = VersionStatus::Withdrawn;
        assert!(withdrawn.validate().is_err());
    }

    #[test]
    fn exact_withdrawn_versions_are_rejected() {
        let mut catalog = catalog();
        catalog.plugins[0].versions[0].status = VersionStatus::Withdrawn;
        catalog.plugins[0].versions[0].withdrawal_reason = Some("unsafe output".into());
        let error = catalog.select("resource-summary@0.1.0").unwrap_err();
        assert!(error.contains("withdrawn"));
    }

    #[test]
    fn selection_skips_prerelease_incompatible_and_unsupported_versions() {
        let mut catalog = catalog();
        let base = catalog.plugins[0].versions[0].clone();
        let mut prerelease = base.clone();
        prerelease.version = "9.0.0-beta.1".into();
        let mut incompatible = base.clone();
        incompatible.version = "8.0.0".into();
        incompatible.sofka = ">=2.0.0".into();
        let mut wrong_platform = base;
        wrong_platform.version = "7.0.0".into();
        wrong_platform.artifacts[0].platform = if platform().unwrap().contains("linux") {
            "aarch64-apple-darwin".into()
        } else {
            "aarch64-unknown-linux-gnu".into()
        };
        catalog.plugins[0]
            .versions
            .extend([prerelease, incompatible, wrong_platform]);

        assert_eq!(
            catalog.select("resource-summary").unwrap().version.version,
            "0.1.0"
        );
        assert!(catalog.select("resource-summary@8.0.0").is_err());
    }

    #[test]
    fn cached_catalog_is_validated_before_offline_use() {
        let path =
            std::env::temp_dir().join(format!("sofka-catalog-cache-{}.json", std::process::id()));
        let cached = CachedCatalog {
            schema_version: 1,
            commit: "a".repeat(40),
            fetched_at: 1,
            catalog: catalog(),
        };
        std::fs::write(&path, serde_json::to_vec(&cached).unwrap()).unwrap();
        let snapshot = load_cached(&path).unwrap();
        assert!(snapshot.offline);
        assert_eq!(snapshot.commit, "a".repeat(40));

        std::fs::write(&path, b"{}").unwrap();
        assert!(load_cached(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn catalog_parser_enforces_the_download_limit() {
        assert!(Catalog::parse(&vec![b' '; CATALOG_MAX_BYTES + 1]).is_err());
    }

    #[tokio::test]
    async fn http_reader_rejects_failures_oversize_and_timeouts() {
        let url = server(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\n\r\ndown".to_vec(),
            Duration::ZERO,
        );
        let error = get_with(&url, 100, Duration::from_secs(1), true)
            .await
            .unwrap_err();
        assert!(error.contains("503") && error.contains("down"));

        let url = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world".to_vec(),
            Duration::ZERO,
        );
        let error = get_with(&url, 5, Duration::from_secs(1), true)
            .await
            .unwrap_err();
        assert!(error.contains("exceeds"));

        let url = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort".to_vec(),
            Duration::ZERO,
        );
        assert!(
            get_with(&url, 100, Duration::from_secs(1), true)
                .await
                .is_err()
        );

        let url = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
            Duration::from_millis(100),
        );
        let error = get_with(&url, 100, Duration::from_millis(10), true)
            .await
            .unwrap_err();
        assert!(error.contains("timed out"));
    }

    #[tokio::test]
    async fn http_reader_reports_public_rate_limits() {
        let url = server(
            b"HTTP/1.1 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nContent-Length: 2\r\n\r\n{}"
                .to_vec(),
            Duration::ZERO,
        );
        let error = get_with(&url, 100, Duration::from_secs(1), true)
            .await
            .unwrap_err();
        assert!(error.contains("rate limit") && error.contains("--offline"));
    }
}
