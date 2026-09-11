//! Safe installation records and filesystem activation for catalog plugins.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::{Component, Path, PathBuf};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::plugin_catalog::{self, CatalogSnapshot};

const RECORD: &str = ".sofka-install.json";
const STAGE_MARKER: &str = ".sofka-install-stage";
const RECORD_SCHEMA: u32 = 1;
/// Directories under this name are committed for deletion: whatever state an
/// interruption left them in, recovery discards them.
const REMOVED_PREFIX: &str = ".plugin-removed-";
const EXPANDED_MAX_BYTES: u64 = 200 * 1024 * 1024;
const FILE_MAX: usize = 2_000;
/// A package may hold thousands of files; an error message may not.
const MODIFIED_LISTED: usize = 5;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationRecord {
    pub schema_version: u32,
    pub id: String,
    pub package_version: String,
    pub catalog_commit: String,
    pub source_commit: String,
    pub artifact_digest: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstalledPackage {
    pub id: String,
    pub version: Option<String>,
    pub path: PathBuf,
    pub managed: bool,
    pub modified: bool,
}

pub struct InstallLock {
    file: File,
}

impl InstallLock {
    pub fn acquire(config: &Path) -> Result<Self, String> {
        ensure_directory_path(config)?;
        std::fs::create_dir_all(config)
            .map_err(|e| format!("creating {}: {e}", config.display()))?;
        ensure_directory_path(config)?;
        let path = config.join(".plugin-install.lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("opening {}: {e}", path.display()))?;
        file.try_lock_exclusive().map_err(|e| {
            format!(
                "another sofka plugin operation holds {}: {e}",
                path.display()
            )
        })?;
        ensure_directory_path(&config.join("plugins"))?;
        recover(config)?;
        Ok(Self { file })
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Debug)]
pub struct PreparedPackage {
    pub id: String,
    pub version: String,
    pub previous_version: Option<String>,
    /// Package directories whose plugin name or palette command collides with
    /// this one. The loader keeps the directory it reads first, so a collision
    /// hides one of the two packages.
    pub conflicts: Vec<PathBuf>,
    stage: Option<PathBuf>,
    destination: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    Installed,
    Updated,
    RolledBack,
    Unchanged,
}

impl PreparedPackage {
    /// Build one without running `prepare`, so the activation contract can be
    /// driven from tests in either module. A stage that does not exist models
    /// the filesystem failing partway through a batch.
    #[cfg(test)]
    pub(crate) fn staged(
        id: &str,
        version: &str,
        previous: Option<&str>,
        stage: PathBuf,
        destination: PathBuf,
    ) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            previous_version: previous.map(str::to_owned),
            conflicts: Vec::new(),
            stage: Some(stage),
            destination,
        }
    }

    pub fn activate(mut self) -> Result<Activation, String> {
        let Some(stage) = self.stage.take() else {
            return Ok(Activation::Unchanged);
        };
        let action = match self.previous_version.as_deref() {
            None => Activation::Installed,
            Some(old) => match (
                semver::Version::parse(old),
                semver::Version::parse(&self.version),
            ) {
                (Ok(old), Ok(new)) if new < old => Activation::RolledBack,
                _ => Activation::Updated,
            },
        };
        let config = self
            .destination
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| "invalid plugin destination".to_string())?;
        let backup = unique_path(config, &format!(".plugin-backup-{}", self.id));
        let had_destination = self.destination.exists();
        if had_destination {
            std::fs::rename(&self.destination, &backup).map_err(|e| {
                format!(
                    "staging previous {} as {}: {e}",
                    self.destination.display(),
                    backup.display()
                )
            })?;
        }
        if let Err(error) = std::fs::rename(&stage, &self.destination) {
            if had_destination {
                let _ = std::fs::rename(&backup, &self.destination);
            }
            return Err(format!(
                "activating {} at {}: {error}",
                self.id,
                self.destination.display()
            ));
        }
        std::fs::remove_file(self.destination.join(STAGE_MARKER)).map_err(|e| {
            format!(
                "{} was activated, but removing its staging marker failed: {e}",
                self.id
            )
        })?;
        if had_destination {
            // Rename first: a half-deleted backup is unidentifiable, so recovery
            // must see it under a name that means "already replaced".
            let discarded = unique_path(config, &format!("{REMOVED_PREFIX}{}", self.id));
            std::fs::rename(&backup, &discarded).map_err(|e| {
                format!(
                    "{} was activated, but retiring {} failed: {e}",
                    self.id,
                    backup.display()
                )
            })?;
            std::fs::remove_dir_all(&discarded).map_err(|e| {
                format!(
                    "{} was activated, but cleanup of {} failed: {e}",
                    self.id,
                    discarded.display()
                )
            })?;
        }
        Ok(action)
    }
}

impl Drop for PreparedPackage {
    fn drop(&mut self) {
        if let Some(stage) = self.stage.take() {
            let _ = std::fs::remove_dir_all(stage);
        }
    }
}

pub async fn prepare(
    snapshot: &CatalogSnapshot,
    requests: &[String],
    offline: bool,
) -> Result<Vec<PreparedPackage>, String> {
    prepare_below(
        &plugin_catalog::config_dir()?,
        &plugin_catalog::cache_dir(),
        snapshot,
        requests,
        offline,
    )
    .await
}

