use super::*;
use lenso_app_plan::authoring::{PluginInstanceId, PluginRootInstance};
use serde_json::json;

fn descriptor(id: &str) -> PluginDescriptor {
    PluginDescriptor::new(id, "1.0.0", "store")
        .with_configuration_schema(json!({"type":"object"}))
        .with_configuration_defaults(json!({"limit": 4, "network": {"hosts": []}}))
}

fn policy(release: PluginDescriptor) -> SlotAdmission {
    SlotAdmission {
        slot: "store".into(),
        max_instances: 1,
        releases: vec![AdmittedRelease {
            descriptor: release,
            manifest_digest: format!("sha256:{}", "a".repeat(64)),
        }],
        configuration_schema: Some(json!({"type":"object", "properties": {
            "limit": {"type":"integer", "maximum":8},
            "network": {"type":"object", "properties": {"hosts": {"type":"array", "items":{"enum":["api.example.com"]}}}}
        }})),
    }
}

fn snapshot(descriptor: &PluginDescriptor, configurations: &[(&str, Value)]) -> PluginRootSnapshot {
    PluginRootSnapshot::new(
        [descriptor.clone()],
        configurations.iter().map(|(name, configuration)| {
            PluginRootInstance::new(descriptor.plugin_id(), *name)
                .with_configuration(configuration.clone())
        }),
        [],
    )
}

#[test]
fn extension_is_inactive_until_enabled_and_counts_only_selected_instances() {
    let release = descriptor("company.store");
    let host = GeneratedHostBuild::lower_with_admission(
        "company.app",
        vec![],
        vec![HostSlot::many("store")],
        vec![policy(release.clone())],
    )
    .unwrap();
    assert_eq!(
        host.resolve(&snapshot(&release, &[]))
            .unwrap()
            .instances()
            .len(),
        0
    );
    assert_eq!(
        host.resolve(&snapshot(&release, &[("a", json!({}))]))
            .unwrap()
            .instances()
            .len(),
        1
    );
    assert!(
        host.resolve(&snapshot(&release, &[("a", json!({})), ("b", json!({}))]))
            .unwrap_err()
            .to_string()
            .contains("maxInstances")
    );
    let disabled = PluginRootSnapshot::new(
        [release],
        [
            PluginRootInstance::new("company.store", "a"),
            PluginRootInstance::new("company.store", "b"),
        ],
        [PluginInstanceId::new("company.store", "b")],
    );
    assert_eq!(host.resolve(&disabled).unwrap().instances().len(), 1);
}

#[test]
fn ceilings_check_merged_values_and_never_echo_sensitive_values() {
    let release = descriptor("company.store");
    let host = GeneratedHostBuild::lower_with_admission(
        "company.app",
        vec![],
        vec![HostSlot::many("store")],
        vec![policy(release.clone())],
    )
    .unwrap();
    assert!(
        host.resolve(&snapshot(
            &release,
            &[(
                "a",
                json!({"limit":8,"network":{"hosts":["api.example.com"]}})
            )]
        ))
        .is_ok()
    );
    for configuration in [
        json!({"limit":9}),
        json!({"network":{"hosts":["do-not-echo-this-secret"]}}),
    ] {
        let error = host
            .resolve(&snapshot(&release, &[("a", configuration)]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("configuration ceiling"));
        assert!(!error.contains("do-not-echo-this-secret"));
    }
    let unsafe_defaults = release.with_configuration_defaults(json!({"limit": 20}));
    let host = GeneratedHostBuild::lower_with_admission(
        "company.app",
        vec![],
        vec![HostSlot::many("store")],
        vec![policy(unsafe_defaults.clone())],
    )
    .unwrap();
    assert!(
        host.resolve(&snapshot(&unsafe_defaults, &[("a", json!({}))]))
            .is_err()
    );
    assert!(
        host.resolve(&snapshot(&unsafe_defaults, &[("a", json!({"limit":4}))]))
            .is_ok()
    );
}

#[test]
fn explicit_replacement_uses_the_shared_resolver_without_counting_the_retired_default() {
    let default = || HostPluginInput {
        descriptor: descriptor("company.original"),
        instance: "default".into(),
        configuration: json!({}),
        source: "app.ts:1".into(),
    };
    let replacement = descriptor("company.replacement");
    let candidate = snapshot(&replacement, &[("default", json!({}))]);
    let closed = GeneratedHostBuild::lower_with_admission(
        "company.app",
        vec![default()],
        vec![HostSlot::one("store")],
        vec![policy(replacement.clone())],
    )
    .unwrap();
    assert!(closed.resolve(&candidate).is_err());
    let open = GeneratedHostBuild::lower_with_admission(
        "company.app",
        vec![default()],
        vec![HostSlot::one("store").replaceable()],
        vec![policy(replacement)],
    )
    .unwrap();
    let resolved = open.resolve(&candidate).unwrap();
    assert_eq!(resolved.instances().len(), 1);
    assert_eq!(
        resolved.instances()[0].id().plugin_id(),
        "company.replacement"
    );
}

#[test]
fn malformed_and_remote_schemas_fail_even_in_an_inactive_branch() {
    for schema in [
        json!({"maximun":8}),
        json!({"if":false,"then":{"$ref":"https://example.com/schema"}}),
        json!({"type":"not-a-type"}),
    ] {
        let mut rule = policy(descriptor("company.store"));
        rule.configuration_schema = Some(schema);
        assert!(
            GeneratedHostBuild::lower_with_admission(
                "company.app",
                vec![],
                vec![HostSlot::many("store")],
                vec![rule]
            )
            .is_err()
        );
    }
}

#[test]
fn changed_ceiling_invalidates_a_reviewed_configuration_proposal() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".lenso")).unwrap();
    let mut host = GeneratedHostBuild::lower_with_admission(
        "company.app",
        vec![HostPluginInput {
            descriptor: descriptor("company.store"),
            instance: "default".into(),
            configuration: json!({}),
            source: "app.ts:1".into(),
        }],
        vec![HostSlot::one("store")],
        vec![policy(descriptor("company.store"))],
    )
    .unwrap();
    let publish = |host: &GeneratedHostBuild| {
        std::fs::write(
            root.path().join(HOST_BUILD),
            serde_json::to_vec(host).unwrap(),
        )
        .unwrap();
    };
    publish(&host);
    let revision = crate::inspect_plugin_root(root.path())
        .unwrap()
        .revision()
        .clone();
    let proposal = crate::propose_instance_configuration(
        root.path(),
        &revision,
        "company.store",
        "default",
        b"limit = 8\n",
    )
    .unwrap();
    host.admissions[0].configuration_schema = Some(json!({"properties":{"limit":{"maximum":5}}}));
    publish(&host);
    assert!(crate::publish_instance_configuration(root.path(), &proposal).is_err());
    assert!(
        !root
            .path()
            .join("plugins/company.store/default.toml")
            .exists()
    );
}
