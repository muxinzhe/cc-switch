use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tempfile::{Builder, NamedTempFile};

use super::{ConfigKey, ConfigWrite, RemoteConfigFile, WriteResult, MAX_BYTES};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    key: ConfigKey,
    draft: Option<ConfigWrite>,
}

pub(crate) fn handle(args: Map<String, Value>) -> Result<Value> {
    let Request { key, draft } = serde_json::from_value(Value::Object(args))?;
    let path = resolve_target(&std::path::absolute(key.path()?)?, 0)?;
    if let Some(draft) = draft {
        draft.validate(key)?;
        let backups = crate::config::get_app_config_dir().join("config-backups");
        Ok(serde_json::to_value(save(&path, &backups, &draft)?)?)
    } else {
        Ok(serde_json::to_value(read(&path)?)?)
    }
}

// Resolve links even when the file or its containing directory has yet to be created.
fn resolve_target(path: &Path, links: usize) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => return Ok(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let parent = path.parent().ok_or(io::ErrorKind::NotFound)?;
    if let Ok(link) = fs::read_link(path) {
        if links >= 40 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Too many configuration symlinks",
            ));
        }
        return resolve_target(&parent.join(link), links + 1);
    }
    Ok(resolve_target(parent, links)?.join(path.file_name().ok_or(io::ErrorKind::NotFound)?))
}

fn contents(path: &Path) -> Result<Option<String>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == unsafe { libc::geteuid() },
        "The application config must be a regular file owned by the SSH user"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "The application config exceeds 1 MiB"
    );
    Ok(Some(
        String::from_utf8(bytes).context("The application config must use UTF-8")?,
    ))
}

fn revision(path: &Path, data: Option<&str>) -> String {
    let mut hash = Sha256::new();
    hash.update(path.as_os_str().as_bytes());
    hash.update([0]);
    if let Some(data) = data {
        hash.update(b"file:");
        hash.update(data.as_bytes());
    } else {
        hash.update(b"missing");
    }
    format!("{:x}", hash.finalize())
}

fn read(path: &Path) -> Result<RemoteConfigFile> {
    let text = contents(path)?;
    Ok(RemoteConfigFile {
        revision: revision(path, text.as_deref()),
        path: path.to_string_lossy().into_owned(),
        exists: text.is_some(),
        contents: text.unwrap_or_default(),
    })
}

fn write_temp(directory: &Path, prefix: &str, text: &str) -> Result<NamedTempFile> {
    let mut temp = Builder::new().prefix(prefix).tempfile_in(directory)?;
    temp.write_all(text.as_bytes())?;
    temp.as_file().sync_all()?;
    Ok(temp)
}

fn save(path: &Path, backups: &Path, draft: &ConfigWrite) -> Result<WriteResult> {
    let current = contents(path)?;
    if current.as_deref() == Some(&draft.contents) {
        return Ok(WriteResult { backup_path: None });
    }
    let conflict = "The remote configuration has changed. Reopen it before saving.";
    ensure!(
        revision(path, current.as_deref()) == draft.revision,
        "{conflict}"
    );
    let parent = path.parent().context("Missing configuration directory")?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let replacement = write_temp(parent, ".cc-switch-config-", &draft.contents)?;
    let backup = if let Some(previous) = &current {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(backups)?;
        let meta = fs::symlink_metadata(backups)?;
        ensure!(
            meta.is_dir()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o777 == 0o700,
            "Configuration backups must be in an owner-only directory"
        );
        Some(write_temp(
            backups,
            &format!("{}-", path.file_name().unwrap().to_string_lossy()),
            previous,
        )?)
    } else {
        None
    };
    ensure!(contents(path)? == current, "{conflict}");
    let backup_path = backup
        .map(|temp| temp.into_temp_path().keep())
        .transpose()?;
    if let Err(err) = replacement.persist(path) {
        if let Some(backup_path) = backup_path {
            fs::remove_file(backup_path)?;
        }
        return Err(err.into());
    }
    Ok(WriteResult {
        backup_path: backup_path.map(|p| p.to_string_lossy().into_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn native_rpc_only_accepts_named_application_files() {
        let args = serde_json::json!({"key": "codex", "path": "/tmp/unrelated"});
        assert!(handle(args.as_object().unwrap().clone()).is_err());

        let draft = ConfigWrite {
            revision: "a".repeat(64),
            contents: "unclosed = [".into(),
        };
        assert!(draft.validate(ConfigKey::Codex).is_err());
    }

    #[test]
    fn native_updates_preserve_symlinks_backups_and_revision_conflicts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let link = dir.path().join("config.toml");
        let target = dir.path().join("native").join("codex.toml");
        symlink("native/codex.toml", &link)?;
        let path = resolve_target(&link, 0)?;
        let backups = dir.path().join("backups");
        let initial = read(&path)?;
        assert!(!initial.exists);
        let draft = |text: &str, revision: &str| ConfigWrite {
            contents: text.to_owned(),
            revision: revision.to_owned(),
        };
        assert!(save(&path, &backups, &draft("a=1", &initial.revision))?
            .backup_path
            .is_none());
        let saved = read(&path)?;
        assert!(save(&path, &backups, &draft("a=2", &initial.revision)).is_err());
        let backup = save(&path, &backups, &draft("a=2", &saved.revision))?
            .backup_path
            .unwrap();
        assert_eq!(fs::read_to_string(&backup)?, "a=1");
        assert_eq!(fs::metadata(backup)?.mode() & 0o777, 0o600);
        assert!(link.is_symlink());
        assert_eq!(fs::read_to_string(&link)?, "a=2");
        assert_eq!(fs::metadata(&target)?.mode() & 0o777, 0o600);
        assert!(save(&path, &backups, &draft("a=2", &saved.revision))?
            .backup_path
            .is_none());
        assert_eq!(fs::read_dir(&backups)?.count(), 1);
        Ok(())
    }
}
