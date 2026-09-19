use super::scaffold::{
    bun_plugin_scaffold, create, multi_plugin_scaffold, plugin_scaffold, process_plugin_scaffold,
    web_plugin_scaffold,
};
use super::*;

#[test]
fn bun_descriptor_lowers_named_dependencies_into_the_plugin_contract() {
    let descriptor = parse_descriptor_bytes(
        br#"{
            "abi":"lenso.json-host-imports@2",
            "configuration_schema":{"type":"object","required":["prefix"]},
            "capabilities":[{
                "capability_id":"company.notes@1",
                "descriptor_version":"1.0.0",
                "request_operations":["list"]
            }],
            "required_capabilities":[{
                "requirement_id":"store",
                "capability_id":"company.notes-store@1",
                "descriptor_version":"1.0.0",
                "cardinality":"one"
            }]
        }"#,
    )
    .unwrap();
    let package = BunPackage {
        version: "1.0.0".to_owned(),
        metadata: BunPackageMetadata {
            source: None,
            plugin_id: "company.notes".to_owned(),
            root_slot: "notes".to_owned(),
            runtime: "bun".to_owned(),
        },
    };

    let contract = contract_from_bun_descriptor(&package, &descriptor).unwrap();
    let requirement = &contract.required_capabilities()[0];

    assert_eq!(
        contract.configuration_schema(),
        Some(&serde_json::json!({"type":"object","required":["prefix"]}))
    );
    assert_eq!(requirement.requirement_id(), "store");
    assert_eq!(requirement.capability_id(), "company.notes-store@1");
}

#[test]
fn bun_descriptor_accepts_a_providerless_lifecycle_plugin() {
    let descriptor = parse_descriptor_bytes(
        br#"{
            "abi":"lenso.json-request@1",
            "capabilities":[],
            "required_capabilities":[{
                "requirement_id":"store",
                "capability_id":"company.notes-store@1",
                "descriptor_version":"1.0.0",
                "cardinality":"one"
            }]
        }"#,
    )
    .unwrap();

    assert!(descriptor.capabilities.is_empty());
    assert_eq!(descriptor.required_capabilities[0].requirement_id, "store");
}

#[test]
fn bun_descriptor_rejects_duplicate_providers() {
    let error = parse_descriptor_bytes(
        br#"{
            "abi":"lenso.json-request@1",
            "capabilities":[
                {"capability_id":"company.notes@1","descriptor_version":"1.0.0","request_operations":["read"]},
                {"capability_id":"company.notes@1","descriptor_version":"1.0.0","request_operations":["write"]}
            ]
        }"#,
    )
    .unwrap_err();

    assert!(error.to_string().contains("repeats provided Capability"));
}

#[test]
fn web_plugin_scaffold_uses_canonical_endpoint_authoring() {
    let files = web_plugin_scaffold("company.greetings-http");
    let manifest = files.get(Path::new("Cargo.toml")).unwrap();
    let source = files.get(Path::new("src/lib.rs")).unwrap();
    let readme = files.get(Path::new("README.md")).unwrap();

    assert!(manifest.contains("plugin-id = \"company.greetings-http\""));
    assert!(manifest.contains("root-slot = \"web\""));
    assert!(manifest.contains("lenso-capability-http-endpoint"));
    assert!(manifest.contains("version = \"0.2.8\""));
    assert!(source.contains("#[lenso::plugin]"));
    assert!(source.contains("#[endpoint]"));
    assert!(source.contains("#[query("));
    assert!(source.contains("Result<(StatusCode, Json<Greeting>), Problem>"));
    assert!(source.contains("EndpointTest"));
    assert!(source.contains("pub const fn link()"));
    assert!(!source.contains("NativeModuleFactory"));
    assert!(!readme.contains("lenso plugin pack"));
    assert!(readme.contains("lenso plugin dev"));
}

#[test]
fn web_plugin_new_writes_the_complete_project() {
    let root = tempfile::tempdir().unwrap();
    create(PluginNewArgs {
        plugin_id: "company.greetings-http".to_owned(),
        repo_root: Some(root.path().to_path_buf()),
        dir: None,
        runtime: PluginRuntimeArg::Multi,
        web: true,
        no_install: true,
        dry_run: false,
    })
    .unwrap();

    let project = root.path().join("company.greetings-http");
    for path in ["Cargo.toml", "src/lib.rs", "README.md"] {
        assert!(project.join(path).is_file(), "missing generated {path}");
    }
}

#[test]
#[ignore = "clean-room test downloads pinned Web dependencies and runs generated tests"]
fn clean_room_web_plugin_runs_generated_tests() {
    let root = tempfile::tempdir().unwrap();
    create(PluginNewArgs {
        plugin_id: "company.greetings-http".to_owned(),
        repo_root: Some(root.path().to_path_buf()),
        dir: None,
        runtime: PluginRuntimeArg::Multi,
        web: true,
        no_install: false,
        dry_run: false,
    })
    .unwrap();
}

