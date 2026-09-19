//! Atomic source-to-Host authoring; runtime startup remains a separate operation.
use crate::archive::{archive_bundle, with_bundle_directory};
use anyhow::{Context, bail};
use clap::Args;
use lenso_app_authoring::{
    discovery::{SourceRole, discover},
    host_authoring::{GeneratedHostBuild, LocalPluginInput},
};
use lenso_app_plan::ExecutionClassId;
use lenso_plugin_bundle::{
    ImplementationPolicy, RuntimeAdmission, read_bundle_manifest, resolve_implementation,
    verify_bundle_directory,
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Args)]
pub struct AssembleArgs {
    /// Source App root. Defaults to the current directory.
    #[arg(long)]
    pub(super) root: Option<PathBuf>,
    /// Host identity recorded in the generated build.
    #[arg(long, default_value = "local.app")]
    pub(super) id: String,
    /// New output directory; existing output is never overwritten.
    #[arg(long)]
    pub(super) out: PathBuf,
    /// Emit a machine-readable receipt.
    #[arg(long)]
    pub(super) json: bool,
    /// Also compile an executable Host with native-linked Plugins and typed codecs.
    #[arg(long)]
    pub(super) executable: bool,
}

pub fn assemble(args: AssembleArgs) -> anyhow::Result<()> {
    let root = crate::plugins::project_root(args.root)?;
    let report = discover(&root)?;
    let convention_plan = lenso_app_authoring::discovery::conventions::plan(&report)?;
    let selection_bytes = serde_json::to_vec(&convention_plan)?;
    let destination = std::path::absolute(&args.out)?;
    if fs::symlink_metadata(&destination).is_ok() {
        bail!("Host output already exists: {}", destination.display());
    }
    let parent = destination
        .parent()
        .context("Host output needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".lenso-local-host-")
        .tempdir_in(parent)?;
    fs::create_dir(stage.path().join(".lenso"))?;
    fs::write(stage.path().join(".lenso/plugin-root-authoring.lock"), [])?;
    fs::create_dir(stage.path().join("bundles"))?;
    fs::write(
        stage.path().join(".lenso/conventions.json"),
        serde_json::to_vec_pretty(&convention_plan)?,
    )?;
    let root_intent = root.join("plugins");
    if root_intent.try_exists()? {
        copy_root(&root_intent, &stage.path().join("plugins"), 0, &mut 0)?;
    }
    let mut inputs = Vec::new();
    let mut inventory = Vec::new();
    let mut sources = Vec::new();
    let generated_sources = tempfile::tempdir().context("stage convention sources")?;
    let mut candidates = convention_plan
        .candidates
        .iter()
        .filter(|candidate| {
            candidate.role != SourceRole::Shared
                || candidate.surface_owner.is_some()
                || stage
                    .path()
                    .join("plugins")
                    .join(&candidate.plugin_id)
                    .exists()
        })
        .cloned()
        .collect::<Vec<_>>();
    let precompiled = super::precompiled::Host::load(&root)?;
    if let Some(host) = &precompiled {
        host.admit(&candidates)?;
    }
    let source_contracts = candidates
        .iter()
        .filter(|c| precompiled.is_none() || !super::local_host::is_native(c))
        .cloned()
        .collect::<Vec<_>>();
    super::contracts::synchronize(&root, &source_contracts)?;
    let convention_inputs = convention_plan
        .compilations
        .iter()
        .flat_map(|compilation| [&compilation.owner_project, &compilation.compiler_project])
        .map(|path| Ok((path.clone(), super::local_host::input_digest(path)?)))
        .collect::<anyhow::Result<std::collections::BTreeMap<_, _>>>()?;
    let generated_candidates =
        super::convention_build::compile(&convention_plan, generated_sources.path())?;
    if let Some(host) = &precompiled {
        host.admit(&generated_candidates)?;
    }
    super::contracts::synchronize(&root, &generated_candidates)?;
    candidates.extend(generated_candidates);
    let source_digests = candidates
        .iter()
        .map(|candidate| {
            Ok((
                candidate.plugin_id.clone(),
                super::local_host::input_digest(&candidate.project)?,
            ))
        })
        .collect::<anyhow::Result<std::collections::BTreeMap<_, _>>>()?;
    let executable = args.executable || candidates.iter().any(super::local_host::is_native);
    if executable {
        super::prepare::target_platform(lenso_app_authoring::native_host_target())?;
    }
    let native = if candidates.iter().any(super::local_host::is_native) {
        if let Some(host) = &precompiled {
            host.install(stage.path(), &candidates)?
        } else {
            super::local_host::generate(stage.path(), &root.join(".lenso/host-cache"), &candidates)?
        }
    } else {
        Vec::new()
    };
    if let Some(ingress) = native.iter().find(|d| d.plugin_id() == "lenso.web-ingress") {
        let mut web_enabled = false;
        for candidate in &candidates {
            if lenso_app_authoring::discovery::conventions::active_instances(&root, candidate)? > 0
                && native.iter().any(|descriptor| {
                    descriptor.plugin_id() == candidate.plugin_id
                        && descriptor.provided_capabilities().iter().any(|provided| {
                            ingress.required_capabilities().iter().any(|required| {
                                required.capability_id() == provided.capability_id()
                            })
                        })
                })
            {
                web_enabled = true;
            }
        }
        inputs.push(LocalPluginInput {
            descriptor: ingress.clone(),
            manifest_digest: super::local_host::digest(&stage.path().join(".lenso/host"))?,
            app_owned: web_enabled,
            source: "lenso.local-host@1 Web ingress".into(),
        });
    }
    let mut runtime_artifacts = Vec::new();
    let mut runtime_codecs = std::collections::BTreeMap::new();
    for candidate in candidates {
        super::preset::checkpoint()?;
        // Shared sources are not built merely because they can be discovered.
        // A Root directory is intent to validate, not implicit enablement.
        if candidate.role == SourceRole::Shared
            && candidate.surface_owner.is_none()
            && !stage
                .path()
                .join("plugins")
                .join(&candidate.plugin_id)
                .try_exists()?
        {
            continue;
        }
        if super::local_host::is_native(&candidate) {
            let descriptor = native
                .iter()
                .find(|d| d.plugin_id() == candidate.plugin_id)
                .context("selected native Plugin is absent from linked registry")?
                .clone();
            if descriptor.release_version() != candidate.release_version {
                bail!(
                    "native source identity changed during build: {}",
                    candidate.project.display()
                );
            }
            inputs.push(LocalPluginInput {
                descriptor,
                manifest_digest: super::local_host::digest(&stage.path().join(".lenso/host"))?,
                app_owned: candidate.role == SourceRole::AppOwned
                    || candidate.surface_owner.is_some(),
                source: candidate.project.display().to_string(),
            });
            sources.push(candidate);
            continue;
        }
        if inputs.len() >= 256 {
            bail!("local Host accepts at most 256 selected Plugin sources");
        }
        let archive_path = format!("bundles/{}.lenso-plugin", candidate.plugin_id);
        let archive = stage.path().join(&archive_path);
        if candidate.format == "bundle" {
            with_bundle_directory(&candidate.project, |directory| {
                archive_bundle(directory, &archive)
            })?;
        } else {
            let build = tempfile::tempdir().context("stage local Plugin build")?;
            let bundle = build.path().join("bundle");
            crate::plugin::materialize(
                &candidate.project,
                &bundle,
                crate::plugin::BuildProfile::Release,
            )
            .with_context(|| format!("build local Plugin {}", candidate.plugin_id))?;
            archive_bundle(&bundle, &archive)?;
        }
        let (verified, selected) = with_bundle_directory(&archive, |directory| {
            Ok((
                verify_bundle_directory(directory)?,
                resolve_implementation(
                    &read_bundle_manifest(directory)?,
                    &local_implementation_policy(),
                )?,
            ))
        })?;
        if verified.plugin_id != candidate.plugin_id
            || verified.release_version != candidate.release_version
        {
            bail!(
                "local source identity changed during build: {}",
                candidate.project.display()
            );
        }
        if executable {
            let path = format!("runtime/artifacts/{}", candidate.plugin_id);
            fs::create_dir_all(stage.path().join("runtime/artifacts"))?;
            with_bundle_directory(&archive, |directory| {
                fs::copy(
                    directory.join(&selected.artifact.path),
                    stage.path().join(&path),
                )?;
                Ok(())
            })?;
            if let Some(evidence) = crate::plugin::local_runtime_descriptor(
                &stage.path().join(&path),
                selected.descriptor.execution_class().as_str(),
            )? {
                runtime_codecs.insert(candidate.plugin_id.clone(), evidence);
            }
            runtime_artifacts.push(json!({"plugin_id": candidate.plugin_id, "path":path,
                "digest":selected.artifact.digest,"size":selected.artifact.size,
                "execution_class":selected.descriptor.execution_class().as_str()}));
        }
        let descriptor = selected.descriptor;
        inventory.push(json!({
            "path": archive_path, "plugin_id": verified.plugin_id,
            "release_version": verified.release_version, "manifest_digest": verified.manifest_digest,
            "execution_class": descriptor.execution_class().as_str(), "runtime_profile": descriptor.runtime_profile(),
            "target": lenso_app_authoring::native_host_target(), "implementation_id": selected.implementation_id,
            "artifact_path": selected.artifact.path, "artifact_digest": selected.artifact.digest,
            "artifact_size": selected.artifact.size, "artifact_media_type": selected.artifact.media_type,
            "artifact_target": selected.artifact.target,
        }));
        sources.push(candidate.clone());
        inputs.push(LocalPluginInput {
            descriptor,
            manifest_digest: verified.manifest_digest,
            app_owned: candidate.role == SourceRole::AppOwned || candidate.surface_owner.is_some(),
            source: candidate.project.display().to_string(),
        });
    }
    let (authority, proposed) =
        GeneratedHostBuild::lower_local(&args.id, inputs)?.with_local_root(stage.path())?;
    if !proposed.dependency_choices().is_empty() {
        fs::create_dir_all(stage.path().join("plugins"))?;
        let legacy = stage.path().join("plugins/dependencies.json");
        if legacy.try_exists()? {
            fs::remove_file(legacy)?;
        }
        let document = lenso_app_authoring::DependencySelectionsDocument {
            schema_version: lenso_app_authoring::DEPENDENCY_SELECTIONS_SCHEMA_VERSION,
            choices: proposed.dependency_choices().to_vec(),
        };
        fs::write(
            stage.path().join("plugins/.dependencies.json"),
            serde_json::to_vec_pretty(&document)?,
        )?;
    }
    fs::write(
        stage.path().join(".lenso/host-build.json"),
        serde_json::to_vec_pretty(&authority)?,
    )?;
    fs::write(
        stage.path().join("bundles.json"),
        serde_json::to_vec_pretty(&inventory)?,
    )?;
    let fresh = lenso_app_authoring::discovery::conventions::plan(&discover(&root)?)?;
    if serde_json::to_vec(&fresh)? != selection_bytes {
        bail!("local convention selection changed during build; retry");
    }
    let resolved = lenso_app_authoring::load_resolved_app(stage.path())
        .context("resolve local Host with Plugin Root intent")?;
    for (path, digest) in &convention_inputs {
        if *digest != super::local_host::input_digest(path)? {
            bail!("convention source changed during build; retry");
        }
    }
    for source in &sources {
        if source_digests[&source.plugin_id] != super::local_host::input_digest(&source.project)? {
            bail!(
                "source changed during build: {}; retry after edits settle",
                source.project.display()
            );
        }
    }
    fs::write(
        stage.path().join("local-sources.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": "lenso.local-sources.v1", "template": "lenso.local-host@1",
            "cli_version": env!("CARGO_PKG_VERSION"), "target": lenso_app_authoring::native_host_target(),
            "sources": sources, "source_digests":source_digests,
        }))?,
    )?;
    if executable {
        fs::write(
            stage.path().join("runtime-codecs.json"),
            serde_json::to_vec_pretty(&runtime_codecs)?,
        )?;
        if native.is_empty() {
            super::portable_runtime::validate(resolved.plan(), &runtime_codecs)?;
        }
        super::local_host::finalize(stage.path(), runtime_artifacts)?;
    }
    super::preset::checkpoint()?;
    super::build::publish_new_output(stage.path(), &destination)?;
    if args.json {
        println!(
            "{}",
            json!({"schema_version":1, "kind":"lenso.app-assemble", "out":destination,
            "plugin_instances": resolved.instances().len(), "capability_bindings": resolved.plan().capability_bindings().len(), "executable":executable})
        );
    } else {
        println!(
            "Assembled {} Plugin Instances at {}.",
            resolved.instances().len(),
            destination.display()
        );
    }
    Ok(())
}

