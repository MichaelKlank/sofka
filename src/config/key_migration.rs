use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::keymap::LEGACY_PALETTE_KEYS;

pub(super) struct Migration {
    path: PathBuf,
    original: String,
    updated: String,
}

/// Convert each source before merging so context bindings retain precedence.
pub(super) fn prepare(
    value: &mut toml::Value,
    path: &Path,
    warnings: &mut Vec<String>,
) -> Option<Migration> {
    let keys = value.get("keys")?.as_table()?;
    let moves: Vec<_> = LEGACY_PALETTE_KEYS
        .iter()
        .filter_map(|&(old, action)| keys.get(old).map(|v| (old, action.name(), v.clone())))
        .collect();
    if moves.is_empty() {
        return None;
    }
    let mut errors = Vec::new();
    for (old, _, spec) in &moves {
        crate::keymap::parse_chords(spec, &format!("keys.{old}"), &mut errors);
    }
    if !errors.is_empty() {
        warnings.push(format!(
            "{}: cannot migrate palette keys: {}; correct the values and move them to {}",
            path.display(),
            errors.join("; "),
            command_section(path)
        ));
        return None;
    }
    if let Some(command) = keys.get("command") {
        for &(old, new, _) in &moves {
            if !command.is_table() || command.get(new).is_some() {
                warnings.push(format!(
                    "{}: cannot migrate keys.{old}; check keys.command.{new} and remove the legacy field",
                    path.display()
                ));
                return None;
            }
        }
    }
    let before = value.clone();
    let keys = value.get_mut("keys").unwrap().as_table_mut().unwrap();
    for (old, new, v) in moves {
        keys.remove(old);
        keys.entry("command")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .unwrap()
            .insert(new.into(), v);
    }
    let prepared = (|| -> Result<Migration, String> {
        if let Some(dir) = path.parent() {
            super::document::select(dir)?;
        }
        let original = fs::read_to_string(path).map_err(|e| e.to_string())?;
        // A prior resolve may already have updated this cached base source.
        let current = super::document::parse(path, &original)?;
        if current == *value {
            return Ok(Migration {
                path: path.into(),
                original: original.clone(),
                updated: original,
            });
        }
        if current != before {
            return Err("config changed since it was loaded; reload it before migration".into());
        }
        let updated = if super::document::is_yaml(path) {
            serde_yaml::to_string(value).map_err(|e| e.to_string())?
        } else {
            edit_document(&original)?
        };
        if super::document::parse(path, &updated)? != *value {
            return Err("migration changed other settings; update the config manually".into());
        }
        Ok(Migration {
            path: path.into(),
            original,
            updated,
        })
    })();
    match prepared {
        Ok(migration) => Some(migration),
        Err(e) => {
            warnings.push(failure(path, &e));
            None
        }
    }
}

fn edit_document(text: &str) -> Result<String, String> {
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| e.to_string())?;
    let keys = doc
        .get_mut("keys")
        .and_then(toml_edit::Item::as_table_like_mut)
        .ok_or("keys is not a table")?;
    let mut moves = Vec::new();
    for &(old, action) in LEGACY_PALETTE_KEYS {
        if let Some(key) = keys.key(old).cloned() {
            let new = toml_edit::Key::new(action.name())
                .with_leaf_decor(key.leaf_decor().clone())
                .with_dotted_decor(key.dotted_decor().clone());
            moves.push((new, keys.remove(old).unwrap()));
        }
    }
    let command = keys
        .entry("command")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or("keys.command is not a table")?;
    for (key, item) in moves {
        command.entry_format(&key).or_insert(item);
    }
    Ok(doc.to_string())
}

pub(super) fn finish(migrations: Vec<Migration>, valid: bool, warnings: &mut Vec<String>) {
    for migration in migrations {
        if migration.original == migration.updated {
            continue;
        }
        if !valid {
            warnings.push(format!(
                "{}: config migration was not saved; correct the config errors, then reload",
                migration.path.display()
            ));
            continue;
        }
        match migration.save() {
            Ok(backup) => warnings.push(format!(
                "{}: moved legacy palette keys to {}; backup: {}",
                migration.path.display(),
                command_section(&migration.path),
                backup.display()
            )),
            Err(e) => warnings.push(failure(&migration.path, &e.to_string())),
        }
    }
}

fn command_section(path: &Path) -> &'static str {
    if super::document::is_yaml(path) {
        "keys.command"
    } else {
        "[keys.command]"
    }
}

fn failure(path: &Path, error: &str) -> String {
    format!(
        "{}: cannot save config migration: {error}; using migrated keys in memory. Update {} in the config source: palette_next -> down, palette_prev -> up, palette_accept -> accept",
        path.display(),
        command_section(path)
    )
}

