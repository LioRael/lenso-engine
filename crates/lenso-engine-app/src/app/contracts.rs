//! Local contract synchronization is authoring work, never runtime discovery.
use anyhow::{Context, bail};
use lenso_app_authoring::discovery::Candidate;
use lenso_contract_codegen_next::{
    ProjectionLanguage, generate_projection, lint_compatibility, load_descriptor,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

pub mod scaffold;
mod source;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Projection {
    projection: String,
    output: PathBuf,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    descriptor: PathBuf,
    #[serde(default)]
    source: Option<PathBuf>,
    #[serde(default)]
    projection: Option<String>,
    #[serde(default)]
    output: Option<PathBuf>,
    #[serde(default)]
    projections: Vec<Projection>,
}
struct Contract {
    root: PathBuf,
    declaration: Declaration,
    cargo: Option<(Value, Value)>,
}

pub(super) fn synchronize(root: &Path, candidates: &[Candidate]) -> anyhow::Result<()> {
    let mut contracts = BTreeMap::new();
    let mut manifests = BTreeSet::new();
    // Only selected Plugin projects and explicitly owned contracts participate.
    for candidate in candidates {
        match candidate.format.as_str() {
            "cargo" => {
                manifests.insert(candidate.project.join("Cargo.toml"));
            }
            "bun" => {
                read_npm(&candidate.project, &mut contracts)?;
            }
            _ => {}
        }
    }
    let directory = root.join("contracts");
    if directory.is_dir() {
        scan(&directory, &mut manifests, &mut contracts, &mut 0, 0)?;
    }
    let mut visited_packages = BTreeSet::new();
    for manifest in manifests {
        let metadata = cargo_metadata(&manifest)?;
        let packages = metadata["packages"].as_array().context("Cargo packages")?;
        // Dependency closure, not every unrelated member in a sibling workspace.
        let selected = packages
            .iter()
            .find(|p| Path::new(p["manifest_path"].as_str().unwrap_or("")) == manifest)
            .or_else(|| {
                packages
                    .iter()
                    .find(|p| metadata["resolve"]["root"] == p["id"])
            });
        let mut pending = if let Some(package) = selected {
            vec![package["id"].clone()]
        } else {
            metadata["workspace_members"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        };
        let nodes = metadata["resolve"]["nodes"]
            .as_array()
            .context("Cargo dependency graph")?;
        while let Some(id) = pending.pop() {
            if !visited_packages.insert(id.as_str().context("Cargo package ID")?.to_owned()) {
                continue;
            }
            if let Some(node) = nodes.iter().find(|n| n["id"] == id) {
                for dep in node["deps"].as_array().context("Cargo dependencies")? {
                    if dep["dep_kinds"].as_array().is_some_and(|kinds| {
                        kinds
                            .iter()
                            .any(|k| k["kind"].is_null() || k["kind"] == "build")
                    }) {
                        pending.push(dep["pkg"].clone());
                    }
                }
            }
            let package = packages
                .iter()
                .find(|p| p["id"] == id)
                .context("Cargo package")?;
            if !package["source"].is_null() {
                continue;
            }
            let Some(value) = package.pointer("/metadata/lenso/contract") else {
                continue;
            };
            let package_root = Path::new(
                package["manifest_path"]
                    .as_str()
                    .context("Cargo manifest")?,
            )
            .parent()
            .context("Cargo root")?
            .to_path_buf();
            insert(
                &mut contracts,
                package_root,
                value.clone(),
                Some((package.clone(), metadata.clone())),
            )?;
        }
    }
    if contracts.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(root.join(".lenso"))?;
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".lenso/contracts.lock"))?;
    lock.lock().context("lock local contract generation")?;
    let staging = tempfile::tempdir_in(root.join(".lenso"))?;
    let inputs = contracts
        .keys()
        .map(|path| Ok((path.clone(), super::local_host::input_digest(path)?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut external_inputs = BTreeMap::new();
    let mut changes = BTreeMap::<PathBuf, Vec<u8>>::new();
    let mut owned = BTreeSet::new();
    let mut baselines = Vec::new();
    let mut ordered = contracts.values().collect::<Vec<_>>();
    ordered.sort_by_key(|c| {
        !(c.declaration.source.is_some() || c.root.join("src/contract.rs").is_file())
    });
    let mut staged_sources = BTreeMap::<PathBuf, PathBuf>::new();
    for (index, contract) in ordered.into_iter().enumerate() {
        let declaration = &contract.declaration;
        let descriptor = normalized(&contract.root.join(&declaration.descriptor));
        let stage = staging.path().join(index.to_string());
        fs::create_dir_all(&stage)?;
        let staged_descriptor = stage.join("capability.json");
        let source_path = declaration.source.clone().or_else(|| {
            (contract.cargo.is_some() && contract.root.join("src/contract.rs").is_file())
                .then(|| PathBuf::from("src/contract.rs"))
        });
        let source_owned = source_path.is_some();
        if !source_owned && descriptor.is_file() {
            for (relative, bytes) in snapshot_files(&descriptor)? {
                let path = if relative == Path::new("capability.json") {
                    descriptor.clone()
                } else {
                    descriptor.parent().unwrap().join(relative)
                };
                external_inputs.insert(path, bytes);
            }
        }
        if let Some(source_path) = source_path {
            let source_path = safe_output(&contract.root, &source_path)?;
            source::extract(root, contract, Some(&source_path), &staged_descriptor)?;
        } else {
            snapshot(
                staged_sources.get(&descriptor).unwrap_or(&descriptor),
                &stage,
            )?;
            if contract.cargo.as_ref().is_some_and(|(package, _)| {
                package["dependencies"].as_array().is_some_and(|deps| {
                    deps.iter().any(|dep| {
                        dep["kind"] == "build" && dep["name"] == "lenso-contract-codegen"
                    })
                })
            }) {
                source::extract(root, contract, None, &staged_descriptor)?;
            }
        }
        if source_owned {
            staged_sources.insert(descriptor.clone(), staged_descriptor.clone());
        }
        let next = load_descriptor(&staged_descriptor)
            .map_err(|e| anyhow::anyhow!("{}: {e}", descriptor.display()))?;
        let key = super::local_host::digest_text(&descriptor.to_string_lossy());
        let baseline = root.join(".lenso/contracts").join(key);
        let previous = if baseline.join("capability.json").is_file() {
            Some(baseline.join("capability.json"))
        } else if source_owned && descriptor.is_file() {
            Some(descriptor.clone())
        } else {
            None
        };
        if let Some(previous) = previous {
            let old = load_descriptor(&previous).map_err(|e| anyhow::anyhow!("{e}"))?;
            if old.descriptor_digest() != next.descriptor_digest() {
                lint_compatibility(&previous, &staged_descriptor).map_err(|e| anyhow::anyhow!("{}: contract change needs an explicit compatible version or a new Capability identity/package: {e}", descriptor.display()))?;
            }
        }
        let mut projections = declaration.projections.clone();
        match (&declaration.projection, &declaration.output) {
            (Some(projection), Some(output)) => projections.push(Projection {
                projection: projection.clone(),
                output: output.clone(),
            }),
            (None, None) => {}
            _ => bail!(
                "{}: contract projection and output must be declared together",
                contract.root.display()
            ),
        }
        if projections.is_empty() {
            bail!(
                "{}: contract needs at least one generated projection",
                contract.root.display()
            );
        }
        for (projection_index, projection) in projections.into_iter().enumerate() {
            let language = match projection.projection.as_str() {
                "rust" => ProjectionLanguage::Rust,
                "rust-runtime" => ProjectionLanguage::RustRuntime,
                "rust-plugin" => ProjectionLanguage::RustPlugin,
                "typescript" => ProjectionLanguage::TypeScript,
                "wit" => ProjectionLanguage::Wit,
                value => bail!("unsupported contract projection {value}"),
            };
            let output = safe_output(&contract.root, &projection.output)?;
            if let Ok(existing) = fs::read_to_string(&output)
                && !existing
                    .lines()
                    .take(3)
                    .any(|line| line.contains("@generated by lenso-contract-codegen"))
            {
                bail!(
                    "refusing to overwrite authored file with a contract projection: {}",
                    output.display()
                );
            }
            if !owned.insert(output.clone()) {
                bail!("generated output has multiple owners: {}", output.display());
            }
            // Existing projections from another compatible generator cohort remain
            // untouched when their exact contract digest has not changed.
            if !source_owned
                && !stage
                    .join(format!("projection-{projection_index}.txt"))
                    .is_file()
                && fs::read_to_string(&output)
                    .is_ok_and(|text| text.contains(next.descriptor_digest()))
            {
                continue;
            }
            let extracted = stage.join(format!("projection-{projection_index}.txt"));
            let bytes = if extracted.is_file() {
                fs::read(extracted)?
            } else {
                if matches!(
                    language,
                    ProjectionLanguage::Rust
                        | ProjectionLanguage::RustRuntime
                        | ProjectionLanguage::RustPlugin
                ) && targets_legacy_guest(contract)
                {
                    bail!(
                        "{}: regeneration needs this contract's compatible lenso-contract-codegen build-dependency; the bundled generator targets Guest SDK 0.5",
                        contract.root.display()
                    );
                }
                generate_projection(&staged_descriptor, language)
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .source
                    .into_bytes()
            };
            changes.insert(output, bytes);
        }
        if declaration.source.is_some()
            || (contract.cargo.is_some() && contract.root.join("src/contract.rs").is_file())
        {
            for (relative, bytes) in snapshot_files(&staged_descriptor)? {
                let target = if relative == Path::new("capability.json") {
                    safe_output(&contract.root, &declaration.descriptor)?
                } else {
                    safe_output(descriptor.parent().context("Descriptor parent")?, &relative)?
                };
                if !owned.insert(target.clone()) {
                    bail!(
                        "generated snapshot has multiple owners: {}",
                        target.display()
                    );
                }
                changes.insert(target, bytes);
            }
        }
        baselines.push((baseline, stage));
    }
    for (path, digest) in inputs {
        if super::local_host::input_digest(&path)? != digest {
            bail!(
                "contract source changed during generation: {}; retry after edits settle",
                path.display()
            );
        }
    }
    for (path, bytes) in external_inputs {
        if fs::read(&path)? != bytes {
            bail!(
                "contract input changed during generation: {}; retry after edits settle",
                path.display()
            );
        }
    }
    // Validate every contract before installing any output. Preserve unchanged mtimes.
    let backups = changes
        .keys()
        .map(|path| {
            Ok((
                path.clone(),
                if path.exists() {
                    Some(fs::read(path)?)
                } else {
                    None
                },
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if let Err(error) = install(&changes) {
        for (path, previous) in backups {
            match previous {
                Some(bytes) => {
                    let _ = fs::write(path, bytes);
                }
                None => {
                    let _ = fs::remove_file(path);
                }
            }
        }
        return Err(error);
    }
    for (baseline, stage) in baselines {
        fs::create_dir_all(&baseline)?;
        snapshot(&stage.join("capability.json"), &baseline)?;
    }
    eprintln!("Synchronized {} local contract packages", contracts.len());
    Ok(())
}

fn targets_legacy_guest(contract: &Contract) -> bool {
    let Some((package, metadata)) = &contract.cargo else {
        return false;
    };
    let Some(nodes) = metadata["resolve"]["nodes"].as_array() else {
        return false;
    };
    let Some(dependencies) = nodes
        .iter()
        .find(|node| node["id"] == package["id"])
        .and_then(|node| node["deps"].as_array())
    else {
        return false;
    };
    metadata["packages"].as_array().is_some_and(|packages| {
        dependencies.iter().any(|dep| {
            packages.iter().any(|p| {
                p["id"] == dep["pkg"]
                    && p["name"] == "lenso-guest-sdk"
                    && p["version"]
                        .as_str()
                        .is_some_and(|version| version.starts_with("0.4."))
            })
        })
    })
}

fn install(changes: &BTreeMap<PathBuf, Vec<u8>>) -> anyhow::Result<()> {
    for (path, bytes) in changes {
        if fs::read(path).ok().as_ref() == Some(bytes) {
            continue;
        }
        fs::create_dir_all(path.parent().context("generated output parent")?)?;
        fs::write(path, bytes)
            .with_context(|| format!("write generated contract {}", path.display()))?;
    }
    Ok(())
}
fn insert(
    contracts: &mut BTreeMap<PathBuf, Contract>,
    root: PathBuf,
    value: Value,
    cargo: Option<(Value, Value)>,
) -> anyhow::Result<()> {
    let declaration: Declaration = serde_json::from_value(value)
        .with_context(|| format!("{}: invalid lenso.contract metadata", root.display()))?;
    contracts.entry(root.clone()).or_insert(Contract {
        root,
        declaration,
        cargo,
    });
    Ok(())
}
fn read_npm(root: &Path, contracts: &mut BTreeMap<PathBuf, Contract>) -> anyhow::Result<()> {
    let document: Value = serde_json::from_slice(&fs::read(root.join("package.json"))?)?;
    if let Some(value) = document.pointer("/lenso/contract") {
        insert(contracts, fs::canonicalize(root)?, value.clone(), None)?;
    }
    Ok(())
}
fn scan(
    root: &Path,
    manifests: &mut BTreeSet<PathBuf>,
    contracts: &mut BTreeMap<PathBuf, Contract>,
    visited: &mut usize,
    depth: usize,
) -> anyhow::Result<()> {
    *visited += 1;
    if *visited > 5000 || depth > 32 {
        bail!("local contracts exceed traversal limits");
    }
    if root.join("Cargo.toml").is_file() {
        manifests.insert(fs::canonicalize(root.join("Cargo.toml"))?);
        return Ok(());
    }
    if root.join("package.json").is_file() {
        read_npm(root, contracts)?;
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_dir()
            && !name.to_string_lossy().starts_with('.')
            && !["target", "node_modules", "dist", "build"]
                .contains(&name.to_string_lossy().as_ref())
        {
            scan(&entry.path(), manifests, contracts, visited, depth + 1)?;
        }
    }
    Ok(())
}
fn cargo_metadata(manifest: &Path) -> anyhow::Result<Value> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--manifest-path"])
        .arg(manifest)
        .output()?;
    if !output.status.success() {
        bail!(
            "contract dependency discovery: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}
fn safe_output(root: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        bail!(
            "contract output must stay inside its package: {}",
            relative.display()
        );
    }
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!(
                "contract output cannot traverse symlinks: {}",
                path.display()
            );
        }
    }
    if path.is_dir() {
        bail!("contract output is a directory: {}", path.display());
    }
    Ok(path)
}
fn snapshot_files(descriptor: &Path) -> anyhow::Result<BTreeMap<PathBuf, Vec<u8>>> {
    let bytes = fs::read(descriptor)?;
    if bytes.len() > 4 * 1024 * 1024 {
        bail!("contract Descriptor exceeds 4 MiB");
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    let mut files = BTreeMap::from([(PathBuf::from("capability.json"), bytes)]);
    let operations = value["operations"]
        .as_array()
        .context("contract operations")?;
    if operations.len() > 256 {
        bail!("contract exceeds 256 Operations");
    }
    let mut pending = Vec::new();
    for operation in operations {
        for (key, value) in operation.as_object().context("contract Operation")? {
            if key.ends_with("_schema") {
                pending.push(PathBuf::from(
                    value.as_str().context("contract schema path")?,
                ));
            }
        }
    }
    let base = descriptor.parent().context("Descriptor parent")?;
    let mut total = 0;
    while let Some(relative) = pending.pop() {
        if files.contains_key(&relative) {
            continue;
        }
        let source = safe_output(base, &relative)?;
        let bytes = fs::read(&source)?;
        total += bytes.len();
        if bytes.len() > 4 * 1024 * 1024 || total > 16 * 1024 * 1024 || files.len() >= 1024 {
            bail!("contract schemas exceed size/count limits");
        }
        let schema: Value = serde_json::from_slice(&bytes)?;
        let mut references = Vec::new();
        schema_references(&schema, &mut references);
        for reference in references {
            let filename = reference.split('#').next().unwrap_or_default();
            if filename.is_empty() {
                continue;
            }
            let path = normalized(&relative.parent().unwrap_or(Path::new("")).join(filename));
            // Parent references are resolved against this schema, but must stay
            // within the Descriptor's package-local schema closure.
            let resolved = normalized(&source.parent().context("schema parent")?.join(filename));
            if !resolved.starts_with(base) {
                bail!("schema reference escapes contract: {reference}");
            }
            pending.push(path);
        }
        files.insert(relative, bytes);
    }
    Ok(files)
}
fn schema_references<'a>(value: &'a Value, references: &mut Vec<&'a str>) {
    match value {
        Value::Object(fields) => {
            if let Some(reference) = fields.get("$ref").and_then(Value::as_str) {
                references.push(reference);
            }
            for value in fields.values() {
                schema_references(value, references);
            }
        }
        Value::Array(values) => {
            for value in values {
                schema_references(value, references);
            }
        }
        _ => {}
    }
}
fn snapshot(descriptor: &Path, destination: &Path) -> anyhow::Result<()> {
    let changes = snapshot_files(descriptor)?
        .into_iter()
        .map(|(relative, bytes)| (destination.join(relative), bytes))
        .collect();
    install(&changes)
}

fn normalized(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            _ => output.push(part),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn descriptor(root: &Path, id: &str) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("capability.json"), serde_json::to_vec(&json!({
            "id":id,"version":"1.0.0","portable":true,"cross_lane_transfer":false,
            "operations":[{"name":"uppercase","interaction":"request","request_schema":"request.json","response_schema":"response.json","domain_error_schema":"error.json"}]
        })).unwrap()).unwrap();
        for file in ["request.json", "response.json"] {
            fs::write(root.join(file), r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"type":"string"}}}"#).unwrap();
        }
        fs::write(root.join("error.json"), r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","oneOf":[{"const":"unavailable"}]}"#).unwrap();
        fs::write(root.join("package.json"), serde_json::to_vec(&json!({"name":"text-contract","version":"1.0.0","lenso":{"contract":{"descriptor":"capability.json","projection":"typescript","output":"generated/text.ts"}}})).unwrap()).unwrap();
    }

    #[test]
    fn descriptor_generation_is_stable_and_rejects_incompatible_changes_before_output() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("contracts/text");
        descriptor(&root, "example.text@1");
        synchronize(temp.path(), &[]).unwrap();
        let generated = root.join("generated/text.ts");
        let before = fs::read(&generated).unwrap();
        let modified = fs::metadata(&generated).unwrap().modified().unwrap();
        assert!(String::from_utf8_lossy(&before).contains("uppercase"));
        synchronize(temp.path(), &[]).unwrap();
        assert_eq!(
            modified,
            fs::metadata(&generated).unwrap().modified().unwrap()
        );
        let schema = root.join("request.json");
        fs::write(
            &schema,
            fs::read_to_string(&schema)
                .unwrap()
                .replace("string", "integer"),
        )
        .unwrap();
        let error = synchronize(temp.path(), &[]).unwrap_err().to_string();
        assert!(error.contains("explicit compatible version"), "{error}");
        assert_eq!(before, fs::read(&generated).unwrap());
    }

    #[test]
    fn all_contracts_validate_before_generated_files_change() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("contracts/a");
        descriptor(&first, "example.a@1");
        let second = temp.path().join("contracts/z");
        descriptor(&second, "example.z@1");
        fs::write(second.join("request.json"), "invalid").unwrap();
        assert!(synchronize(temp.path(), &[]).is_err());
        assert!(!first.join("generated/text.ts").exists());
    }

    #[test]
    fn output_cannot_escape_package_or_overwrite_contract_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("contracts/text");
        descriptor(&root, "example.text@1");
        let path = root.join("package.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["lenso"]["contract"]["output"] = json!("../outside.ts");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            synchronize(temp.path(), &[])
                .unwrap_err()
                .to_string()
                .contains("inside its package")
        );
        value["lenso"]["contract"]["output"] = json!("capability.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            synchronize(temp.path(), &[])
                .unwrap_err()
                .to_string()
                .contains("authored file")
        );
    }

    #[test]
    #[ignore = "requires Cargo registry access; compiles actual source extraction"]
    fn clean_room_source_contract_generates_before_a_stale_library_can_compile() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("contracts/text");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"
[package]
name = "source-text-contract"
version = "1.0.0"
edition = "2024"
[workspace]
[package.metadata.lenso.contract]
descriptor = "capability.json"
source = "src/contract.rs"
projection = "typescript"
output = "generated/text.ts"
[build-dependencies]
schemars = "1.2"
lenso-contract-authoring = "=0.1.1"
lenso-contract-codegen = "=0.9.0"
"#,
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "compile_error!(\"stale consumer must not compile during extraction\");",
        )
        .unwrap();
        fs::write(root.join("src/contract.rs"), r#"
use lenso_contract_authoring as lenso;
#[derive(lenso::JsonSchema)]
#[schemars(deny_unknown_fields)]
struct Input { text: String }
#[derive(lenso::DomainError)]
enum Error { Unavailable }
#[lenso::capability(id = "example.text", major = 1, version = "1.0.0", portable = true, cross_lane_transfer = false)]
trait Text {
    async fn uppercase(&self, context: lenso::Ctx<'_>, request: Input) -> Result<Input, Error>;
}
"#).unwrap();
        synchronize(temp.path(), &[]).unwrap();
        assert!(root.join("capability.json").is_file());
        assert!(
            fs::read_to_string(root.join("generated/text.ts"))
                .unwrap()
                .contains("uppercase")
        );
        synchronize(temp.path(), &[]).unwrap();
    }
}
