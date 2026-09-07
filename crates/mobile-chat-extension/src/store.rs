//! Private, atomic conversation snapshots. The server is the only writer.

use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};

use crate::model::{Conversation, valid_id};

const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        if fs::symlink_metadata(&root)?.file_type().is_symlink() {
            bail!("Conversation directory cannot be a symlink.");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root })
    }

    pub(crate) fn owner(&self, owner: &str) -> Result<Self> {
        check_id(owner)?;
        Self::new(self.root.join(owner))
    }

    pub(crate) fn save(&self, value: &Conversation) -> Result<()> {
        check_id(&value.id)?;
        atomic_json(&self.root.join(format!("{}.json", value.id)), value)
    }

    pub(crate) fn load_all(&self) -> Result<(Vec<Conversation>, Vec<String>)> {
        let mut values = Vec::new();
        let mut errors = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if !valid_id(id) || id == "bindings" {
                continue;
            }
            let result = read_json::<Conversation>(&entry.path()).and_then(|value| {
                if value.version != 1 || value.id != id {
                    bail!("Unsupported or inconsistent conversation record.");
                }
                Ok(value)
            });
            match result {
                Ok(value) => values.push(value),
                Err(error) => errors.push(format!("Cannot load {name}: {error:#}")),
            }
        }
        Ok((values, errors))
    }

    pub(crate) fn bindings(&self) -> Result<BTreeMap<String, String>> {
        let path = self.root.join("bindings.json");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            _ => read_json(&path),
        }
    }

    pub(crate) fn save_bindings(&self, bindings: &BTreeMap<String, String>) -> Result<()> {
        atomic_json(&self.root.join("bindings.json"), bindings)
    }

    pub(crate) fn descriptor(&self, id: &str) -> Result<PathBuf> {
        check_id(id)?;
        Ok(self.root.join(format!("{id}.agent.json")))
    }
}

fn check_id(id: &str) -> Result<()> {
    if !valid_id(id) {
        bail!("Invalid storage ID.");
    }
    Ok(())
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
        bail!(
            "Expected a regular file of at most 64 MiB: {}",
            path.display()
        );
    }
    serde_json::from_slice(&fs::read(path)?).with_context(|| format!("parsing {}", path.display()))
}

pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("record has no parent directory")?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.is_file()
    {
        bail!(
            "Refusing to replace a non-regular record: {}",
            path.display()
        );
    }
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        bail!("Conversation reached the 64 MiB storage limit.");
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("saving {}", path.display()))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_replace_atomically_and_reject_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf()).unwrap();
        assert!(store.owner("../escape").is_err());
        let path = dir.path().join("test.json");
        atomic_json(&path, &json!({"value": 1})).unwrap();
        atomic_json(&path, &json!({"value": 2})).unwrap();
        assert_eq!(read_json::<serde_json::Value>(&path).unwrap()["value"], 2);
        fs::write(&path, "partial").unwrap();
        assert!(read_json::<serde_json::Value>(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn records_are_private_and_symlinks_are_refused() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        atomic_json(&path, &json!({"private": true})).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = dir.path().join("link.json");
        symlink(&path, &link).unwrap();
        assert!(read_json::<serde_json::Value>(&link).is_err());
        assert!(atomic_json(&link, &json!({})).is_err());
    }
}
