//! Explicit local processor manifests bootstrap extensions without running their
//! source conventions. The process protocol is independent of source language.
use crate::{
    ContextView, Plugin, Resource, Snapshot, Step,
    process::{self, ProcessSpec},
};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, atomic::AtomicI32},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: String,
    pub identity: String,
    pub entries: Vec<String>,
    pub program: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// Explicit predecessor step IDs; {path} expands to the matched input.
    #[serde(default)]
    pub after: Vec<String>,
}
#[derive(Debug)]
pub struct ProcessPlugin {
    manifest: Manifest,
    spec: ProcessSpec,
    proofs: Vec<crate::bootstrap::Proof>,
}
impl ProcessPlugin {
    pub fn load_locked(locked: &crate::bootstrap::LockedPlugin) -> anyhow::Result<Self> {
        for proof in &locked.proofs {
            proof.verify()?;
        }
        let mut plugin = Self::load(&locked.manifest)?;
        if plugin.manifest.identity != locked.identity {
            bail!("locked plugin identity mismatch");
        }
        plugin.spec.program = locked.executable.to_string_lossy().into_owned();
        plugin.proofs = locked.proofs.clone();
        Ok(plugin)
    }
    /// Explicitly adopted manifests only; discovery never executes a processor.
    /// Programs are trusted tools, not sandboxed by this API.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if std::fs::metadata(path)?.len() > 16384 {
            bail!("processor manifest exceeds 16 KiB");
        }
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(path)?)?;
        if manifest.schema != "lenso.engine-plugin.v1" || manifest.identity.is_empty() {
            bail!("invalid engine plugin manifest");
        }
        if manifest.entries.is_empty() {
            bail!("processor must declare entry patterns");
        }
        for entry in &manifest.entries {
            glob::Pattern::new(entry)?;
        }
        let directory = path
            .canonicalize()?
            .parent()
            .context("manifest has no parent")?
            .to_owned();
        let spec = ProcessSpec {
            program: manifest.program.clone(),
            args: manifest.args.clone(),
            directory,
        };
        Ok(Self {
            manifest,
            spec,
            proofs: vec![],
        })
    }
}
impl Plugin for ProcessPlugin {
    fn identity(&self) -> &str {
        &self.manifest.identity
    }
    fn plan(&self, snapshot: &Snapshot) -> anyhow::Result<Vec<Step>> {
        let patterns = self
            .manifest
            .entries
            .iter()
            .map(|p| glob::Pattern::new(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(snapshot
            .files()
            .keys()
            .filter(|path| patterns.iter().any(|p| p.matches(path)))
            .map(|path| Step {
                id: format!("{}/{}", self.identity(), path),
                inputs: vec![path.clone()],
                after: self
                    .manifest
                    .after
                    .iter()
                    .map(|id| id.replace("{path}", path))
                    .collect(),
                options: serde_json::Value::Null,
            })
            .collect())
    }
    fn process(&self, context: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>> {
        for proof in &self.proofs {
            proof.verify()?;
        }
        let request = serde_json::json!({"schema":"lenso.engine-process.v1", "step":context.step, "files":context.files, "dependencies":context.dependencies});
        let response = process::execute_cancellable(
            &self.spec,
            &request,
            Arc::new(AtomicI32::new(0)),
            context.cancelled,
        )?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Response {
            schema: String,
            outputs: BTreeMap<String, Resource>,
        }
        let response: Response = serde_json::from_slice(&response)?;
        if response.schema != "lenso.engine-processed.v1" {
            bail!("unsupported processing response");
        }
        for proof in &self.proofs {
            proof.verify()?;
        }
        Ok(response.outputs)
    }
}
