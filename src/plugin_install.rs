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
const EXPANDED_MAX_BYTES: u64 = 200 * 1024 * 1024;
const FILE_MAX: usize = 2_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationRecord {
    pub schema_version: u32,
    pub id: String,
    pub package_version: String,
    pub catalog_commit: String,
    pub source_commit: String,
    pub artifact_sha256: String,
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
            std::fs::remove_dir_all(&backup).map_err(|e| {
                format!(
                    "{} was activated, but cleanup of {} failed: {e}",
                    self.id,
                    backup.display()
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
    let config = plugin_catalog::config_dir()?;
    let plugins = config.join("plugins");
    ensure_directory_path(&config)?;
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
                .artifact_sha256
                .eq_ignore_ascii_case(&artifact.sha256)
            {
                return Err(format!(
                    "catalog digest for {id}@{version} differs from the installed immutable version"
                ));
            }
            prepared.push(PreparedPackage {
                id,
                version,
                previous_version: previous.map(|record| record.package_version),
                stage: None,
                destination,
            });
            continue;
        }
        let archive = plugin_catalog::artifact(&artifact, offline).await?;
        let stage = unique_path(&config, &format!(".plugin-stage-{id}"));
        std::fs::create_dir(&stage).map_err(|e| format!("creating {}: {e}", stage.display()))?;
        std::fs::write(
            stage.join(STAGE_MARKER),
            b"sofka plugin installation staging\n",
        )
        .map_err(|e| format!("marking {}: {e}", stage.display()))?;
        if let Err(error) = extract(&archive, &stage)
            .and_then(|()| crate::plugins::read_package(&stage).map(|_| ()))
        {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(format!("preparing {id}@{version}: {error}"));
        }
        let files = hash_files(&stage)?;
        let record = InstallationRecord {
            schema_version: RECORD_SCHEMA,
            id: id.clone(),
            package_version: version.clone(),
            catalog_commit: snapshot.commit.clone(),
            source_commit,
            artifact_sha256: artifact.sha256.to_ascii_lowercase(),
            files,
        };
        let json = serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?;
        crate::atomicfile::write(&stage.join(RECORD), &json)?;
        prepared.push(PreparedPackage {
            id,
            version,
            previous_version: previous.map(|record| record.package_version),
            stage: Some(stage),
            destination,
        });
    }
    Ok(prepared)
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
    let plugins = plugin_catalog::config_dir()?.join("plugins");
    let entries = match std::fs::read_dir(&plugins) {
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
                modified: record.id != id || verify_record(&path, &record).is_err(),
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
    Ok(installed()?
        .into_iter()
        .filter(|package| package.managed)
        .map(|package| package.id)
        .collect())
}

pub fn remove(ids: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
    let config = plugin_catalog::config_dir()?;
    let plugins = config.join("plugins");
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
        let staged = unique_path(&config, &format!(".plugin-removed-{id}"));
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
        Ok(())
    } else {
        Err(format!(
            "plugin {} at {} has local modifications; restore it or manage the directory manually",
            record.id,
            dir.display()
        ))
    }
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
                let bytes =
                    std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
                files.insert(relative, plugin_catalog::digest(&bytes));
            }
        } else {
            return Err(format!("plugin contains special file {}", path.display()));
        }
    }
    Ok(())
}

fn extract(archive: &Path, destination: &Path) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| format!("opening {}: {e}", archive.display()))?;
    let gzip = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gzip);
    let entries = archive
        .entries()
        .map_err(|e| format!("reading archive: {e}"))?;
    let mut paths = HashSet::new();
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
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| "archive expanded size overflow".to_string())?;
        if expanded > EXPANDED_MAX_BYTES {
            return Err("archive expands beyond 200 MiB".into());
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let mut output = File::options()
            .create_new(true)
            .write(true)
            .open(&target)
            .map_err(|e| format!("creating {}: {e}", target.display()))?;
        std::io::copy(&mut entry, &mut output)
            .map_err(|e| format!("extracting {}: {e}", target.display()))?;
        output
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
    Ok(())
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
        } else if name.starts_with(".plugin-removed-") {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) && read_record(&path).is_ok() {
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
        let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(gzip);
        for (name, bytes, kind) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(if *name == "adapter" { 0o755 } else { 0o644 });
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sofka-plugin-install-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
            artifact_sha256: "2".repeat(64),
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
        let source = dir.join("package.tar.gz");
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
        let duplicate = dir.join("duplicate.tar.gz");
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

        let linked = dir.join("link.tar.gz");
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
            artifact_sha256: "2".repeat(64),
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