async fn prepare_below(
    config: &Path,
    cache: &Path,
    snapshot: &CatalogSnapshot,
    requests: &[String],
    offline: bool,
) -> Result<Vec<PreparedPackage>, String> {
    let plugins = config.join("plugins");
    ensure_directory_path(config)?;
    std::fs::create_dir_all(&plugins)
        .map_err(|e| format!("creating {}: {e}", plugins.display()))?;
    ensure_directory_path(&plugins)?;

    let mut requested: HashMap<String, String> = HashMap::new();
    let mut selections = Vec::new();
    for request in requests {
        let selection = snapshot.catalog.select(request)?;
        if let Some(previous) = requested.insert(
            selection.plugin.id.clone(),
            selection.version.version.clone(),
        ) && previous != selection.version.version
        {
            return Err(format!(
                "conflicting versions requested for {}: {previous} and {}",
                selection.plugin.id, selection.version.version
            ));
        }
        if selections
            .iter()
            .any(|(id, _, _, _): &(String, String, String, _)| id == &selection.plugin.id)
        {
            continue;
        }
        selections.push((
            selection.plugin.id.clone(),
            selection.version.version.clone(),
            selection.version.source_commit.clone(),
            selection.artifact.clone(),
        ));
    }

    let mut prepared = Vec::new();
    for (id, version, source_commit, artifact) in selections {
        let destination = plugins.join(&id);
        let previous = inspect_destination(&destination, &id)?;
        if let Some(record) = previous.as_ref()
            && record.package_version == version
        {
            if !record
                .artifact_digest
                .eq_ignore_ascii_case(&artifact.blake3)
            {
                return Err(format!(
                    "catalog digest for {id}@{version} differs from the installed immutable version"
                ));
            }
            prepared.push(PreparedPackage {
                id,
                version,
                previous_version: previous.map(|record| record.package_version),
                conflicts: Vec::new(),
                stage: None,
                destination,
            });
            continue;
        }
        let archive = plugin_catalog::artifact_in(cache, &artifact, offline).await?;
        let stage = unique_path(config, &format!(".plugin-stage-{id}"));
        std::fs::create_dir(&stage).map_err(|e| format!("creating {}: {e}", stage.display()))?;
        std::fs::write(
            stage.join(STAGE_MARKER),
            b"sofka plugin installation staging\n",
        )
        .map_err(|e| format!("marking {}: {e}", stage.display()))?;
        // Extraction hashes every byte it writes, so the record below needs no
        // second pass over the package.
        let staged = extract(&archive, &stage)
            .and_then(|files| crate::plugins::read_package(&stage).map(|package| (files, package)));
        let (files, package) = match staged {
            Ok(staged) => staged,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&stage);
                return Err(format!("preparing {id}@{version}: {error}"));
            }
        };
        let conflicts = conflicts(&plugins, &destination, &package);
        let record = InstallationRecord {
            schema_version: RECORD_SCHEMA,
            id: id.clone(),
            package_version: version.clone(),
            catalog_commit: snapshot.commit.clone(),
            source_commit,
            artifact_digest: artifact.blake3.to_ascii_lowercase(),
            files,
        };
        let json = serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?;
        crate::atomicfile::write(&stage.join(RECORD), &json)?;
        prepared.push(PreparedPackage {
            id,
            version,
            previous_version: previous.map(|record| record.package_version),
            conflicts,
            stage: Some(stage),
            destination,
        });
    }
    Ok(prepared)
}

/// The installed packages a freshly staged one would collide with. Package
/// directories load in sorted order and the first plugin name or palette
/// command wins, so either side of a collision can end up unreachable.
fn conflicts(plugins: &Path, destination: &Path, package: &crate::config::Plugin) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(plugins) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path != destination && path.is_dir())
        .collect();
    paths.sort();
    paths.retain(|path| {
        crate::plugins::read_package(path).is_ok_and(|other| {
            other.name == package.name
                || (package.palette.is_some() && other.palette == package.palette)
        })
    });
    paths
}

fn inspect_destination(path: &Path, id: &str) -> Result<Option<InstallationRecord>, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("inspecting {}: {e}", path.display())),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "refusing symlinked plugin destination {}",
                path.display()
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!(
                "plugin destination {} is not a directory",
                path.display()
            ));
        }
        Ok(_) => {}
    }
    let record = read_record(path).map_err(|error| {
        format!(
            "refusing unmanaged plugin directory {}: {error}; move or remove it manually",
            path.display()
        )
    })?;
    if record.id != id {
        return Err(format!(
            "installation record in {} belongs to {}, not {id}",
            path.display(),
            record.id
        ));
    }
    verify_record(path, &record)?;
    Ok(Some(record))
}

pub fn installed() -> Result<Vec<InstalledPackage>, String> {
    installed_in(&plugin_catalog::config_dir()?.join("plugins"))
}

/// Installed versions by ID, without hashing a single file. Search and describe
/// report what is installed, never whether it was edited, and verifying every
/// file of every package to answer that costs more than the rest of the command.
pub fn installed_versions() -> Result<BTreeMap<String, String>, String> {
    Ok(scan(&plugin_catalog::config_dir()?.join("plugins"), false)?
        .into_iter()
        .filter(|package| package.managed)
        .filter_map(|package| Some((package.id, package.version?)))
        .collect())
}

fn installed_in(plugins: &Path) -> Result<Vec<InstalledPackage>, String> {
    scan(plugins, true)
}

/// `verify` hashes each package's files to decide whether it still matches its
/// installation record. Callers that only report versions leave it off.
fn scan(plugins: &Path, verify: bool) -> Result<Vec<InstalledPackage>, String> {
    let entries = match std::fs::read_dir(plugins) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("reading {}: {e}", plugins.display())),
    };
    let mut packages = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", plugins.display()))?;
        let metadata = entry
            .file_type()
            .map_err(|e| format!("inspecting {}: {e}", entry.path().display()))?;
        if !metadata.is_dir() || metadata.is_symlink() {
            continue;
        }
        let path = entry.path();
        let id = entry.file_name().to_string_lossy().into_owned();
        match read_record(&path) {
            Ok(record) => packages.push(InstalledPackage {
                modified: record.id != id || (verify && verify_record(&path, &record).is_err()),
                id,
                version: Some(record.package_version.clone()),
                path,
                managed: true,
            }),
            Err(_) => {
                let has_record = path.join(RECORD).exists();
                packages.push(InstalledPackage {
                    id,
                    version: None,
                    path,
                    managed: has_record,
                    modified: has_record,
                });
            }
        }
    }
    packages.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(packages)
}

pub fn managed_ids() -> Result<Vec<String>, String> {
    managed_ids_in(&plugin_catalog::config_dir()?.join("plugins"))
}

fn managed_ids_in(plugins: &Path) -> Result<Vec<String>, String> {
    Ok(installed_in(plugins)?
        .into_iter()
        .filter(|package| package.managed)
        .map(|package| package.id)
        .collect())
}

pub fn remove(ids: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
    remove_below(&plugin_catalog::config_dir()?, ids)
}

fn remove_below(config: &Path, ids: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
    ensure_directory_path(config)?;
    let plugins = config.join("plugins");
    ensure_directory_path(&plugins)?;
    let mut checked = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        plugin_catalog::parse_request(id).and_then(|(_, version)| {
            if version.is_some() {
                Err("remove accepts plugin IDs without versions".into())
            } else {
                Ok(())
            }
        })?;
        if !seen.insert(id) {
            continue;
        }
        let path = plugins.join(id);
        let record = inspect_destination(&path, id)?
            .ok_or_else(|| format!("plugin {id} is not installed at {}", path.display()))?;
        checked.push((id.clone(), path, record));
    }
    let mut removed = Vec::new();
    for (id, path, _) in checked {
        let staged = unique_path(config, &format!("{REMOVED_PREFIX}{id}"));
        std::fs::rename(&path, &staged).map_err(|e| format!("removing {}: {e}", path.display()))?;
        std::fs::remove_dir_all(&staged).map_err(|e| {
            format!(
                "plugin {id} was deactivated, but cleanup of {} failed: {e}",
                staged.display()
            )
        })?;
        removed.push((id, path));
    }
    Ok(removed)
}

