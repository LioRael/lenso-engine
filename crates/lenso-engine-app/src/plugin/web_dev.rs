use std::{
    env,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, bail};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::process::{Child, Command as TokioCommand};

use crate::watch::SourceWatcher;

use super::{
    CargoPackage, DevImplementationArg, PluginDevArgs, cargo_target_directory, project_root,
    read_package,
    scaffold::{LENSO_APP_PLAN_REVISION, LENSO_NATIVE_REVISION, LENSO_WEB_REVISION},
};

const HOST_SOURCE: &str = r#"use std::time::{Duration, Instant};

use futures::future::LocalBoxFuture;
use lenso_app_plan::{
    AppComposition, CapabilityBinding, CapabilityEndpointPlan, CapabilityRequirementPlan,
    PluginInstancePlan,
};
use lenso_capability_http_endpoint::{
    CAPABILITY_ID, DESCRIBE_OPERATION, DESCRIPTOR_VERSION, HANDLE_OPERATION,
};
use lenso_kernel::{Kernel, ShutdownOutcome};
use lenso_native_adapter::NativePluginRegistry;
use lenso_runner::TokioDriver;
use lenso_web_ingress_plugin::{
    PACKAGE_ID as INGRESS_PACKAGE_ID, WebIngressConfig, WebIngressFactory,
    WebIngressDiagnostics, WebIngressEndpointFailure, WebIngressMiddleware,
    WebIngressMiddlewareOutcome, WebIngressRequest, WebIngressResponse,
};
use tokio::task::LocalSet;

#[derive(Debug)]
struct DevRequestTrace;

#[derive(Debug)]
struct DevDiagnostics;

impl WebIngressDiagnostics for DevDiagnostics {
    fn endpoint_runtime_failure(&self, event: WebIngressEndpointFailure<'_>) {
        if std::env::var_os("LENSO_WEB_DEV_JSON").is_some() {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "kind": "lenso.web-endpoint-failure",
                    "request_id": event.request_id(),
                    "route_id": event.route_id(),
                    "provider_index": event.provider_index(),
                    "failure": format!("{:?}", event.failure()),
                })
            );
        } else {
            println!(
                "Endpoint {} failed for request {} on provider {}: {:?}",
                event.route_id(),
                event.request_id(),
                event.provider_index(),
                event.failure(),
            );
        }
    }
}

impl WebIngressMiddleware for DevRequestTrace {
    fn identity(&self) -> &'static str {
        "lenso.web-dev.request-trace@1"
    }

    fn before_request<'a>(
        &'a self,
        request: &'a mut WebIngressRequest,
    ) -> LocalBoxFuture<'a, Result<WebIngressMiddlewareOutcome, lenso_kernel::RuntimeFailure>> {
        request.extensions_mut().insert(Instant::now());
        Box::pin(futures::future::ready(Ok(
            WebIngressMiddlewareOutcome::Continue,
        )))
    }

    fn after_response<'a>(
        &'a self,
        request: &'a WebIngressRequest,
        response: &'a mut WebIngressResponse,
    ) -> LocalBoxFuture<'a, Result<(), lenso_kernel::RuntimeFailure>> {
        let elapsed_ms = request
            .extensions()
            .get::<Instant>()
            .map_or(0, |started| started.elapsed().as_millis());
        let request_id = request
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown");
        if std::env::var_os("LENSO_WEB_DEV_JSON").is_some() {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "kind": "lenso.web-request",
                    "request_id": request_id,
                    "method": request.method().as_str(),
                    "path": request.uri().path(),
                    "status": response.status().as_u16(),
                    "elapsed_ms": elapsed_ms,
                })
            );
        } else {
            println!(
                "{} {} -> {} ({} ms, request {})",
                request.method(),
                request.uri().path(),
                response.status().as_u16(),
                elapsed_ms,
                request_id,
            );
        }
        Box::pin(futures::future::ready(Ok(())))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    LocalSet::new().run_until(run()).await
}

