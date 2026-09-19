// Embedded by the local Host generator. Generated contract crates retain the
// same Cargo package identities as the native Plugin's normal dependencies.
use anyhow::{Context, bail};
// LENSO_NATIVE_RESOURCES
use lenso_app_plan::ResolvedAppPlan;
#[cfg(generated_native_host)]
use lenso_app_plan::authoring::HostCatalog;
use lenso_kernel::{ExecutionAdapterCatalog, Kernel, ShutdownOutcome};
#[cfg(generated_native_host)]
use lenso_native_adapter::NativePluginRegistry;
use lenso_runtime_codec::{ArtifactCatalog, ArtifactHandle};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, time::Duration};

#[derive(Deserialize)]
struct Resolution {
    schema: String,
    plan: ResolvedAppPlan,
}
#[derive(Deserialize)]
struct Artifact {
    plugin_id: String,
    artifact_digest: String,
    artifact_size: u64,
}
#[derive(Deserialize)]
struct FileProof {
    path: String,
    sha256: String,
    size: u64,
    role: String,
}
#[derive(Deserialize)]
struct DistributionLock {
    schema: String,
    files: Vec<FileProof>,
}

#[cfg(generated_native_host)]
fn main() -> anyhow::Result<()> {
    // LENSO_LINK_PLUGINS
    run(std::env::args().skip(1).collect())
}

