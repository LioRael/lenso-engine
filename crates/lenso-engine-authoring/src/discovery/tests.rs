use super::*;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn rust(root: &Path, path: &str, id: &str) {
    write(
        root,
        &format!("{path}/Cargo.toml"),
        &format!(
            r#"
[package]
name = "fixture"
version = "1.0.0"
[package.metadata.lenso]
plugin-id = "{id}"
root-slot = "tools"
[package.metadata.lenso-cli]
outputs = ["wasm", "process"]
"#
        ),
    );
}

fn bun(root: &Path, path: &str, id: &str) {
    write(root, &format!("{path}/package.json"), &serde_json::json!({
        "name": "fixture", "version": "1.0.0", "lenso": {"pluginId": id, "rootSlot": "tools", "runtime": "bun"},
        "scripts": {"prepare": "exit 99"}
    }).to_string());
}

#[test]
fn discovers_multiple_languages_without_configuration_or_executing_code() {
    let root = tempfile::tempdir().unwrap();
    rust(root.path(), "app/z", "example.rust");
    bun(root.path(), "app/a", "example.bun");
    let report = discover(root.path()).unwrap();
    assert_eq!(
        report
            .candidates
            .iter()
            .map(|candidate| candidate.plugin_id.as_str())
            .collect::<Vec<_>>(),
        ["example.bun", "example.rust"]
    );
    assert!(
        report
            .candidates
            .iter()
            .all(|candidate| candidate.role == SourceRole::AppOwned)
    );
    assert_eq!(report.candidates[1].implementations.len(), 2);
    assert!(!root.path().join(".lenso").exists());
    assert!(!root.path().join("plugins").exists());
}

#[test]
fn optional_shared_globs_are_relative_to_app_and_remain_candidates() {
    let root = tempfile::tempdir().unwrap();
    rust(root.path(), "shared/a", "example.shared");
    write(
        root.path(),
        "project/lenso.toml",
        "plugin_sources = [\"../shared/*\", \"../shared/a\"]",
    );
    let report = discover(&root.path().join("project")).unwrap();
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].role, SourceRole::Shared);
}

#[test]
fn excludes_outputs_dependency_trees_and_plugin_root() {
    let root = tempfile::tempdir().unwrap();
    for directory in ["target", "node_modules", "dist", ".git", "plugins"] {
        rust(
            root.path(),
            &format!("app/{directory}/bad"),
            "example.duplicate",
        );
    }
    assert!(discover(root.path()).unwrap().candidates.is_empty());
}

#[test]
fn rejects_identity_collisions_instead_of_choosing_by_order() {
    let root = tempfile::tempdir().unwrap();
    rust(root.path(), "app/a", "example.same");
    bun(root.path(), "app/b", "example.same");
    let error = format!("{:#}", discover(root.path()).unwrap_err());
    assert!(error.contains("duplicate Plugin identity"));
    assert!(error.contains("app/a") && error.contains("app/b"));
}

#[test]
fn rejects_unknown_configuration_missing_roots_and_remote_sources() {
    let root = tempfile::tempdir().unwrap();
    for text in [
        "preset = 'web'",
        "plugin_sources = ['missing']",
        "plugin_sources = ['missing/*']",
        "plugin_sources = ['https://example.com']",
    ] {
        write(root.path(), "lenso.toml", text);
        assert!(discover(root.path()).is_err(), "{text}");
    }
}

#[test]
fn discovers_native_web_and_inherited_workspace_version() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "app/Cargo.toml",
        "[workspace]\nmembers = ['web']\n[workspace.package]\nversion = '2.0.0'",
    );
    write(
        root.path(),
        "app/web/Cargo.toml",
        "[package]\nname = 'web'\nversion.workspace = true\n[package.metadata.lenso]\nplugin-id = 'example.web'\nroot-slot = 'web'",
    );
    let report = discover(root.path()).unwrap();
    assert_eq!(report.candidates[0].release_version, "2.0.0");
    assert_eq!(
        report.candidates[0].implementations[0].runtime,
        "native-linked"
    );
}

#[test]
fn composite_owns_nested_implementations_without_duplicate_plugins() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "app/mixed/Cargo.toml",
        r#"
[package]
name = "mixed"
version = "1.0.0"
[package.metadata.lenso]
plugin-id = "example.mixed"
[package.metadata.lenso-cli]
implementations = [
  { id = "rust", runtime = "process", path = "." },
  { id = "ts", runtime = "bun", path = "typescript" },
]
"#,
    );
    bun(root.path(), "app/mixed/typescript", "example.mixed");
    let report = discover(root.path()).unwrap();
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].implementations.len(), 2);
    bun(root.path(), "app/mixed/typescript", "example.other");
    assert!(
        format!("{:#}", discover(root.path()).unwrap_err()).contains("different Plugin identity")
    );
}

