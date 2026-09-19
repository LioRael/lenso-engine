use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Write},
    process::Stdio,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use lenso_app_plan::{
    CapabilityBinding, CapabilityEndpointPlan, CapabilityRequirementPlan, PluginInstancePlan,
    ResolvedAppPlan,
};
use lenso_bun_adapter::{BUN_AUTHORING_RUNTIME_PROFILE, BunAdapter};
use lenso_kernel::{
    CancellationToken, ExecutionAdapter, ExecutionAdapterCatalog, Kernel, NoopPluginLifecycle,
    PreparedNativeApp, PreparedNativePlugin, RuntimeFailure, ShutdownOutcome,
};
use lenso_plugin_bundle::{
    ImplementationPolicy, ResolvedPluginImplementation, RuntimeAdmission, resolve_implementation,
};
use lenso_process_adapter::ProcessAdapter;
use lenso_runner::TokioDriver;
use lenso_runtime_codec::{ArtifactCatalog, ArtifactHandle, JsonCapabilityCodec};
use lenso_wasm_component_adapter::{WasmComponentAdapter, WasmComponentLimits};

use crate::watch::SourceWatcher;

use super::{
    BuildProfile, BunPackage, Command, DevImplementationArg, ExecutionClassId,
    PROCESS_EXECUTION_CLASS, PROCESS_RUNTIME_PROFILE_V1, PROCESS_RUNTIME_PROFILE_V2, Path,
    PluginCapability, PluginDescriptor, PluginDevArgs, ProjectRuntime, Value, VerifiedBundle,
    WASM_EXECUTION_CLASS, env, fs, implementation_root, materialize_bun, materialize_composite,
    materialize_dev, native_host_target, one_capability, parse_descriptor, project_root,
    project_runtime, read_bun_package, read_bundle_manifest, read_package, resolve_dev_selection,
};

pub(super) async fn run(args: PluginDevArgs) -> anyhow::Result<()> {
    let root = project_root(args.repo_root.clone())?;
    if super::web_dev::is_web_plugin(&root)? {
        return super::web_dev::run(args).await;
    }
    if !args.watch {
        return dev_once(&args).await;
    }
    let mut watcher = SourceWatcher::new(&root)?;
    dev_once(&args).await?;
    if !args.json {
        println!(
            "Watching {} for Plugin changes. Press Ctrl-C to stop.",
            root.display()
        );
    }
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("listen for Ctrl-C")?;
                return Ok(());
            }
            result = watcher.changed() => {
                result?;
                if let Err(error) = dev_once(&args).await {
                    eprintln!("Plugin rebuild failed: {error:#}");
                }
            }
        }
    }
}

async fn dev_once(args: &PluginDevArgs) -> anyhow::Result<()> {
    // Bun's synchronous startup handshake owns an RPC runtime. Drive the local
    // Kernel outside Tokio::block_on, while retaining I/O and timer workers.
    let args = args.clone();
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let result = {
            let _entered = runtime.enter();
            futures::executor::block_on(dev_once_inner(&args))
        };
        runtime.shutdown_background();
        result
    })
    .await
    .context("Plugin development worker panicked")?
}

async fn dev_once_inner(args: &PluginDevArgs) -> anyhow::Result<()> {
    let root = project_root(args.repo_root.clone())?;
    if let Some(package) = read_bun_package(&root)? {
        if !matches!(
            args.implementation,
            DevImplementationArg::Auto | DevImplementationArg::Bun | DevImplementationArg::All
        ) {
            bail!("Bun Plugin projects support `--implementation auto` or `all`");
        }
        return dev_bun(&root, &package, args).await;
    }
    let package = read_package(&root.join("Cargo.toml"))?;
    let declared_runtime = project_runtime(&package)?;
    if declared_runtime == ProjectRuntime::Composite {
        return dev_composite(&root, &package, args).await;
    }
    let selection = resolve_dev_selection(declared_runtime, args.implementation)?;
    dev_cargo(&root, &package, declared_runtime, selection, args).await
}