pub fn run(args: Vec<String>) -> anyhow::Result<()> {
    #[cfg(generated_native_host)]
    if args == ["--describe"] {
        let catalog: HostCatalog =
            NativePluginRegistry::host_catalog([], []).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        // LENSO_DESCRIBE_WEB
        println!("{}", serde_json::to_string(&catalog)?);
        return Ok(());
    }
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let root = executable
        .parent()
        .and_then(|p| p.parent())
        .context("Host location")?;
    let mut intent = root.join("intent");
    let mut check = false;
    let mut command_args = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                index += 1;
                intent = PathBuf::from(args.get(index).context("--root needs a directory")?);
            }
            "--check" => check = true,
            "--" => { command_args = Some(args[index + 1..].to_vec()); break; },
            other => bail!("unknown Host argument: {other}"),
        }
        index += 1;
    }
    let lock: DistributionLock =
        serde_json::from_slice(&fs::read(root.join(".lenso/distribution.lock.json"))?)?;
    if lock.schema != "lenso.local-host-distribution.v1" || lock.files.len() > 2048 {
        bail!("unsupported local Host distribution lock");
    }
    let mut locked = std::collections::BTreeSet::new();
    for file in lock.files {
        if file.path.contains('\\')
            || !std::path::Path::new(&file.path)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            || !locked.insert(file.path.clone())
            || file.role.is_empty()
        {
            bail!("invalid distribution file: {}", file.path);
        }
        let path = root.join(&file.path);
        if !fs::symlink_metadata(&path)?.file_type().is_file() {
            bail!("runtime file is not regular: {}", file.path);
        }
        let bytes = fs::read(path)?;
        if bytes.len() as u64 != file.size
            || format!(
                "sha256:{}",
                Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ) != file.sha256
        {
            bail!("runtime file changed: {}", file.path);
        }
    }
    for required in [
        ".lenso/host",
        ".lenso/host-build.json",
        "runtime/lenso-resolver",
        "bundles.json",
        "runtime-codecs.json",
        ".lenso/host-mode",
    ] {
        if !locked.contains(required) {
            bail!("distribution missing locked file: {required}");
        }
    }
    let output = std::process::Command::new(root.join("runtime/lenso-resolver"))
        .args(["app", "show", "--runtime-json", "--host-build"])
        .arg(root.join(".lenso/host-build.json"))
        .arg("--root")
        .arg(&intent)
        .output()?;
    if !output.status.success() {
        bail!("resolve App: {}", String::from_utf8_lossy(&output.stderr));
    }
    let resolution: Resolution = serde_json::from_slice(&output.stdout)?;
    if resolution.schema != "lenso.runtime-app-resolution.v1" {
        bail!("unsupported resolver schema");
    }
    let inventory: Vec<Artifact> = serde_json::from_slice(&fs::read(root.join("bundles.json"))?)?;
    let mut artifacts = ArtifactCatalog::new();
    for instance in resolution.plan.plugin_instances() {
        if instance.execution_class().as_str() == "lenso.native-rust@1" {
            continue;
        }
        let artifact = inventory
            .iter()
            .find(|a| a.plugin_id == instance.package_id())
            .with_context(|| format!("missing built artifact for {}", instance.package_id()))?;
        let path = format!("runtime/artifacts/{}", artifact.plugin_id);
        if !locked.contains(&path) {
            bail!("selected artifact is not locked: {path}");
        }
        if instance.execution_class().as_str() == "lenso.bun-process@1"
            && !locked.contains("runtime/bun")
        {
            bail!("Bun runtime is not locked");
        }
        artifacts = artifacts
            .with_artifact(
                instance.instance_key(),
                ArtifactHandle::open(
                    root.join(&path),
                    &artifact.artifact_digest,
                    artifact.artifact_size,
                )
                .map_err(|e| anyhow::anyhow!("{e:?}"))?,
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    #[cfg(generated_native_host)]
    let native = {
        let mut resources = native_resources::InstanceResourceCatalog::new();
        for instance in resolution.plan.plugin_instances() {
            if instance.execution_class().as_str() != "lenso.native-rust@1" {
                continue;
            }
            let directory = intent.join("plugins").join(instance.instance_key());
            if directory.try_exists()? {
                let mut files = Vec::new();
                read_resources(&directory, &directory, &mut files, &mut 0, 0)?;
                let snapshot = native_resources::InstanceResources::from_files(files)
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                resources = resources
                    .with_resources(instance.instance_key(), snapshot)
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            }
        }
        NativePluginRegistry::new()
            .with_linked_factories()
            .with_resources(resources)
    };
    // LENSO_RUNTIME_WEB
    let bun = lenso_bun_adapter::BunAdapter::production(root.join("runtime/bun"))
        .with_artifacts(artifacts.clone());
    let process = lenso_process_adapter::ProcessAdapter::new(artifacts.clone());
    let wasm = lenso_wasm_component_adapter::WasmComponentAdapter::new(artifacts);
    #[cfg(not(generated_native_host))]
    let typed = std::collections::BTreeSet::from([super::terminal::command::CAPABILITY_ID, super::terminal::provider::CAPABILITY_ID]);
    #[cfg(not(generated_native_host))]
    let bun = bun.with_authoring_codec(super::terminal::command::CommandJsonCodec).with_authoring_codec(super::terminal::provider::CommandProviderJsonCodec);
    #[cfg(not(generated_native_host))]
    let process = process.with_codec(super::terminal::command::CommandJsonCodec).with_codec(super::terminal::provider::CommandProviderJsonCodec);
    #[cfg(not(generated_native_host))]
    let wasm = wasm.with_codec(super::terminal::command::CommandJsonCodec).with_codec(super::terminal::provider::CommandProviderJsonCodec);
    // LENSO_REGISTER_CODECS
    let evidence = serde_json::from_slice(&fs::read(root.join("runtime-codecs.json"))?)?;
    let mut bun = bun;
    let mut process = process;
    let mut wasm = wasm;
    for codec in portable_codecs(&resolution.plan, &typed, &evidence)? {
        bun = bun
            .with_codec(LegacyBunCodec(codec.clone()))
            .with_authoring_codec(codec.clone());
        process = process.with_codec(codec.clone());
        wasm = wasm.with_codec(codec);
    }
    let catalog = ExecutionAdapterCatalog::new();
    #[cfg(generated_native_host)]
    let catalog = catalog
        .with_adapter(native)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let catalog = catalog
        .with_adapter(bun)
        .and_then(|c| c.with_adapter(process))
        .and_then(|c| c.with_adapter(wasm))
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    // Drive the local Kernel outside Tokio's block_on execution context: Bun's
    // synchronous startup handshake owns a separate RPC runtime. Tokio workers
    // still service I/O/timers, and LocalSet retains thread-local Plugin state.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _entered = runtime.enter();
    futures::executor::block_on(
        tokio::task::LocalSet::new().run_until(async move {
            let app = Kernel::start(resolution.plan, lenso_runner::TokioDriver::new(), catalog)
                .await.map_err(|e| anyhow::anyhow!("Host startup failed: {e:?}"))?;
            eprintln!("Local App ready");
            // LENSO_WEB_READY
            let mut command_result: anyhow::Result<()> = Ok(());
            #[cfg(not(generated_native_host))]
            if let Some(args) = &command_args { command_result = super::terminal::run(&app, args).await; }
            // LENSO_TERMINAL_RUN
            if !check && command_args.is_none() {
                #[cfg(unix)]
                let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
                loop {
                    tokio::select! {
                        signal = tokio::signal::ctrl_c() => { signal?; break; }
                        _ = async { #[cfg(unix)] { terminate.recv().await; }
                            #[cfg(not(unix))] { std::future::pending::<()>().await; } } => break,
                        () = tokio::time::sleep(Duration::from_millis(50)) => { if app.is_failed() { break; } }
                    }
                }
            }
            let failure = app.terminal_failure();
            let outcome = app.shutdown(Duration::from_secs(10)).await;
            if let Some(error) = failure { bail!("App failed: {error:?}; shutdown: {outcome:?}"); }
            if outcome != ShutdownOutcome::Clean { bail!("App shutdown failed: {outcome:?}"); }
            eprintln!("Local App stopped cleanly");
            command_result
        })
    )
}

#[cfg(generated_native_host)]
fn read_resources(
    root: &std::path::Path,
    directory: &std::path::Path,
    files: &mut Vec<(String, Vec<u8>)>,
    total: &mut usize,
    depth: usize,
) -> anyhow::Result<()> {
    if depth > 32 || !fs::symlink_metadata(directory)?.is_dir() {
        bail!("invalid Plugin resource directory");
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            read_resources(root, &path, files, total, depth + 1)?;
        } else if metadata.is_file() {
            if files.len() >= 4096 || metadata.len() > 1024 * 1024 {
                bail!("Plugin resource exceeds snapshot limits");
            }
            let bytes = fs::read(&path)?;
            *total += bytes.len();
            if *total > 16 * 1024 * 1024 {
                bail!("Plugin resources exceed 16 MiB");
            }
            files.push((
                path.strip_prefix(root)?
                    .to_str()
                    .context("resource path must be UTF-8")?
                    .replace('\\', "/"),
                bytes,
            ));
        } else {
            bail!("Plugin resource must be a regular file or directory");
        }
    }
    Ok(())
}

#[cfg(not(generated_native_host))]
pub fn validate(
    plan: &ResolvedAppPlan,
    evidence: &std::collections::BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<()> {
    portable_codecs(plan, &std::collections::BTreeSet::from([super::terminal::command::CAPABILITY_ID, super::terminal::provider::CAPABILITY_ID]), evidence).map(|_| ())
}