#[test]
#[ignore = "clean-room test downloads pinned Web dependencies and builds the generated dev Host"]
fn clean_room_web_plugin_builds_generated_dev_host() {
    let root = tempfile::tempdir().unwrap();
    create(PluginNewArgs {
        plugin_id: "company.greetings-http".to_owned(),
        repo_root: Some(root.path().to_path_buf()),
        dir: None,
        runtime: PluginRuntimeArg::Multi,
        web: true,
        no_install: false,
        dry_run: false,
    })
    .unwrap();
    let project = root.path().join("company.greetings-http");
    let package = read_package(&project.join("Cargo.toml")).unwrap();
    let host = super::web_dev::DevHost::prepare(&project, &package).unwrap();
    host.build().unwrap();
}

#[test]
fn rust_plugin_scaffold_exposes_only_portable_authoring() {
    let files = plugin_scaffold("uppercase");
    let author_source = files.get(Path::new("src/lib.rs")).unwrap();
    let all = files.values().cloned().collect::<String>();

    assert!(author_source.contains("#[lenso::plugin]"));
    assert!(author_source.contains("#[lenso_agent_tool_sdk::tool_provider]"));
    assert!(author_source.contains("#[tool("));
    assert!(author_source.contains("fn execute(arguments: Arguments)"));
    assert!(all.contains("plugin-id = \"uppercase\""));
    assert!(all.contains("root-slot = \"tool-providers\""));
    assert!(all.contains("lenso plugin new"));
    assert!(all.contains("lenso plugin dev"));
    assert!(all.contains("lenso plugin check"));
    assert!(all.contains("lenso plugin pack"));
    for internal in [
        "wit_bindgen",
        "guest_request_plugin",
        "ProcessPlugin",
        "ProcessOutcome",
        "request_json",
        "arguments_json",
        "lenso.agent.tool-provider",
        "lenso.generated",
    ] {
        assert!(
            !author_source.contains(internal),
            "author source leaked `{internal}`"
        );
    }
    for removed in [
        "src/plugin.rs",
        "src/lenso.generated.rs",
        "src/lenso.wasm.generated.rs",
        "src/lenso.process.generated.rs",
        "lenso.generated.descriptor.json",
        "wit/world.wit",
    ] {
        assert!(
            !files.contains_key(Path::new(removed)),
            "unexpected `{removed}`"
        );
    }
}

#[test]
fn bun_plugin_scaffold_uses_generic_and_product_owned_declarations() {
    let files = bun_plugin_scaffold("example.echo");
    let package = files.get(Path::new("package.json")).unwrap();
    let author = files.get(Path::new("src/plugin.ts")).unwrap();

    assert!(package.contains("\"runtime\": \"bun\""));
    assert!(package.contains("\"@lenso/bun-plugin\": \"0.2.2\""));
    assert!(package.contains("\"@lenso/agent-tool-sdk\": \"0.1.0\""));
    assert!(author.contains("tools(["));
    assert!(author.contains("schema.object"));
    assert!(author.contains("definePlugin"));
    assert!(!author.contains("serve("));
    for generated in [
        "src/lenso.bun.generated.ts",
        "src/lenso.describe.generated.ts",
        "src/lenso.invoke.generated.ts",
    ] {
        assert!(!files.contains_key(Path::new(generated)));
    }
}

#[test]
fn process_plugin_scaffold_uses_the_sdk_owned_lowering() {
    let files = process_plugin_scaffold("uppercase");
    let manifest = files.get(Path::new("Cargo.toml")).unwrap();
    let entrypoint = files.get(Path::new("src/main.rs")).unwrap();

    assert!(manifest.contains("runtime = \"process\""));
    assert!(manifest.contains("package = \"lenso-plugin-sdk\", version = \"0.4.1\""));
    assert!(!manifest.contains("lenso-runtime-rust\""));
    assert!(manifest.contains("lenso-agent-tool-sdk"));
    assert!(!manifest.contains("github.com/LioRael/lenso-agent"));
    assert_eq!(
        entrypoint,
        "// Cargo Process entrypoint; the SDK supplies main and protocol lowering.\ninclude!(\"lib.rs\");\n"
    );
    assert!(!files.contains_key(Path::new("lenso.generated.descriptor.json")));
}