#[test]
fn rejects_corrupt_local_archives_without_installation() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "app/bad.lenso-plugin", "not a bundle");
    assert!(discover(root.path()).is_err());
    assert!(!root.path().join("plugins").exists());
}

#[cfg(unix)]
#[test]
fn nested_symlinks_do_not_escape_sources_or_loop() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    rust(root.path(), "external", "example.external");
    fs::create_dir(root.path().join("app")).unwrap();
    symlink(root.path(), root.path().join("app/loop")).unwrap();
    symlink(root.path().join("external"), root.path().join("app/linked")).unwrap();
    assert!(discover(root.path()).unwrap().candidates.is_empty());
    write(root.path(), "lenso.toml", "plugin_sources = ['app/linked']");
    assert_eq!(
        discover(root.path()).unwrap().candidates[0].role,
        SourceRole::Shared
    );
}

#[test]
fn rejects_sources_with_conflicting_roles() {
    let root = tempfile::tempdir().unwrap();
    rust(root.path(), "app/owned", "example.owned");
    write(root.path(), "lenso.toml", "plugin_sources = ['app/owned']");
    assert!(
        discover(root.path())
            .unwrap_err()
            .to_string()
            .contains("overlaps")
    );
}

#[test]
fn workspace_members_and_excludes_bound_discovery() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "app/Cargo.toml",
        "[workspace]\nmembers = ['packages/*']\nexclude = ['packages/excluded']",
    );
    rust(root.path(), "app/packages/included", "example.included");
    rust(root.path(), "app/packages/excluded", "example.excluded");
    rust(root.path(), "app/not-a-member", "example.other");
    let report = discover(root.path()).unwrap();
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].plugin_id, "example.included");
}

#[test]
fn npm_workspace_members_are_discovered_without_running_package_scripts() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "app/package.json",
        r#"{"workspaces":{"packages":["packages/*"]}}"#,
    );
    bun(root.path(), "app/packages/one", "example.one");
    bun(root.path(), "app/outside", "example.outside");
    assert_eq!(discover(root.path()).unwrap().candidates.len(), 1);
}

#[test]
fn recursive_globs_are_rejected_in_favor_of_bounded_directory_scanning() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "lenso.toml", "plugin_sources = ['../**']");
    assert!(
        discover(root.path())
            .unwrap_err()
            .to_string()
            .contains("recursive **")
    );
}

fn support(root: &Path, path: &str, id: &str, entry: &str) {
    bun(root, path, id);
    let manifest = root.join(path).join("package.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    value["lenso"]["conventions"] = serde_json::json!([{ "id": id, "entries": [entry] }]);
    fs::write(manifest, value.to_string()).unwrap();
}

fn surfaces(root: &Path) {
    bun(root, "app/notes", "example.notes");
    bun(root, "app/notes/cli", "example.notes-cli");
    write(root, "app/notes/cli/cli.ts", "export {};");
    // Deliberately invalid package. Inactive packages must never be parsed.
    write(root, "app/notes/tui/Cargo.toml", "INVALID TOML !!!");
    write(
        root,
        "app/notes/tui/tui.rs",
        "compile_error!(\"inactive\");",
    );
    let manifest = root.join("app/notes/package.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    value["lenso"]["surfaces"] = serde_json::json!([
        {"entry":"cli/cli.ts", "project":"cli"},
        {"entry":"tui/tui.rs", "project":"tui"}
    ]);
    fs::write(manifest, value.to_string()).unwrap();
}

#[test]
fn selected_surface_packages_are_independent_and_inactive_metadata_is_not_read() {
    let root = tempfile::tempdir().unwrap();
    surfaces(root.path());
    support(root.path(), "app/support", "example.cli", "cli.ts");
    let plan = conventions::plan(&discover(root.path()).unwrap()).unwrap();
    assert_eq!(plan.candidates.len(), 3);
    assert_eq!(
        plan.surfaces[0].plugin_id.as_deref(),
        Some("example.notes-cli")
    );
    assert_eq!(plan.surfaces[1].reason, "support_not_adopted");
    assert!(!root.path().join(".lenso").exists());
}

