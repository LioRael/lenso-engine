use super::*;
use lenso_app_plan::authoring::{PluginInstanceId, PluginRootInstance};
use lenso_app_plan::{CapabilityEndpointPlan, CapabilityRequirementPlan};
use std::fs;

fn input(plugin: &str, slot: &str) -> HostPluginInput {
    HostPluginInput {
        descriptor: PluginDescriptor::new(plugin, "1.0.0", slot),
        instance: "default".to_owned(),
        configuration: serde_json::json!({}),
        source: format!("{plugin}.ts:2:1"),
    }
}

fn build() -> GeneratedHostBuild {
    GeneratedHostBuild::lower("company.app", vec![input("company.store", "store")], vec![]).unwrap()
}

fn root(build: &GeneratedHostBuild) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".lenso")).unwrap();
    fs::write(
        root.path().join(HOST_BUILD),
        serde_json::to_vec(build).unwrap(),
    )
    .unwrap();
    root
}

#[test]
fn closed_host_preserves_unique_binding_and_identity_independent_of_order() {
    let mut store = input("company.store", "store");
    store.descriptor = store
        .descriptor
        .with_capability(CapabilityEndpointPlan::new("company.notes", "1", ["get"]));
    let mut notes = input("company.notes", "notes");
    notes.descriptor = notes
        .descriptor
        .with_requirement(CapabilityRequirementPlan::one("company.notes", "1"));
    let build = GeneratedHostBuild::lower("company.app", vec![notes, store], vec![]).unwrap();
    let resolved = build.resolve(&PluginRootSnapshot::default()).unwrap();
    assert_eq!(resolved.instances().len(), 2);
    assert_eq!(resolved.plan().capability_bindings().len(), 1);
    assert!(
        resolved
            .instances()
            .iter()
            .any(|item| item.id().to_string() == "company.store/default")
    );

    let a = GeneratedHostBuild::lower(
        "company.app",
        vec![input("company.a", "a"), input("company.b", "b")],
        vec![],
    )
    .unwrap();
    let b = GeneratedHostBuild::lower(
        "company.app",
        vec![input("company.b", "b"), input("company.a", "a")],
        vec![],
    )
    .unwrap();
    assert_eq!(
        serde_json::to_vec(&a).unwrap(),
        serde_json::to_vec(&b).unwrap()
    );
}

#[test]
fn closed_host_rejects_duplicates_and_requires_explicit_shared_slot() {
    let duplicate = GeneratedHostBuild::lower(
        "company.app",
        vec![input("company.a", "a"), input("company.a", "a")],
        vec![],
    )
    .unwrap_err();
    assert!(duplicate.to_string().contains("duplicate Host Instance"));
    let shared = GeneratedHostBuild::lower(
        "company.app",
        vec![input("company.a", "shared"), input("company.b", "shared")],
        vec![],
    )
    .unwrap_err();
    assert!(shared.to_string().contains("explicit Slot cardinality"));
    let explicit = GeneratedHostBuild::lower(
        "company.app",
        vec![input("company.a", "shared"), input("company.b", "shared")],
        vec![HostSlot::many("shared")],
    )
    .unwrap();
    assert_eq!(
        explicit
            .resolve(&PluginRootSnapshot::default())
            .unwrap()
            .instances()
            .len(),
        2
    );
}

#[test]
fn closed_host_does_not_choose_an_ambiguous_capability() {
    let provider = |id, slot| {
        let mut value = input(id, slot);
        value.descriptor = value
            .descriptor
            .with_capability(CapabilityEndpointPlan::new("company.notes", "1", ["get"]));
        value
    };
    let mut notes = input("company.notes", "notes");
    notes.descriptor = notes
        .descriptor
        .with_requirement(CapabilityRequirementPlan::one("company.notes", "1"));
    let error = GeneratedHostBuild::lower(
        "company.app",
        vec![
            notes,
            provider("company.a", "a"),
            provider("company.b", "b"),
        ],
        vec![],
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("multiple providers"));
}

#[test]
fn closed_host_denies_inactive_bundles_new_instances_and_disablement() {
    let build = build();
    for snapshot in [
        PluginRootSnapshot::new(
            [PluginDescriptor::new("company.extra", "1.0.0", "store")],
            [],
            [],
        ),
        PluginRootSnapshot::new([], [PluginRootInstance::new("company.store", "extra")], []),
        PluginRootSnapshot::new([], [], [PluginInstanceId::new("company.store", "extra")]),
        PluginRootSnapshot::new([], [], [PluginInstanceId::new("company.store", "default")]),
    ] {
        assert!(build.resolve(&snapshot).is_err());
    }
}

#[test]
fn closed_host_configuration_and_management_share_admission_without_writes_on_failure() {
    let mut store = input("company.store", "store");
    store.descriptor = store.descriptor.with_configuration_schema(serde_json::json!({
        "type": "object", "properties": {"limit": {"type": "integer"}}, "additionalProperties": false
    }));
    let root = root(&GeneratedHostBuild::lower("company.app", vec![store], vec![]).unwrap());
    crate::configure_instance(root.path(), "company.store", "default", b"limit = 8\n").unwrap();
    let config = root.path().join("plugins/company.store/default.toml");
    assert!(crate::configure_instance(root.path(), "company.store", "other", b"").is_err());
    assert!(
        !root
            .path()
            .join("plugins/company.store/other.toml")
            .exists()
    );
    assert!(
        crate::configure_instance(
            root.path(),
            "company.store",
            "default",
            b"filesystem = '/'\n"
        )
        .is_err()
    );
    assert_eq!(fs::read(&config).unwrap(), b"limit = 8\n");
    assert!(crate::set_instance_disabled(root.path(), "company.store", "default", true).is_err());
    assert!(!config.with_extension("disabled").exists());
    fs::write(root.path().join("plugins/company.store/other.toml"), "").unwrap();
    assert!(crate::load_resolved_app(root.path()).is_err());
    assert!(crate::inspect_plugin_root(root.path()).is_err());
}

#[test]
fn closed_host_does_not_fall_back_from_invalid_or_competing_authority() {
    let mut unsupported = serde_json::to_value(build()).unwrap();
    unsupported["schema"] = serde_json::json!("future-version");
    let unsupported: GeneratedHostBuild = serde_json::from_value(unsupported).unwrap();
    assert!(unsupported.resolve(&PluginRootSnapshot::default()).is_err());
    let root = root(&build());
    fs::write(
        root.path().join(crate::HOST_CATALOG),
        serde_json::to_vec(&HostCatalog::default()).unwrap(),
    )
    .unwrap();
    assert!(
        crate::load_resolved_app(root.path())
            .unwrap_err()
            .to_string()
            .contains("competing")
    );
    fs::remove_file(root.path().join(crate::HOST_CATALOG)).unwrap();
    fs::write(root.path().join(HOST_BUILD), b"{}").unwrap();
    assert!(crate::load_resolved_app(root.path()).is_err());
}
