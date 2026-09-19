use std::{fs, path::PathBuf, process::Command};

use anyhow::{Context, bail};
use clap::Args;
use lenso_app_authoring::host_authoring::{
    AdmittedRelease, GeneratedHostBuild, HostDependencyInput, HostPluginInput, SlotAdmission,
};
use lenso_app_plan::{
    ExecutionClassId,
    authoring::{HostSlot, PluginInstanceId},
};
use lenso_plugin_bundle::{
    ImplementationPolicy, RuntimeAdmission, read_bundle_manifest, resolve_implementation,
    verify_bundle_directory,
};
use serde::Deserialize;
use serde_json::Value;

use crate::archive::{archive_bundle, with_bundle_directory};

#[derive(Args, Clone, Debug)]
pub struct HostBuildArgs {
    /// Static TypeScript Host entrypoint. Does not execute application code.
    #[arg(long)]
    pub(super) source: PathBuf,
    /// Exact implementation target, as used by the Plugin bundles.
    #[arg(long)]
    pub(super) target: String,
    /// New authoring output directory. Existing output is never overwritten.
    #[arg(long)]
    pub(super) out: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    id: String,
    plugins: Vec<Instance>,
    #[serde(default)]
    slots: Vec<Slot>,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Instance {
    Bare(Reference),
    Named(NamedInstance),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedInstance {
    plugin: Reference,
    instance: String,
    #[serde(default = "empty_configuration")]
    configuration: Value,
}

fn empty_configuration() -> Value {
    serde_json::json!({})
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    bundle: PathBuf,
    execution: Execution,
    source: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Execution {
    Bun,
    Process,
}

impl Execution {
    fn class(self) -> ExecutionClassId {
        ExecutionClassId::new(match self {
            Self::Bun => "lenso.bun-process@1",
            Self::Process => "lenso.process@1",
        })
    }

    fn admissions(self) -> Vec<RuntimeAdmission> {
        match self {
            Self::Bun => vec![
                lenso_app_plan::PLUGIN_AUTHORING_V2_RUNTIME_PROFILE,
                "lenso.bun-process@1",
            ],
            Self::Process => vec!["lenso.process-stdio@2", "lenso.process@1"],
        }
        .into_iter()
        .map(|runtime_profile| RuntimeAdmission {
            execution_class: self.class(),
            runtime_profile: runtime_profile.to_owned(),
        })
        .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Slot {
    id: String,
    cardinality: Cardinality,
    #[serde(default)]
    replaceable: bool,
    max_instances: Option<usize>,
    #[serde(default)]
    allow: Vec<Reference>,
    configuration_schema: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Cardinality {
    One,
    Many,
    Optional,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceIdentity {
    plugin: String,
    #[serde(default = "default_instance")]
    instance: String,
}

fn default_instance() -> String {
    "default".to_owned()
}

impl InstanceIdentity {
    fn into_core(self) -> PluginInstanceId {
        PluginInstanceId::new(self.plugin, self.instance)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dependency {
    consumer: InstanceIdentity,
    requirement: String,
    allow: Vec<InstanceIdentity>,
    default: Option<InstanceIdentity>,
}

pub fn build(args: &HostBuildArgs) -> anyhow::Result<()> {
    let source = fs::canonicalize(&args.source).context("locate TS Host source")?;
    let extractor = std::env::var_os("LENSO_HOST_EXTRACTOR").context(
        "TS Host builds need the npm CLI compiler; invoke lenso through @lenso/cli (Node or Bun)",
    )?;
    let javascript = std::env::var_os("LENSO_HOST_JS_RUNTIME")
        .context("TS Host build JavaScript runtime was not supplied by the npm CLI")?;
    let output = Command::new(javascript)
        .arg(extractor)
        .arg(&source)
        .output()
        .context("extract static TS Host declaration")?;
    if !output.status.success() {
        bail!(
            "TS Host declaration rejected: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.len() > 8 * 1024 * 1024 {
        bail!("extracted Host declaration exceeds 8 MiB");
    }
    let declaration: Declaration = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("{}: invalid Host declaration", source.display()))?;
    materialize(declaration, args)
}

#[expect(
    clippy::too_many_lines,
    reason = "keeps one atomic Host authoring publication"
)]
fn materialize(declaration: Declaration, args: &HostBuildArgs) -> anyhow::Result<()> {
    if declaration.plugins.len() > 256
        || declaration.slots.len() > 256
        || declaration.dependencies.len() > 256
    {
        bail!(
            "first Host authoring profile accepts at most 256 Instances, Slots, and dependency rules"
        );
    }
    if args.target.trim().is_empty() {
        bail!("Host implementation target must not be empty");
    }
    let destination = std::path::absolute(&args.out)?;
    if fs::symlink_metadata(&destination).is_ok() {
        bail!("Host output already exists: {}", destination.display());
    }
    let parent = destination
        .parent()
        .context("Host output needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".lenso-host-")
        .tempdir_in(parent)?;
    fs::create_dir(stage.path().join(".lenso"))?;
    fs::create_dir(stage.path().join("bundles"))?;
    let mut work = Vec::new();
    for instance in declaration.plugins {
        let (reference, instance, configuration) = match instance {
            Instance::Bare(reference) => (reference, "default".to_owned(), empty_configuration()),
            Instance::Named(NamedInstance {
                plugin,
                instance,
                configuration,
            }) => (plugin, instance, configuration),
        };
        work.push((reference, Some((instance, configuration)), None));
    }
    let mut slots = Vec::new();
    let mut admissions = Vec::new();
    for slot in declaration.slots {
        if !slot.allow.is_empty() && slot.max_instances.is_none() {
            bail!(
                "extensible Slot `{}` requires an explicit maxInstances",
                slot.id
            );
        }
        let max_instances = slot.max_instances.unwrap_or(match slot.cardinality {
            Cardinality::Many => 256,
            _ => 1,
        });
        let core_slot = match slot.cardinality {
            Cardinality::One => HostSlot::one(&slot.id),
            Cardinality::Many => HostSlot::many(&slot.id),
            Cardinality::Optional => HostSlot::optional(&slot.id),
        };
        slots.push(if slot.replaceable {
            core_slot.replaceable()
        } else {
            core_slot
        });
        let policy_index = admissions.len();
        admissions.push(SlotAdmission {
            slot: slot.id,
            max_instances,
            releases: vec![],
            configuration_schema: slot.configuration_schema,
        });
        for reference in slot.allow {
            work.push((reference, None, Some(policy_index)));
        }
    }
    if work.len() > 256 {
        bail!("Host build accepts at most 256 default/admitted bundle references");
    }
    let mut inputs = Vec::new();
    let mut inventory = Vec::new();
    for (index, (reference, instance, policy_index)) in work.into_iter().enumerate() {
        let archive_path = format!("bundles/{index}.lenso-plugin");
        // Copy first and verify the immutable staged bytes, so source mutation cannot
        // mix a checked Descriptor with a different packaged implementation.
        with_bundle_directory(&reference.bundle, |directory| {
            archive_bundle(directory, &stage.path().join(&archive_path))
        })
        .with_context(|| {
            format!(
                "{}: stage bundle {}",
                reference.source,
                reference.bundle.display()
            )
        })?;
        let (verified, selected) =
            with_bundle_directory(&stage.path().join(&archive_path), |directory| {
                let verified = verify_bundle_directory(directory)?;
                let manifest = read_bundle_manifest(directory)?;
                let selected = resolve_implementation(
                    &manifest,
                    &ImplementationPolicy {
                        host_target: args.target.clone(),
                        runtimes: reference.execution.admissions(),
                    },
                )?;
                if selected
                    .descriptor
                    .provided_capabilities()
                    .iter()
                    .any(|endpoint| {
                        !endpoint.stream_operations().is_empty()
                            || !endpoint.event_operations().is_empty()
                    })
                {
                    bail!("first TS Host authoring profile supports Request Capabilities only");
                }
                Ok((verified, selected))
            })
            .with_context(|| {
                format!(
                    "{}: verify/select bundle {}",
                    reference.source,
                    reference.bundle.display()
                )
            })?;
        inventory.push(serde_json::json!({
            "path": archive_path, "plugin_id": verified.plugin_id,
            "release_version": verified.release_version, "manifest_digest": verified.manifest_digest,
            "execution_class": reference.execution.class(), "target": args.target,
            "runtime_profile": selected.descriptor.runtime_profile(),
            "implementation_id": selected.implementation_id,
            "artifact_path": selected.artifact.path,
            "artifact_digest": selected.artifact.digest,
            "artifact_size": selected.artifact.size,
            "artifact_media_type": selected.artifact.media_type,
            "artifact_target": selected.artifact.target,
        }));
        let descriptor = selected.descriptor;
        if let Some((instance, configuration)) = instance {
            inputs.push(HostPluginInput {
                descriptor,
                instance,
                configuration,
                source: reference.source,
            });
        } else if let Some(policy_index) = policy_index {
            admissions[policy_index].releases.push(AdmittedRelease {
                descriptor,
                manifest_digest: verified.manifest_digest,
            });
        }
    }
    let dependencies = declaration
        .dependencies
        .into_iter()
        .map(|dependency| HostDependencyInput {
            consumer: dependency.consumer.into_core(),
            requirement: dependency.requirement,
            providers: dependency
                .allow
                .into_iter()
                .map(InstanceIdentity::into_core)
                .collect(),
            default_provider: dependency.default.map(InstanceIdentity::into_core),
        })
        .collect();
    let build = GeneratedHostBuild::lower_with_dependencies(
        &declaration.id,
        inputs,
        slots,
        admissions,
        dependencies,
    )?;
    fs::write(
        stage.path().join(".lenso/host-build.json"),
        serde_json::to_vec_pretty(&build)?,
    )?;
    fs::write(
        stage.path().join("bundles.json"),
        serde_json::to_vec_pretty(&inventory)?,
    )?;
    let proposed = build.propose(&lenso_app_plan::authoring::PluginRootSnapshot::default())?;
    if !proposed.dependency_choices().is_empty() {
        fs::create_dir(stage.path().join("plugins"))?;
        fs::write(stage.path().join(".lenso/plugin-root-authoring.lock"), [])?;
        let document = lenso_app_authoring::DependencySelectionsDocument {
            schema_version: lenso_app_authoring::DEPENDENCY_SELECTIONS_SCHEMA_VERSION,
            choices: proposed.dependency_choices().to_vec(),
        };
        fs::write(
            stage.path().join("plugins/.dependencies.json"),
            serde_json::to_vec_pretty(&document)?,
        )?;
    }
    // Validate through exactly the same loader used by check/show/configure/install.
    let resolved = lenso_app_authoring::load_resolved_app(stage.path())?;
    publish_new_output(stage.path(), &destination).context("publish new Host authoring output")?;
    println!(
        "Built Host authoring output at {}: {} Instance(s), {} binding(s). Runtime assembly is not included.",
        destination.display(),
        resolved.instances().len(),
        resolved.plan().capability_bindings().len()
    );
    Ok(())
}

pub(super) fn publish_new_output(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> anyhow::Result<()> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE)?;
    }
    #[cfg(windows)]
    fs::rename(source, destination)?; // Windows rejects an existing destination directory.
    #[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
    bail!(
        "atomic Host output publication is unsupported on this platform: {} -> {}",
        source.display(),
        destination.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests;
