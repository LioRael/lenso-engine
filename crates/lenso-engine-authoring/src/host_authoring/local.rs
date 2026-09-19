//! Versioned local-source Host policy. Does not relax closed TS Host authority.
use super::*;

/// One built, selected implementation from a local source. Shared inputs do not
/// create Instances; only existing Plugin Root intent can select them.
#[derive(Debug)]
pub struct LocalPluginInput {
    pub descriptor: PluginDescriptor,
    pub manifest_digest: String,
    pub app_owned: bool,
    pub source: String,
}

impl GeneratedHostBuild {
    /// Permit saved named choices only among compatible Instances already
    /// declared by this exact Host and Root. Does not discover/adopt providers.
    pub fn with_local_root(mut self, root: &Path) -> anyhow::Result<(Self, ResolvedApp)> {
        if self.schema != LOCAL_SCHEMA {
            bail!("local Root choices require the local Host profile");
        }
        let snapshot = crate::snapshot_plugin_root(root, &HostInput::Generated(self.clone()))?;
        let instances = self
            .catalog
            .defaults()
            .iter()
            .map(|instance| instance.id().clone())
            .chain(
                snapshot
                    .instances()
                    .iter()
                    .map(|instance| instance.id().clone()),
            )
            .collect::<BTreeSet<_>>();
        let mut bindings = Vec::new();
        for consumer in &instances {
            let Some(descriptor) = self.descriptor(&snapshot, consumer.plugin_id()) else {
                continue;
            };
            for requirement in descriptor.required_capabilities() {
                if requirement.cardinality() == lenso_app_plan::CapabilityCardinality::Many {
                    continue;
                }
                let providers = instances
                    .iter()
                    .filter(|provider| {
                        self.descriptor(&snapshot, provider.plugin_id())
                            .is_some_and(|descriptor| {
                                descriptor.provided_capabilities().iter().any(|endpoint| {
                                    endpoint.capability_id() == requirement.capability_id()
                                        && endpoint.descriptor_version()
                                            == requirement.descriptor_version()
                                })
                            })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if !providers.is_empty() {
                    let binding = HostBinding::to_instances(
                        consumer.clone(),
                        requirement.capability_id(),
                        providers,
                    );
                    // Legacy Capability-only requirements have a synthetic `~`
                    // key. Only named roles may persist a selectable Root choice.
                    bindings.push(if requirement.requirement_id().starts_with('~') {
                        binding
                    } else {
                        binding
                            .with_requirement_id(requirement.requirement_id())
                            .selectable(None)
                    });
                }
            }
        }
        self.catalog = self.catalog.with_bindings(bindings);
        let proposed = self
            .propose(&snapshot)
            .context("resolve local Host with explicit Root choices")?;
        Ok((self, proposed))
    }

    /// Generate exact local-source authority without resolving an empty Root:
    /// required dependencies may be supplied by explicitly adopted shared Instances.
    pub fn lower_local(host_id: &str, inputs: Vec<LocalPluginInput>) -> anyhow::Result<Self> {
        crate::identity::validate_plugin_id_v1(host_id)?;
        let mut releases = BTreeMap::new();
        let mut defaults = Vec::new();
        let mut admissions: BTreeMap<String, SlotAdmission> = BTreeMap::new();
        let mut normalized = BTreeMap::new();
        for input in inputs {
            let descriptor = input.descriptor;
            crate::validate_existing_plugin_id(descriptor.plugin_id())?;
            crate::reject_case_collision(
                &mut normalized,
                descriptor.plugin_id(),
                "local Plugin identity",
            )?;
            if releases.contains_key(descriptor.plugin_id()) {
                bail!(
                    "duplicate local Plugin `{}` from {}",
                    descriptor.plugin_id(),
                    input.source
                );
            }
            if input.app_owned {
                defaults
                    .push(HostDefaultPlugin::new(descriptor.plugin_id(), "default").disableable());
            }
            admissions
                .entry(descriptor.root_slot().to_owned())
                .or_insert_with(|| SlotAdmission {
                    slot: descriptor.root_slot().to_owned(),
                    max_instances: 256,
                    releases: Vec::new(),
                    configuration_schema: None,
                })
                .releases
                .push(AdmittedRelease {
                    descriptor: descriptor.clone(),
                    manifest_digest: input.manifest_digest,
                });
            releases.insert(
                descriptor.plugin_id().to_owned(),
                HostPluginRelease::new(descriptor),
            );
        }
        defaults.sort_by(|a, b| a.id().cmp(b.id()));
        for admission in admissions.values_mut() {
            admission
                .releases
                .sort_by(|a, b| a.descriptor.plugin_id().cmp(b.descriptor.plugin_id()));
        }
        let build = Self {
            schema: LOCAL_SCHEMA.to_owned(),
            host_id: host_id.to_owned(),
            catalog: HostCatalog::new(
                admissions.keys().map(HostSlot::many),
                releases.into_values(),
                defaults,
            ),
            admissions: admissions.into_values().collect(),
        };
        build.validate_policy()?;
        Ok(build)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lenso_app_plan::{CapabilityEndpointPlan, CapabilityRequirementPlan};

    fn input(id: &str, app_owned: bool) -> LocalPluginInput {
        LocalPluginInput {
            descriptor: PluginDescriptor::new(id, "1.0.0", "tools"),
            manifest_digest: format!("sha256:{}", "a".repeat(64)),
            app_owned,
            source: id.to_owned(),
        }
    }

    #[test]
    fn local_defaults_are_disableable_and_shared_sources_do_not_activate() {
        let build = GeneratedHostBuild::lower_local(
            "example.app",
            vec![input("example.owned", true), input("example.shared", false)],
        )
        .unwrap();
        let resolved = build.resolve(&PluginRootSnapshot::default()).unwrap();
        assert_eq!(resolved.instances().len(), 1);
        let disabled =
            PluginRootSnapshot::new([], [], [PluginInstanceId::new("example.owned", "default")]);
        assert!(build.resolve(&disabled).unwrap().instances().is_empty());
        let adopted =
            PluginRootSnapshot::new([], [PluginRootInstance::new("example.shared", "named")], []);
        assert_eq!(build.resolve(&adopted).unwrap().instances().len(), 2);
    }

    #[test]
    fn missing_or_disabled_required_provider_fails_without_auto_adoption() {
        let mut consumer = input("example.consumer", true);
        consumer.descriptor = consumer
            .descriptor
            .with_requirement(CapabilityRequirementPlan::one("example.store@1", "1"));
        let mut provider = input("example.provider", false);
        provider.descriptor = provider
            .descriptor
            .with_capability(CapabilityEndpointPlan::new("example.store@1", "1", ["get"]));
        let build =
            GeneratedHostBuild::lower_local("example.app", vec![consumer, provider]).unwrap();
        assert!(build.resolve(&PluginRootSnapshot::default()).is_err());
        let adopted = PluginRootSnapshot::new(
            [],
            [PluginRootInstance::new("example.provider", "default")],
            [],
        );
        assert!(build.resolve(&adopted).is_ok());
    }

    #[test]
    fn capability_only_native_requirements_keep_legacy_fixed_binding_semantics() {
        let root = tempfile::tempdir().unwrap();
        let mut consumer = input("example.consumer", true);
        consumer.descriptor = consumer
            .descriptor
            .with_requirement(CapabilityRequirementPlan::one("example.store@1", "1"));
        let mut provider = input("example.provider", true);
        provider.descriptor = provider
            .descriptor
            .with_capability(CapabilityEndpointPlan::new("example.store@1", "1", ["get"]));
        let (_, resolved) =
            GeneratedHostBuild::lower_local("example.app", vec![consumer, provider])
                .unwrap()
                .with_local_root(root.path())
                .unwrap();
        assert_eq!(resolved.plan().capability_bindings().len(), 1);
        assert!(resolved.dependency_choices().is_empty());
    }

    #[test]
    fn local_policy_does_not_change_closed_profile_or_admit_unknown_instances() {
        let local =
            GeneratedHostBuild::lower_local("example.app", vec![input("example.owned", true)])
                .unwrap();
        let unknown = PluginRootSnapshot::new(
            [],
            [PluginRootInstance::new("example.unknown", "default")],
            [],
        );
        assert!(local.resolve(&unknown).is_err());
        let mut bytes = serde_json::to_value(local).unwrap();
        bytes["schema"] = SCHEMA.into();
        let closed: GeneratedHostBuild = serde_json::from_value(bytes).unwrap();
        assert!(closed.validate().is_err());
    }
}
