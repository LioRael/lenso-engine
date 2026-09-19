//! Closed Host lowering and admission above the shared Plugin Root resolver.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
    path::Path,
};

use anyhow::{Context, bail};
use lenso_app_plan::authoring::{
    HostBinding, HostCatalog, HostDefaultPlugin, HostPluginRelease, HostSlot, PluginDescriptor,
    PluginInstanceId, PluginRootInstance, PluginRootResolutionError, PluginRootSnapshot,
    ResolvedApp, propose_plugin_root, resolve_plugin_root,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod local;
mod policy;
pub use local::LocalPluginInput;
pub use policy::{AdmittedRelease, SlotAdmission};

pub(crate) const HOST_BUILD: &str = ".lenso/host-build.json";
const SCHEMA: &str = "lenso.host-build.v1";
const LOCAL_SCHEMA: &str = "lenso.local-host-build.v1";

/// Generated Host authority. This is a build artifact, never App-owner input.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedHostBuild {
    schema: String,
    host_id: String,
    catalog: HostCatalog,
    admissions: Vec<SlotAdmission>,
}

/// A selected, verified implementation and its authored default Instance.
#[derive(Debug)]
pub struct HostPluginInput {
    pub descriptor: PluginDescriptor,
    pub instance: String,
    pub configuration: Value,
    pub source: String,
}

/// Host-granted candidate set for one App-selectable named dependency.
#[derive(Debug)]
pub struct HostDependencyInput {
    pub consumer: PluginInstanceId,
    pub requirement: String,
    pub providers: Vec<PluginInstanceId>,
    pub default_provider: Option<PluginInstanceId>,
}

impl GeneratedHostBuild {
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// Confirms that one re-verified bundle Descriptor belongs to this immutable Host build.
    pub fn verify_distribution_bundle(
        &self,
        descriptor: &PluginDescriptor,
        manifest_digest: &str,
    ) -> anyhow::Result<()> {
        let default = self
            .catalog
            .plugins()
            .iter()
            .any(|release| release.descriptor() == descriptor);
        let admitted = self.admissions.iter().any(|rule| {
            rule.releases.iter().any(|release| {
                &release.descriptor == descriptor && release.manifest_digest == manifest_digest
            })
        });
        if !default && !admitted {
            bail!(
                "bundle `{}` is not part of this Host build",
                descriptor.plugin_id()
            );
        }
        Ok(())
    }

    /// Lowers closed defaults without rewriting offers or resolving dependencies in TS.
    pub fn lower(
        host_id: &str,
        plugins: Vec<HostPluginInput>,
        explicit_slots: Vec<HostSlot>,
    ) -> anyhow::Result<Self> {
        Self::lower_with_admission(host_id, plugins, explicit_slots, vec![])
    }

    pub fn lower_with_admission(
        host_id: &str,
        plugins: Vec<HostPluginInput>,
        explicit_slots: Vec<HostSlot>,
        admissions: Vec<SlotAdmission>,
    ) -> anyhow::Result<Self> {
        Self::lower_with_dependencies(host_id, plugins, explicit_slots, admissions, vec![])
    }

    pub fn lower_with_dependencies(
        host_id: &str,
        plugins: Vec<HostPluginInput>,
        explicit_slots: Vec<HostSlot>,
        admissions: Vec<SlotAdmission>,
        dependencies: Vec<HostDependencyInput>,
    ) -> anyhow::Result<Self> {
        crate::identity::validate_plugin_id_v1(host_id).context("invalid Host identity")?;
        let mut slots = BTreeMap::new();
        for slot in explicit_slots {
            let id = slot.id().to_owned();
            if slots.insert(id.clone(), slot).is_some() {
                bail!("duplicate explicit Host Slot `{id}`");
            }
        }
        let mut offers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut releases = BTreeMap::new();
        let mut defaults = BTreeMap::new();
        let mut normalized_ids = BTreeMap::new();
        for input in plugins {
            let descriptor = input.descriptor;
            crate::validate_existing_plugin_id(descriptor.plugin_id())?;
            crate::validate_instance_filename(&input.instance)?;
            let default = HostDefaultPlugin::new(descriptor.plugin_id(), &input.instance)
                .with_configuration(input.configuration);
            let id = default.id().clone();
            crate::reject_case_collision(
                &mut normalized_ids,
                &id.to_string(),
                "Host Instance identity",
            )?;
            if let Some(previous) = defaults.insert(id.clone(), default) {
                bail!(
                    "{}: duplicate Host Instance `{}`",
                    input.source,
                    previous.id()
                );
            }
            offers
                .entry(descriptor.root_slot().to_owned())
                .or_default()
                .push(format!("{id} ({})", input.source));
            if let Some(previous) =
                releases.insert(descriptor.plugin_id().to_owned(), descriptor.clone())
                && previous != descriptor
            {
                bail!(
                    "{}: conflicting Releases or implementations for Plugin `{}`",
                    input.source,
                    descriptor.plugin_id()
                );
            }
        }
        for (slot, candidates) in &offers {
            if !slots.contains_key(slot) {
                if candidates.len() != 1 {
                    bail!(
                        "Host Slot `{slot}` has multiple defaults: {}; declare explicit Slot cardinality",
                        candidates.join(", ")
                    );
                }
                slots.insert(slot.clone(), HostSlot::one(slot));
            }
        }
        for slot in slots.keys() {
            if !offers.contains_key(slot)
                && !admissions
                    .iter()
                    .any(|rule| rule.slot == *slot && !rule.releases.is_empty())
            {
                bail!("closed Host Slot `{slot}` has no authored default");
            }
        }
        let bindings = lower_dependency_bindings(dependencies, &defaults, &releases)?;
        let build = Self {
            schema: SCHEMA.to_owned(),
            host_id: host_id.to_owned(),
            catalog: HostCatalog::new(
                slots.into_values(),
                releases.into_values().map(|descriptor| {
                    let replace = admissions.iter().any(|rule| {
                        rule.releases
                            .iter()
                            .any(|release| release.descriptor.plugin_id() == descriptor.plugin_id())
                    });
                    let release = HostPluginRelease::new(descriptor);
                    if replace {
                        release.allow_root_override()
                    } else {
                        release
                    }
                }),
                defaults.into_values(),
            )
            .with_bindings(bindings),
            admissions,
        };
        build.validate()?;
        Ok(build)
    }