#[test]
fn shared_support_requires_a_live_root_instance() {
    let root = tempfile::tempdir().unwrap();
    surfaces(root.path());
    support(root.path(), "shared/support", "example.cli", "cli.ts");
    write(root.path(), "lenso.toml", "plugin_sources = [\"shared\"]");
    let inspect = || conventions::plan(&discover(root.path()).unwrap()).unwrap();
    assert_eq!(inspect().surfaces[0].reason, "support_not_adopted");
    write(root.path(), "plugins/example.cli/default.toml", "# adopted");
    assert_eq!(inspect().surfaces[0].reason, "selected");
    write(root.path(), "plugins/example.cli/default.disabled", "");
    assert_eq!(inspect().surfaces[0].reason, "support_not_adopted");
}

#[test]
fn convention_conflicts_are_rejected_without_scan_order_preference() {
    let root = tempfile::tempdir().unwrap();
    support(root.path(), "app/a", "example.a", "cli.ts");
    support(root.path(), "app/b", "example.b", "cli.ts");
    let error = conventions::plan(&discover(root.path()).unwrap()).unwrap_err();
    assert!(error.to_string().contains("conflicting convention entry"));
}

#[test]
fn composite_identity_comes_from_core_package() {
    let root = tempfile::tempdir().unwrap();
    bun(root.path(), "app/notes/core", "example.notes");
    write(
        root.path(),
        "app/notes/plugin.json",
        r#"{"schema":"lenso.plugin-project.v1","core":"core"}"#,
    );
    let report = discover(root.path()).unwrap();
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].plugin_id, "example.notes");
    assert!(report.candidates[0].project.ends_with("notes/core"));
    assert_eq!(conventions::plan(&report).unwrap().candidates.len(), 1);
}

