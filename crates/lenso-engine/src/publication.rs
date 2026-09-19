//! Immutable content-addressed resources and atomic generation selection.
use crate::{Generation, Resource, validate_name};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileResource {
    pub path: String,
    pub bytes: Vec<u8>,
}
impl Resource {
    pub fn file(path: String, bytes: Vec<u8>) -> anyhow::Result<Self> {
        validate_name(&path)?;
        Ok(Self {
            schema: "lenso.engine.file.v1".into(),
            value: serde_json::to_value(FileResource { path, bytes })?,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceRef {
    pub path: String,
    pub digest: String,
    pub size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Publication {
    pub schema: String,
    pub generation: String,
    pub files: Vec<ResourceRef>,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn directory(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        if !fs::symlink_metadata(path)?.is_dir() {
            bail!("publication path is not a real directory");
        }
    } else {
        fs::create_dir(path)?;
    }
    Ok(())
}
/// Publishes only a complete validated generation. `current.json` is the commit
/// point; readers never follow source-machine paths or half-written artifacts.
pub fn publish(root: &Path, generation: &Generation) -> anyhow::Result<Publication> {
    directory(root)?;
    let lock_path = root.join("publication.lock");
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if !metadata.is_file() => bail!("invalid publication lock"),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    lock.lock()?;
    let mut files = BTreeMap::from([(
        "generation.json".to_owned(),
        serde_json::to_vec(&generation.outputs)?,
    )]);
    for resources in generation.outputs.values() {
        for resource in resources
            .values()
            .filter(|r| r.schema == "lenso.engine.file.v1")
        {
            let file: FileResource = serde_json::from_value(resource.value.clone())?;
            validate_name(&file.path)?;
            if files
                .insert(format!("files/{}", file.path), file.bytes)
                .is_some()
            {
                bail!("conflicting artifact path: {}", file.path);
            }
        }
    }
    let total: usize = files.values().map(Vec::len).sum();
    if total > 128 * 1024 * 1024 || files.len() > 4096 {
        bail!("publication exceeds file/byte budget");
    }
    let refs: Vec<_> = files
        .iter()
        .map(|(path, bytes)| ResourceRef {
            path: path.clone(),
            digest: digest(bytes),
            size: bytes.len() as u64,
        })
        .collect();
    let id = digest(&serde_json::to_vec(&refs)?);
    let publication = Publication {
        schema: "lenso.engine-publication.v1".into(),
        generation: id.clone(),
        files: refs,
    };
    let generations = root.join("generations");
    directory(&generations)?;
    let target = generations.join(&id);
    if target.exists() {
        verify(root, &publication)?;
    } else {
        let staging = tempfile::tempdir_in(&generations)?;
        for (path, bytes) in &files {
            let target = staging.path().join(path);
            fs::create_dir_all(target.parent().context("artifact parent")?)?;
            let mut file = fs::File::create(target)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(staging.path(), &target)?;
    }
    let mut selected = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(&mut selected, &publication)?;
    selected.as_file().sync_all()?;
    selected.persist(root.join("current.json"))?;
    Ok(publication)
}
pub fn verify(root: &Path, publication: &Publication) -> anyhow::Result<PathBuf> {
    if publication.schema != "lenso.engine-publication.v1"
        || publication.generation != digest(&serde_json::to_vec(&publication.files)?)
    {
        bail!("invalid generation reference");
    }
    let path = root.join("generations").join(&publication.generation);
    for file in &publication.files {
        validate_name(&file.path)?;
        let mut current = path.clone();
        if !fs::symlink_metadata(&current)?.is_dir() {
            bail!("invalid generation directory");
        }
        for component in Path::new(&file.path).components() {
            current.push(component);
            if fs::symlink_metadata(&current)?.file_type().is_symlink() {
                bail!("artifact symlink");
            }
        }
        if fs::metadata(&current)?.len() != file.size || digest(&fs::read(&current)?) != file.digest
        {
            bail!("artifact integrity mismatch: {}", file.path);
        }
    }
    Ok(path)
}