    pub const fn catalog(&self) -> &HostCatalog {
        &self.catalog
    }

    /// Rejects malformed/unsupported authority instead of treating it as a legacy Catalog.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema == LOCAL_SCHEMA {
            // Local Hosts may require explicitly adopted shared Instances. The
            // complete Root is validated by resolve/propose after it is loaded.
            self.validate_policy()?;
            return Ok(());
        }
        // The shared resolver remains the sole validator for complete bindings/configuration.
        self.propose(&PluginRootSnapshot::default())
            .context("resolve Host defaults")?;
        Ok(())
    }

    fn validate_policy(&self) -> anyhow::Result<BTreeMap<String, jsonschema::Validator>> {
        let mut validators = BTreeMap::new();
        if self.admissions.len() > 256
            || self.catalog.defaults().len() > 256
            || self.catalog.slots().len() > 256
            || self
                .admissions
                .iter()
                .map(|rule| rule.releases.len())
                .sum::<usize>()
                > 256
        {
            bail!("Host policy exceeds the 256 Instance/Slot/release profile limit");
        }
        if self.schema != SCHEMA && self.schema != LOCAL_SCHEMA {
            bail!("unsupported Host build schema `{}`", self.schema);
        }
        crate::identity::validate_plugin_id_v1(&self.host_id)?;
        if self.schema == SCHEMA
            && self
                .catalog
                .defaults()
                .iter()
                .any(HostDefaultPlugin::is_disableable)
        {
            bail!("Host build cannot contain disableable defaults in this profile");
        }
        let mut slots = BTreeSet::new();
        let mut plugins = BTreeSet::new();
        for rule in &self.admissions {
            if !slots.insert(&rule.slot)
                || !self
                    .catalog
                    .slots()
                    .iter()
                    .any(|slot| slot.id() == rule.slot)
            {
                bail!("duplicate or unknown admission Slot `{}`", rule.slot);
            }
            if rule.max_instances == 0 || rule.max_instances > 256 {
                bail!("Slot `{}` maxInstances must be 1..=256", rule.slot);
            }
            if let Some(schema) = &rule.configuration_schema {
                validators.insert(
                    rule.slot.clone(),
                    policy::compile_ceiling(schema)
                        .with_context(|| format!("Slot `{}`", rule.slot))?,
                );
            }
            for release in &rule.releases {
                let descriptor = &release.descriptor;
                crate::identity::validate_plugin_id_v1(descriptor.plugin_id())?;
                if descriptor.root_slot() != rule.slot || !plugins.insert(descriptor.plugin_id()) {
                    bail!(
                        "admitted Plugin `{}` has a conflicting Slot or multiple release policies",
                        descriptor.plugin_id()
                    );
                }
                if self.catalog.plugins().iter().any(|item| {
                    item.descriptor().plugin_id() == descriptor.plugin_id()
                        && item.descriptor().root_slot() != rule.slot
                }) {
                    bail!("replacement cannot move a Plugin to another Slot");
                }
                let digest = release
                    .manifest_digest
                    .strip_prefix("sha256:")
                    .context("invalid admitted manifest digest")?;
                if digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                {
                    bail!("invalid admitted manifest digest");
                }
            }
        }
        for slot in self
            .catalog
            .slots()
            .iter()
            .filter(|slot| slot.is_replaceable())
        {
            if !self
                .admissions
                .iter()
                .any(|rule| rule.slot == slot.id() && !rule.releases.is_empty())
            {
                bail!(
                    "replaceable Slot `{}` needs an explicit release admission",
                    slot.id()
                );
            }
        }
        Ok(validators)
    }

    pub fn resolve(
        &self,
        snapshot: &PluginRootSnapshot,
    ) -> Result<ResolvedApp, PluginRootResolutionError> {
        self.resolve_with(snapshot, resolve_plugin_root)
    }

    pub fn propose(
        &self,
        snapshot: &PluginRootSnapshot,
    ) -> Result<ResolvedApp, PluginRootResolutionError> {
        self.resolve_with(snapshot, propose_plugin_root)
    }

    fn resolve_with(
        &self,
        snapshot: &PluginRootSnapshot,
        resolver: impl FnOnce(
            &HostCatalog,
            &PluginRootSnapshot,
        ) -> Result<ResolvedApp, PluginRootResolutionError>,
    ) -> Result<ResolvedApp, PluginRootResolutionError> {
        let validators = self.validate_policy().map_err(|error| {
            PluginRootResolutionError::InvalidHostConfiguration(error.to_string())
        })?;
        self.admit(snapshot)?;
        let resolved_app = resolver(&self.catalog, snapshot)?;
        if resolved_app.instances().len() > 256 {
            return Err(denied("Host exceeds 256 active Instances"));
        }
        for rule in &self.admissions {
            let selected = resolved_app
                .instances()
                .iter()
                .filter(|instance| {
                    self.descriptor(snapshot, instance.id().plugin_id())
                        .is_some_and(|descriptor| descriptor.root_slot() == rule.slot)
                })
                .collect::<Vec<_>>();
            if selected.len() > rule.max_instances {
                return Err(denied(format!(
                    "Slot `{}` exceeds maxInstances {}",
                    rule.slot, rule.max_instances
                )));
            }
            if let Some(validator) = validators.get(&rule.slot) {
                for instance in selected {
                    let plan = resolved_app
                        .plan()
                        .plugin_instances()
                        .iter()
                        .find(|plan| plan.instance_key() == instance.plan_key())
                        .ok_or_else(|| denied("missing resolved Instance"))?;
                    let configuration: Value = serde_json::from_str(plan.configuration())
                        .map_err(|_| denied("invalid resolved configuration"))?;
                    if let Err(error) = validator.validate(&configuration) {
                        return Err(denied(format!(
                            "Instance `{}` exceeds configuration ceiling in Slot `{}` at {} (schema {})",
                            instance.id(),
                            rule.slot,
                            error.instance_path(),
                            error.schema_path()
                        )));
                    }
                }
            }
        }
        Ok(resolved_app)
    }

    fn admit(&self, snapshot: &PluginRootSnapshot) -> Result<(), PluginRootResolutionError> {
        let deny = |detail| {
            PluginRootResolutionError::InvalidHostConfiguration(format!(
                "Host admission denied: {detail}; change the Host declaration and rebuild"
            ))
        };
        for release in snapshot.releases() {
            if !self.admissions.iter().any(|rule| {
                rule.releases
                    .iter()
                    .any(|allowed| allowed.descriptor == *release)
            }) {
                return Err(deny(format!(
                    "Root bundle `{}` is not admitted",
                    release.plugin_id()
                )));
            }
        }
        for id in snapshot
            .instances()
            .iter()
            .map(PluginRootInstance::id)
            .chain(snapshot.disabled())
        {
            if !self
                .catalog
                .defaults()
                .iter()
                .any(|default| default.id() == id)
                && !self
                    .descriptor(snapshot, id.plugin_id())
                    .is_some_and(|descriptor| {
                        self.admissions.iter().any(|rule| {
                            rule.releases
                                .iter()
                                .any(|release| release.descriptor == *descriptor)
                        })
                    })
            {
                return Err(deny(format!(
                    "Instance `{id}` is not an exact Host default"
                )));
            }
        }
        Ok(())
    }

    fn descriptor<'a>(
        &'a self,
        snapshot: &'a PluginRootSnapshot,
        plugin_id: &str,
    ) -> Option<&'a PluginDescriptor> {
        snapshot
            .releases()
            .iter()
            .find(|descriptor| descriptor.plugin_id() == plugin_id)
            .or_else(|| {
                self.catalog
                    .plugins()
                    .iter()
                    .map(HostPluginRelease::descriptor)
                    .find(|descriptor| descriptor.plugin_id() == plugin_id)
            })
    }

    fn select_bundle(
        &self,
        verified: &lenso_plugin_bundle::VerifiedBundle,
    ) -> anyhow::Result<PluginDescriptor> {
        // HostInput is validated before scanning or mutating bundles.
        self.admissions
            .iter()
            .flat_map(|rule| &rule.releases)
            .find(|release| {
                release.descriptor.plugin_id() == verified.plugin_id
                    && release.manifest_digest == verified.manifest_digest
            })
            .map(|release| release.descriptor.clone())
            .with_context(|| {
                format!(
                    "Host admission denied: bundle `{}` at `{}` is not an exact admitted release",
                    verified.plugin_id, verified.manifest_digest
                )
            })
    }
}