async fn dev_cargo(
    root: &Path,
    package: &super::CargoPackage,
    declared_runtime: ProjectRuntime,
    selection: super::DevSelection,
    args: &PluginDevArgs,
) -> anyhow::Result<()> {
    let dev_runtime = selection.invoke;
    let temporary = tempfile::tempdir().context("create Plugin dev directory")?;
    let output = temporary.path().join("dev.lenso-plugin");
    let verified = materialize_dev(
        root,
        &output,
        package,
        declared_runtime,
        selection.build,
        BuildProfile::Development,
    )?;
    let dev_class = if dev_runtime == ProjectRuntime::Process {
        PROCESS_EXECUTION_CLASS
    } else {
        WASM_EXECUTION_CLASS
    };
    let selected = resolve_implementation(
        &read_bundle_manifest(&output)?,
        &ImplementationPolicy {
            host_target: super::rust_host_target(root)?,
            runtimes: match dev_runtime {
                ProjectRuntime::Process => vec![
                    RuntimeAdmission {
                        execution_class: ExecutionClassId::new(dev_class),
                        runtime_profile: PROCESS_RUNTIME_PROFILE_V2.to_owned(),
                    },
                    RuntimeAdmission {
                        execution_class: ExecutionClassId::new(dev_class),
                        runtime_profile: PROCESS_RUNTIME_PROFILE_V1.to_owned(),
                    },
                ],
                ProjectRuntime::Wasm => vec![RuntimeAdmission {
                    execution_class: ExecutionClassId::new(dev_class),
                    runtime_profile: "lenso.wasm-component@1".to_owned(),
                }],
                ProjectRuntime::Composite | ProjectRuntime::Multi | ProjectRuntime::Bun => {
                    unreachable!("development resolves to one invocation runtime")
                }
            },
        },
    )?;
    invoke_selected_dev(&output, &verified, selected, dev_runtime, args).await
}

async fn dev_composite(
    root: &Path,
    package: &super::CargoPackage,
    args: &PluginDevArgs,
) -> anyhow::Result<()> {
    let declarations = &package
        .metadata
        .lenso_cli
        .as_ref()
        .expect("composite projects have CLI metadata")
        .implementations;
    let declared = |runtime: &str| -> anyhow::Result<&super::LensoCliImplementation> {
        let matches = declarations
            .iter()
            .filter(|implementation| implementation.runtime == runtime)
            .collect::<Vec<_>>();
        let [implementation] = matches.as_slice() else {
            bail!("composite Plugin must declare exactly one `{runtime}` implementation");
        };
        Ok(*implementation)
    };
    match args.implementation {
        DevImplementationArg::Wasm => {
            bail!("composite Plugin declares Process and Bun implementations")
        }
        DevImplementationArg::Bun => {
            let implementation = declared("bun")?;
            let implementation_root = implementation_root(root, &implementation.path)?;
            let bun_package = read_bun_package(&implementation_root)?
                .context("declared Bun implementation has no Lenso package metadata")?;
            dev_bun(&implementation_root, &bun_package, args).await
        }
        DevImplementationArg::Auto | DevImplementationArg::Process => {
            let implementation = declared("process")?;
            let implementation_root = implementation_root(root, &implementation.path)?;
            let process_package = read_package(&implementation_root.join("Cargo.toml"))?;
            dev_cargo(
                &implementation_root,
                &process_package,
                ProjectRuntime::Process,
                super::DevSelection {
                    build: super::DevBuild::Process,
                    invoke: ProjectRuntime::Process,
                },
                args,
            )
            .await
        }
        DevImplementationArg::All => {
            let temporary = tempfile::tempdir().context("create Plugin dev directory")?;
            let output = temporary.path().join("dev.lenso-plugin");
            let verified =
                materialize_composite(root, &output, package, BuildProfile::Development)?;
            let selected = resolve_implementation(
                &read_bundle_manifest(&output)?,
                &ImplementationPolicy {
                    host_target: super::rust_host_target(root)?,
                    runtimes: vec![RuntimeAdmission {
                        execution_class: ExecutionClassId::new(PROCESS_EXECUTION_CLASS),
                        runtime_profile: PROCESS_RUNTIME_PROFILE_V2.to_owned(),
                    }],
                },
            )?;
            invoke_selected_dev(&output, &verified, selected, ProjectRuntime::Process, args).await
        }
    }
}

