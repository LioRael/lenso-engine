//! Explicit bootstrap manifests and integrity locks, independent of conventions.
use crate::{Snapshot, external::ProcessPlugin};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Workflow {
    schema: String,
    #[serde(default)]
    sources: Vec<PathBuf>,
    #[serde(default)]
    plugin_sources: Vec<PathBuf>,
    #[serde(default)]
    plugins: Vec<String>,
    #[serde(default)]
    presets: Vec<PathBuf>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub path: PathBuf,
    pub sha256: String,
    #[serde(default)]
    pub directory: bool,
}
impl Proof {
    pub fn capture(path: &Path) -> anyhow::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() && !metadata.is_dir() {
            bail!(
                "locked artifact must be a regular file or directory: {}",
                path.display()
            );
        }
        let path = fs::canonicalize(path)?;
        if metadata.is_dir() {
            let snapshot = Snapshot::read(&path)?;
            return Ok(Self {
                path,
                directory: true,
                sha256: format!("{:x}", Sha256::digest(serde_json::to_vec(&snapshot)?)),
            });
        }
        let mut hash = Sha256::new();
        let mut file = fs::File::open(&path)?;
        let mut buffer = [0; 65536];
        loop {
            let size = file.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            hash.update(&buffer[..size]);
        }
        Ok(Self {
            path,
            sha256: format!("{:x}", hash.finalize()),
            directory: false,
        })
    }
    pub fn verify(&self) -> anyhow::Result<()> {
        let observed = Self::capture(&self.path)?;
        if observed.sha256 != self.sha256 || observed.directory != self.directory {
            bail!("locked input changed: {}", self.path.display());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPlugin {
    pub manifest: PathBuf,
    pub identity: String,
    pub executable: PathBuf,
    pub proofs: Vec<Proof>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapLock {
    pub schema: String,
    pub sources: Vec<PathBuf>,
    pub plugins: Vec<LockedPlugin>,
    pub configuration: Vec<Proof>,
}
#[derive(Debug, Default)]
struct Composition {
    sources: Vec<PathBuf>,
    roots: Vec<PathBuf>,
    selected: BTreeSet<String>,
    configuration: Vec<Proof>,
}
fn compose(
    path: &Path,
    visited: &mut BTreeSet<PathBuf>,
    result: &mut Composition,
    depth: usize,
) -> anyhow::Result<()> {
    if depth > 8 {
        bail!("preset nesting exceeds 8");
    }
    let path = fs::canonicalize(path)?;
    if !visited.insert(path.clone()) {
        bail!("cyclic or repeated preset: {}", path.display());
    }
    let bytes = fs::read(&path)?;
    if bytes.len() > 65536 {
        bail!("workflow manifest exceeds 64 KiB");
    }
    let workflow: Workflow = serde_json::from_slice(&bytes)?;
    if workflow.schema != "lenso.engine-workflow.v1" {
        bail!("unsupported workflow schema");
    }
    let base = path.parent().context("workflow parent")?;
    for preset in workflow.presets {
        compose(&base.join(preset), visited, result, depth + 1)?;
    }
    for source in workflow.sources {
        result.sources.push(fs::canonicalize(base.join(source))?);
    }
    for source in workflow.plugin_sources {
        result.roots.push(fs::canonicalize(base.join(source))?);
    }
    result.selected.extend(workflow.plugins);
    result.configuration.push(Proof::capture(&path)?);
    Ok(())
}
fn discover(
    root: &Path,
    paths: &mut Vec<PathBuf>,
    count: &mut usize,
    depth: usize,
) -> anyhow::Result<()> {
    if depth > 16 {
        bail!("plugin source exceeds directory depth budget");
    }
    if !fs::symlink_metadata(root)?.is_dir() {
        bail!("plugin source must be a directory");
    }
    for entry in fs::read_dir(root)? {
        *count += 1;
        if *count > 4096 {
            bail!("plugin source exceeds 4096 entries");
        }
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!("plugin source symlinks are not admitted");
        }
        if kind.is_dir() {
            discover(&entry.path(), paths, count, depth + 1)?;
        } else if kind.is_file() && entry.file_name() == "engine-plugin.json" {
            paths.push(entry.path());
        }
    }
    Ok(())
}
fn executable(program: &str, directory: &Path) -> anyhow::Result<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() > 1 {
        return Ok(fs::canonicalize(directory.join(path))?);
    }
    for root in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let path = root.join(program);
        if path.is_file() {
            return Ok(fs::canonicalize(path)?);
        }
    }
    bail!("bootstrap executable is unavailable: {program}")
}
impl BootstrapLock {
    /// Locking is explicit and read-only with respect to plugins: it never runs tools.
    pub fn resolve(workflow: &Path) -> anyhow::Result<Self> {
        let mut composition = Composition::default();
        compose(workflow, &mut BTreeSet::new(), &mut composition, 0)?;
        let mut candidates = BTreeMap::new();
        let mut count = 0;
        for root in &composition.roots {
            let mut paths = Vec::new();
            discover(root, &mut paths, &mut count, 0)?;
            for path in paths {
                let bytes = fs::read(&path)?;
                if bytes.len() > 16384 {
                    bail!("plugin manifest exceeds 16 KiB");
                }
                let manifest: crate::external::Manifest = serde_json::from_slice(&bytes)?;
                if candidates
                    .insert(manifest.identity.clone(), (path, manifest))
                    .is_some()
                {
                    bail!("ambiguous plugin identity in local sources");
                }
            }
        }
        let mut plugins = Vec::new();
        for id in composition.selected {
            let (path, manifest) = candidates
                .remove(&id)
                .with_context(|| format!("selected plugin is unavailable: {id}"))?;
            let base = path.parent().context("plugin parent")?;
            let program = executable(&manifest.program, base)?;
            let mut proofs = vec![Proof::capture(&path)?, Proof::capture(&program)?];
            for file in &manifest.artifacts {
                crate::validate_name(file)?;
                let mut artifact = base.to_path_buf();
                for component in Path::new(file).components() {
                    artifact.push(component);
                    if fs::symlink_metadata(&artifact)?.file_type().is_symlink() {
                        bail!("plugin artifacts cannot traverse symlinks");
                    }
                }
                proofs.push(Proof::capture(&artifact)?);
            }
            plugins.push(LockedPlugin {
                manifest: path,
                identity: id,
                executable: program,
                proofs,
            });
        }
        Ok(Self {
            schema: "lenso.engine-lock.v1".into(),
            sources: composition.sources,
            plugins,
            configuration: composition.configuration,
        })
    }
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut temporary =
            tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
        serde_json::to_writer_pretty(&mut temporary, self)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    }
    pub fn load(path: &Path, workflow: &Path) -> anyhow::Result<Self> {
        if fs::metadata(path)?.len() > 1024 * 1024 {
            bail!("bootstrap lock exceeds 1 MiB");
        }
        let lock: Self = serde_json::from_slice(&fs::read(path)?)?;
        if lock.schema != "lenso.engine-lock.v1"
            || !lock
                .configuration
                .iter()
                .any(|p| p.path == workflow.canonicalize().unwrap_or_default())
        {
            bail!("bootstrap lock belongs to a different workflow");
        }
        for proof in &lock.configuration {
            proof.verify()?;
        }
        for plugin in &lock.plugins {
            for proof in &plugin.proofs {
                proof.verify()?;
            }
        }
        Ok(lock)
    }
    pub fn processors(&self) -> anyhow::Result<Vec<ProcessPlugin>> {
        self.plugins
            .iter()
            .map(ProcessPlugin::load_locked)
            .collect()
    }
}
