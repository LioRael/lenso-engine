use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use lenso_app_plan::{
    CapabilityEndpointPlan, CapabilityRequirementPlan, ExecutionClassId, authoring::PluginContract,
};
use lenso_plugin_bundle::{
    PluginManifest, SourcePluginBuild, SourcePluginImplementation, SourcePluginReleaseBuild,
    SourceProcessPluginBuild, VerifiedBundle, build_source_plugin_bundle,
    build_source_plugin_release_bundle, build_source_process_plugin_bundle,
    extract_plugin_descriptor, read_bundle_manifest, verify_bundle_directory,
};
use lenso_wasm_component_adapter::EXECUTION_CLASS as WASM_EXECUTION_CLASS;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::archive::{archive_bundle, with_bundle_directory};
use lenso_app_authoring::identity::{
    PluginIdVersion, classify_existing_plugin_id, validate_release_version,
};
use lenso_app_authoring::native_host_target;

mod dev;
mod scaffold;
mod web_dev;

const WASM_TARGET: &str = "wasm32-unknown-unknown";
const PROCESS_EXECUTION_CLASS: &str = "lenso.process@1";
const PROCESS_RUNTIME_PROFILE_V1: &str = "lenso.process@1";
const PROCESS_RUNTIME_PROFILE_V2: &str = "lenso.process-stdio@2";
const BUN_PLUGIN_BUILDER: &str = include_str!("../assets/plugin-build.mjs");

fn rust_host_target(root: &Path) -> anyhow::Result<String> {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("-vV")
        .current_dir(root)
        .output()
        .context("read Rust host target")?;
    if !output.status.success() {
        bail!("read Rust host target: `rustc -vV` failed");
    }
    let version = String::from_utf8(output.stdout).context("Rust version output is not UTF-8")?;
    version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .context("Rust version output has no host target")
}

#[derive(Clone, Debug, Subcommand)]
pub enum PluginCommand {
    /// Create an Agent Tool Plugin, or a linked Web Plugin with `--web`.
    New(PluginNewArgs),
    /// Build and run the Plugin through its SDK-selected execution adapter.
    Dev(PluginDevArgs),
    /// Validate Plugin source and generated descriptor evidence.
    Check(PluginCheckArgs),
    /// Build and verify one immutable `.lenso-plugin` archive.
    Pack(PluginPackArgs),
}

#[derive(Args, Clone, Debug)]
pub struct PluginNewArgs {
    /// Namespaced Plugin id, such as company.uppercase.
    plugin_id: String,
    /// Base directory for the new Plugin project.
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// New Plugin project directory. Defaults to the Plugin id.
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Implementation runtime. The default builds an official Rust Process Plugin.
    #[arg(long, value_enum, default_value_t = PluginRuntimeArg::Process)]
    runtime: PluginRuntimeArg,
    /// Create a linked native Rust HTTP Endpoint Plugin instead of an Agent Tool Plugin.
    #[arg(long, conflicts_with = "runtime")]
    pub web: bool,
    /// Skip lockfile generation and the initial compile check.
    #[arg(long)]
    no_install: bool,
    /// Print generated paths without writing them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum PluginRuntimeArg {
    Multi,
    #[value(alias = "rust")]
    Wasm,
    Process,
    /// TypeScript Plugin executed through the Bun child-process Adapter.
    Bun,
}

