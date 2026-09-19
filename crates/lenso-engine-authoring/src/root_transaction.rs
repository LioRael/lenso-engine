use std::{
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use super::{PLUGIN_ROOT, TRANSACTION_GUARD, atomic_write, runtime_sha256};

const TRANSACTIONS: &str = ".lenso/plugin-root-transactions";
const RECORD: &str = "record.json";
const MAX_TRANSACTION_BYTES: usize = 16 * 1024 * 1024;
const MAX_TRANSACTION_CHANGES: usize = 4_096;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TransactionStatus {
    Prepared,
    Committed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionRecord {
    schema_version: u32,
    id: String,
    status: TransactionStatus,
    changes: Vec<TransactionRecordChange>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionRecordChange {
    path: String,
    old_digest: Option<String>,
    new_digest: Option<String>,
    old_staged: Option<String>,
    new_staged: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct RootFileChange {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
}

impl RootFileChange {
    pub(crate) fn write(path: impl Into<PathBuf>, bytes: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            bytes: Some(bytes),
        }
    }

    pub(crate) fn remove(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            bytes: None,
        }
    }
}

pub(crate) fn publish_root_files(root: &Path, changes: Vec<RootFileChange>) -> anyhow::Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    if changes.len() > MAX_TRANSACTION_CHANGES {
        bail!("Plugin Root transaction exceeds {MAX_TRANSACTION_CHANGES} paths");
    }
    let id = uuid::Uuid::now_v7().to_string();
    let transaction = root.join(TRANSACTIONS).join(&id);
    fs::create_dir_all(&transaction).context("create Plugin Root transaction staging")?;

    let mut total = 0_usize;
    let mut seen = std::collections::BTreeSet::new();
    let mut recorded = Vec::with_capacity(changes.len());
    for (index, change) in changes.into_iter().enumerate() {
        validate_relative_path(&change.path)?;
        let path = root.join(PLUGIN_ROOT).join(&change.path);
        let key = change.path.to_string_lossy().replace('\\', "/");
        if !seen.insert(key.clone()) {
            bail!("duplicate Plugin Root transaction path `{key}`");
        }
        let old = read_regular_optional(&path)?;
        total = total
            .checked_add(old.as_ref().map_or(0, Vec::len))
            .and_then(|value| value.checked_add(change.bytes.as_ref().map_or(0, Vec::len)))
            .context("Plugin Root transaction size overflow")?;
        if total > MAX_TRANSACTION_BYTES {
            bail!("Plugin Root transaction exceeds 16 MiB");
        }
        let old_staged = stage_optional(&transaction, "old", index, old.as_deref())?;
        let new_staged = stage_optional(&transaction, "new", index, change.bytes.as_deref())?;
        recorded.push(TransactionRecordChange {
            path: key,
            old_digest: old.as_deref().map(runtime_sha256),
            new_digest: change.bytes.as_deref().map(runtime_sha256),
            old_staged,
            new_staged,
        });
    }

    let mut record = TransactionRecord {
        schema_version: 1,
        id: id.clone(),
        status: TransactionStatus::Prepared,
        changes: recorded,
    };
    write_record(&transaction, &record)?;
    create_guard(root, &id)?;

    apply_record(root, &transaction, &record, TransactionStatus::Committed)?;
    record.status = TransactionStatus::Committed;
    write_record(&transaction, &record)?;
    verify_record(root, &record, TransactionStatus::Committed)?;
    remove_guard(root)?;
    if let Err(error) = fs::remove_dir_all(&transaction) {
        eprintln!(
            "warning: Plugin Root transaction committed, but staging cleanup failed: {error}"
        );
    }
    Ok(())
}

pub(crate) fn recover_plugin_root_transaction(root: &Path) -> anyhow::Result<()> {
    let guard = root.join(PLUGIN_ROOT).join(TRANSACTION_GUARD);
    let Some(bytes) = read_regular_optional(&guard)? else {
        return Ok(());
    };
    let id = std::str::from_utf8(&bytes)
        .context("Plugin Root transaction guard is not UTF-8")?
        .trim();
    if uuid::Uuid::parse_str(id).is_err() {
        bail!("Plugin Root transaction guard contains an invalid transaction identity");
    }
    let transaction = root.join(TRANSACTIONS).join(id);
    let record_path = transaction.join(RECORD);
    let record: TransactionRecord = serde_json::from_slice(
        &read_regular_optional(&record_path)?
            .context("Plugin Root transaction record is missing")?,
    )
    .context("Plugin Root transaction record is invalid")?;
    if record.schema_version != 1 || record.id != id {
        bail!("Plugin Root transaction record identity or schema is invalid");
    }
    validate_record(&transaction, &record)?;
    let target = record.status;
    ensure_recoverable(root, &record)?;
    apply_record(root, &transaction, &record, target)?;
    verify_record(root, &record, target)?;
    remove_guard(root)?;
    if let Err(error) = fs::remove_dir_all(&transaction) {
        eprintln!("warning: recovered Plugin Root transaction staging remains: {error}");
    }
    Ok(())
}

fn ensure_recoverable(root: &Path, record: &TransactionRecord) -> anyhow::Result<()> {
    for change in &record.changes {
        let path = root.join(PLUGIN_ROOT).join(&change.path);
        let digest = read_regular_optional(&path)?.as_deref().map(runtime_sha256);
        if digest != change.old_digest && digest != change.new_digest {
            bail!(
                "Plugin Root transaction conflicts with a newer or manual edit at {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn validate_record(transaction: &Path, record: &TransactionRecord) -> anyhow::Result<()> {
    if record.changes.is_empty() || record.changes.len() > MAX_TRANSACTION_CHANGES {
        bail!("Plugin Root transaction record has an invalid path count");
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0_usize;
    for change in &record.changes {
        validate_relative_path(Path::new(&change.path))?;
        if !seen.insert(&change.path) {
            bail!("Plugin Root transaction record contains a duplicate path");
        }
        for (staged, digest) in [
            (change.old_staged.as_deref(), change.old_digest.as_deref()),
            (change.new_staged.as_deref(), change.new_digest.as_deref()),
        ] {
            if staged.is_some() != digest.is_some() {
                bail!("Plugin Root transaction staged file and digest disagree");
            }
            let Some(staged) = staged else {
                continue;
            };
            validate_staged_name(staged)?;
            let bytes = read_regular_optional(&transaction.join(staged))?
                .context("Plugin Root transaction staged file is missing")?;
            if Some(runtime_sha256(&bytes)).as_deref() != digest {
                bail!("Plugin Root transaction staged file digest is invalid");
            }
            total = total
                .checked_add(bytes.len())
                .context("Plugin Root transaction size overflow")?;
            if total > MAX_TRANSACTION_BYTES {
                bail!("Plugin Root transaction exceeds 16 MiB");
            }
        }
    }
    Ok(())
}

fn apply_record(
    root: &Path,
    transaction: &Path,
    record: &TransactionRecord,
    target: TransactionStatus,
) -> anyhow::Result<()> {
    for change in &record.changes {
        let path = root.join(PLUGIN_ROOT).join(&change.path);
        let staged = match target {
            TransactionStatus::Prepared => change.old_staged.as_deref(),
            TransactionStatus::Committed => change.new_staged.as_deref(),
        };
        match staged {
            Some(name) => {
                let bytes = read_regular_optional(&transaction.join(name))?
                    .context("Plugin Root transaction staged file is missing")?;
                atomic_write(&path, &bytes)?;
            }
            None => remove_file_if_exists(&path)?,
        }
    }
    Ok(())
}

fn verify_record(
    root: &Path,
    record: &TransactionRecord,
    target: TransactionStatus,
) -> anyhow::Result<()> {
    for change in &record.changes {
        let path = root.join(PLUGIN_ROOT).join(&change.path);
        let actual = read_regular_optional(&path)?.as_deref().map(runtime_sha256);
        let expected = match target {
            TransactionStatus::Prepared => &change.old_digest,
            TransactionStatus::Committed => &change.new_digest,
        };
        if &actual != expected {
            bail!(
                "Plugin Root transaction did not produce the expected bytes at {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn create_guard(root: &Path, id: &str) -> anyhow::Result<()> {
    let plugin_root = root.join(PLUGIN_ROOT);
    fs::create_dir_all(&plugin_root)?;
    let path = plugin_root.join(TRANSACTION_GUARD);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("create Plugin Root transaction guard {}", path.display()))?;
    file.write_all(id.as_bytes())?;
    file.sync_all()?;
    sync_directory(&plugin_root)?;
    Ok(())
}

fn remove_guard(root: &Path) -> anyhow::Result<()> {
    let plugin_root = root.join(PLUGIN_ROOT);
    remove_file_if_exists(&plugin_root.join(TRANSACTION_GUARD))?;
    sync_directory(&plugin_root)
}

fn write_record(transaction: &Path, record: &TransactionRecord) -> anyhow::Result<()> {
    atomic_write(
        &transaction.join(RECORD),
        &serde_json::to_vec_pretty(record)?,
    )?;
    sync_directory(transaction)
}

fn stage_optional(
    transaction: &Path,
    prefix: &str,
    index: usize,
    bytes: Option<&[u8]>,
) -> anyhow::Result<Option<String>> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let name = format!("{prefix}-{index}");
    let path = transaction.join(&name);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(Some(name))
}

fn read_regular_optional(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if !metadata.file_type().is_file() {
        bail!(
            "Plugin Root transaction path must be a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_TRANSACTION_BYTES as u64 {
        bail!(
            "Plugin Root transaction file exceeds 16 MiB: {}",
            path.display()
        );
    }
    fs::read(path)
        .map(Some)
        .with_context(|| format!("read {}", path.display()))
}

fn remove_file_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn validate_relative_path(path: &Path) -> anyhow::Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("Plugin Root transaction path must stay beneath `plugins/`");
    }
    Ok(())
}

fn validate_staged_name(name: &str) -> anyhow::Result<()> {
    let path = Path::new(name);
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        bail!("Plugin Root transaction staged path is invalid");
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> anyhow::Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interrupted(root: &Path, status: TransactionStatus, live: &[u8]) -> (PathBuf, PathBuf) {
        let id = uuid::Uuid::now_v7().to_string();
        let transaction = root.join(TRANSACTIONS).join(&id);
        fs::create_dir_all(root.join(PLUGIN_ROOT)).unwrap();
        fs::create_dir_all(&transaction).unwrap();
        let old = b"old";
        let new = b"new";
        let old_staged = stage_optional(&transaction, "old", 0, Some(old)).unwrap();
        let new_staged = stage_optional(&transaction, "new", 0, Some(new)).unwrap();
        let record = TransactionRecord {
            schema_version: 1,
            id: id.clone(),
            status,
            changes: vec![TransactionRecordChange {
                path: "state".to_owned(),
                old_digest: Some(runtime_sha256(old)),
                new_digest: Some(runtime_sha256(new)),
                old_staged,
                new_staged,
            }],
        };
        write_record(&transaction, &record).unwrap();
        create_guard(root, &id).unwrap();
        let state = root.join(PLUGIN_ROOT).join("state");
        atomic_write(&state, live).unwrap();
        (state, root.join(PLUGIN_ROOT).join(TRANSACTION_GUARD))
    }

    #[test]
    fn prepared_transaction_recovers_old_bytes() {
        let root = tempfile::tempdir().unwrap();
        let (state, guard) = interrupted(root.path(), TransactionStatus::Prepared, b"new");

        recover_plugin_root_transaction(root.path()).unwrap();

        assert_eq!(fs::read(state).unwrap(), b"old");
        assert!(!guard.exists());
    }

    #[test]
    fn committed_transaction_finishes_new_bytes() {
        let root = tempfile::tempdir().unwrap();
        let (state, guard) = interrupted(root.path(), TransactionStatus::Committed, b"old");

        recover_plugin_root_transaction(root.path()).unwrap();

        assert_eq!(fs::read(state).unwrap(), b"new");
        assert!(!guard.exists());
    }

    #[test]
    fn recovery_preserves_a_conflicting_manual_edit_and_guard() {
        let root = tempfile::tempdir().unwrap();
        let (state, guard) = interrupted(root.path(), TransactionStatus::Prepared, b"manual");

        let error = recover_plugin_root_transaction(root.path()).unwrap_err();

        assert!(error.to_string().contains("newer or manual edit"));
        assert_eq!(fs::read(state).unwrap(), b"manual");
        assert!(guard.is_file());
    }
}