#[test]
fn required_surfaces_fail_before_compilation_and_disabled_owners_do_not_activate() {
    let root = tempfile::tempdir().unwrap();
    surfaces(root.path());
    let manifest = root.path().join("app/notes/package.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    value["lenso"]["surfaces"][0]["required"] = true.into();
    fs::write(manifest, value.to_string()).unwrap();
    assert!(
        conventions::plan(&discover(root.path()).unwrap())
            .unwrap_err()
            .to_string()
            .contains("required surface")
    );
    write(root.path(), "plugins/example.notes/default.disabled", "");
    let plan = conventions::plan(&discover(root.path()).unwrap()).unwrap();
    assert!(
        plan.surfaces
            .iter()
            .all(|s| s.reason == "owner_not_adopted")
    );
}

#[test]
fn surface_paths_cannot_escape_the_logical_project() {
    let root = tempfile::tempdir().unwrap();
    bun(root.path(), "app/core", "example.core");
    write(root.path(), "outside.ts", "");
    let manifest = root.path().join("app/core/package.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    value["lenso"]["surfaces"] = serde_json::json!([{"entry":"../../outside.ts","project":"."}]);
    fs::write(manifest, value.to_string()).unwrap();
    assert!(
        conventions::plan(&discover(root.path()).unwrap())
            .unwrap_err()
            .to_string()
            .contains("stay inside")
    );
}

#[test]
fn multiple_owner_instances_do_not_silently_share_one_surface() {
    let root = tempfile::tempdir().unwrap();
    surfaces(root.path());
    write(root.path(), "plugins/example.notes/second.toml", "");
    assert!(
        conventions::plan(&discover(root.path()).unwrap())
            .unwrap_err()
            .to_string()
            .contains("one active owner instance")
    );
}

#[cfg(unix)]
#[test]
fn surface_symlinks_cannot_escape_the_owner() {
    let root = tempfile::tempdir().unwrap();
    surfaces(root.path());
    fs::remove_file(root.path().join("app/notes/cli/cli.ts")).unwrap();
    write(root.path(), "external.ts", "");
    std::os::unix::fs::symlink(
        root.path().join("external.ts"),
        root.path().join("app/notes/cli/cli.ts"),
    )
    .unwrap();
    assert!(
        conventions::plan(&discover(root.path()).unwrap())
            .unwrap_err()
            .to_string()
            .contains("symbolic links")
    );
}

#[test]
fn bare_entries_are_planned_without_running_the_selected_compiler() {
    let root = tempfile::tempdir().unwrap();
    support(root.path(), "app/support", "example.cli", "cli.ts");
    let manifest = root.path().join("app/support/package.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    metadata["lenso"]["conventions"][0]["compiler"] =
        serde_json::json!({"program":"must-not-run","args":[]});
    fs::write(manifest, serde_json::to_vec(&metadata).unwrap()).unwrap();
    write(
        root.path(),
        "app/hello/cli.ts",
        "throw new Error('must not execute during discovery');",
    );
    let plan = conventions::plan(&discover(root.path()).unwrap()).unwrap();
    assert_eq!(plan.compilations.len(), 1);
    assert_eq!(plan.compilations[0].compiler.program, "must-not-run");
    assert!(!root.path().join(".lenso").exists());
    write(root.path(), "plugins/example.cli/default.disabled", "");
    let plan = conventions::plan(&discover(root.path()).unwrap()).unwrap();
    assert!(plan.compilations.is_empty());
    assert_eq!(plan.surfaces[0].reason, "support_not_adopted");
}

#[test]
fn convention_outputs_cannot_change_identity_or_activate_more_conventions() {
    let root = tempfile::tempdir().unwrap();
    bun(root.path(), "output", "example.other");
    let compilation = conventions::Compilation {
        owner: "example.owner".into(),
        version: "1.0.0".into(),
        role: SourceRole::AppOwned,
        owner_project: root.path().into(),
        entry: root.path().join("cli.ts"),
        plugin_id: "example.generated".into(),
        convention: "example.cli".into(),
        compiler_project: root.path().into(),
        compiler: conventions::Compiler {
            program: "never-run".into(),
            args: vec![],
        },
    };
    assert!(
        conventions::generated_candidate(&root.path().join("output"), &compilation)
            .unwrap_err()
            .to_string()
            .contains("identity or version")
    );
    let manifest = root.path().join("output/package.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    metadata["lenso"]["pluginId"] = "example.generated".into();
    metadata["version"] = "1.0.0".into();
    metadata["lenso"]["conventions"] = serde_json::json!([]);
    fs::write(manifest, serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert!(
        conventions::generated_candidate(&root.path().join("output"), &compilation)
            .unwrap_err()
            .to_string()
            .contains("recursively activate")
    );
}

#[test]
fn composite_compilers_fingerprint_the_logical_owner_not_only_its_core() {
    let root = tempfile::tempdir().unwrap();
    support(root.path(), "app/support", "example.cli", "cli.ts");
    let manifest = root.path().join("app/support/package.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    metadata["lenso"]["conventions"][0]["compiler"] =
        serde_json::json!({"program":"never-run","args":[]});
    fs::write(manifest, serde_json::to_vec(&metadata).unwrap()).unwrap();
    bun(root.path(), "app/product/core", "example.product");
    write(
        root.path(),
        "app/product/plugin.json",
        r#"{"schema":"lenso.plugin-project.v1","core":"core","surfaces":[{"entry":"cli.ts"}]}"#,
    );
    write(root.path(), "app/product/cli.ts", "export {};\n");
    write(root.path(), "app/cli.ts", "export {};\n");
    let plan = conventions::plan(&discover(root.path()).unwrap()).unwrap();
    assert_eq!(
        plan.compilations
            .iter()
            .find(|c| c.owner == "example.product")
            .unwrap()
            .owner_project,
        fs::canonicalize(root.path().join("app/product")).unwrap()
    );
    assert_eq!(
        plan.compilations.len(),
        2,
        "bare discovery must stop at composite boundaries"
    );
}

// Prevent a private frontend/tool package from being parsed or installed before
// support selection, and prevent nested pages from becoming separate Plugins.
#[test]
fn directory_conventions_are_one_opaque_optional_surface() {
    let root = tempfile::tempdir().unwrap();
    bun(root.path(), "app/orders", "example.orders");
    support(root.path(), "app/support", "example.console", "console");
    let manifest = root.path().join("app/support/package.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    metadata["lenso"]["conventions"][0]["compiler"] =
        serde_json::json!({"program":"must-not-run","args":[]});
    fs::write(manifest, serde_json::to_vec(&metadata).unwrap()).unwrap();
    write(
        root.path(),
        "app/orders/console/package.json",
        "intentionally invalid until selected by its processor",
    );
    write(root.path(), "app/orders/console/page.tsx", "not executed");
    write(
        root.path(),
        "app/orders/console/orders/[id]/page.tsx",
        "not executed",
    );
    let plan = conventions::plan(&discover(root.path()).unwrap()).unwrap();
    assert_eq!(plan.compilations.len(), 1);
    assert!(plan.compilations[0].entry.ends_with("orders/console"));
    write(root.path(), "plugins/example.console/default.disabled", "");
    let plan = conventions::plan(&discover(root.path()).unwrap()).unwrap();
    assert!(plan.compilations.is_empty());
    assert_eq!(plan.surfaces[0].reason, "support_not_adopted");
}
