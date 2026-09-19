//! Domain-neutral, embeddable processing. Extensions are explicitly registered;
//! filenames, languages and App composition have no built-in meaning.
pub mod bootstrap;
pub mod external;
pub mod process;
pub mod publication;
pub mod session;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    pub schema: String,
    pub value: serde_json::Value,
}

/// A bounded immutable input set. Plugins receive bytes, never ambient file access
/// through the Engine API. Native extensions remain trusted host code.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    files: BTreeMap<String, Vec<u8>>,
}
impl Snapshot {
    pub fn read(root: &Path) -> anyhow::Result<Self> {
        if !fs::symlink_metadata(root)?.is_dir() {
            bail!("input root must be a directory, not a symlink");
        }
        let mut snapshot = Self::default();
        snapshot.walk(root, root, 0, &mut 0)?;
        Ok(snapshot)
    }
    fn walk(
        &mut self,
        root: &Path,
        path: &Path,
        depth: usize,
        entries: &mut usize,
    ) -> anyhow::Result<()> {
        if depth > 32 {
            bail!("input exceeds 32 directory levels");
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            *entries += 1;
            if *entries > 4096 {
                bail!("input exceeds 4096 directory entries");
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
                bail!("input contains symlink or special file");
            }
            if kind.is_dir() {
                self.walk(root, &entry.path(), depth + 1, entries)?;
            } else {
                if entry.metadata()?.len() > 16 * 1024 * 1024 {
                    bail!("input file exceeds 16 MiB");
                }
                let name = entry
                    .path()
                    .strip_prefix(root)?
                    .to_str()
                    .context("non-UTF8 input path")?
                    .replace('\\', "/");
                use std::io::Read;
                let mut bytes = Vec::new();
                fs::File::open(entry.path())?
                    .take(16 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                self.insert(name, bytes)?;
            }
        }
        Ok(())
    }
    pub fn insert(&mut self, path: String, bytes: Vec<u8>) -> anyhow::Result<()> {
        validate_name(&path)?;
        if self.files.contains_key(&path) {
            bail!("duplicate input {path}");
        }
        if self.files.len() >= 4096
            || self.files.values().map(Vec::len).sum::<usize>() + bytes.len() > 16 * 1024 * 1024
        {
            bail!("input exceeds 4096 files / 16 MiB");
        }
        self.files.insert(path, bytes);
        Ok(())
    }
    pub fn files(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.files
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub inputs: Vec<String>,
    pub after: Vec<String>,
    pub options: serde_json::Value,
}

/// All declared dependencies and input bytes are supplied to the processor.
#[derive(Debug)]
pub struct ContextView<'a> {
    pub step: &'a Step,
    pub files: BTreeMap<String, &'a [u8]>,
    pub dependencies: BTreeMap<String, &'a BTreeMap<String, Resource>>,
    pub cancelled: &'a std::sync::Arc<AtomicBool>,
}

pub trait Plugin: std::fmt::Debug + Send + Sync {
    /// Stable identity including implementation/configuration revision. Change it
    /// whenever behavior or undeclared toolchain inputs change.
    fn identity(&self) -> &str;
    /// Opt in only when returned resources are the complete effect and all
    /// semantic inputs are declared. Filesystem-producing compilers do not cache.
    fn cacheable(&self) -> bool {
        false
    }
    /// Planning must be side-effect free.
    fn plan(&self, snapshot: &Snapshot) -> anyhow::Result<Vec<Step>>;
    fn process(&self, context: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>>;
}

#[derive(Clone, Debug, Serialize)]
pub struct PlannedStep {
    pub plugin: String,
    pub step: Step,
}
#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    steps: Vec<PlannedStep>,
    #[serde(skip_serializing)]
    snapshot: Snapshot,
}
impl Plan {
    pub fn steps(&self) -> &[PlannedStep] {
        &self.steps
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Generation {
    pub outputs: BTreeMap<String, BTreeMap<String, Resource>>,
    pub cache_hits: usize,
}

#[derive(Debug, Default)]
pub struct Engine {
    plugins: BTreeMap<String, Box<dyn Plugin>>,
    cache: BTreeMap<String, BTreeMap<String, Resource>>,
}
impl Engine {
    pub fn register(&mut self, plugin: impl Plugin + 'static) -> anyhow::Result<()> {
        let id = plugin.identity().to_owned();
        if id.is_empty() || self.plugins.contains_key(&id) {
            bail!("empty or duplicate plugin identity: {id}");
        }
        self.plugins.insert(id, Box::new(plugin));
        Ok(())
    }
    pub fn plan(&self, snapshot: Snapshot) -> anyhow::Result<Plan> {
        let mut remaining = BTreeMap::new();
        for (id, plugin) in &self.plugins {
            for step in plugin
                .plan(&snapshot)
                .with_context(|| format!("plan {id}"))?
            {
                validate_name(&step.id)?;
                for input in &step.inputs {
                    if !snapshot.files.contains_key(input) {
                        bail!("missing input {input}");
                    }
                }
                if remaining
                    .insert(
                        step.id.clone(),
                        PlannedStep {
                            plugin: id.clone(),
                            step,
                        },
                    )
                    .is_some()
                {
                    bail!("duplicate step identity");
                }
            }
        }
        if remaining.len() > 4096 {
            bail!("plan exceeds 4096 steps");
        }
        let mut completed = BTreeSet::new();
        let mut steps = Vec::new();
        while !remaining.is_empty() {
            let next = remaining
                .iter()
                .find(|(_, s)| s.step.after.iter().all(|d| completed.contains(d)))
                .map(|(id, _)| id.clone());
            let Some(next) = next else {
                bail!("missing dependency or cycle in processing plan");
            };
            steps.push(remaining.remove(&next).unwrap());
            completed.insert(next);
        }
        Ok(Plan { steps, snapshot })
    }
    /// A failed/cancelled execution never returns a partial generation. Publication
    /// and watch integration are owned by the embedding host.
    pub fn execute(
        &mut self,
        plan: &Plan,
        cancelled: &std::sync::Arc<AtomicBool>,
    ) -> anyhow::Result<Generation> {
        if cancelled.load(Ordering::SeqCst) {
            bail!("processing cancelled");
        }
        let mut generation = Generation::default();
        for item in &plan.steps {
            if cancelled.load(Ordering::SeqCst) {
                bail!("processing cancelled");
            }
            let plugin = self
                .plugins
                .get(&item.plugin)
                .context("planned plugin is unavailable")?;
            let context = ContextView {
                step: &item.step,
                files: item
                    .step
                    .inputs
                    .iter()
                    .map(|p| (p.clone(), plan.snapshot.files[p].as_slice()))
                    .collect(),
                dependencies: item
                    .step
                    .after
                    .iter()
                    .map(|id| (id.clone(), &generation.outputs[id]))
                    .collect(),
                cancelled,
            };
            let key = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&(
                    &item,
                    &context.files,
                    &context.dependencies
                ))?)
            );
            let output = if let Some(output) = self.cache.get(&key).filter(|_| plugin.cacheable()) {
                generation.cache_hits += 1;
                output.clone()
            } else {
                plugin
                    .process(&context)
                    .with_context(|| format!("process {} via {}", item.step.id, item.plugin))?
            };
            if cancelled.load(Ordering::SeqCst) {
                bail!("processing cancelled");
            }
            if output.len() > 4096 || serde_json::to_vec(&output)?.len() > 16 * 1024 * 1024 {
                bail!("processor output exceeds budget");
            }
            for (name, resource) in &output {
                validate_name(name)?;
                if resource.schema.is_empty() {
                    bail!("output schema is required");
                }
            }
            // Bounded session cache; eviction changes performance, never semantics.
            if self.cache.len() >= 128
                || self
                    .cache
                    .values()
                    .map(|v| serde_json::to_vec(v).map_or(0, |b| b.len()))
                    .sum::<usize>()
                    + serde_json::to_vec(&output)?.len()
                    > 32 * 1024 * 1024
            {
                self.cache.clear();
            }
            if plugin.cacheable() {
                self.cache.insert(key, output.clone());
            }
            generation.outputs.insert(item.step.id.clone(), output);
            if serde_json::to_vec(&generation.outputs)?.len() > 16 * 1024 * 1024 {
                bail!("generation exceeds 16 MiB");
            }
        }
        Ok(generation)
    }
}
fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.contains(['\\', ':'])
        || name
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        bail!("invalid relative identity: {name}");
    }
    Ok(())
}
