//! Explicit, immutable native Host admission. No product-specific factories live here.
use anyhow::{Context, bail};
use lenso_app_authoring::discovery::Candidate;
use lenso_app_plan::authoring::{HostCatalog, PluginDescriptor};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    target: String,
    executable: String,
    sha256: String,
    sources: BTreeMap<String, Source>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    release_version: String,
    input_digest: String,
    #[serde(default)]
    companions: Vec<String>,
}
pub(super) struct Host {
    binary: PathBuf,
    bytes: Vec<u8>,
    manifest: Manifest,
}
impl Host {
    pub(super) fn load(root: &Path) -> anyhow::Result<Option<Self>> {
        let config = root.join("lenso.toml");
        if !config.exists() {
            return Ok(None);
        }
        let config: toml::Value = toml::from_str(&fs::read_to_string(config)?)?;
        let Some(path) = config.get("development_host") else {
            return Ok(None);
        };
        let path = root.join(
            path.as_str()
                .context("development_host must be a local manifest path")?,
        );
        let bytes = fs::read(&path).context("read precompiled development Host manifest")?;
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        if manifest.schema != "lenso.precompiled-host.v1"
            || manifest.target != lenso_app_authoring::native_host_target()
        {
            bail!("incompatible precompiled development Host schema or target");
        }
        let relative = Path::new(&manifest.executable);
        if relative
            .components()
            .any(|p| !matches!(p, std::path::Component::Normal(_)))
            || relative.as_os_str().is_empty()
        {
            bail!("precompiled Host executable must be a contained relative path");
        }
        let directory = fs::canonicalize(path.parent().context("Host manifest directory")?)?;
        let binary = fs::canonicalize(directory.join(relative))?;
        if !binary.starts_with(&directory) || super::local_host::digest(&binary)? != manifest.sha256
        {
            bail!("precompiled Host executable integrity mismatch");
        }
        Ok(Some(Self {
            binary,
            bytes,
            manifest,
        }))
    }
    pub(super) fn admit(&self, candidates: &[Candidate]) -> anyhow::Result<()> {
        for candidate in candidates
            .iter()
            .filter(|c| super::local_host::is_native(c))
        {
            let source = self.manifest.sources.get(&candidate.plugin_id)
                .with_context(|| format!("precompiled Host does not admit native Plugin {}; choose a matching Host or remove development_host to build with Cargo", candidate.plugin_id))?;
            if source.release_version != candidate.release_version
                || source.input_digest != super::local_host::input_digest(&candidate.project)?
            {
                bail!(
                    "precompiled Host source identity mismatch for {}; rebuild the Host package",
                    candidate.plugin_id
                );
            }
        }
        Ok(())
    }
    pub(super) fn install(
        &self,
        stage: &Path,
        candidates: &[Candidate],
    ) -> anyhow::Result<Vec<PluginDescriptor>> {
        self.admit(candidates)?;
        let installed = stage.join(".lenso/host");
        fs::copy(&self.binary, &installed)?;
        if super::local_host::digest(&installed)? != self.manifest.sha256 {
            bail!("precompiled Host changed during copy");
        }
        let output = Command::new(&installed).arg("--describe").output()?;
        if !output.status.success() {
            bail!("precompiled Host catalog probe failed");
        }
        let catalog: HostCatalog = serde_json::from_slice(&output.stdout)?;
        let mut selected = BTreeSet::new();
        for candidate in candidates
            .iter()
            .filter(|c| super::local_host::is_native(c))
        {
            selected.insert(candidate.plugin_id.as_str());
            selected.extend(
                self.manifest.sources[&candidate.plugin_id]
                    .companions
                    .iter()
                    .map(String::as_str),
            );
        }
        let descriptors = catalog
            .plugins()
            .iter()
            .map(|p| p.descriptor())
            .filter(|d| selected.contains(d.plugin_id()))
            .cloned()
            .collect::<Vec<_>>();
        let actual = descriptors
            .iter()
            .map(|d| d.plugin_id())
            .collect::<BTreeSet<_>>();
        if actual != selected || actual.len() != descriptors.len() {
            bail!("precompiled Host catalog does not match selected native Plugins");
        }
        for candidate in candidates
            .iter()
            .filter(|c| super::local_host::is_native(c))
        {
            if descriptors
                .iter()
                .find(|d| d.plugin_id() == candidate.plugin_id)
                .context("native descriptor")?
                .release_version()
                != candidate.release_version
            {
                bail!("precompiled Host release mismatch");
            }
        }
        fs::write(stage.join(".lenso/precompiled-host.json"), &self.bytes)?;
        Ok(descriptors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Prevent substitution of native source/binaries under an unchanged identity.
    // Portable distribution tests do not exercise development Host admission.
    #[test]
    fn pinned_host_rejects_modified_source_binary_and_unadmitted_native() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let package = root.path().join("support");
        fs::create_dir(&package)?;
        fs::write(package.join("Cargo.toml"), "original")?;
        fs::write(root.path().join("host"), "not executed during admission")?;
        fs::write(
            root.path().join("lenso.toml"),
            "development_host = 'host.json'",
        )?;
        let mut candidate = Candidate {
            surface_owner: None,
            composite: None,
            plugin_id: "example.support".into(),
            release_version: "1.0.0".into(),
            project: package.clone(),
            metadata: package.join("Cargo.toml"),
            format: "cargo".into(),
            role: lenso_app_authoring::discovery::SourceRole::Shared,
            implementations: vec![lenso_app_authoring::discovery::Implementation {
                id: "native".into(),
                runtime: "native-linked".into(),
                project: package.clone(),
            }],
            evidence: "test".into(),
        };
        fs::write(
            root.path().join("host.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema":"lenso.precompiled-host.v1", "target":lenso_app_authoring::native_host_target(), "executable":"host",
                "sha256":super::super::local_host::digest(&root.path().join("host"))?,
                "sources":{"example.support":{"release_version":"1.0.0","input_digest":super::super::local_host::input_digest(&package)?}}
            }))?,
        )?;
        let host = Host::load(root.path())?.context("host")?;
        host.admit(&[candidate.clone()])?;
        fs::write(package.join("Cargo.toml"), "changed")?;
        assert!(
            host.admit(&[candidate.clone()])
                .unwrap_err()
                .to_string()
                .contains("source identity mismatch")
        );
        candidate.plugin_id = "example.other".into();
        assert!(
            host.admit(&[candidate])
                .unwrap_err()
                .to_string()
                .contains("does not admit")
        );
        fs::write(root.path().join("host"), "tampered")?;
        assert!(
            matches!(Host::load(root.path()), Err(error) if error.to_string().contains("integrity mismatch"))
        );
        Ok(())
    }
}