#[derive(Args, Clone, Debug)]
pub struct PluginCheckArgs {
    /// Plugin project root. Defaults to the current directory.
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Emit a stable JSON report.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Clone, Debug)]
pub struct PluginDevArgs {
    /// Plugin project root. Defaults to the current directory.
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Request operation to invoke. Defaults to the first declared operation.
    #[arg(long)]
    operation: Option<String>,
    /// JSON request passed to the Plugin.
    #[arg(long, default_value = "{}")]
    request_json: String,
    /// JSON configuration passed when the Plugin starts.
    #[arg(long, default_value = "{}")]
    config_json: String,
    /// Emit a stable JSON result.
    #[arg(long)]
    json: bool,
    /// Rebuild and rerun after source or manifest changes.
    #[arg(long)]
    watch: bool,
    /// Implementation to build and invoke. Auto chooses the fastest declared local implementation.
    #[arg(long, value_enum, default_value_t = DevImplementationArg::Auto)]
    implementation: DevImplementationArg,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum DevImplementationArg {
    Auto,
    Wasm,
    Process,
    Bun,
    /// Build every declared implementation, then invoke the fastest local one.
    All,
}

#[derive(Args, Clone, Debug)]
pub struct PluginPackArgs {
    /// Plugin project root. Defaults to the current directory.
    #[arg(long)]
    repo_root: Option<PathBuf>,
    /// Output `.lenso-plugin` archive. Defaults under `dist/`.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Emit a stable JSON result.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Deserialize)]
struct CargoDocument {
    package: CargoPackage,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    metadata: CargoMetadata,
}

#[derive(Debug, Deserialize)]
struct CargoTargetMetadata {
    target_directory: PathBuf,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    lenso: LensoMetadata,
    #[serde(default, rename = "lenso-cli")]
    lenso_cli: Option<LensoCliMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct LensoMetadata {
    plugin_id: String,
    #[serde(default)]
    root_slot: String,
}

#[derive(Debug, Deserialize)]
struct LensoCliMetadata {
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    implementations: Vec<LensoCliImplementation>,
}

#[derive(Clone, Debug, Deserialize)]
struct LensoCliImplementation {
    id: String,
    path: PathBuf,
    runtime: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectRuntime {
    Composite,
    Multi,
    Wasm,
    Process,
    Bun,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DevBuild {
    Wasm,
    Process,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DevSelection {
    build: DevBuild,
    invoke: ProjectRuntime,
}

#[derive(Debug, Deserialize)]
struct BunPackageDocument {
    #[serde(rename = "name")]
    _name: String,
    version: String,
    #[serde(default)]
    lenso: Option<BunPackageMetadata>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BunPackageMetadata {
    #[serde(default)]
    source: Option<String>,
    plugin_id: String,
    root_slot: String,
    runtime: String,
}

#[derive(Clone, Debug)]
struct BunPackage {
    version: String,
    metadata: BunPackageMetadata,
}

#[derive(Debug, Deserialize)]
struct BunBuildReport {
    fingerprint: String,
    descriptor: PluginDescriptor,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PluginDescriptor {
    abi: String,
    #[serde(default)]
    configuration_schema: Option<Value>,
    capabilities: Vec<PluginCapability>,
    #[serde(default)]
    required_capabilities: Vec<PluginRequirement>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PluginCapability {
    capability_id: String,
    descriptor_version: String,
    #[serde(default)]
    descriptor_digest: Option<String>,
    request_operations: Vec<String>,
    #[serde(default)]
    stream_operations: Vec<String>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PluginRequirement {
    requirement_id: String,
    capability_id: String,
    descriptor_version: String,
    cardinality: String,
}

pub async fn plugin(command: PluginCommand) -> anyhow::Result<()> {
    match command {
        PluginCommand::New(args) => scaffold::create(args),
        PluginCommand::Dev(args) => dev::run(args).await,
        PluginCommand::Check(args) => check(args),
        PluginCommand::Pack(args) => pack(args),
    }
}

fn check(args: PluginCheckArgs) -> anyhow::Result<()> {
    let root = project_root(args.repo_root)?;
    let temporary = tempfile::tempdir().context("create Plugin check directory")?;
    let output = temporary.path().join("checked.lenso-plugin");
    let verified = materialize(&root, &output, BuildProfile::Development)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "kind": "lenso.plugin-check",
                "status": "passed",
                "plugin_id": verified.plugin_id,
                "release_version": verified.release_version,
                "manifest_digest": verified.manifest_digest,
            }))?
        );
    } else {
        println!(
            "Plugin check passed: {}@{}",
            verified.plugin_id, verified.release_version
        );
    }
    Ok(())
}

fn pack(args: PluginPackArgs) -> anyhow::Result<()> {
    let root = project_root(args.repo_root)?;
    let bun_package = read_bun_package(&root)?;
    let (plugin_id, version) = if let Some(package) = bun_package.as_ref() {
        (&package.metadata.plugin_id, &package.version)
    } else {
        let package = read_package(&root.join("Cargo.toml"))?;
        let output = args.output.unwrap_or_else(|| {
            root.join("dist").join(format!(
                "{}-{}.lenso-plugin",
                package.metadata.lenso.plugin_id, package.version
            ))
        });
        return pack_to(&root, &output, args.json);
    };
    let output = args.output.unwrap_or_else(|| {
        root.join("dist")
            .join(format!("{plugin_id}-{version}.lenso-plugin"))
    });
    pack_to(&root, &output, args.json)
}

fn pack_to(root: &Path, output: &Path, json: bool) -> anyhow::Result<()> {
    let staging = tempfile::tempdir().context("stage packed Plugin Bundle")?;
    let directory = staging.path().join("bundle");
    let verified = materialize(root, &directory, BuildProfile::Release)?;
    archive_bundle(&directory, output)?;
    let reopened = with_bundle_directory(output, |directory| {
        verify_bundle_directory(directory)
            .with_context(|| format!("reopen packed Plugin `{}`", output.display()))
    })?;
    if verified != reopened {
        bail!("packed Plugin verification result changed after publication");
    }
    print_verified(&reopened, Some(output), json)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildProfile {
    Development,
    Release,
}

impl BuildProfile {
    const fn directory(self) -> &'static str {
        match self {
            Self::Development => "debug",
            Self::Release => "release",
        }
    }

    fn cargo_args<'a>(self, arguments: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
        let mut output = vec!["build", "--locked"];
        if self == Self::Release {
            output.push("--release");
        }
        output.extend(arguments);
        output
    }
}

fn read_bun_package(root: &Path) -> anyhow::Result<Option<BunPackage>> {
    let path = root.join("package.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let document: BunPackageDocument =
        serde_json::from_slice(&bytes).context("parse Bun Plugin package.json")?;
    let Some(metadata) = document.lenso else {
        return Ok(None);
    };
    if metadata.runtime != "bun" {
        bail!(
            "unsupported package.json Lenso runtime `{}`; expected `bun`",
            metadata.runtime
        );
    }
    warn_for_legacy_plugin_id(&metadata.plugin_id)?;
    if metadata.root_slot.trim().is_empty() {
        bail!("Bun Plugin rootSlot must not be empty");
    }
    validate_release_version(&document.version)?;
    Ok(Some(BunPackage {
        version: document.version,
        metadata,
    }))
}

fn materialize_bun(
    root: &Path,
    output: &Path,
    package: &BunPackage,
    profile: BuildProfile,
) -> anyhow::Result<(VerifiedBundle, PluginDescriptor)> {
    let source = package
        .metadata
        .source
        .as_deref()
        .unwrap_or("src/plugin.ts");
    let relative = Path::new(source);
    if relative.as_os_str().is_empty()
        || relative.components().any(|part| {
            !matches!(
                part,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        bail!("Bun Plugin source must be a relative path inside its package");
    }
    let source_path = fs::canonicalize(root.join(relative)).context("resolve Bun Plugin source")?;
    if !source_path.starts_with(fs::canonicalize(root)?) || !source_path.is_file() {
        bail!("Bun Plugin source must be a file inside its package");
    }
    run_bun(root, &["run", "check"], "typecheck Bun Plugin")?;
    let staging = tempfile::tempdir().context("stage Bun Plugin implementation")?;
    let artifact = staging.path().join("plugin.js");
    let report_path = staging.path().join("build-report.json");
    let builder = staging.path().join("lenso-plugin-build.mjs");
    fs::write(&builder, BUN_PLUGIN_BUILDER).context("stage Bun Plugin build frontend")?;
    let builder_text = builder.to_string_lossy().into_owned();
    let artifact_text = artifact.to_string_lossy().into_owned();
    let report_text = report_path.to_string_lossy().into_owned();
    let profile_text = match profile {
        BuildProfile::Development => "development",
        BuildProfile::Release => "release",
    };
    run_bun(
        root,
        &[
            "run",
            builder_text.as_str(),
            source,
            artifact_text.as_str(),
            report_text.as_str(),
            profile_text,
        ],
        "compile Bun Plugin declarations",
    )?;
    let report_metadata =
        fs::symlink_metadata(&report_path).context("inspect Bun Plugin build report")?;
    if !report_metadata.file_type().is_file() {
        bail!("Bun Plugin build report must be a regular file");
    }
    if report_metadata.len() > 8 * 1024 * 1024 {
        bail!("Bun Plugin build report exceeds 8 MiB");
    }
    let build_report = fs::read(&report_path).context("read Bun Plugin build report")?;
    let report: BunBuildReport =
        serde_json::from_slice(&build_report).context("parse Bun Plugin build report")?;
    if !is_sha256_digest(&report.fingerprint) {
        bail!("Bun Plugin build returned an invalid source fingerprint");
    }
    let descriptor = report.descriptor;
    validate_capabilities(&descriptor)?;
    for capability in &descriptor.capabilities {
        if !capability
            .descriptor_digest
            .as_deref()
            .is_some_and(is_sha256_digest)
        {
            bail!(
                "Bun Plugin Capability `{}` has an invalid Descriptor digest",
                capability.capability_id
            );
        }
    }
    // Preserve the generated runtime declaration inside the digest-addressed
    // artifact so a precompiled Host can build exact JSON codecs without Cargo.
    let declaration = serde_json::to_string(&descriptor)?;
    if declaration.len() > 1024 * 1024 {
        bail!("Bun runtime declaration exceeds 1 MiB");
    }
    let body = fs::read(&artifact)?;
    let mut encoded = format!("// lenso-runtime-descriptor.v1:{declaration}\n").into_bytes();
    encoded.extend(body);
    fs::write(&artifact, encoded)?;
    let contract = contract_from_bun_descriptor(package, &descriptor)?;
    let verified = build_source_plugin_release_bundle(&SourcePluginReleaseBuild {
        contract,
        implementations: vec![SourcePluginImplementation {
            id: "bun".to_owned(),
            host_targets: vec!["*".to_owned()],
            artifact,
            bundle_path: "implementations/bun/plugin.js".to_owned(),
            media_type: "application/javascript".to_owned(),
            target: "javascript-bun".to_owned(),
            entrypoint: "plugin.js".to_owned(),
            execution_class: ExecutionClassId::bun_child_process(),
            runtime_profile: "lenso.bun-authoring@2".to_owned(),
        }],
        output: output.to_path_buf(),
    })?;
    Ok((verified, descriptor))
}

fn contract_from_bun_descriptor(
    package: &BunPackage,
    descriptor: &PluginDescriptor,
) -> anyhow::Result<PluginContract> {
    let mut capabilities = descriptor.capabilities.iter().collect::<Vec<_>>();
    capabilities.sort_by_key(|capability| &capability.capability_id);
    let mut contract = capabilities.into_iter().fold(
        PluginContract::new(
            &package.metadata.plugin_id,
            &package.version,
            &package.metadata.root_slot,
        )
        .with_authoring_version(2),
        |contract, capability| {
            let endpoint = CapabilityEndpointPlan::new(
                &capability.capability_id,
                &capability.descriptor_version,
                {
                    let mut operations = capability.request_operations.clone();
                    operations.extend(capability.stream_operations.clone());
                    operations.sort();
                    operations
                },
            );
            let endpoint = capability
                .stream_operations
                .iter()
                .fold(endpoint, |endpoint, operation| {
                    endpoint.with_stream_operation(operation)
                });
            contract.with_capability(endpoint)
        },
    );
    if let Some(schema) = &descriptor.configuration_schema {
        contract = contract.with_configuration_schema(schema.clone());
    }
    let mut requirements = descriptor.required_capabilities.iter().collect::<Vec<_>>();
    requirements.sort_by_key(|requirement| &requirement.requirement_id);
    for requirement in requirements {
        let constructor = match requirement.cardinality.as_str() {
            "one" => CapabilityRequirementPlan::one,
            "optional" => CapabilityRequirementPlan::optional,
            "many" => CapabilityRequirementPlan::many,
            other => bail!("unsupported Bun dependency cardinality {other}"),
        };
        contract = contract.with_requirement(
            constructor(&requirement.capability_id, &requirement.descriptor_version)
                .with_requirement_id(&requirement.requirement_id),
        );
    }
    Ok(contract)
}

pub fn materialize(
    root: &Path,
    output: &Path,
    profile: BuildProfile,
) -> anyhow::Result<VerifiedBundle> {
    if let Some(package) = read_bun_package(root)? {
        return materialize_bun(root, output, &package, profile).map(|(verified, _)| verified);
    }
    let package = read_package(&root.join("Cargo.toml"))?;
    synchronize_plugin_lock(root, &package)?;
    let target_directory = cargo_target_directory(root)?;
    match project_runtime(&package)? {
        ProjectRuntime::Composite => materialize_composite(root, output, &package, profile),
        ProjectRuntime::Multi => {
            materialize_multi(root, output, &package, &target_directory, profile)
        }
        ProjectRuntime::Wasm => {
            materialize_wasm(root, output, &package, &target_directory, profile, false)
        }
        ProjectRuntime::Process => {
            materialize_process(root, output, &package, &target_directory, profile)
        }
        ProjectRuntime::Bun => unreachable!("Bun projects do not use Cargo metadata"),
    }
    .with_context(|| format!("package Plugin `{}`", package.metadata.lenso.plugin_id))
}

fn materialize_dev(
    root: &Path,
    output: &Path,
    package: &CargoPackage,
    declared_runtime: ProjectRuntime,
    build: DevBuild,
    profile: BuildProfile,
) -> anyhow::Result<VerifiedBundle> {
    synchronize_plugin_lock(root, package)?;
    let target_directory = cargo_target_directory(root)?;
    match build {
        DevBuild::All => materialize_multi(root, output, package, &target_directory, profile),
        DevBuild::Wasm => materialize_wasm(
            root,
            output,
            package,
            &target_directory,
            profile,
            declared_runtime == ProjectRuntime::Multi,
        ),
        DevBuild::Process => materialize_process(root, output, package, &target_directory, profile),
    }
    .with_context(|| {
        format!(
            "package Plugin `{}` for development",
            package.metadata.lenso.plugin_id
        )
    })
}

fn materialize_wasm(
    root: &Path,
    output: &Path,
    package: &CargoPackage,
    target_directory: &Path,
    profile: BuildProfile,
    explicit_library_target: bool,
) -> anyhow::Result<VerifiedBundle> {
    let arguments = if explicit_library_target {
        profile.cargo_args(["--lib", "--target", WASM_TARGET])
    } else {
        profile.cargo_args(["--target", WASM_TARGET])
    };
    run_cargo(root, &arguments, "build Plugin Wasm implementation")?;
    let artifact = target_directory
        .join(WASM_TARGET)
        .join(profile.directory())
        .join(format!("{}.wasm", package.name.replace('-', "_")));
    Ok(build_source_plugin_bundle(&SourcePluginBuild {
        package_manifest: root.join("Cargo.toml"),
        wasm_module: artifact,
        output: output.to_path_buf(),
    })?)
}

fn materialize_process(
    root: &Path,
    output: &Path,
    package: &CargoPackage,
    target_directory: &Path,
    profile: BuildProfile,
) -> anyhow::Result<VerifiedBundle> {
    let arguments = profile.cargo_args(["--bin", package.name.as_str()]);
    run_cargo(root, &arguments, "build Plugin Process implementation")?;
    let executable = target_directory
        .join(profile.directory())
        .join(&package.name);
    let source = dev::read_process_descriptor(&executable)?;
    let descriptor = tempfile::NamedTempFile::new().context("stage Process descriptor")?;
    serde_json::to_writer(descriptor.as_file(), &source.descriptor)?;
    Ok(build_source_process_plugin_bundle(
        &SourceProcessPluginBuild {
            package_manifest: root.join("Cargo.toml"),
            executable,
            runtime_descriptor: descriptor.path().to_path_buf(),
            authoring_version: source.authoring_version,
            runtime_profile: source.runtime_profile,
            target: rust_host_target(root)?,
            output: output.to_path_buf(),
        },
    )?)
}

fn shared_portable_contract(
    mut left: PluginContract,
    mut right: PluginContract,
) -> anyhow::Result<PluginContract> {
    // Authoring v2 changes requirement identity. With no requirements, promotion
    // preserves semantics; all remaining product Contract fields must still match.
    if left.authoring_version() != right.authoring_version()
        && matches!(left.authoring_version(), 1 | 2)
        && matches!(right.authoring_version(), 1 | 2)
        && left.required_capabilities().is_empty()
        && right.required_capabilities().is_empty()
    {
        left = left.with_authoring_version(2);
        right = right.with_authoring_version(2);
    }
    if left != right {
        bail!("Plugin implementations do not expose the same Contract");
    }
    Ok(left)
}

fn materialize_multi(
    root: &Path,
    output: &Path,
    package: &CargoPackage,
    target_directory: &Path,
    profile: BuildProfile,
) -> anyhow::Result<VerifiedBundle> {
    let wasm_arguments = profile.cargo_args(["--lib", "--target", WASM_TARGET]);
    run_cargo(root, &wasm_arguments, "build Plugin Wasm implementation")?;
    let process_arguments = profile.cargo_args(["--bin", package.name.as_str()]);
    run_cargo(
        root,
        &process_arguments,
        "build Plugin Process implementation",
    )?;
    let staging = tempfile::tempdir().context("stage Plugin implementations")?;
    let wasm_bundle = staging.path().join("wasm");
    build_source_plugin_bundle(&SourcePluginBuild {
        package_manifest: root.join("Cargo.toml"),
        wasm_module: target_directory
            .join(WASM_TARGET)
            .join(profile.directory())
            .join(format!("{}.wasm", package.name.replace('-', "_"))),
        output: wasm_bundle.clone(),
    })?;
    let process_bundle = staging.path().join("process");
    let host_target = rust_host_target(root)?;
    let executable = target_directory
        .join(profile.directory())
        .join(&package.name);
    let runtime_descriptor = staging.path().join("process-descriptor.json");
    let source = dev::read_process_descriptor(&executable)?;
    fs::write(&runtime_descriptor, serde_json::to_vec(&source.descriptor)?)?;
    build_source_process_plugin_bundle(&SourceProcessPluginBuild {
        package_manifest: root.join("Cargo.toml"),
        executable,
        runtime_descriptor,
        authoring_version: source.authoring_version,
        runtime_profile: source.runtime_profile,
        target: host_target.clone(),
        output: process_bundle.clone(),
    })?;
    let wasm_descriptor = v2_descriptor(&wasm_bundle)?;
    let process_descriptor = v2_descriptor(&process_bundle)?;
    let contract =
        shared_portable_contract(wasm_descriptor.contract(), process_descriptor.contract())?;
    let process_name = if cfg!(windows) {
        "plugin.exe"
    } else {
        "plugin"
    };
    Ok(build_source_plugin_release_bundle(
        &SourcePluginReleaseBuild {
            contract,
            implementations: vec![
                SourcePluginImplementation {
                    id: "wasm".to_owned(),
                    host_targets: vec!["*".to_owned()],
                    artifact: wasm_bundle.join("plugin.wasm"),
                    bundle_path: "implementations/wasm/plugin.wasm".to_owned(),
                    media_type: "application/wasm".to_owned(),
                    target: WASM_TARGET.to_owned(),
                    entrypoint: "plugin".to_owned(),
                    execution_class: ExecutionClassId::new(WASM_EXECUTION_CLASS),
                    runtime_profile: wasm_descriptor.runtime_profile().to_owned(),
                },
                SourcePluginImplementation {
                    id: "process".to_owned(),
                    host_targets: vec![host_target.clone()],
                    artifact: process_bundle.join(process_name),
                    bundle_path: format!("implementations/process/{process_name}"),
                    media_type: "application/vnd.lenso.process".to_owned(),
                    target: host_target,
                    entrypoint: "plugin".to_owned(),
                    execution_class: ExecutionClassId::new(PROCESS_EXECUTION_CLASS),
                    runtime_profile: process_descriptor.runtime_profile().to_owned(),
                },
            ],
            output: output.to_path_buf(),
        },
    )?)
}

fn materialize_composite(
    root: &Path,
    output: &Path,
    package: &CargoPackage,
    profile: BuildProfile,
) -> anyhow::Result<VerifiedBundle> {
    let declarations = &package
        .metadata
        .lenso_cli
        .as_ref()
        .expect("composite projects have CLI metadata")
        .implementations;
    if declarations.len() < 2 {
        bail!("composite Plugin projects require at least two implementations");
    }
    let staging = tempfile::tempdir().context("stage declared Plugin implementations")?;
    let mut contract = None::<PluginContract>;
    let mut implementations = Vec::with_capacity(declarations.len());
    let mut ids = BTreeSet::new();
    for declaration in declarations {
        if declaration.id.is_empty()
            || !declaration
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !ids.insert(declaration.id.as_str())
        {
            bail!(
                "invalid or duplicate Plugin implementation id `{}`",
                declaration.id
            );
        }
        let (candidate_contract, source) =
            materialize_declared_implementation(root, staging.path(), declaration, profile)?;
        if let Some(expected) = &contract {
            if expected != &candidate_contract {
                bail!("declared Plugin implementations do not expose the same Contract");
            }
        } else {
            contract = Some(candidate_contract);
        }
        implementations.push(source);
    }
    build_source_plugin_release_bundle(&SourcePluginReleaseBuild {
        contract: contract.expect("composite Plugin has implementations"),
        implementations,
        output: output.to_path_buf(),
    })
    .map_err(Into::into)
}

fn materialize_declared_implementation(
    root: &Path,
    staging: &Path,
    declaration: &LensoCliImplementation,
    profile: BuildProfile,
) -> anyhow::Result<(PluginContract, SourcePluginImplementation)> {
    let implementation_root = implementation_root(root, &declaration.path)?;
    let implementation_bundle = staging.join(&declaration.id);
    match declaration.runtime.as_str() {
        "process" => {
            let implementation_package = read_package(&implementation_root.join("Cargo.toml"))?;
            let target_directory = cargo_target_directory(&implementation_root)?;
            materialize_process(
                &implementation_root,
                &implementation_bundle,
                &implementation_package,
                &target_directory,
                profile,
            )?;
            let descriptor = v2_descriptor(&implementation_bundle)?;
            let filename = if cfg!(windows) {
                "plugin.exe"
            } else {
                "plugin"
            };
            let host_target = rust_host_target(&implementation_root)?;
            let source = SourcePluginImplementation {
                id: declaration.id.clone(),
                host_targets: vec![host_target.clone()],
                artifact: implementation_bundle.join(filename),
                bundle_path: format!("implementations/{}/{filename}", declaration.id),
                media_type: "application/vnd.lenso.process".to_owned(),
                target: host_target,
                entrypoint: descriptor.implementation().entrypoint().to_owned(),
                execution_class: descriptor.implementation().execution_class().clone(),
                runtime_profile: descriptor.runtime_profile().to_owned(),
            };
            Ok((descriptor.contract(), source))
        }
        "bun" => {
            let implementation_package =
                read_bun_package(&implementation_root)?.ok_or_else(|| {
                    anyhow::anyhow!("Bun implementation has no Lenso package metadata")
                })?;
            materialize_bun(
                &implementation_root,
                &implementation_bundle,
                &implementation_package,
                profile,
            )?;
            let PluginManifest::V4(manifest) = read_bundle_manifest(&implementation_bundle)? else {
                bail!("Bun implementation did not produce Bundle 4");
            };
            let [implementation] = manifest.implementations.as_slice() else {
                bail!("Bun implementation must produce exactly one artifact");
            };
            let filename = Path::new(&implementation.artifact.path)
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    anyhow::anyhow!("implementation artifact has no portable filename")
                })?;
            let source = SourcePluginImplementation {
                id: declaration.id.clone(),
                host_targets: implementation.host_targets.clone(),
                artifact: implementation_bundle.join(&implementation.artifact.path),
                bundle_path: format!("implementations/{}/{filename}", declaration.id),
                media_type: implementation.artifact.media_type.clone(),
                target: implementation.artifact.target.clone(),
                entrypoint: implementation.runtime.entrypoint().to_owned(),
                execution_class: implementation.runtime.execution_class().clone(),
                runtime_profile: implementation.runtime.runtime_profile().to_owned(),
            };
            Ok((manifest.contract, source))
        }
        runtime => bail!("unsupported declared Plugin implementation runtime `{runtime}`"),
    }
}

fn v2_descriptor(root: &Path) -> anyhow::Result<lenso_app_plan::authoring::PluginDescriptor> {
    let PluginManifest::V2(manifest) = read_bundle_manifest(root)? else {
        bail!("implementation staging Bundle did not use V2")
    };
    serde_json::from_value(manifest.entry.descriptor)
        .context("parse staged Plugin implementation descriptor")
}

fn synchronize_plugin_lock(root: &Path, package: &CargoPackage) -> anyhow::Result<()> {
    run_cargo(
        root,
        &[
            "update",
            "--offline",
            "--package",
            &package.name,
            "--precise",
            &package.version,
        ],
        "synchronize Plugin version in Cargo.lock",
    )
}

fn cargo_target_directory(root: &Path) -> anyhow::Result<PathBuf> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--locked", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .context("inspect Plugin Cargo target directory")?;
    if !output.status.success() {
        bail!(
            "inspect Plugin Cargo target directory failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let metadata: CargoTargetMetadata =
        serde_json::from_slice(&output.stdout).context("parse Plugin Cargo metadata")?;
    Ok(metadata.target_directory)
}

fn read_package(manifest: &Path) -> anyhow::Result<CargoPackage> {
    let bytes = fs::read(manifest)
        .with_context(|| format!("read Plugin manifest {}", manifest.display()))?;
    let document: CargoDocument =
        toml::from_slice(&bytes).context("parse Plugin Cargo manifest")?;
    warn_for_legacy_plugin_id(&document.package.metadata.lenso.plugin_id)?;
    validate_release_version(&document.package.version)?;
    Ok(document.package)
}

fn project_runtime(package: &CargoPackage) -> anyhow::Result<ProjectRuntime> {
    let Some(metadata) = package.metadata.lenso_cli.as_ref() else {
        return Ok(ProjectRuntime::Wasm);
    };
    if !metadata.implementations.is_empty() {
        if metadata.runtime.is_some() || !metadata.outputs.is_empty() {
            bail!("Plugin implementations cannot be combined with runtime or outputs");
        }
        return Ok(ProjectRuntime::Composite);
    }
    if !metadata.outputs.is_empty() {
        return match metadata.outputs.as_slice() {
            [output] if output == "wasm" => Ok(ProjectRuntime::Wasm),
            [output] if output == "process" => Ok(ProjectRuntime::Process),
            [wasm, process] if wasm == "wasm" && process == "process" => Ok(ProjectRuntime::Multi),
            outputs => bail!("unsupported Plugin implementation outputs `{outputs:?}`"),
        };
    }
    match metadata.runtime.as_deref().unwrap_or("wasm") {
        "wasm" => Ok(ProjectRuntime::Wasm),
        "process" => Ok(ProjectRuntime::Process),
        "bun" => Ok(ProjectRuntime::Bun),
        runtime => bail!("unsupported Plugin project runtime `{runtime}`"),
    }
}

fn implementation_root(root: &Path, declared: &Path) -> anyhow::Result<PathBuf> {
    if declared.is_absolute()
        || declared
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("Plugin implementation path must stay inside the project root");
    }
    let canonical_root = fs::canonicalize(root).context("resolve Plugin project root")?;
    let candidate = fs::canonicalize(root.join(declared)).with_context(|| {
        format!(
            "resolve Plugin implementation path `{}`",
            declared.display()
        )
    })?;
    if !candidate.starts_with(&canonical_root) {
        bail!("Plugin implementation path must stay inside the project root");
    }
    Ok(candidate)
}

fn resolve_dev_selection(
    declared: ProjectRuntime,
    implementation: DevImplementationArg,
) -> anyhow::Result<DevSelection> {
    match (declared, implementation) {
        (ProjectRuntime::Composite, _) => {
            bail!("composite Plugin development is dispatched from its declarations")
        }
        (ProjectRuntime::Multi, DevImplementationArg::Auto | DevImplementationArg::Process) => {
            Ok(DevSelection {
                build: DevBuild::Process,
                invoke: ProjectRuntime::Process,
            })
        }
        (ProjectRuntime::Multi, DevImplementationArg::Wasm)
        | (
            ProjectRuntime::Wasm,
            DevImplementationArg::Auto | DevImplementationArg::Wasm | DevImplementationArg::All,
        ) => Ok(DevSelection {
            build: DevBuild::Wasm,
            invoke: ProjectRuntime::Wasm,
        }),
        (ProjectRuntime::Multi, DevImplementationArg::All) => Ok(DevSelection {
            build: DevBuild::All,
            invoke: ProjectRuntime::Process,
        }),
        (
            ProjectRuntime::Process,
            DevImplementationArg::Auto | DevImplementationArg::Process | DevImplementationArg::All,
        ) => Ok(DevSelection {
            build: DevBuild::Process,
            invoke: ProjectRuntime::Process,
        }),
        (ProjectRuntime::Wasm, DevImplementationArg::Process | DevImplementationArg::Bun) => {
            bail!("Plugin project declares only a Wasm implementation")
        }
        (ProjectRuntime::Process, DevImplementationArg::Wasm | DevImplementationArg::Bun) => {
            bail!("Plugin project declares only a Process implementation")
        }
        (ProjectRuntime::Multi, DevImplementationArg::Bun) => {
            bail!("Plugin project declares only Rust implementations")
        }
        (ProjectRuntime::Bun, _) => bail!("Bun development is dispatched separately"),
    }
}

fn parse_descriptor_bytes(bytes: &[u8]) -> anyhow::Result<PluginDescriptor> {
    let descriptor: PluginDescriptor =
        serde_json::from_slice(bytes).context("parse generated Plugin descriptor")?;
    if !matches!(
        descriptor.abi.as_str(),
        "lenso.json-request@1" | "lenso.json-interactions@1" | "lenso.json-host-imports@2"
    ) {
        bail!(
            "unsupported Plugin descriptor ABI `{}`; expected request V1 or host-imports V2",
            descriptor.abi
        );
    }
    validate_capabilities(&descriptor)?;
    Ok(descriptor)
}

fn parse_descriptor(component: &[u8]) -> anyhow::Result<PluginDescriptor> {
    let bytes = extract_plugin_descriptor(component)?;
    parse_descriptor_bytes(&bytes)
}

fn validate_capabilities(descriptor: &PluginDescriptor) -> anyhow::Result<()> {
    if descriptor.capabilities.len() > 256 {
        bail!("Plugin descriptor exceeds 256 provided Capabilities");
    }
    let mut seen = BTreeSet::new();
    for capability in &descriptor.capabilities {
        if !seen.insert(&capability.capability_id) {
            bail!(
                "Plugin descriptor repeats provided Capability `{}`",
                capability.capability_id
            );
        }
        if capability.request_operations.is_empty() && capability.stream_operations.is_empty() {
            bail!(
                "Plugin Capability `{}` must declare at least one Request or Stream operation",
                capability.capability_id
            );
        }
    }
    Ok(())
}

fn is_sha256_digest(value: &str) -> bool {
    value.starts_with("sha256:")
        && value.len() == 71
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn one_capability(descriptor: &PluginDescriptor) -> anyhow::Result<&PluginCapability> {
    let [capability] = descriptor.capabilities.as_slice() else {
        bail!("Plugin dev invocation requires exactly one provided Capability");
    };
    Ok(capability)
}

fn project_root(root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    root.map_or_else(
        || env::current_dir().context("resolve Plugin project root"),
        Ok,
    )
}

fn warn_for_legacy_plugin_id(plugin_id: &str) -> anyhow::Result<()> {
    if classify_existing_plugin_id(plugin_id)? == PluginIdVersion::Legacy {
        eprintln!(
            "warning: `{plugin_id}` is a legacy unnamespaced Plugin id; migrate to a namespaced v1 id such as `company.{plugin_id}`"
        );
    }
    Ok(())
}

fn run_cargo(root: &Path, args: &[&str], action: &str) -> anyhow::Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| action.to_owned())?;
    if !status.success() {
        bail!("{action} failed with {status}");
    }
    Ok(())
}

fn run_bun(root: &Path, args: &[&str], action: &str) -> anyhow::Result<()> {
    let bun = env::var_os("BUN_BIN").unwrap_or_else(|| "bun".into());
    let status = Command::new(bun)
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| action.to_owned())?;
    if !status.success() {
        bail!("{action} failed with {status}");
    }
    Ok(())
}

fn print_verified(
    verified: &VerifiedBundle,
    output: Option<&Path>,
    json: bool,
) -> anyhow::Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "kind": "lenso.plugin-pack",
                "plugin_id": verified.plugin_id,
                "release_version": verified.release_version,
                "manifest_digest": verified.manifest_digest,
                "artifact_digests": verified.artifact_digests,
                "output": output.map(|path| path.display().to_string()),
            }))?
        );
    } else {
        println!("Packed {}@{}", verified.plugin_id, verified.release_version);
        if let Some(output) = output {
            println!("output {}", output.display());
        }
        println!("manifest {}", verified.manifest_digest);
    }
    Ok(())
}

#[cfg(test)]
mod tests;

pub fn local_runtime_descriptor(
    path: &Path,
    execution_class: &str,
) -> anyhow::Result<Option<Value>> {
    match execution_class {
        "lenso.bun-process@1" => {
            use std::io::{BufRead, Read};
            let mut line = Vec::new();
            std::io::BufReader::new(fs::File::open(path)?)
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut line)?;
            let prefix = b"// lenso-runtime-descriptor.v1:";
            if !line.starts_with(prefix) {
                return Ok(None);
            }
            if line.len() > 1024 * 1024 {
                bail!("Bun runtime declaration exceeds 1 MiB");
            }
            Ok(Some(serde_json::from_slice(&line[prefix.len()..])?))
        }
        "lenso.process@1" => Ok(Some(serde_json::to_value(
            dev::read_process_descriptor(path)?.descriptor,
        )?)),
        "lenso.wasm-component@1" => Ok(Some(serde_json::from_slice(
            &lenso_plugin_bundle::extract_plugin_descriptor(&fs::read(path)?)?,
        )?)),
        _ => Ok(None),
    }
}