fn lower_dependency_bindings(
    dependencies: Vec<HostDependencyInput>,
    defaults: &BTreeMap<PluginInstanceId, HostDefaultPlugin>,
    releases: &BTreeMap<String, PluginDescriptor>,
) -> anyhow::Result<Vec<HostBinding>> {
    dependencies
        .into_iter()
        .map(|dependency| {
            crate::validate_existing_plugin_id(dependency.consumer.plugin_id())?;
            crate::validate_instance_filename(dependency.consumer.instance_key())?;
            if dependency.providers.is_empty() {
                bail!(
                    "selectable requirement `{}` needs at least one Host-permitted provider",
                    dependency.requirement
                );
            }
            if !defaults.contains_key(&dependency.consumer) {
                bail!(
                    "selectable requirement `{}` has unknown consumer `{}`",
                    dependency.requirement,
                    dependency.consumer
                );
            }
            let descriptor = releases
                .get(dependency.consumer.plugin_id())
                .context("selectable dependency consumer has no exact Release")?;
            let requirement = descriptor
                .required_capabilities()
                .iter()
                .find(|requirement| requirement.requirement_id() == dependency.requirement)
                .with_context(|| {
                    format!(
                        "Plugin `{}` does not declare requirement `{}`",
                        dependency.consumer.plugin_id(),
                        dependency.requirement
                    )
                })?;
            let mut seen = BTreeSet::new();
            for provider in &dependency.providers {
                crate::validate_existing_plugin_id(provider.plugin_id())?;
                crate::validate_instance_filename(provider.instance_key())?;
                if !defaults.contains_key(provider) || !seen.insert(provider) {
                    bail!(
                        "requirement `{}` contains an unknown or duplicate provider `{provider}`",
                        dependency.requirement
                    );
                }
            }
            if dependency
                .default_provider
                .as_ref()
                .is_some_and(|provider| !seen.contains(provider))
            {
                bail!(
                    "default provider for requirement `{}` is outside its Host-permitted set",
                    dependency.requirement
                );
            }
            Ok(HostBinding::to_instances(
                dependency.consumer,
                requirement.capability_id(),
                dependency.providers,
            )
            .with_requirement_id(dependency.requirement)
            .selectable(dependency.default_provider))
        })
        .collect()
}