async fn invoke_selected_dev(
    output: &Path,
    verified: &VerifiedBundle,
    selected: ResolvedPluginImplementation,
    dev_runtime: ProjectRuntime,
    args: &PluginDevArgs,
) -> anyhow::Result<()> {
    let artifact_path = output.join(&selected.artifact.path);
    let artifact_bytes = fs::read(&artifact_path)?;
    let descriptor = &selected.descriptor;
    let [capability] = descriptor.provided_capabilities() else {
        bail!("Plugin development requires exactly one provided Capability");
    };
    let source_descriptor = match dev_runtime {
        ProjectRuntime::Wasm => parse_descriptor(&artifact_bytes)?,
        ProjectRuntime::Process => read_process_descriptor(&artifact_path)?.descriptor,
        ProjectRuntime::Composite | ProjectRuntime::Multi | ProjectRuntime::Bun => {
            unreachable!("development resolves to one invocation runtime")
        }
    };
    let source_capability = one_capability(&source_descriptor)?;
    if source_capability.capability_id != capability.capability_id()
        || source_capability.descriptor_version != capability.descriptor_version()
        || source_capability.request_operations != capability.request_operations()
    {
        bail!("packaged Capability differs from source descriptor evidence");
    }
    let operation = args.operation.clone()
        .or_else(|| source_capability.request_operations.first().cloned())
        .context("plugin dev invokes Request operations; use an App with typed Stream support for this Capability")?;
    if !source_capability.request_operations.contains(&operation) {
        bail!(
            "Plugin Capability `{}` does not declare Request operation `{operation}`",
            source_capability.capability_id
        );
    }
    let request: Value = serde_json::from_str(&args.request_json)
        .context("Plugin development request is not valid JSON")?;
    let invocation = DevAdapterInvocation {
        artifact_path: &artifact_path,
        artifact_digest: &selected.artifact.digest,
        artifact_bytes: &artifact_bytes,
        plugin_id: &verified.plugin_id,
        descriptor,
        capability,
        source_capability,
        operation: &operation,
        request,
        config_json: &args.config_json,
    };
    if dev_runtime == ProjectRuntime::Process {
        if !descriptor.required_capabilities().is_empty() {
            bail!("Plugin development requires an App Root to resolve declared dependencies");
        }
        if descriptor.authoring_version() == 2 && source_capability.descriptor_digest.is_none() {
            bail!("Process V2 source descriptor omitted its Capability digest proof");
        }
        let response = invoke_dev_process(invocation).await?;
        return print_dev_response(
            verified,
            capability.capability_id(),
            &operation,
            &response,
            args.json,
        );
    }
    let response = invoke_dev_wasm(invocation).await?;
    print_dev_response(
        verified,
        capability.capability_id(),
        &operation,
        &response,
        args.json,
    )
}

struct DevAdapterInvocation<'a> {
    artifact_path: &'a Path,
    artifact_digest: &'a str,
    artifact_bytes: &'a [u8],
    plugin_id: &'a str,
    descriptor: &'a lenso_app_plan::authoring::PluginDescriptor,
    capability: &'a CapabilityEndpointPlan,
    source_capability: &'a PluginCapability,
    operation: &'a str,
    request: Value,
    config_json: &'a str,
}

async fn invoke_dev_wasm(invocation: DevAdapterInvocation<'_>) -> anyhow::Result<Value> {
    let DevAdapterInvocation {
        artifact_path,
        artifact_digest,
        artifact_bytes,
        plugin_id,
        descriptor,
        capability,
        source_capability,
        operation,
        request,
        config_json,
    } = invocation;
    let configuration: Value = serde_json::from_str(config_json)
        .context("Plugin development configuration is not valid JSON")?;
    let artifact =
        ArtifactHandle::open(artifact_path, artifact_digest, artifact_bytes.len() as u64)
            .map_err(|error| runtime_error("open Plugin artifact", &error))?;
    let artifacts = ArtifactCatalog::new()
        .with_artifact("plugin", artifact)
        .map_err(|error| runtime_error("register Plugin artifact", &error))?;
    let plan = ResolvedAppPlan::new(
        vec![
            PluginInstancePlan::new("plugin", plugin_id)
                .with_authoring(descriptor.authoring_version(), descriptor.runtime_profile())
                .with_entrypoint(descriptor.entrypoint())
                .with_configuration(serde_json::to_string(&configuration)?)
                .with_execution_class(descriptor.execution_class().clone())
                .with_capability(capability.clone()),
            dev_client_plan(capability),
        ],
        vec![dev_client_binding(capability)],
    );
    let adapter = WasmComponentAdapter::new(artifacts)
        .with_limits(WasmComponentLimits {
            max_component_bytes: artifact_bytes.len(),
            ..WasmComponentLimits::default()
        })
        .with_codec(DynamicJsonCodec::new(source_capability));
    invoke_dev_adapter(adapter, &plan, operation, request).await
}

