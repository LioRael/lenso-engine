use super::*;
use crate::archive::archive_bundle;
use lenso_app_authoring::host_authoring::{GeneratedHostBuild, HostPluginInput};
use lenso_app_plan::{CapabilityEndpointPlan, ExecutionClassId, authoring::PluginContract};
use lenso_plugin_bundle::{
    ImplementationPolicy, RuntimeAdmission, SourcePluginImplementation, SourcePluginReleaseBuild,
    build_source_plugin_release_bundle, read_bundle_manifest, resolve_implementation,
    verify_bundle_directory,
};

fn executable(path: &Path) {
    fs::write(path, "fixture").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn authoring(root: &Path) -> PathBuf {
    let artifact = root.join("plugin.js");
    fs::write(&artifact, "fixture").unwrap();
    let bundle = root.join("bundle");
    build_source_plugin_release_bundle(&SourcePluginReleaseBuild {
        contract: PluginContract::new("company.store", "1.0.0", "store")
            .with_authoring_version(2)
            .with_capability(CapabilityEndpointPlan::new("company.notes", "1", ["get"])),
        implementations: vec![SourcePluginImplementation {
            id: "bun".into(),
            host_targets: vec!["*".into()],
            artifact,
            bundle_path: "implementations/bun/plugin.js".into(),
            media_type: "application/javascript".into(),
            target: "javascript-bun".into(),
            entrypoint: "plugin.js".into(),
            execution_class: ExecutionClassId::bun_child_process(),
            runtime_profile: lenso_app_plan::PLUGIN_AUTHORING_V2_RUNTIME_PROFILE.into(),
        }],
        output: bundle.clone(),
    })
    .unwrap();
    let verified = verify_bundle_directory(&bundle).unwrap();
    let manifest = read_bundle_manifest(&bundle).unwrap();
    let descriptor = resolve_implementation(
        &manifest,
        &ImplementationPolicy {
            host_target: "aarch64-apple-darwin".into(),
            runtimes: vec![RuntimeAdmission {
                execution_class: ExecutionClassId::bun_child_process(),
                runtime_profile: lenso_app_plan::PLUGIN_AUTHORING_V2_RUNTIME_PROFILE.into(),
            }],
        },
    )
    .unwrap()
    .descriptor;
    let build = GeneratedHostBuild::lower(
        "company.app",
        vec![HostPluginInput {
            descriptor,
            instance: "default".into(),
            configuration: serde_json::json!({}),
            source: "fixture".into(),
        }],
        vec![],
    )
    .unwrap();
    let output = root.join("authoring");
    fs::create_dir_all(output.join(".lenso")).unwrap();
    fs::create_dir_all(output.join("bundles")).unwrap();
    fs::write(
        output.join(".lenso/host-build.json"),
        serde_json::to_vec_pretty(&build).unwrap(),
    )
    .unwrap();
    archive_bundle(&bundle, &output.join("bundles/0.lenso-plugin")).unwrap();
    fs::write(
        output.join("bundles.json"),
        serde_json::to_vec_pretty(&serde_json::json!([{
            "path": "bundles/0.lenso-plugin",
            "plugin_id": verified.plugin_id,
            "release_version": verified.release_version,
            "manifest_digest": verified.manifest_digest,
            "execution_class": "lenso.bun-process@1",
            "runtime_profile": "lenso.plugin-authoring@2",
            "target": "aarch64-apple-darwin",
            "implementation_id": "bun",
            "artifact_path": "implementations/bun/plugin.js",
            "artifact_digest": verified.artifact_digests[0],
            "artifact_size": fs::metadata(bundle.join("implementations/bun/plugin.js")).unwrap().len(),
            "artifact_media_type": "application/javascript",
            "artifact_target": "javascript-bun"
        }]))
        .unwrap(),
    )
    .unwrap();
    output
}

#[test]
fn prepares_a_complete_digest_locked_distribution_without_live_state() {
    let temporary = tempfile::tempdir().unwrap();
    let build = authoring(temporary.path());
    let runtime = temporary.path().join("runtime");
    let owner = temporary.path().join("owner");
    let resolver = temporary.path().join("resolver");
    let bun = temporary.path().join("bun");
    for path in [&runtime, &owner, &resolver, &bun] {
        executable(path);
    }
    let notices = temporary.path().join("notices");
    fs::write(&notices, "fixture notices").unwrap();
    let library = temporary.path().join("library");
    fs::create_dir(&library).unwrap();
    for name in ["distribution-host.js", "host-app.js", "host-owner.js"] {
        fs::write(library.join(name), name).unwrap();
    }
    let out = temporary.path().join("distribution");
    prepare(PrepareArgs {
        build,
        target: "aarch64-apple-darwin".into(),
        runtime,
        owner,
        resolver,
        bun: Some(bun),
        notices,
        out: out.clone(),
        library: Some(library),
    })
    .unwrap();
    let lock: serde_json::Value =
        serde_json::from_slice(&fs::read(out.join(".lenso/distribution.lock.json")).unwrap())
            .unwrap();
    assert_eq!(lock["app_id"], "company.app");
    assert_eq!(lock["platform"], "darwin");
    assert!(out.join("runtime/lenso-host-runtime").is_file());
    assert!(out.join("runtime/lenso-process-owner").is_file());
    assert!(out.join("runtime/bun").is_file());
    assert!(out.join("bundles/0.lenso-plugin").is_file());
    assert!(out.join("artifacts/0/plugin.js").is_file());
    assert!(!out.join("plugins").exists());
    assert!(!out.join(".lenso/control-state.json").exists());
    assert!(
        prepare(PrepareArgs {
            build: out.clone(),
            target: "aarch64-apple-darwin".into(),
            runtime: out.join("runtime/lenso-host-runtime"),
            owner: out.join("runtime/lenso-process-owner"),
            resolver: out.join("runtime/lenso-resolver"),
            bun: Some(out.join("runtime/bun")),
            notices: out.join("THIRD_PARTY_NOTICES.txt"),
            out,
            library: None,
        })
        .is_err()
    );
}

#[test]
fn rejects_missing_bun_wrong_targets_and_non_executable_runtime() {
    let temporary = tempfile::tempdir().unwrap();
    let build = authoring(temporary.path());
    let plain = temporary.path().join("plain");
    fs::write(&plain, "plain").unwrap();
    let notices = temporary.path().join("notices");
    fs::write(&notices, "notices").unwrap();
    let args = PrepareArgs {
        build,
        target: "aarch64-apple-darwin".into(),
        runtime: plain.clone(),
        owner: plain.clone(),
        resolver: plain,
        bun: None,
        notices,
        out: temporary.path().join("out"),
        library: Some(temporary.path().into()),
    };
    let error = prepare(args).unwrap_err();
    assert!(format!("{error:#}").contains("supply the exact --bun executable"));
    assert!(!temporary.path().join("out").exists());
    assert!(target_platform("wasm32-wasi").is_err());
}