#[test]
fn multi_scaffold_keeps_one_business_source_for_two_outputs() {
    let files = multi_plugin_scaffold("uppercase");
    let manifest = files.get(Path::new("Cargo.toml")).unwrap();

    assert!(manifest.contains("outputs = [\"wasm\", \"process\"]"));
    assert!(files.contains_key(Path::new("src/lib.rs")));
    assert!(files.contains_key(Path::new("src/main.rs")));
    assert_eq!(
        files
            .keys()
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
            .count(),
        2
    );
    let author_source = files.get(Path::new("src/lib.rs")).unwrap();
    for runtime_detail in ["wit_bindgen", "ProcessPlugin", "ProcessOutcome", "Guest"] {
        assert!(
            !author_source.contains(runtime_detail),
            "author source leaked runtime detail `{runtime_detail}`"
        );
    }
}

#[test]
fn cargo_project_can_declare_rust_and_typescript_implementations() {
    let root = tempfile::tempdir().unwrap();
    let manifest = root.path().join("Cargo.toml");
    fs::write(
        &manifest,
        r#"[package]
name = "document-sync"
version = "0.1.0"
[package.metadata.lenso]
plugin-id = "example.document-sync"
root-slot = "document-sync"
[package.metadata.lenso-cli]
implementations = [
  { id = "rust-process", path = ".", runtime = "process" },
  { id = "typescript-bun", path = "typescript", runtime = "bun" },
]
"#,
    )
    .unwrap();

    let package = read_package(&manifest).unwrap();
    assert_eq!(
        project_runtime(&package).unwrap(),
        ProjectRuntime::Composite
    );
    let implementations = &package.metadata.lenso_cli.unwrap().implementations;
    assert_eq!(implementations[0].id, "rust-process");
    assert_eq!(implementations[1].path, Path::new("typescript"));
}

#[test]
fn process_artifacts_use_the_canonical_rust_host_target() {
    assert_eq!(
        rust_host_target(Path::new(".")).unwrap(),
        native_host_target()
    );
}

#[test]
fn duplicate_plugin_identity_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let manifest = root.path().join("Cargo.toml");
    fs::write(
        &manifest,
        r#"[package]
name = "duplicate"
version = "0.1.0"
[package.metadata.lenso]
plugin-id = "first"
plugin-id = "second"
"#,
    )
    .unwrap();

    assert!(read_package(&manifest).is_err());
}

#[test]
fn multi_dev_auto_selects_only_the_fast_process_path() {
    assert_eq!(
        resolve_dev_selection(ProjectRuntime::Multi, DevImplementationArg::Auto).unwrap(),
        DevSelection {
            build: DevBuild::Process,
            invoke: ProjectRuntime::Process,
        }
    );
    assert_eq!(
        resolve_dev_selection(ProjectRuntime::Multi, DevImplementationArg::Wasm).unwrap(),
        DevSelection {
            build: DevBuild::Wasm,
            invoke: ProjectRuntime::Wasm,
        }
    );
    assert_eq!(
        resolve_dev_selection(ProjectRuntime::Multi, DevImplementationArg::All).unwrap(),
        DevSelection {
            build: DevBuild::All,
            invoke: ProjectRuntime::Process,
        }
    );
}

#[test]
fn dev_rejects_an_implementation_the_project_does_not_declare() {
    assert!(resolve_dev_selection(ProjectRuntime::Wasm, DevImplementationArg::Process).is_err());
    assert!(resolve_dev_selection(ProjectRuntime::Process, DevImplementationArg::Wasm).is_err());
}

#[test]
fn malformed_plugin_package_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("lenso-plugin.json"), b"{}\n").unwrap();

    assert!(verify_bundle_directory(root.path()).is_err());
}

#[tokio::test]
#[ignore = "clean-room test downloads released crates and compiles wasm32"]
async fn clean_room_plugin_runs_new_check_dev_and_pack() {
    let root = tempfile::tempdir().unwrap();
    create(PluginNewArgs {
        plugin_id: "company.uppercase".to_owned(),
        repo_root: Some(root.path().to_path_buf()),
        dir: None,
        runtime: PluginRuntimeArg::Wasm,
        web: false,
        no_install: false,
        dry_run: false,
    })
    .unwrap();
    let project = root.path().join("company.uppercase");
    check(PluginCheckArgs {
        repo_root: Some(project.clone()),
        json: true,
    })
    .unwrap();
    dev::run(PluginDevArgs {
        repo_root: Some(project.clone()),
        operation: Some("execute".to_owned()),
        request_json: r#"{"name":"company.uppercase","arguments_json":"{\"text\":\"hello\"}"}"#
            .to_owned(),
        config_json: "{}".to_owned(),
        json: true,
        watch: false,
        implementation: DevImplementationArg::Auto,
    })
    .await
    .unwrap();
    let output = project.join("dist/company.uppercase.lenso-plugin");
    pack(PluginPackArgs {
        repo_root: Some(project.clone()),
        output: Some(output.clone()),
        json: true,
    })
    .unwrap();
    with_bundle_directory(&output, |directory| {
        verify_bundle_directory(directory)
            .map(|_| ())
            .map_err(Into::into)
    })
    .unwrap();
    assert!(
        pack(PluginPackArgs {
            repo_root: Some(project),
            output: Some(output),
            json: false,
        })
        .is_err()
    );
}