async fn invoke_dev_process(invocation: DevAdapterInvocation<'_>) -> anyhow::Result<Value> {
    let DevAdapterInvocation {
        artifact_path: executable,
        artifact_digest,
        artifact_bytes,
        plugin_id,
        descriptor,
        capability,
        source_capability,
        operation,
        request,
        config_json,
    } = invocation;
    let configuration: Value = serde_json::from_str(config_json)
        .context("Plugin development configuration is not valid JSON")?;
    let artifact = ArtifactHandle::open(executable, artifact_digest, artifact_bytes.len() as u64)
        .map_err(|error| runtime_error("open Plugin artifact", &error))?;
    let artifacts = ArtifactCatalog::new()
        .with_artifact("plugin", artifact)
        .map_err(|error| runtime_error("register Plugin artifact", &error))?;
    let plan = ResolvedAppPlan::new(
        vec![
            PluginInstancePlan::new("plugin", plugin_id)
                .with_authoring(descriptor.authoring_version(), descriptor.runtime_profile())
                .with_entrypoint(descriptor.entrypoint())
                .with_configuration(serde_json::to_string(&configuration)?)
                .with_execution_class(descriptor.execution_class().clone())
                .with_capability(capability.clone()),
            dev_client_plan(capability),
        ],
        vec![dev_client_binding(capability)],
    );
    let adapter =
        ProcessAdapter::new(artifacts).with_codec(DynamicJsonCodec::new(source_capability));
    invoke_dev_adapter(adapter, &plan, operation, request).await
}

const DEV_CLIENT_INSTANCE: &str = "lenso-dev-client";
const DEV_CLIENT_PACKAGE: &str = "lenso.dev-client";
const DEV_CLIENT_EXECUTION_CLASS: &str = "lenso.dev-client@1";

fn dev_client_plan(capability: &CapabilityEndpointPlan) -> PluginInstancePlan {
    PluginInstancePlan::new(DEV_CLIENT_INSTANCE, DEV_CLIENT_PACKAGE)
        .with_authoring(1, DEV_CLIENT_EXECUTION_CLASS)
        .with_entrypoint("dev")
        .with_execution_class(ExecutionClassId::new(DEV_CLIENT_EXECUTION_CLASS))
        .with_requirement(CapabilityRequirementPlan::one(
            capability.capability_id(),
            capability.descriptor_version(),
        ))
}

fn dev_client_binding(capability: &CapabilityEndpointPlan) -> CapabilityBinding {
    CapabilityBinding::new(
        DEV_CLIENT_INSTANCE,
        capability.capability_id(),
        capability.descriptor_version(),
        "plugin",
    )
}

#[derive(Debug)]
struct DevClientAdapter;

impl ExecutionAdapter for DevClientAdapter {
    fn execution_class(&self) -> ExecutionClassId {
        ExecutionClassId::new(DEV_CLIENT_EXECUTION_CLASS)
    }

    fn prepare(&self, plan: &ResolvedAppPlan) -> Result<PreparedNativeApp, RuntimeFailure> {
        let generations = plan
            .plugin_instances()
            .iter()
            .filter(|instance| instance.execution_class() == &self.execution_class())
            .map(|instance| {
                (
                    instance.instance_key().to_owned(),
                    PreparedNativePlugin::new(Vec::new(), NoopPluginLifecycle),
                )
            })
            .collect::<BTreeMap<_, _>>();
        Ok(PreparedNativeApp::new(Vec::new(), generations))
    }
}

pub(super) struct ProcessSourceDescriptor {
    pub(super) descriptor: PluginDescriptor,
    pub(super) authoring_version: u32,
    pub(super) runtime_profile: String,
}

