use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use clap::Args;
use lenso_app_plan::ExecutionClassId;
use lenso_plugin_bundle::{
    ImplementationPolicy, RuntimeAdmission, read_bundle_manifest, resolve_implementation,
    verify_bundle_directory,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::archive::with_bundle_directory;
use lenso_app_authoring::host_authoring::GeneratedHostBuild;

#[derive(Args, Clone, Debug)]
pub struct PrepareArgs {
    /// Host authoring output created by `lenso app build`.
    #[arg(long)]
    build: PathBuf,
    /// Exact supported Rust target triple.
    #[arg(long)]
    target: String,
    /// Precompiled Host runtime for the target.
    #[arg(long)]
    runtime: PathBuf,
    /// Precompiled native process owner for the target.
    #[arg(long)]
    owner: PathBuf,
    /// Same-cohort precompiled resolver CLI for the target.
    #[arg(long)]
    resolver: PathBuf,
    /// Exact Bun executable, required when the App selects Bun implementations.
    #[arg(long)]
    bun: Option<PathBuf>,
    /// Redistribution and third-party notices for this exact artifact cohort.
    #[arg(long)]
    notices: PathBuf,
    /// New prepared distribution directory. Existing output is never overwritten.
    #[arg(long)]
    out: PathBuf,
    /// Generated npm control library directory. Supplied automatically by @lenso/cli.
    #[arg(long, hide = true)]
    library: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleInventory {
    path: String,
    plugin_id: String,
    release_version: String,
    manifest_digest: String,
    execution_class: String,
    runtime_profile: String,
    target: String,
    implementation_id: String,
    artifact_path: String,
    artifact_digest: String,
    artifact_size: u64,
    artifact_media_type: String,
    artifact_target: String,
}

#[derive(Debug, Serialize)]
pub(super) struct DistributionLock {
    pub(super) schema: &'static str,
    pub(super) app_id: String,
    pub(super) target: String,
    pub(super) platform: &'static str,
    pub(super) arch: &'static str,
    pub(super) files: Vec<DistributionFile>,
}

#[derive(Debug, Serialize)]
pub(super) struct DistributionFile {
    pub(super) path: String,
    pub(super) role: String,
    pub(super) sha256: String,
    pub(super) size: u64,
    pub(super) executable: bool,
}

#[derive(Debug)]
struct PreparedInputs {
    build_root: PathBuf,
    authority_path: PathBuf,
    authority: GeneratedHostBuild,
    bundle_inventory_path: PathBuf,
    bundles: Vec<BundleInventory>,
    runtime: PathBuf,
    owner: PathBuf,
    resolver: PathBuf,
    bun: Option<PathBuf>,
    notices: PathBuf,
    host_js: PathBuf,
    host_app: PathBuf,
    host_owner: PathBuf,
    platform: &'static str,
    arch: &'static str,
}

pub fn prepare(args: PrepareArgs) -> anyhow::Result<()> {
    let inputs = validate_inputs(&args)?;
    let destination = std::path::absolute(&args.out)?;
    if fs::symlink_metadata(&destination).is_ok() {
        bail!(
            "Host distribution already exists: {}",
            destination.display()
        );
    }
    let parent = destination
        .parent()
        .context("Host distribution needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".lenso-distribution-")
        .tempdir_in(parent)?;
    let files = stage_artifacts(stage.path(), &inputs)?;
    let lock = DistributionLock {
        schema: "lenso.host-distribution.v1",
        app_id: inputs.authority.host_id().to_owned(),
        target: args.target,
        platform: inputs.platform,
        arch: inputs.arch,
        files,
    };
    let bytes = serde_json::to_vec_pretty(&lock)?;
    let identity = sha256(&bytes);
    fs::write(stage.path().join(".lenso/distribution.lock.json"), bytes)?;
    super::build::publish_new_output(stage.path(), &destination)
        .context("publish prepared Host distribution")?;
    println!(
        "Prepared Host distribution at {} ({identity}).",
        destination.display()
    );
    Ok(())
}

fn validate_inputs(args: &PrepareArgs) -> anyhow::Result<PreparedInputs> {
    let build_root = fs::canonicalize(&args.build).context("locate Host authoring output")?;
    if !fs::metadata(&build_root)?.is_dir() {
        bail!(
            "Host authoring output must be a directory: {}",
            build_root.display()
        );
    }
    let (platform, arch) = target_platform(&args.target)?;
    let authority_path = regular_file(&build_root.join(".lenso/host-build.json"), false)?;
    let authority: GeneratedHostBuild = serde_json::from_slice(&fs::read(&authority_path)?)
        .context("invalid generated Host build")?;
    authority.validate()?;
    let bundle_inventory_path = regular_file(&build_root.join("bundles.json"), false)?;
    let bundles: Vec<BundleInventory> = serde_json::from_slice(&fs::read(&bundle_inventory_path)?)
        .context("invalid Host bundle inventory")?;
    if bundles.len() > 256 {
        bail!("Host bundle inventory exceeds 256 entries");
    }
    let mut bundle_paths = BTreeSet::new();
    let mut needs_bun = false;
    for bundle in &bundles {
        needs_bun |= validate_bundle(&build_root, args, &authority, bundle, &mut bundle_paths)?;
    }
    if needs_bun && args.bun.is_none() {
        bail!("the selected App contains Bun implementations; supply the exact --bun executable");
    }

    let runtime = regular_file(&args.runtime, true).context("validate Host runtime")?;
    let owner = regular_file(&args.owner, true).context("validate native process owner")?;
    let resolver = regular_file(&args.resolver, true).context("validate runtime resolver")?;
    let bun = args
        .bun
        .as_ref()
        .map(|path| regular_file(path, true).context("validate Bun runtime"))
        .transpose()?;
    let notices = regular_file(&args.notices, false).context("validate redistribution notices")?;
    if fs::metadata(&notices)?.len() == 0 {
        bail!("redistribution notices must not be empty");
    }
    let library = args
        .library
        .clone()
        .or_else(|| std::env::var_os("LENSO_HOST_DISTRIBUTION_LIB").map(PathBuf::from))
        .context(
            "prepared Host needs the generated npm control library; invoke through @lenso/cli",
        )?;
    let library = fs::canonicalize(library).context("locate generated Host control library")?;
    let host_js = regular_file(&library.join("distribution-host.js"), false)?;
    let host_app = regular_file(&library.join("host-app.js"), false)?;
    let host_owner = regular_file(&library.join("host-owner.js"), false)?;
    Ok(PreparedInputs {
        build_root,
        authority_path,
        authority,
        bundle_inventory_path,
        bundles,
        runtime,
        owner,
        resolver,
        bun,
        notices,
        host_js,
        host_app,
        host_owner,
        platform,
        arch,
    })
}

fn stage_artifacts(root: &Path, inputs: &PreparedInputs) -> anyhow::Result<Vec<DistributionFile>> {
    for directory in [".lenso", "artifacts", "bundles", "runtime"] {
        fs::create_dir(root.join(directory))?;
    }
    let mut files = Vec::new();
    copy_artifact(
        &inputs.authority_path,
        root,
        ".lenso/host-build.json",
        "host_authority",
        false,
        &mut files,
    )?;
    copy_artifact(
        &inputs.bundle_inventory_path,
        root,
        "bundles.json",
        "bundle_inventory",
        false,
        &mut files,
    )?;
    for (index, bundle) in inputs.bundles.iter().enumerate() {
        let source = regular_file(&inputs.build_root.join(&bundle.path), false)
            .with_context(|| format!("validate Plugin bundle {}", bundle.path))?;
        copy_artifact(
            &source,
            root,
            &bundle.path,
            "plugin_bundle",
            false,
            &mut files,
        )?;
        stage_selected_artifact(root, &inputs.build_root, bundle, index, &mut files)?;
    }
    stage_runtime_files(root, inputs, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn stage_runtime_files(
    root: &Path,
    inputs: &PreparedInputs,
    files: &mut Vec<DistributionFile>,
) -> anyhow::Result<()> {
    copy_artifact(
        &inputs.runtime,
        root,
        "runtime/lenso-host-runtime",
        "host_runtime",
        true,
        files,
    )?;
    copy_artifact(
        &inputs.owner,
        root,
        "runtime/lenso-process-owner",
        "process_owner",
        true,
        files,
    )?;
    copy_artifact(
        &inputs.resolver,
        root,
        "runtime/lenso-resolver",
        "runtime_resolver",
        true,
        files,
    )?;
    if let Some(bun) = &inputs.bun {
        copy_artifact(bun, root, "runtime/bun", "javascript_runtime", true, files)?;
    }
    copy_artifact(
        &inputs.host_app,
        root,
        "host-app.js",
        "control_library",
        false,
        files,
    )?;
    copy_artifact(
        &inputs.host_owner,
        root,
        "host-owner.js",
        "control_library",
        false,
        files,
    )?;
    copy_artifact(&inputs.host_js, root, "host.js", "entrypoint", true, files)?;
    copy_artifact(
        &inputs.notices,
        root,
        "THIRD_PARTY_NOTICES.txt",
        "notices",
        false,
        files,
    )?;
    Ok(())
}

fn validate_bundle(
    build_root: &Path,
    args: &PrepareArgs,
    authority: &GeneratedHostBuild,
    bundle: &BundleInventory,
    paths: &mut BTreeSet<String>,
) -> anyhow::Result<bool> {
    if bundle.target != args.target {
        bail!(
            "Plugin `{}` targets `{}` instead of `{}`",
            bundle.plugin_id,
            bundle.target,
            args.target
        );
    }
    if bundle.plugin_id.trim().is_empty()
        || bundle.release_version.trim().is_empty()
        || !bundle.manifest_digest.starts_with("sha256:")
    {
        bail!("invalid bundle inventory identity");
    }
    validate_relative(&bundle.path)?;
    if !bundle.path.starts_with("bundles/") || !bundle.path.ends_with(".lenso-plugin") {
        bail!("bundle inventory path is outside bundles/: {}", bundle.path);
    }
    if !paths.insert(bundle.path.clone()) {
        bail!("duplicate bundle inventory path: {}", bundle.path);
    }
    if !matches!(
        bundle.execution_class.as_str(),
        "lenso.bun-process@1" | "lenso.process@1"
    ) {
        bail!(
            "unsupported distribution execution class `{}`",
            bundle.execution_class
        );
    }
    let bundle_path = regular_file(&build_root.join(&bundle.path), false)
        .with_context(|| format!("validate Plugin bundle {}", bundle.path))?;
    with_bundle_directory(&bundle_path, |directory| {
        let verified = verify_bundle_directory(directory)?;
        if verified.plugin_id != bundle.plugin_id
            || verified.release_version != bundle.release_version
            || verified.manifest_digest != bundle.manifest_digest
        {
            bail!("bundle inventory identity differs from `{}`", bundle.path);
        }
        let manifest = read_bundle_manifest(directory)?;
        let selected = resolve_implementation(
            &manifest,
            &ImplementationPolicy {
                host_target: args.target.clone(),
                runtimes: vec![RuntimeAdmission {
                    execution_class: ExecutionClassId::new(&bundle.execution_class),
                    runtime_profile: bundle.runtime_profile.clone(),
                }],
            },
        )?;
        if selected.implementation_id != bundle.implementation_id
            || selected.artifact.path != bundle.artifact_path
            || selected.artifact.digest != bundle.artifact_digest
            || selected.artifact.size != bundle.artifact_size
            || selected.artifact.media_type != bundle.artifact_media_type
            || selected.artifact.target != bundle.artifact_target
        {
            bail!(
                "selected Artifact differs from bundle inventory `{}`",
                bundle.path
            );
        }
        authority.verify_distribution_bundle(&selected.descriptor, &verified.manifest_digest)
    })
    .with_context(|| format!("re-verify distribution bundle {}", bundle.path))?;
    Ok(bundle.execution_class == "lenso.bun-process@1")
}

fn stage_selected_artifact(
    root: &Path,
    build_root: &Path,
    bundle: &BundleInventory,
    index: usize,
    files: &mut Vec<DistributionFile>,
) -> anyhow::Result<()> {
    let name = Path::new(&bundle.artifact_path)
        .file_name()
        .and_then(|name| name.to_str())
        .context("selected Artifact needs a UTF-8 filename")?;
    let relative = format!("artifacts/{index}/{name}");
    with_bundle_directory(&build_root.join(&bundle.path), |directory| {
        let source = regular_file(
            &directory.join(&bundle.artifact_path),
            bundle.execution_class == "lenso.process@1",
        )?;
        copy_artifact(
            &source,
            root,
            &relative,
            "plugin_artifact",
            bundle.execution_class == "lenso.process@1",
            files,
        )
    })
}

pub(super) fn target_platform(target: &str) -> anyhow::Result<(&'static str, &'static str)> {
    match target {
        "aarch64-apple-darwin" => Ok(("darwin", "arm64")),
        "x86_64-unknown-linux-gnu" => Ok(("linux", "x64")),
        _ => bail!("unsupported first-release Host distribution target `{target}`"),
    }
}

fn validate_relative(path: &str) -> anyhow::Result<()> {
    let value = Path::new(path);
    if path.is_empty()
        || path.contains('\\')
        || value.is_absolute()
        || value
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("invalid distribution-relative path `{path}`");
    }
    Ok(())
}

fn regular_file(path: &Path, executable: bool) -> anyhow::Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect required artifact {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "required artifact must be a regular file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("required artifact is not executable: {}", path.display());
        }
    }
    Ok(path.to_path_buf())
}

fn copy_artifact(
    source: &Path,
    root: &Path,
    relative: &str,
    role: &str,
    executable: bool,
    files: &mut Vec<DistributionFile>,
) -> anyhow::Result<()> {
    validate_relative(relative)?;
    let destination = root.join(relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, &destination)?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = fs::metadata(&destination)?.permissions();
        permissions.set_mode(permissions.mode() | 0o500);
        fs::set_permissions(&destination, permissions)?;
    }
    let bytes = fs::read(&destination)?;
    files.push(DistributionFile {
        path: relative.to_owned(),
        role: role.to_owned(),
        sha256: sha256(&bytes),
        size: u64::try_from(bytes.len()).context("artifact exceeds u64")?,
        executable,
    });
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut value = String::with_capacity(71);
    value.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

#[cfg(test)]
mod tests;