fn read_record(dir: &Path) -> Result<InstallationRecord, String> {
    let path = dir.join(RECORD);
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() > 1024 * 1024 {
        return Err(format!("{} exceeds 1 MiB", path.display()));
    }
    let record: InstallationRecord =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid {}: {e}", path.display()))?;
    if record.schema_version != RECORD_SCHEMA {
        return Err(format!(
            "unsupported installation record in {}",
            path.display()
        ));
    }
    Ok(record)
}

fn verify_record(dir: &Path, record: &InstallationRecord) -> Result<(), String> {
    let current = hash_files(dir)?;
    if current == record.files {
        return Ok(());
    }
    // The record stores a digest per file, so say which files moved rather than
    // leaving the user to diff an installation directory by hand.
    let mut changed: Vec<String> = Vec::new();
    for (path, digest) in &current {
        match record.files.get(path) {
            None => changed.push(format!("added {path}")),
            Some(expected) if expected != digest => changed.push(format!("changed {path}")),
            Some(_) => {}
        }
    }
    changed.extend(
        record
            .files
            .keys()
            .filter(|path| !current.contains_key(*path))
            .map(|path| format!("removed {path}")),
    );
    let listed = changed.len().min(MODIFIED_LISTED);
    let rest = changed.len() - listed;
    let mut detail = changed[..listed].join(", ");
    if rest > 0 {
        detail.push_str(&format!(", and {rest} more"));
    }
    Err(format!(
        "plugin {} at {} has local modifications ({detail}); restore it or manage the directory manually",
        record.id,
        dir.display()
    ))
}

/// Hash a file without reading it into memory: BLAKE3 maps it and hashes the
/// pages across every core, which is the whole reason verification is cheap
/// enough to run on every list, update, and removal.
fn digest_file(path: &Path) -> Result<String, String> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_mmap_rayon(path)
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    Ok(plugin_catalog::hex(hasher.finalize().as_bytes()))
}

fn hash_files(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut files = BTreeMap::new();
    hash_directory(root, root, &mut files)?;
    Ok(files)
}

fn hash_directory(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("reading {}: {e}", dir.display()))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("inspecting {}: {e}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("plugin contains symlink {}", path.display()));
        }
        if metadata.is_dir() {
            hash_directory(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("walk remains below root")
                .to_str()
                .ok_or_else(|| format!("plugin path is not UTF-8: {}", path.display()))?
                .replace(std::path::MAIN_SEPARATOR, "/");
            if relative != RECORD && relative != STAGE_MARKER {
                files.insert(relative, digest_file(&path)?);
            }
        } else {
            return Err(format!("plugin contains special file {}", path.display()));
        }
    }
    Ok(())
}

/// Bounds a decompressed stream, so a small archive cannot expand without
/// limit however its bytes are declared.
struct Bounded<R> {
    inner: R,
    remaining: u64,
    limit: u64,
}

impl<R: std::io::Read> std::io::Read for Bounded<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.remaining = self
            .remaining
            .checked_sub(read as u64)
            .ok_or_else(|| std::io::Error::other(beyond_limit(self.limit)))?;
        Ok(read)
    }
}

fn beyond_limit(limit: u64) -> String {
    format!("archive expands beyond {} MiB", limit / (1024 * 1024))
}

/// Hashes what it writes, so extraction and the installation record cost one
/// pass over the package instead of two.
struct Hashing<W> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: std::io::Write> std::io::Write for Hashing<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn extract(archive: &Path, destination: &Path) -> Result<BTreeMap<String, String>, String> {
    extract_bounded(archive, destination, EXPANDED_MAX_BYTES)
}

fn extract_bounded(
    archive: &Path,
    destination: &Path,
    limit: u64,
) -> Result<BTreeMap<String, String>, String> {
    let file = File::open(archive).map_err(|e| format!("opening {}: {e}", archive.display()))?;
    let zstd = zstd::stream::read::Decoder::new(file)
        .map_err(|e| format!("reading {}: {e}", archive.display()))?;
    // The reader budget covers payload, TAR headers, and padding together; the
    // per-entry tally below only turns an oversized archive into a clearer
    // error before its bytes are read.
    let mut archive = tar::Archive::new(Bounded {
        inner: zstd,
        remaining: limit,
        limit,
    });
    let entries = archive
        .entries()
        .map_err(|e| format!("reading archive: {e}"))?;
    let mut paths = HashSet::new();
    let mut files = BTreeMap::new();
    let mut count = 0usize;
    let mut expanded = 0u64;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("reading archive entry: {e}"))?;
        count += 1;
        if count > FILE_MAX {
            return Err(format!("archive contains more than {FILE_MAX} entries"));
        }
        let path = entry
            .path()
            .map_err(|e| format!("invalid archive path: {e}"))?
            .into_owned();
        validate_relative_path(&path)?;
        let normalized = path
            .to_str()
            .ok_or_else(|| "archive path is not UTF-8".to_string())?
            .replace('\\', "/");
        if !paths.insert(normalized) {
            return Err(format!("duplicate archive path {}", path.display()));
        }
        // Every entry counts, whatever its type: a directory that declares a
        // payload expands the stream exactly as a regular file does.
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| "archive expanded size overflow".to_string())?;
        if expanded > limit {
            return Err(beyond_limit(limit));
        }
        let target = destination.join(&path);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("creating {}: {e}", target.display()))?;
            continue;
        }
        if !kind.is_file() {
            return Err(format!(
                "archive contains link or special file {}",
                path.display()
            ));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let output = File::options()
            .create_new(true)
            .write(true)
            .open(&target)
            .map_err(|e| format!("creating {}: {e}", target.display()))?;
        let mut output = Hashing {
            inner: output,
            hasher: blake3::Hasher::new(),
        };
        std::io::copy(&mut entry, &mut output)
            .map_err(|e| format!("extracting {}: {e}", target.display()))?;
        let relative = path
            .to_str()
            .expect("archive path checked as UTF-8")
            .replace('\\', "/");
        files.insert(
            relative,
            plugin_catalog::hex(output.hasher.finalize().as_bytes()),
        );
        output
            .inner
            .sync_all()
            .map_err(|e| format!("flushing {}: {e}", target.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let source_mode = entry.header().mode().unwrap_or(0);
            let mode = if source_mode & 0o111 == 0 {
                0o644
            } else {
                0o755
            };
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                .map_err(|e| format!("setting permissions on {}: {e}", target.display()))?;
        }
    }
    if !destination.join("plugin.toml").is_file() {
        return Err("archive has no plugin.toml at its root".into());
    }
    Ok(files)
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe archive path {}", path.display()));
    }
    Ok(())
}

fn ensure_directory_path(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("refusing symlinked directory {}", path.display()))
        }
        Ok(metadata) if !metadata.is_dir() => Err(format!("{} is not a directory", path.display())),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspecting {}: {e}", path.display())),
    }
}