pub(super) fn read_process_descriptor(
    executable: &Path,
) -> anyhow::Result<ProcessSourceDescriptor> {
    const MAX_FRAME_BYTES: usize = 1024 * 1024;
    let mut child = Command::new(executable)
        .arg("--lenso-describe")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start Process Plugin `{}`", executable.display()))?;
    let mut input = child.stdin.take().context("open Process Plugin stdin")?;
    let output = child.stdout.take().context("open Process Plugin stdout")?;
    let mut output = BufReader::new(output);

    let result = (|| {
        let ready = read_process_frame(&mut output, MAX_FRAME_BYTES)?;
        if ready.get("abi").and_then(Value::as_str).is_some() {
            let descriptor = super::parse_descriptor_bytes(
                &serde_json::to_vec(&ready).context("encode generated Process descriptor")?,
            )?;
            return Ok(ProcessSourceDescriptor {
                descriptor,
                authoring_version: 2,
                runtime_profile: PROCESS_RUNTIME_PROFILE_V2.to_owned(),
            });
        }
        if ready.get("type").and_then(Value::as_str) != Some("ready")
            || ready.get("protocol").and_then(Value::as_str) != Some("lenso.process-stdio@1")
        {
            bail!("Process Plugin did not complete the expected readiness handshake");
        }
        let descriptor = serde_json::from_value(
            ready
                .get("descriptor")
                .cloned()
                .context("Process Plugin readiness omitted its descriptor")?,
        )
        .context("Process Plugin readiness descriptor is invalid")?;
        Ok(ProcessSourceDescriptor {
            descriptor,
            authoring_version: 1,
            runtime_profile: PROCESS_RUNTIME_PROFILE_V1.to_owned(),
        })
    })();

    let _ = write_process_frame(&mut input, &serde_json::json!({ "type": "shutdown" }));
    drop(input);
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

fn read_process_frame(reader: &mut impl BufRead, limit: usize) -> anyhow::Result<Value> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        bail!("Process Plugin closed stdout before returning a frame");
    }
    if line.len() > limit {
        bail!("Process Plugin frame exceeds the development limit");
    }
    serde_json::from_str(&line).context("Process Plugin returned invalid framed JSON")
}

fn write_process_frame(writer: &mut impl Write, frame: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, frame)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

async fn invoke_dev_adapter(
    adapter: impl ExecutionAdapter,
    plan: &ResolvedAppPlan,
    operation: &str,
    request: Value,
) -> anyhow::Result<Value> {
    let plan = plan.clone();
    let operation = operation.to_owned();
    tokio::task::LocalSet::new()
        .run_until(async move {
            let adapters = ExecutionAdapterCatalog::new()
                .with_adapter(adapter)
                .map_err(|error| anyhow!("register Plugin Adapter: {error}"))?
                .with_adapter(DevClientAdapter)
                .map_err(|error| anyhow!("register development client Adapter: {error}"))?;
            let app = Kernel::start(plan, TokioDriver::new(), adapters)
                .await
                .map_err(|error| runtime_error("start Plugin development App", &error))?;
            let invocation = async {
                let dependencies = app
                    .dependencies(DEV_CLIENT_INSTANCE)
                    .map_err(|error| runtime_error("resolve Plugin development binding", &error))?;
                let [dependency] = dependencies.bindings() else {
                    bail!("Plugin development requires exactly one resolved provider");
                };
                let handle = dependency.handle().ok_or_else(|| {
                    anyhow!("Plugin development provider has no request endpoint")
                })?;
                let context = dependencies
                    .invocation_context(None, CancellationToken::new())
                    .map_err(|error| runtime_error("create Plugin invocation context", &error))?;
                handle
                    .invoke_erased(&operation, Box::new(request), context)
                    .await
                    .map_err(|error| runtime_error("invoke Plugin", &error))
            }
            .await;
            let shutdown = app.shutdown(Duration::from_secs(3)).await;
            if shutdown != ShutdownOutcome::Clean {
                bail!("Plugin development App shutdown was {shutdown:?}");
            }
            let outcome = invocation?;
            let response = outcome
                .map_err(|error| {
                    error.downcast::<Value>().map_or_else(
                        |_| anyhow!("Plugin returned an unknown Domain Error"),
                        |value| anyhow!("Plugin returned Domain Error: {value}"),
                    )
                })?
                .downcast::<Value>()
                .map_err(|_| anyhow!("Plugin returned a response with an unexpected type"))?;
            Ok(*response)
        })
        .await
}

fn print_dev_response(
    verified: &VerifiedBundle,
    capability_id: &str,
    operation: &str,
    response: &Value,
    json: bool,
) -> anyhow::Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "kind": "lenso.plugin-dev",
                "plugin_id": verified.plugin_id,
                "capability_id": capability_id,
                "operation": operation,
                "response": response,
            }))?
        );
    } else {
        println!("Plugin {} returned {}", verified.plugin_id, response);
    }
    Ok(())
}