#[tokio::test]
#[ignore = "clean-room test downloads git dependencies and compiles both scaffold outputs"]
async fn clean_room_multi_plugin_auto_dev_runs_the_process_build() {
    let root = tempfile::tempdir().unwrap();
    create(PluginNewArgs {
        plugin_id: "company.multi-smoke".to_owned(),
        repo_root: Some(root.path().to_path_buf()),
        dir: None,
        runtime: PluginRuntimeArg::Multi,
        web: false,
        no_install: false,
        dry_run: false,
    })
    .unwrap();
    dev::run(PluginDevArgs {
        repo_root: Some(root.path().join("company.multi-smoke")),
        operation: Some("execute".to_owned()),
        request_json:
            r#"{"name":"company.multi-smoke","arguments_json":"{\"text\":\"auto-process\"}"}"#
                .to_owned(),
        config_json: "{}".to_owned(),
        json: true,
        watch: false,
        implementation: DevImplementationArg::Auto,
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "clean-room test downloads git dependencies and compiles a native executable"]
async fn clean_room_process_plugin_runs_new_check_dev_and_pack() {
    let root = tempfile::tempdir().unwrap();
    create(PluginNewArgs {
        plugin_id: "company.uppercase".to_owned(),
        repo_root: Some(root.path().to_path_buf()),
        dir: None,
        runtime: PluginRuntimeArg::Process,
        web: false,
        no_install: false,
        dry_run: false,
    })
    .unwrap();
    let project = root.path().join("company.uppercase");
    check(PluginCheckArgs {
        repo_root: Some(project.clone()),
        json: true,
    })
    .unwrap();
    dev::run(PluginDevArgs {
        repo_root: Some(project.clone()),
        operation: Some("execute".to_owned()),
        request_json: r#"{"name":"company.uppercase","arguments_json":"{\"text\":\"hello\"}"}"#
            .to_owned(),
        config_json: "{}".to_owned(),
        json: true,
        watch: false,
        implementation: DevImplementationArg::Auto,
    })
    .await
    .unwrap();
    let output = project.join("dist/company.uppercase.lenso-plugin");
    pack(PluginPackArgs {
        repo_root: Some(project),
        output: Some(output.clone()),
        json: true,
    })
    .unwrap();
    with_bundle_directory(&output, |directory| {
        verify_bundle_directory(directory)
            .map(|_| ())
            .map_err(Into::into)
    })
    .unwrap();
}

#[test]
fn dependency_free_implementations_share_the_same_authoring_contract() {
    let legacy =
        lenso_app_plan::authoring::PluginContract::new("dev.fixture.echo", "1.0.0", "tools");
    let modern = legacy.clone().with_authoring_version(2);
    assert_eq!(
        super::shared_portable_contract(legacy.clone(), modern.clone()).unwrap(),
        modern
    );
    assert_eq!(
        super::shared_portable_contract(legacy.clone(), legacy.clone()).unwrap(),
        legacy
    );
    let different = modern
        .clone()
        .with_capability(lenso_app_plan::CapabilityEndpointPlan::new(
            "dev.fixture.other@1",
            "1.0.0",
            ["echo"],
        ));
    assert!(super::shared_portable_contract(legacy.clone(), different).is_err());
    let legacy_dependency = legacy.with_requirement(
        lenso_app_plan::CapabilityRequirementPlan::one("dev.fixture.store@1", "1.0.0"),
    );
    let modern_dependency = legacy_dependency.clone().with_authoring_version(2);
    assert!(super::shared_portable_contract(legacy_dependency, modern_dependency).is_err());
}

// The CLI runs under Tokio; Bun startup has its own synchronous RPC runtime.
// This regression must exercise real startup and invocation, not only lowering.
#[tokio::test]
#[ignore = "clean-room test installs Bun dependencies and invokes a Tool Provider"]
async fn clean_room_bun_tool_dev_does_not_nest_tokio_runtimes() {
    let root = tempfile::tempdir().unwrap();
    create(PluginNewArgs {
        plugin_id: "example.echo".into(),
        repo_root: Some(root.path().to_path_buf()),
        dir: None,
        runtime: PluginRuntimeArg::Bun,
        web: false,
        no_install: false,
        dry_run: false,
    })
    .unwrap();
    dev::run(PluginDevArgs {
        repo_root: Some(root.path().join("example.echo")),
        operation: Some("execute".into()),
        request_json: r#"{"name":"example.echo","arguments_json":"{\"text\":\"hello\"}"}"#.into(),
        config_json: "{}".into(),
        json: true,
        watch: false,
        implementation: DevImplementationArg::Bun,
    })
    .await
    .unwrap();
}