fn denied(detail: impl std::fmt::Display) -> PluginRootResolutionError {
    PluginRootResolutionError::InvalidHostConfiguration(format!("Host admission denied: {detail}"))
}

/// Loaded atomically from one authority file; configuration proposals bind this full value.
#[derive(Debug, Serialize)]
pub(crate) enum HostInput {
    Legacy(HostCatalog),
    Generated(GeneratedHostBuild),
}

impl Deref for HostInput {
    type Target = HostCatalog;
    fn deref(&self) -> &HostCatalog {
        match self {
            Self::Legacy(catalog) => catalog,
            Self::Generated(build) => build.catalog(),
        }
    }
}

impl HostInput {
    pub(crate) fn select_bundle(
        &self,
        path: &Path,
        verified: &lenso_plugin_bundle::VerifiedBundle,
    ) -> anyhow::Result<PluginDescriptor> {
        match self {
            Self::Legacy(_) => {
                crate::read_verified_bundle_descriptor(path, &verified.plugin_id, verified)
            }
            Self::Generated(build) => build.select_bundle(verified),
        }
    }
    pub(crate) fn resolve(
        &self,
        snapshot: &PluginRootSnapshot,
    ) -> Result<ResolvedApp, PluginRootResolutionError> {
        match self {
            Self::Legacy(catalog) => resolve_plugin_root(catalog, snapshot),
            Self::Generated(build) => build.resolve(snapshot),
        }
    }

    pub(crate) fn propose(
        &self,
        snapshot: &PluginRootSnapshot,
    ) -> Result<ResolvedApp, PluginRootResolutionError> {
        match self {
            Self::Legacy(catalog) => propose_plugin_root(catalog, snapshot),
            Self::Generated(build) => build.propose(snapshot),
        }
    }
}

#[cfg(test)]
mod admission_tests;
#[cfg(test)]
mod tests;