async fn dev_bun(root: &Path, package: &BunPackage, args: &PluginDevArgs) -> anyhow::Result<()> {
    let temporary = tempfile::tempdir().context("create Bun Plugin dev directory")?;
    let output = temporary.path().join("dev.lenso-plugin");
    let (verified, descriptor) =
        materialize_bun(root, &output, package, BuildProfile::Development)?;
    if !descriptor.required_capabilities.is_empty() {
        bail!("Bun Plugin development invocation requires an App to bind declared dependencies");
    }
    let capability = one_capability(&descriptor)?;
    let operation = args.operation.clone()
        .or_else(|| capability.request_operations.first().cloned())
        .context("plugin dev invokes Request operations; use an App with typed Stream support for this Capability")?;
    if !capability.request_operations.contains(&operation) {
        bail!(
            "Plugin Capability `{}` does not declare Request operation `{operation}`",
            capability.capability_id
        );
    }
    let request: Value = serde_json::from_str(&args.request_json)
        .context("Plugin development request is not valid JSON")?;
    let configuration: Value = serde_json::from_str(&args.config_json)
        .context("Plugin development configuration is not valid JSON")?;
    let selected = resolve_implementation(
        &read_bundle_manifest(&output)?,
        &ImplementationPolicy {
            host_target: native_host_target().to_owned(),
            runtimes: vec![RuntimeAdmission {
                execution_class: ExecutionClassId::bun_child_process(),
                runtime_profile: BUN_AUTHORING_RUNTIME_PROFILE.to_owned(),
            }],
        },
    )?;
    let artifact_path = output.join(&selected.artifact.path);
    let artifact_bytes = fs::read(&artifact_path)?;
    let artifact = ArtifactHandle::open(
        &artifact_path,
        &selected.artifact.digest,
        artifact_bytes.len() as u64,
    )
    .map_err(|error| runtime_error("open Bun Plugin artifact", &error))?;
    let artifacts = ArtifactCatalog::new()
        .with_artifact("plugin", artifact)
        .map_err(|error| runtime_error("register Bun Plugin artifact", &error))?;
    let [selected_capability] = selected.descriptor.provided_capabilities() else {
        bail!("Bun Plugin development requires exactly one provided Capability");
    };
    let plan = ResolvedAppPlan::new(
        vec![
            PluginInstancePlan::new("plugin", &verified.plugin_id)
                .with_authoring(2, BUN_AUTHORING_RUNTIME_PROFILE)
                .with_entrypoint(selected.descriptor.entrypoint())
                .with_configuration(serde_json::to_string(&configuration)?)
                .with_execution_class(ExecutionClassId::bun_child_process())
                .with_capability(selected_capability.clone()),
            dev_client_plan(selected_capability),
        ],
        vec![dev_client_binding(selected_capability)],
    );
    let bun = env::var_os("BUN_BIN").unwrap_or_else(|| "bun".into());
    let adapter = BunAdapter::production(bun)
        .with_artifacts(artifacts)
        .with_authoring_codec(DynamicJsonCodec::new(capability));
    let response = invoke_dev_adapter(adapter, &plan, &operation, request).await?;
    print_dev_response(
        &verified,
        &capability.capability_id,
        &operation,
        &response,
        args.json,
    )
}

fn runtime_error(action: &str, error: &RuntimeFailure) -> anyhow::Error {
    anyhow!("{action}: {error:?}")
}

#[derive(Debug)]
struct DynamicJsonCodec {
    capability_id: &'static str,
    descriptor_version: &'static str,
    descriptor_digest: &'static str,
    request_operations: &'static [&'static str],
}

impl DynamicJsonCodec {
    fn new(capability: &PluginCapability) -> Self {
        Self {
            capability_id: intern_string(&capability.capability_id),
            descriptor_version: intern_string(&capability.descriptor_version),
            descriptor_digest: intern_string(
                capability.descriptor_digest.as_deref().unwrap_or_default(),
            ),
            request_operations: intern_operations(&capability.request_operations),
        }
    }
}