fn unique_path(parent: &Path, prefix: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    parent.join(format!("{prefix}-{}-{nanos:x}", std::process::id()))
}

fn recover(config: &Path) -> Result<(), String> {
    let entries = match std::fs::read_dir(config) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("reading {}: {e}", config.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", config.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name.starts_with(".plugin-stage-") {
            if entry.file_type().is_ok_and(|kind| kind.is_dir())
                && path.join(STAGE_MARKER).is_file()
            {
                std::fs::remove_dir_all(&path)
                    .map_err(|e| format!("recovering {}: {e}", path.display()))?;
            }
        } else if name.starts_with(REMOVED_PREFIX) {
            // The rename that created this directory committed the removal, so
            // there is nothing left to identify and nothing to keep.
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                std::fs::remove_dir_all(&path)
                    .map_err(|e| format!("recovering {}: {e}", path.display()))?;
            }
        } else if name.starts_with(".plugin-backup-") {
            let record = read_record(&path).map_err(|e| {
                format!(
                    "cannot identify interrupted backup {}: {e}; inspect it manually",
                    path.display()
                )
            })?;
            crate::plugin_catalog::parse_request(&record.id)?;
            let destination = config.join("plugins").join(&record.id);
            if destination.exists() {
                read_record(&destination).map_err(|e| {
                    format!(
                        "refusing to discard backup {} because destination {} is unmanaged: {e}",
                        path.display(),
                        destination.display()
                    )
                })?;
                std::fs::remove_dir_all(&path)
                    .map_err(|e| format!("recovering {}: {e}", path.display()))?;
            } else {
                std::fs::create_dir_all(config.join("plugins"))
                    .map_err(|e| format!("recovering plugins directory: {e}"))?;
                std::fs::rename(&path, &destination).map_err(|e| {
                    format!(
                        "restoring {} to {}: {e}",
                        path.display(),
                        destination.display()
                    )
                })?;
            }
        }
    }
    let plugins = config.join("plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins) {
        for entry in entries {
            let entry = entry.map_err(|e| format!("reading {}: {e}", plugins.display()))?;
            let path = entry.path();
            let marker = path.join(STAGE_MARKER);
            if marker.is_file() && read_record(&path).is_ok() {
                std::fs::remove_file(&marker)
                    .map_err(|e| format!("recovering {}: {e}", marker.display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(path: &Path, entries: &[(&str, &[u8], tar::EntryType)]) {
        let file = File::create(path).unwrap();
        let zstd = zstd::stream::write::Encoder::new(file, 19)
            .unwrap()
            .auto_finish();
        let mut builder = tar::Builder::new(zstd);
        for (name, bytes, kind) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(if *name == "adapter" { 0o755 } else { 0o644 });
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap();
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sofka-plugin-install-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const MANIFEST: &str = concat!(
        "schema_version = 1\n",
        "[plugin]\n",
        "name = \"Resource summary\"\n",
        "palette = \"resource-summary\"\n",
        "command = \"/bin/echo\"\n",
        "output = \"report\"\n",
    );

    /// A catalog whose single artifact is a real archive in `cache`, so the
    /// whole offline install path runs without a network or a cluster.
    fn published(cache: &Path, id: &str, version: &str, manifest: &str) -> CatalogSnapshot {
        let bytes = {
            let path = cache.join(format!("{id}-{version}.tar.zst"));
            archive(
                &path,
                &[("plugin.toml", manifest.as_bytes(), tar::EntryType::Regular)],
            );
            std::fs::read(&path).unwrap()
        };
        let digest = plugin_catalog::digest(&bytes);
        let stored = cache.join("artifacts").join(format!("{digest}.tar.zst"));
        std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
        std::fs::write(&stored, &bytes).unwrap();
        let index = serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-09-11T00:00:00Z",
            "plugins": [{
                "id": id,
                "display_name": "Resource summary",
                "description": "Summarize a resource.",
                "tags": [],
                "publisher": "sofka",
                "repository": "https://github.com/nklmilojevic/sofka-plugins",
                "versions": [{
                    "version": version,
                    "sofka": ">=0.0.1",
                    "source_commit": "1".repeat(40),
                    "license": "MIT",
                    "readme": "https://example.invalid/readme",
                    "requirements": [],
                    "command": "/bin/echo",
                    "target": "selection",
                    "output": "report",
                    "mutating": false,
                    "confirm": false,
                    "dangerous": false,
                    "network_load": false,
                    "status": "active",
                    "artifacts": [{
                        "platform": "any",
                        "url": format!(
                            "{}{id}-v{version}/{id}.tar.zst",
                            plugin_catalog::RELEASE_ROOT
                        ),
                        "blake3": digest,
                        "size": bytes.len(),
                    }],
                }],
            }],
        });
        CatalogSnapshot {
            catalog: plugin_catalog::Catalog::parse(&serde_json::to_vec(&index).unwrap()).unwrap(),
            commit: "0".repeat(40),
            fetched_at: 0,
            offline: true,
        }
    }

    fn record_for(id: &str, version: &str) -> InstallationRecord {
        InstallationRecord {
            schema_version: 1,
            id: id.into(),
            package_version: version.into(),
            catalog_commit: "0".repeat(40),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: BTreeMap::new(),
        }
    }

    fn write_record(dir: &Path, record: &InstallationRecord) {
        std::fs::create_dir_all(dir).unwrap();
        let mut record = record.clone();
        record.files = hash_files(dir).unwrap();
        std::fs::write(dir.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn an_offline_install_stages_records_and_activates_one_package() {
        let config = scratch("install-offline");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let snapshot = published(&cache, "resource-summary", "1.0.0", MANIFEST);

        let prepared = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].previous_version, None);
        assert!(prepared[0].conflicts.is_empty());
        // Nothing is visible to the loader until activation.
        let destination = config.join("plugins").join("resource-summary");
        assert!(!destination.exists());

        assert_eq!(
            prepared.into_iter().next().unwrap().activate().unwrap(),
            Activation::Installed
        );
        assert!(destination.join("plugin.toml").is_file());
        assert!(!destination.join(STAGE_MARKER).exists());
        let record = read_record(&destination).unwrap();
        assert_eq!(record.package_version, "1.0.0");
        assert_eq!(record.catalog_commit, "0".repeat(40));
        assert_eq!(record.source_commit, "1".repeat(40));
        assert!(record.files.contains_key("plugin.toml"));
        verify_record(&destination, &record).unwrap();

        // Reinstalling the same intact version changes nothing.
        let again = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary@1.0.0".to_string()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(again[0].previous_version.as_deref(), Some("1.0.0"));
        let before = std::fs::read(destination.join("plugin.toml")).unwrap();
        assert_eq!(
            again.into_iter().next().unwrap().activate().unwrap(),
            Activation::Unchanged
        );
        assert_eq!(
            std::fs::read(destination.join("plugin.toml")).unwrap(),
            before
        );

        let packages = installed_in(&config.join("plugins")).unwrap();
        assert_eq!(packages.len(), 1);
        assert!(packages[0].managed && !packages[0].modified);
        assert_eq!(packages[0].version.as_deref(), Some("1.0.0"));
        assert_eq!(
            managed_ids_in(&config.join("plugins")).unwrap(),
            ["resource-summary"]
        );

        let removed = remove_below(&config, &["resource-summary".to_string()]).unwrap();
        assert_eq!(
            removed,
            vec![("resource-summary".to_string(), destination.clone())]
        );
        assert!(!destination.exists());
        assert!(installed_in(&config.join("plugins")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn preparation_refuses_a_batch_before_touching_any_installed_package() {
        let config = scratch("install-batch");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let mut snapshot = published(&cache, "resource-summary", "1.0.0", MANIFEST);
        let mut newer = snapshot.catalog.plugins[0].versions[0].clone();
        newer.version = "2.0.0".into();
        snapshot.catalog.plugins[0].versions.push(newer);
        snapshot.catalog.validate().unwrap();
        let plugins = config.join("plugins");

        // Two versions of one ID in a single request is a conflict, not a pick.
        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &[
                "resource-summary@1.0.0".to_string(),
                "resource-summary@2.0.0".to_string(),
            ],
            true,
        )
        .await
        .unwrap_err();
        assert!(error.contains("conflicting versions"), "{error}");

        // An unmanaged directory is never taken over.
        let destination = plugins.join("resource-summary");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("plugin.toml"), "local").unwrap();
        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("refusing unmanaged plugin directory"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("plugin.toml")).unwrap(),
            "local"
        );

        // A managed package the user edited is refused just as firmly.
        write_record(&destination, &record_for("resource-summary", "1.0.0"));
        std::fs::write(destination.join("extra"), "mine").unwrap();
        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(error.contains("local modifications"), "{error}");

        // No stage survives a refused batch.
        let leftovers: Vec<_> = std::fs::read_dir(&config)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".plugin-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn a_repeated_id_resolves_once_and_a_changed_digest_is_refused() {
        let config = scratch("install-digest");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let snapshot = published(&cache, "resource-summary", "1.0.0", MANIFEST);

        let prepared = prepare_below(
            &config,
            &cache,
            &snapshot,
            &[
                "resource-summary".to_string(),
                "resource-summary@1.0.0".to_string(),
            ],
            true,
        )
        .await
        .unwrap();
        assert_eq!(prepared.len(), 1);
        for package in prepared {
            package.activate().unwrap();
        }

        // The catalog may not repoint an immutable version at other bytes.
        let mut repointed = published(&cache, "resource-summary", "1.0.0", MANIFEST);
        repointed.catalog.plugins[0].versions[0].artifacts[0].blake3 = "9".repeat(64);
        let error = prepare_below(
            &config,
            &cache,
            &repointed,
            &["resource-summary@1.0.0".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("differs from the installed immutable version"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn an_install_names_the_package_it_would_be_hidden_by() {
        let config = scratch("install-conflict");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let snapshot = published(&cache, "resource-summary", "1.0.0", MANIFEST);
        let manual = config.join("plugins").join("aaa-manual");
        std::fs::create_dir_all(&manual).unwrap();
        std::fs::write(manual.join("plugin.toml"), MANIFEST).unwrap();

        let prepared = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(prepared[0].conflicts, vec![manual]);
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn an_unbuildable_package_never_reaches_the_loader() {
        let config = scratch("install-invalid");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let snapshot = published(
            &cache,
            "resource-summary",
            "1.0.0",
            "schema_version = 1\n[plugin]\nname = \"X\"\ncommand = \"/bin/echo\"\nshell = true\n",
        );
        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("preparing resource-summary@1.0.0"),
            "{error}"
        );
        assert!(!config.join("plugins").join("resource-summary").exists());
        let leftovers = std::fs::read_dir(&config).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".plugin-stage-")
        });
        assert!(!leftovers);
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn a_failed_activation_keeps_the_package_it_was_replacing() {
        let config = scratch("activation-failure");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("plugin.toml"), "previous").unwrap();
        write_record(&destination, &record_for("sample", "1.0.0"));

        // A stage that is gone models the filesystem failing mid-batch.
        let error = PreparedPackage::staged(
            "sample",
            "2.0.0",
            Some("1.0.0"),
            config.join(".plugin-stage-sample-absent"),
            destination.clone(),
        )
        .activate()
        .unwrap_err();

        assert!(error.contains("activating sample"), "{error}");
        assert_eq!(
            std::fs::read_to_string(destination.join("plugin.toml")).unwrap(),
            "previous"
        );
        let record = read_record(&destination).unwrap();
        assert_eq!(record.package_version, "1.0.0");
        verify_record(&destination, &record).unwrap();
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn extraction_digests_match_verification_for_nested_and_empty_files() {
        let dir = scratch("digest-agreement");
        let source = dir.join("package.tar.zst");
        archive(
            &source,
            &[
                (
                    "plugin.toml",
                    b"schema_version = 1\n",
                    tar::EntryType::Regular,
                ),
                // An empty file cannot be memory-mapped on every platform, and a
                // nested path is spelled differently by the two hashers.
                ("empty", b"", tar::EntryType::Regular),
                ("bin/adapter", b"binary", tar::EntryType::Regular),
            ],
        );
        let destination = dir.join("out");
        std::fs::create_dir(&destination).unwrap();

        let extracted = extract(&source, &destination).unwrap();
        let walked = hash_files(&destination).unwrap();

        assert_eq!(
            extracted, walked,
            "extraction and verification disagree, so every install would look modified"
        );
        assert!(walked.contains_key("bin/adapter"));
        assert_eq!(
            walked["empty"],
            digest_file(&destination.join("empty")).unwrap()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn modification_reports_name_the_files_that_moved() {
        let dir = scratch("modified-detail");
        std::fs::write(dir.join("plugin.toml"), "one").unwrap();
        std::fs::write(dir.join("adapter"), "binary").unwrap();
        let record = InstallationRecord {
            files: hash_files(&dir).unwrap(),
            ..record_for("sample", "1.0.0")
        };

        std::fs::write(dir.join("adapter"), "edited").unwrap();
        std::fs::write(dir.join("notes"), "mine").unwrap();
        std::fs::remove_file(dir.join("plugin.toml")).unwrap();
        let error = verify_record(&dir, &record).unwrap_err();
        assert!(error.contains("changed adapter"), "{error}");
        assert!(error.contains("added notes"), "{error}");
        assert!(error.contains("removed plugin.toml"), "{error}");

        // A package that lost everything reports a bounded list, not a wall.
        let many: BTreeMap<String, String> = (0..50)
            .map(|i| (format!("file-{i}"), "0".repeat(64)))
            .collect();
        let error = verify_record(
            &dir,
            &InstallationRecord {
                files: many,
                ..record_for("sample", "1.0.0")
            },
        )
        .unwrap_err();
        assert!(error.contains("and 47 more"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn destinations_are_inspected_before_anything_is_written() {
        let dir = scratch("inspect");
        let missing = dir.join("absent");
        assert!(inspect_destination(&missing, "sample").unwrap().is_none());

        let file = dir.join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert!(
            inspect_destination(&file, "sample")
                .unwrap_err()
                .contains("is not a directory")
        );

        let unmanaged = dir.join("unmanaged");
        std::fs::create_dir(&unmanaged).unwrap();
        assert!(
            inspect_destination(&unmanaged, "sample")
                .unwrap_err()
                .contains("refusing unmanaged plugin directory")
        );

        let foreign = dir.join("foreign");
        write_record(&foreign, &record_for("other", "1.0.0"));
        assert!(
            inspect_destination(&foreign, "sample")
                .unwrap_err()
                .contains("belongs to other")
        );

        let managed = dir.join("sample");
        std::fs::create_dir(&managed).unwrap();
        std::fs::write(managed.join("plugin.toml"), "body").unwrap();
        write_record(&managed, &record_for("sample", "1.0.0"));
        assert_eq!(
            inspect_destination(&managed, "sample")
                .unwrap()
                .unwrap()
                .package_version,
            "1.0.0"
        );
        std::fs::write(managed.join("plugin.toml"), "edited").unwrap();
        assert!(
            inspect_destination(&managed, "sample")
                .unwrap_err()
                .contains("local modifications")
        );

        #[cfg(unix)]
        {
            let linked = dir.join("linked");
            std::os::unix::fs::symlink(&managed, &linked).unwrap();
            assert!(
                inspect_destination(&linked, "sample")
                    .unwrap_err()
                    .contains("refusing symlinked plugin destination")
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listing_separates_managed_manual_and_edited_packages() {
        let plugins = scratch("listing").join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        assert!(installed_in(&plugins.join("absent")).unwrap().is_empty());

        let managed = plugins.join("managed");
        std::fs::create_dir(&managed).unwrap();
        std::fs::write(managed.join("plugin.toml"), "body").unwrap();
        write_record(&managed, &record_for("managed", "1.0.0"));

        let edited = plugins.join("edited");
        std::fs::create_dir(&edited).unwrap();
        std::fs::write(edited.join("plugin.toml"), "body").unwrap();
        write_record(&edited, &record_for("edited", "2.0.0"));
        std::fs::write(edited.join("extra"), "mine").unwrap();

        let manual = plugins.join("manual");
        std::fs::create_dir(&manual).unwrap();
        std::fs::write(manual.join("plugin.toml"), "body").unwrap();

        let broken = plugins.join("broken");
        std::fs::create_dir(&broken).unwrap();
        std::fs::write(broken.join(RECORD), "not json").unwrap();

        std::fs::write(plugins.join("loose-file"), "ignored").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&managed, plugins.join("linked")).unwrap();

        let packages = installed_in(&plugins).unwrap();
        let rows: Vec<_> = packages
            .iter()
            .map(|p| (p.id.as_str(), p.version.as_deref(), p.managed, p.modified))
            .collect();
        assert_eq!(
            rows,
            [
                ("broken", None, true, true),
                ("edited", Some("2.0.0"), true, true),
                ("managed", Some("1.0.0"), true, false),
                ("manual", None, false, false),
            ]
        );
        assert_eq!(
            managed_ids_in(&plugins).unwrap(),
            ["broken", "edited", "managed"]
        );
        let _ = std::fs::remove_dir_all(plugins.parent().unwrap());
    }

    #[test]
    fn removal_rejects_versions_unknown_ids_and_edited_packages() {
        let config = scratch("removal-guards");
        let plugins = config.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();

        assert!(
            remove_below(&config, &["sample@1.0.0".to_string()])
                .unwrap_err()
                .contains("without versions")
        );
        assert!(
            remove_below(&config, &["sample".to_string()])
                .unwrap_err()
                .contains("is not installed")
        );

        let managed = plugins.join("sample");
        std::fs::create_dir(&managed).unwrap();
        std::fs::write(managed.join("plugin.toml"), "body").unwrap();
        write_record(&managed, &record_for("sample", "1.0.0"));
        std::fs::write(managed.join("extra"), "mine").unwrap();
        assert!(
            remove_below(&config, &["sample".to_string()])
                .unwrap_err()
                .contains("local modifications")
        );
        assert!(managed.is_dir());

        // One bad ID in a batch removes nothing at all.
        std::fs::remove_file(managed.join("extra")).unwrap();
        let other = plugins.join("other");
        std::fs::create_dir(&other).unwrap();
        write_record(&other, &record_for("other", "1.0.0"));
        assert!(
            remove_below(&config, &["sample".to_string(), "absent".to_string()])
                .unwrap_err()
                .contains("is not installed")
        );
        assert!(managed.is_dir() && other.is_dir());

        // A repeated ID is removed once.
        let removed = remove_below(&config, &["sample".to_string(), "sample".to_string()]).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(!managed.exists() && other.is_dir());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn recovery_finishes_an_interruption_at_every_filesystem_step() {
        let config = scratch("recovery-steps");
        let plugins = config.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let destination = plugins.join("sample");
        let record = record_for("sample", "1.0.0");

        // 1. Interrupted while staging: the marker identifies a partial stage.
        let stage = unique_path(&config, ".plugin-stage-sample");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join(STAGE_MARKER), "stage").unwrap();
        recover(&config).unwrap();
        assert!(!stage.exists());

        // 2. Interrupted between the two renames: the backup is restored.
        let backup = unique_path(&config, ".plugin-backup-sample");
        write_record(&backup, &record);
        recover(&config).unwrap();
        assert!(!backup.exists());
        assert_eq!(read_record(&destination).unwrap().package_version, "1.0.0");

        // 3. Interrupted after the second rename: the backup is now redundant.
        let backup = unique_path(&config, ".plugin-backup-sample");
        write_record(&backup, &record);
        recover(&config).unwrap();
        assert!(!backup.exists());
        assert!(destination.is_dir());

        // 4. Interrupted before the staging marker was cleared.
        std::fs::write(destination.join(STAGE_MARKER), "stage").unwrap();
        recover(&config).unwrap();
        assert!(!destination.join(STAGE_MARKER).exists());

        // 5. Interrupted while deleting a committed removal, in any state.
        for leftover in ["with-record", "without-record"] {
            let removed = unique_path(&config, &format!("{REMOVED_PREFIX}{leftover}"));
            std::fs::create_dir(&removed).unwrap();
            if leftover == "with-record" {
                std::fs::write(removed.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
            }
            recover(&config).unwrap();
            assert!(!removed.exists(), "{leftover}");
        }

        // 6. A backup that cannot be told apart from user data stops the world
        //    rather than guessing.
        let opaque = unique_path(&config, ".plugin-backup-sample");
        std::fs::create_dir(&opaque).unwrap();
        std::fs::write(opaque.join("data"), "unknown").unwrap();
        let error = recover(&config).unwrap_err();
        assert!(
            error.contains("cannot identify interrupted backup"),
            "{error}"
        );
        std::fs::remove_dir_all(&opaque).unwrap();

        // 7. A backup whose destination was replaced by something unmanaged.
        let backup = unique_path(&config, ".plugin-backup-sample");
        write_record(&backup, &record);
        std::fs::remove_file(destination.join(RECORD)).unwrap();
        let error = recover(&config).unwrap_err();
        assert!(error.contains("is unmanaged"), "{error}");
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn hashing_refuses_anything_that_is_not_a_plain_file_or_directory() {
        let dir = scratch("hashing");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("plugin.toml"), "a").unwrap();
        std::fs::write(dir.join("nested/adapter"), "b").unwrap();
        std::fs::write(dir.join(RECORD), "ignored").unwrap();
        std::fs::write(dir.join(STAGE_MARKER), "ignored").unwrap();
        let files = hash_files(&dir).unwrap();
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["nested/adapter", "plugin.toml"]
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("plugin.toml"), dir.join("linked")).unwrap();
            assert!(hash_files(&dir).unwrap_err().contains("symlink"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extraction_requires_a_manifest_at_the_root_and_caps_the_entry_count() {
        let dir = scratch("extract-shape");
        let nested = dir.join("nested.tar.zst");
        archive(
            &nested,
            &[(
                "inner/plugin.toml",
                b"schema_version = 1\n",
                tar::EntryType::Regular,
            )],
        );
        let out = dir.join("nested-out");
        std::fs::create_dir(&out).unwrap();
        assert!(
            extract(&nested, &out)
                .unwrap_err()
                .contains("no plugin.toml at its root")
        );

        let many = dir.join("many.tar.zst");
        let names: Vec<String> = (0..=FILE_MAX).map(|i| format!("file-{i}")).collect();
        let entries: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), b"x".as_slice(), tar::EntryType::Regular))
            .collect();
        archive(&many, &entries);
        let out = dir.join("many-out");
        std::fs::create_dir(&out).unwrap();
        assert!(
            extract(&many, &out)
                .unwrap_err()
                .contains("more than 2000 entries")
        );

        // The builder refuses to write a traversing name, so the header is
        // filled in the way an attacker would have to.
        let traversal = dir.join("traversal.tar.zst");
        let file = File::create(&traversal).unwrap();
        let zstd = zstd::stream::write::Encoder::new(file, 19)
            .unwrap()
            .auto_finish();
        let mut builder = tar::Builder::new(zstd);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(1);
        let name = b"../escape.toml";
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        builder.append(&header, &b"x"[..]).unwrap();
        builder.into_inner().unwrap();
        let out = dir.join("traversal-out");
        std::fs::create_dir(&out).unwrap();
        assert!(extract(&traversal, &out).unwrap_err().contains("unsafe"));
        assert!(!dir.join("escape.toml").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn records_detect_changed_and_extra_files() {
        let dir = scratch("modified");
        std::fs::write(dir.join("plugin.toml"), "one").unwrap();
        let record = InstallationRecord {
            schema_version: 1,
            id: "sample".into(),
            package_version: "1.0.0".into(),
            catalog_commit: "0".repeat(40),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: hash_files(&dir).unwrap(),
        };
        assert!(verify_record(&dir, &record).is_ok());
        std::fs::write(dir.join("extra"), "local").unwrap();
        assert!(verify_record(&dir, &record).is_err());
        std::fs::remove_file(dir.join("extra")).unwrap();
        std::fs::write(dir.join("plugin.toml"), "two").unwrap();
        assert!(verify_record(&dir, &record).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn archive_paths_must_be_plain_relative_components() {
        for bad in ["", ".", "../escape", "/absolute", "dir/../escape"] {
            assert!(
                validate_relative_path(Path::new(bad)).is_err(),
                "accepted {bad}"
            );
        }
        assert!(validate_relative_path(Path::new("bin/adapter")).is_ok());
    }

    #[test]
    fn extraction_preserves_only_required_executable_bits() {
        let dir = scratch("extract");
        let source = dir.join("package.tar.zst");
        archive(
            &source,
            &[
                (
                    "plugin.toml",
                    b"schema_version = 1\n",
                    tar::EntryType::Regular,
                ),
                ("adapter", b"binary", tar::EntryType::Regular),
            ],
        );
        let destination = dir.join("out");
        std::fs::create_dir(&destination).unwrap();
        extract(&source, &destination).unwrap();
        assert_eq!(
            std::fs::read(destination.join("adapter")).unwrap(),
            b"binary"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(destination.join("adapter"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert_eq!(
                std::fs::metadata(destination.join("plugin.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o644
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extraction_rejects_links_and_duplicate_paths() {
        let dir = scratch("archive-safety");
        let duplicate = dir.join("duplicate.tar.zst");
        archive(
            &duplicate,
            &[
                ("plugin.toml", b"first", tar::EntryType::Regular),
                ("plugin.toml", b"second", tar::EntryType::Regular),
            ],
        );
        let output = dir.join("duplicate");
        std::fs::create_dir(&output).unwrap();
        assert!(
            extract(&duplicate, &output)
                .unwrap_err()
                .contains("duplicate")
        );

        let linked = dir.join("link.tar.zst");
        archive(
            &linked,
            &[
                ("plugin.toml", b"manifest", tar::EntryType::Regular),
                ("adapter", b"target", tar::EntryType::Symlink),
            ],
        );
        let output = dir.join("linked");
        std::fs::create_dir(&output).unwrap();
        assert!(extract(&linked, &output).unwrap_err().contains("link"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extraction_bounds_the_whole_decompressed_stream() {
        let dir = scratch("expansion");
        let source = dir.join("bomb.tar.zst");
        let payload = vec![0u8; 2 * 1024 * 1024];
        archive(
            &source,
            &[
                (
                    "plugin.toml",
                    b"schema_version = 1\n",
                    tar::EntryType::Regular,
                ),
                // A directory entry is skipped rather than written, so only a
                // bound on the stream itself can catch its declared payload.
                ("payload", payload.as_slice(), tar::EntryType::Directory),
            ],
        );
        let destination = dir.join("out");
        std::fs::create_dir(&destination).unwrap();
        let error = extract_bounded(&source, &destination, 1024 * 1024).unwrap_err();
        assert!(error.contains("expands beyond 1 MiB"), "{error}");

        // TAR metadata is consumed before an entry is ever yielded, so no
        // per-entry accounting can see it.
        let metadata = dir.join("metadata.tar.zst");
        let file = File::create(&metadata).unwrap();
        let zstd = zstd::stream::write::Encoder::new(file, 19)
            .unwrap()
            .auto_finish();
        let mut builder = tar::Builder::new(zstd);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::GNULongName);
        header.set_mode(0o644);
        header.set_size(payload.len() as u64);
        header.set_cksum();
        builder
            .append(&header, std::io::Cursor::new(payload))
            .unwrap();
        builder.into_inner().unwrap();
        let destination = dir.join("metadata");
        std::fs::create_dir(&destination).unwrap();
        let error = extract_bounded(&metadata, &destination, 1024 * 1024).unwrap_err();
        assert!(error.contains("expands beyond 1 MiB"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recovery_discards_a_backup_left_without_its_record() {
        let config = scratch("interrupted-cleanup");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        let record = InstallationRecord {
            schema_version: 1,
            id: "sample".into(),
            package_version: "2.0.0".into(),
            catalog_commit: "0".repeat(40),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: BTreeMap::new(),
        };
        std::fs::write(
            destination.join(RECORD),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        // Cleanup deletes the record before the rest of the directory, which
        // used to leave a backup nothing could identify.
        let orphan = config.join(".plugin-removed-sample-1-1");
        std::fs::create_dir(&orphan).unwrap();
        std::fs::write(orphan.join("leftover"), "old").unwrap();

        recover(&config).unwrap();

        assert!(!orphan.exists());
        assert!(destination.join(RECORD).is_file());
        assert!(InstallLock::acquire(&config).is_ok());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn activation_retires_the_backup_through_a_recoverable_name() {
        let config = scratch("retired-backup");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("old"), "old").unwrap();
        let stage = unique_path(&config, ".plugin-stage-sample");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join(STAGE_MARKER), "stage").unwrap();
        std::fs::write(stage.join("new"), "new").unwrap();

        PreparedPackage {
            id: "sample".into(),
            version: "2.0.0".into(),
            previous_version: Some("1.0.0".into()),
            conflicts: Vec::new(),
            stage: Some(stage),
            destination: destination.clone(),
        }
        .activate()
        .unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(&config)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".plugin-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(destination.join("new").is_file());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn removal_refuses_a_symlinked_plugins_parent() {
        let config = scratch("symlinked-parent");
        let elsewhere = config.join("elsewhere");
        let package = elsewhere.join("sample");
        std::fs::create_dir_all(&package).unwrap();
        let record = InstallationRecord {
            schema_version: 1,
            id: "sample".into(),
            package_version: "1.0.0".into(),
            catalog_commit: "0".repeat(40),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: BTreeMap::new(),
        };
        std::fs::write(package.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, config.join("plugins")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&elsewhere, config.join("plugins")).unwrap();

        let error = remove_below(&config, &["sample".to_string()]).unwrap_err();

        assert!(error.contains("refusing symlinked directory"), "{error}");
        assert!(package.is_dir());
        assert!(InstallLock::acquire(&config).is_err());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn conflicts_name_the_package_that_shadows_a_staged_one() {
        let dir = scratch("conflicts");
        let plugins = dir.join("plugins");
        let manual = plugins.join("aaa-manual");
        std::fs::create_dir_all(&manual).unwrap();
        let manifest = concat!(
            "schema_version = 1\n",
            "[plugin]\n",
            "name = \"Resource summary\"\n",
            "palette = \"resource-summary\"\n",
            "command = \"/bin/echo\"\n",
            "output = \"report\"\n",
        );
        std::fs::write(manual.join("plugin.toml"), manifest).unwrap();
        let unrelated = plugins.join("zzz-unrelated");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(
            unrelated.join("plugin.toml"),
            manifest
                .replace("Resource summary", "Something else")
                .replace("resource-summary", "something-else"),
        )
        .unwrap();
        let destination = plugins.join("resource-summary");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("plugin.toml"), manifest).unwrap();

        let staged = crate::plugins::read_package(&destination).unwrap();
        let found = conflicts(&plugins, &destination, &staged);

        assert_eq!(found, vec![manual]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recovery_restores_hyphenated_ids_and_ignores_unmarked_directories() {
        let config = scratch("recovery");
        let backup = config.join(".plugin-backup-resource-summary-1-1");
        std::fs::create_dir(&backup).unwrap();
        let record = InstallationRecord {
            schema_version: 1,
            id: "resource-summary".into(),
            package_version: "1.0.0".into(),
            catalog_commit: "0".repeat(40),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: BTreeMap::new(),
        };
        std::fs::write(backup.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
        let unrelated = config.join(".plugin-stage-user-data");
        std::fs::create_dir(&unrelated).unwrap();

        recover(&config).unwrap();

        assert!(config.join("plugins").join("resource-summary").is_dir());
        assert!(unrelated.is_dir());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn activation_replaces_nonempty_directories_and_identifies_rollbacks() {
        let config = scratch("activation");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("old"), "old").unwrap();

        let activate = |version: &str, previous: &str, contents: &str| {
            let stage = unique_path(&config, ".plugin-stage-sample");
            std::fs::create_dir(&stage).unwrap();
            std::fs::write(stage.join(STAGE_MARKER), "stage").unwrap();
            std::fs::write(stage.join("current"), contents).unwrap();
            PreparedPackage {
                id: "sample".into(),
                version: version.into(),
                previous_version: Some(previous.into()),
                conflicts: Vec::new(),
                stage: Some(stage),
                destination: destination.clone(),
            }
            .activate()
            .unwrap()
        };

        assert_eq!(activate("2.0.0", "1.0.0", "new"), Activation::Updated);
        assert!(!destination.join("old").exists());
        assert_eq!(
            std::fs::read_to_string(destination.join("current")).unwrap(),
            "new"
        );
        assert!(!destination.join(STAGE_MARKER).exists());
        assert_eq!(activate("1.5.0", "2.0.0", "older"), Activation::RolledBack);
        assert_eq!(
            std::fs::read_to_string(destination.join("current")).unwrap(),
            "older"
        );
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn install_lock_serializes_writers() {
        let config = scratch("lock");
        let first = InstallLock::acquire(&config).unwrap();
        assert!(InstallLock::acquire(&config).is_err());
        drop(first);
        assert!(InstallLock::acquire(&config).is_ok());
        let _ = std::fs::remove_dir_all(config);
    }
}