fn local_implementation_policy() -> ImplementationPolicy {
    ImplementationPolicy {
        host_target: lenso_app_authoring::native_host_target().to_owned(),
        runtimes: [
            ("lenso.process@1", "lenso.process-stdio@2"),
            ("lenso.process@1", "lenso.process@1"),
            ("lenso.wasm-component@1", "lenso.wasm-component@1"),
            (
                "lenso.bun-process@1",
                lenso_bun_adapter::BUN_AUTHORING_RUNTIME_PROFILE,
            ),
            (
                "lenso.bun-process@1",
                lenso_app_plan::PLUGIN_AUTHORING_V2_RUNTIME_PROFILE,
            ),
            ("lenso.bun-process@1", "lenso.bun-process@1"),
        ]
        .into_iter()
        .map(|(class, profile)| RuntimeAdmission {
            execution_class: ExecutionClassId::new(class),
            runtime_profile: profile.to_owned(),
        })
        .collect(),
    }
}

pub(super) fn copy_root(
    source: &Path,
    destination: &Path,
    depth: usize,
    bytes: &mut u64,
) -> anyhow::Result<()> {
    if depth > 32 {
        bail!("Plugin Root exceeds 32 directory levels");
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_root(
                &entry.path(),
                &destination.join(entry.file_name()),
                depth + 1,
                bytes,
            )?;
        }
    } else if metadata.is_file() {
        *bytes += metadata.len();
        if *bytes > 256 * 1024 * 1024 {
            bail!("Plugin Root snapshot exceeds 256 MiB");
        }
        fs::copy(source, destination)?;
    } else {
        bail!(
            "Plugin Root contains a symlink or special file: {}",
            source.display()
        );
    }
    Ok(())
}