impl JsonCapabilityCodec for DynamicJsonCodec {
    fn capability_id(&self) -> &'static str {
        self.capability_id
    }

    fn descriptor_version(&self) -> &'static str {
        self.descriptor_version
    }

    fn descriptor_digest(&self) -> &'static str {
        self.descriptor_digest
    }

    fn request_operations(&self) -> &'static [&'static str] {
        self.request_operations
    }

    fn encode_request(&self, _: &str, request: &dyn Any) -> Result<Value, RuntimeFailure> {
        request
            .downcast_ref::<Value>()
            .cloned()
            .ok_or(RuntimeFailure::ProtocolViolation {
                capability: self.capability_id,
            })
    }

    fn decode_response(&self, _: &str, value: Value) -> Result<Box<dyn Any>, RuntimeFailure> {
        Ok(Box::new(value))
    }

    fn decode_domain_error(&self, _: &str, value: Value) -> Result<Box<dyn Any>, RuntimeFailure> {
        Ok(Box::new(value))
    }
}

fn intern_string(value: &str) -> &'static str {
    static VALUES: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let mut values = VALUES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("Plugin dev string interner lock");
    if let Some(value) = values.get(value) {
        return value;
    }
    let interned = Box::leak(value.to_owned().into_boxed_str());
    values.insert(value.to_owned(), interned);
    interned
}

fn intern_operations(operations: &[String]) -> &'static [&'static str] {
    type OperationSet = HashMap<Vec<String>, &'static [&'static str]>;
    static VALUES: OnceLock<Mutex<OperationSet>> = OnceLock::new();
    let mut values = VALUES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("Plugin dev operation interner lock");
    if let Some(value) = values.get(operations) {
        return value;
    }
    let interned = Box::leak(
        operations
            .iter()
            .map(|operation| intern_string(operation))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    values.insert(operations.to_vec(), interned);
    interned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn descriptor_fixture(frame: &str) -> tempfile::TempPath {
        use std::os::unix::fs::PermissionsExt;

        // Close the writable handle before another test can fork a child that
        // inherits it; Linux rejects execution while such a handle survives.
        let file = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        fs::write(
            &file,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}'\n",
                frame.replace('\'', "'\\''")
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&file, permissions).unwrap();
        file
    }

    #[test]
    fn repeated_wasm_rebuilds_reuse_interned_contract_storage() {
        let capability = PluginCapability {
            capability_id: "example.echo@1".to_owned(),
            descriptor_version: "1".to_owned(),
            descriptor_digest: Some(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
            ),
            request_operations: vec!["execute".to_owned()],
            stream_operations: vec![],
        };
        let first = DynamicJsonCodec::new(&capability);
        let second = DynamicJsonCodec::new(&capability);

        assert!(std::ptr::eq(first.capability_id(), second.capability_id()));
        assert!(std::ptr::eq(
            first.descriptor_version(),
            second.descriptor_version()
        ));
        assert!(std::ptr::eq(
            first.descriptor_digest(),
            second.descriptor_digest()
        ));
        assert!(std::ptr::eq(
            first.request_operations(),
            second.request_operations()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn process_descriptor_prefers_portable_authoring_v2() {
        let executable = descriptor_fixture(
            r#"{"abi":"lenso.json-request@1","capabilities":[{"capability_id":"example.echo@1","descriptor_version":"1","descriptor_digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","request_operations":["execute"]}],"required_capabilities":[]}"#,
        );

        let source = read_process_descriptor(&executable).unwrap();

        assert_eq!(source.authoring_version, 2);
        assert_eq!(source.runtime_profile, PROCESS_RUNTIME_PROFILE_V2);
        assert_eq!(
            source.descriptor.capabilities[0]
                .descriptor_digest
                .as_deref(),
            Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            source.descriptor.capabilities[0].capability_id,
            "example.echo@1"
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_descriptor_keeps_v1_readiness_compatibility() {
        let executable = descriptor_fixture(
            r#"{"type":"ready","protocol":"lenso.process-stdio@1","descriptor":{"abi":"lenso.json-request@1","capabilities":[{"capability_id":"example.echo@1","descriptor_version":"1","request_operations":["execute"]}],"required_capabilities":[]}}"#,
        );

        let source = read_process_descriptor(&executable).unwrap();

        assert_eq!(source.authoring_version, 1);
        assert_eq!(source.runtime_profile, PROCESS_RUNTIME_PROFILE_V1);
        assert_eq!(
            source.descriptor.capabilities[0].capability_id,
            "example.echo@1"
        );
    }
}