async fn run() -> Result<(), String> {
    plugin::link();
    let endpoint = PluginInstancePlan::new("web-plugin", plugin::PACKAGE_ID).with_capability(
        CapabilityEndpointPlan::new(
            CAPABILITY_ID,
            DESCRIPTOR_VERSION,
            [DESCRIBE_OPERATION, HANDLE_OPERATION],
        )
        .with_cross_lane_transfer(),
    );
    let ingress = WebIngressFactory::new()
        .with_diagnostics(DevDiagnostics)
        .with_middleware(DevRequestTrace);
    let ingress_plan = PluginInstancePlan::new("web-ingress", INGRESS_PACKAGE_ID)
        .with_configuration(
            serde_json::to_string(&WebIngressConfig::default()).map_err(|error| error.to_string())?,
        )
        .with_requirement(CapabilityRequirementPlan::many(
            CAPABILITY_ID,
            DESCRIPTOR_VERSION,
        ));
    let plan = AppComposition::new(
        vec![endpoint, ingress_plan],
        vec![CapabilityBinding::new(
            "web-ingress",
            CAPABILITY_ID,
            DESCRIPTOR_VERSION,
            "web-plugin",
        )],
    )
    .resolve()
    .map_err(|error| format!("resolve Web development App: {error:?}"))?;
    let registry = NativePluginRegistry::new()
        .with_linked_factories()
        .with_factory(ingress.clone());
    let app = Kernel::start_native(plan, TokioDriver::new(), registry)
        .await
        .map_err(|error| format!("start Web development App: {error:?}"))?;
    let address = ingress
        .local_address()
        .ok_or_else(|| "Web Ingress did not publish its listener address".to_owned())?;
    let routes = ingress
        .route_manifest()
        .ok_or_else(|| "Web Ingress did not publish its route manifest".to_owned())?;

    if std::env::var_os("LENSO_WEB_DEV_JSON").is_some() {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": 1,
                "kind": "lenso.web-dev-ready",
                "address": format!("http://{address}"),
                "routes": routes.routes().iter().map(|route| serde_json::json!({
                    "method": route.method,
                    "path": route.path,
                    "route_id": route.route_id,
                })).collect::<Vec<_>>(),
            })
        );
    } else {
        println!("Lenso Web Plugin development server");
        println!("Listening on http://{address}");
        for route in routes.routes() {
            println!("  {:<7} {:<32} {}", route.method, route.path, route.route_id);
        }
        println!("Press Ctrl-C to stop.");
    }

    tokio::signal::ctrl_c()
        .await
        .map_err(|error| format!("listen for Ctrl-C: {error}"))?;
    let outcome = app.shutdown(Duration::from_secs(3)).await;
    if outcome != ShutdownOutcome::Clean {
        return Err(format!("Web development App shutdown was {outcome:?}"));
    }
    Ok(())
}
"#;

pub(super) fn is_web_plugin(root: &Path) -> anyhow::Result<bool> {
    if root.join("package.json").is_file() {
        return Ok(false);
    }
    let manifest = root.join("Cargo.toml");
    if !manifest.is_file() {
        return Ok(false);
    }
    Ok(read_package(&manifest)?.metadata.lenso.root_slot == "web")
}

pub(super) async fn run(args: PluginDevArgs) -> anyhow::Result<()> {
    validate_args(&args)?;
    let root = project_root(args.repo_root.clone())?;
    let package = read_package(&root.join("Cargo.toml"))?;
    let host = DevHost::prepare(&root, &package)?;
    let mut watcher = args.watch.then(|| SourceWatcher::new(&root)).transpose()?;

    loop {
        host.build()?;
        let mut child = host.spawn(args.json)?;
        if let Some(watcher) = watcher.as_mut() {
            tokio::select! {
                result = child.wait() => {
                    let status = result.context("wait for Web development Host")?;
                    if !status.success() {
                        bail!("Web development Host exited with {status}");
                    }
                    return Ok(());
                }
                result = watcher.changed() => {
                    result?;
                    stop(&mut child).await;
                    if !args.json {
                        println!("Rebuilding Web Plugin after source changes.");
                    }
                }
                result = tokio::signal::ctrl_c() => {
                    result.context("listen for Ctrl-C")?;
                    wait_for_signal_shutdown(&mut child).await;
                    return Ok(());
                }
            }
        } else {
            tokio::select! {
                result = child.wait() => {
                    let status = result.context("wait for Web development Host")?;
                    if !status.success() {
                        bail!("Web development Host exited with {status}");
                    }
                    return Ok(());
                }
                result = tokio::signal::ctrl_c() => {
                    result.context("listen for Ctrl-C")?;
                    wait_for_signal_shutdown(&mut child).await;
                    return Ok(());
                }
            }
        }
    }
}