impl Migration {
    fn save(&self) -> io::Result<PathBuf> {
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.is_file() || metadata.permissions().readonly() {
            return Err(io::Error::other("config is read-only or a symlink"));
        }
        self.check_source()?;
        let extension = self
            .path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("toml");
        let backup = self.path.with_extension(format!("{extension}.bak"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut backup_file = options.open(&backup).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot create backup {}: {e}", backup.display()),
            )
        })?;
        if let Err(e) = backup_file
            .write_all(self.original.as_bytes())
            .and_then(|_| backup_file.sync_all())
            .and_then(|_| backup_file.set_permissions(metadata.permissions()))
        {
            let _ = fs::remove_file(&backup);
            return Err(e);
        }
        let temporary = self
            .path
            .with_extension(format!("{extension}.migration.tmp"));
        let mut file = options.open(&temporary)?;
        let saved = (|| {
            file.write_all(self.updated.as_bytes())?;
            file.set_permissions(metadata.permissions())?;
            file.sync_all()?;
            self.check_source()?;
            fs::rename(&temporary, &self.path)
        })();
        if saved.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        saved.map(|_| backup)
    }

    fn check_source(&self) -> io::Result<()> {
        if fs::symlink_metadata(&self.path)?.file_type().is_symlink()
            || fs::read_to_string(&self.path)? != self.original
        {
            return Err(io::Error::other("config changed during migration"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_migration_handles_toml_forms_without_changing_other_values() {
        for text in [
            "# heading\n[keys]\n# move me\npalette_next = ['ctrl-n', 'down'] # next\n[aliases]\nx = 'palette_next'\n",
            "keys.palette_next = 'ctrl-n'\n",
            "keys = { palette_next = 'ctrl-n' }\n",
            "[keys]\ncommand = { up = 'ctrl-p' }\npalette_next = 'ctrl-n'\n",
            "[keys]\n'palette_next' = '''ctrl-n'''\n",
            "[keys]\npalette_next = [\n  'ctrl-n', # first\n  'down',\n]\n",
        ] {
            let updated = edit_document(text).unwrap();
            let parsed = super::super::parse_doc(&updated).unwrap();
            let before = super::super::parse_doc(text).unwrap();
            assert_eq!(
                parsed["keys"]["command"]["down"], before["keys"]["palette_next"],
                "{updated}"
            );
            assert!(parsed["keys"].get("palette_next").is_none());
            assert_eq!(parsed.get("aliases"), before.get("aliases"));
            for comment in ["# heading", "# move me", "# next", "# first"] {
                if text.contains(comment) {
                    assert!(updated.contains(comment), "{updated}");
                }
            }
        }
    }

    #[test]
    fn migration_preserves_existing_backups_and_detects_changed_sources() {
        for extension in ["toml", "yaml", "yml"] {
            let dir =
                std::env::temp_dir().join(format!("sofka-migrate-save-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("config.{extension}"));
            let backup = dir.join(format!("config.{extension}.bak"));
            let temporary = dir.join(format!("config.{extension}.migration.tmp"));
            let migration = Migration {
                path: path.clone(),
                original: "[keys]\npalette_next = 'ctrl-n'\n".into(),
                updated: "[keys.command]\ndown = 'ctrl-n'\n".into(),
            };
            fs::write(&path, &migration.original).unwrap();
            fs::write(&backup, "previous backup").unwrap();
            assert!(migration.save().is_err());
            assert_eq!(fs::read_to_string(&backup).unwrap(), "previous backup");
            assert_eq!(fs::read_to_string(&path).unwrap(), migration.original);
            fs::remove_file(&backup).unwrap();

            fs::write(&path, "hide_header = true\n").unwrap();
            assert!(migration.save().is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "hide_header = true\n");
            assert!(!backup.exists());

            fs::write(&path, &migration.original).unwrap();
            fs::write(&temporary, "another migration").unwrap();
            assert!(migration.save().is_err());
            assert_eq!(fs::read_to_string(&temporary).unwrap(), "another migration");
            assert_eq!(fs::read_to_string(&path).unwrap(), migration.original);
            assert_eq!(fs::read_to_string(&backup).unwrap(), migration.original);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn migration_preserves_file_and_backup_permissions() {
        for extension in ["toml", "yaml", "yml"] {
            use std::os::unix::fs::PermissionsExt;
            let dir =
                std::env::temp_dir().join(format!("sofka-migrate-mode-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("config.{extension}"));
            let migration = Migration {
                path: path.clone(),
                original: "[keys]\npalette_next = 'ctrl-n'\n".into(),
                updated: "[keys.command]\ndown = 'ctrl-n'\n".into(),
            };
            fs::write(&path, &migration.original).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            let backup = migration.save().unwrap();
            for file in [&path, &backup] {
                assert_eq!(
                    fs::metadata(file).unwrap().permissions().mode() & 0o777,
                    0o640
                );
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), migration.updated);
            assert_eq!(fs::read_to_string(&backup).unwrap(), migration.original);
            fs::remove_dir_all(dir).unwrap();
        }
    }
}