fn validate_args(args: &PluginDevArgs) -> anyhow::Result<()> {
    if args.operation.is_some() || args.request_json != "{}" {
        bail!(
            "Web Plugin development serves real HTTP; remove `--operation` and `--request-json`, then send requests to the printed listener address"
        );
    }
    if args.implementation != DevImplementationArg::Auto {
        bail!("Web Plugins use the linked native implementation; remove `--implementation`");
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct DevHost {
    project: TempDir,
    executable: PathBuf,
    project_root: PathBuf,
    target_directory: PathBuf,
}

impl DevHost {
    pub(super) fn prepare(root: &Path, package: &CargoPackage) -> anyhow::Result<Self> {
        let project = tempfile::tempdir().context("create Web development Host directory")?;
        let target_directory = cargo_target_directory(root)?;
        let package_name = host_package_name(root, &package.name);
        let manifest = host_manifest(root, package, &package_name);
        fs::write(project.path().join("Cargo.toml"), manifest)
            .context("write Web development Host manifest")?;
        fs::create_dir(project.path().join("src"))
            .context("create Web development Host source directory")?;
        fs::write(project.path().join("src/main.rs"), HOST_SOURCE)
            .context("write Web development Host source")?;
        run_cargo(
            project.path(),
            &target_directory,
            ["generate-lockfile"],
            "lock Web development Host",
        )?;
        let executable = target_directory.join("debug").join(if cfg!(windows) {
            format!("{package_name}.exe")
        } else {
            package_name
        });
        Ok(Self {
            project,
            executable,
            project_root: root.to_path_buf(),
            target_directory,
        })
    }

    pub(super) fn build(&self) -> anyhow::Result<()> {
        run_cargo(
            self.project.path(),
            &self.target_directory,
            ["build", "--locked"],
            "build Web development Host",
        )
    }

    fn spawn(&self, json: bool) -> anyhow::Result<Child> {
        let mut command = TokioCommand::new(&self.executable);
        command.current_dir(&self.project_root).kill_on_drop(true);
        if json {
            command.env("LENSO_WEB_DEV_JSON", "1");
        }
        command
            .spawn()
            .with_context(|| format!("start Web development Host `{}`", self.executable.display()))
    }
}

fn host_package_name(root: &Path, package_name: &str) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let mut suffix = String::with_capacity(8);
    for byte in &digest[..4] {
        write!(&mut suffix, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!(
        "lenso-web-dev-{}-{}",
        package_name.replace('_', "-"),
        suffix
    )
}

fn host_manifest(root: &Path, package: &CargoPackage, host_package_name: &str) -> String {
    let plugin_path =
        serde_json::to_string(&root.to_string_lossy()).expect("serialize Plugin path");
    format!(
        r#"[package]
name = "{host_package_name}"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
futures = "0.3"
lenso-app-plan = {{ version = "0.3.0", git = "https://github.com/LioRael/lenso", rev = "{LENSO_APP_PLAN_REVISION}" }}
lenso-capability-http-endpoint = {{ version = "0.2.8", git = "https://github.com/LioRael/lenso-web", rev = "{LENSO_WEB_REVISION}" }}
lenso-kernel = {{ version = "0.2.0", git = "https://github.com/LioRael/lenso", rev = "{LENSO_APP_PLAN_REVISION}" }}
lenso-native-adapter = {{ version = "0.3.0", git = "https://github.com/LioRael/lenso-runtime-rust", rev = "{LENSO_NATIVE_REVISION}" }}
lenso-runner = {{ version = "0.2.0", git = "https://github.com/LioRael/lenso-runtime-rust", rev = "{LENSO_NATIVE_REVISION}" }}
lenso-web-ingress-plugin = {{ version = "0.3.7", git = "https://github.com/LioRael/lenso-web", rev = "{LENSO_WEB_REVISION}" }}
plugin = {{ package = "{}", path = {plugin_path} }}
serde_json = "1"
tokio = {{ version = "1.52", features = ["macros", "rt", "signal"] }}

[workspace]
"#,
        package.name,
    )
}

fn run_cargo<const N: usize>(
    root: &Path,
    target_directory: &Path,
    args: [&str; N],
    action: &str,
) -> anyhow::Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(args)
        .env("CARGO_TARGET_DIR", target_directory)
        .current_dir(root)
        .status()
        .with_context(|| action.to_owned())?;
    if !status.success() {
        bail!("{action} failed with {status}");
    }
    Ok(())
}

async fn wait_for_signal_shutdown(child: &mut Child) {
    if tokio::time::timeout(Duration::from_secs(4), child.wait())
        .await
        .is_err()
    {
        stop(child).await;
    }
}

async fn stop(child: &mut Child) {
    #[cfg(unix)]
    if let Some(id) = child.id()
        && let Ok(raw) = i32::try_from(id)
    {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(raw),
            nix::sys::signal::Signal::SIGINT,
        );
    }
    #[cfg(not(unix))]
    let _ = child.start_kill();

    if tokio::time::timeout(Duration::from_secs(4), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::{host_manifest, host_package_name};
    use crate::plugin::{CargoMetadata, CargoPackage, LensoMetadata};
    use std::path::Path;

    #[test]
    fn generated_host_uses_the_real_native_web_path() {
        let root = Path::new("/tmp/company.greetings-http");
        let package = CargoPackage {
            name: "company-greetings-http".to_owned(),
            version: "0.1.0".to_owned(),
            metadata: CargoMetadata {
                lenso: LensoMetadata {
                    plugin_id: "company.greetings-http".to_owned(),
                    root_slot: "web".to_owned(),
                },
                lenso_cli: None,
            },
        };
        let name = host_package_name(root, &package.name);
        let manifest = host_manifest(root, &package, &name);

        assert!(name.starts_with("lenso-web-dev-company-greetings-http-"));
        assert!(manifest.contains("lenso-web-ingress-plugin"));
        assert!(manifest.contains("plugin = { package = \"company-greetings-http\""));
    }
}
