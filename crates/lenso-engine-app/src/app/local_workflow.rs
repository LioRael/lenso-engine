use anyhow::{Context, bail};
use clap::{Args, ValueEnum};
use std::{fs, path::PathBuf, process::Command};

#[derive(Clone, Debug, Args)]
pub struct BuildArgs {
    /// Existing static TypeScript Host source. Omit for the local App convention.
    #[arg(long, requires_all = ["target", "out"], conflicts_with = "root")]
    source: Option<PathBuf>,
    /// Exact target for an explicit TypeScript Host build.
    #[arg(long, requires = "source")]
    target: Option<String>,
    /// Local App source root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
    /// New output directory. Defaults to dist for a local App.
    #[arg(long)]
    out: Option<PathBuf>,
}
pub fn build(args: BuildArgs) -> anyhow::Result<()> {
    if let Some(source) = args.source {
        return super::build::build(&super::build::HostBuildArgs {
            source,
            target: args.target.context("TS Host target")?,
            out: args.out.context("TS Host output")?,
        });
    }
    let root = crate::plugins::project_root(args.root)?;
    let out = args.out.unwrap_or_else(|| root.join("dist"));
    let mut engine = lenso_engine::Engine::default();
    engine.register(lenso_engine_runtime::RuntimeProcessor::new(
        super::preset::AppProject {
            root,
            output: out,
            runtime_executable: std::env::current_exe()?,
        },
    ))?;
    let plan = engine.plan(lenso_engine::Snapshot::default())?;
    engine.execute(
        &plan,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Starter {
    Process,
    Bun,
    Wasm,
    Multi,
    Empty,
}
#[derive(Clone, Debug, Args)]
pub struct CreateArgs {
    /// New App directory. Existing directories are never overwritten.
    directory: PathBuf,
    /// Starter implementation; more languages and Plugin types can be added under app/.
    #[arg(long, value_enum, default_value = "process")]
    runtime: Starter,
    /// Create a native Rust Web Plugin with Plugin-owned HTML assets.
    #[arg(long, conflicts_with = "runtime")]
    web: bool,
    /// Create a TypeScript CLI App with bundled convention support.
    #[arg(long, conflicts_with_all = ["runtime", "web"])]
    cli: bool,
    /// Skip the starter's package installation and initial compile check.
    #[arg(long)]
    no_install: bool,
}
pub fn create(args: CreateArgs) -> anyhow::Result<()> {
    let destination = std::path::absolute(args.directory)?;
    if fs::symlink_metadata(&destination).is_ok() {
        bail!("App directory already exists: {}", destination.display());
    }
    let parent = destination
        .parent()
        .context("App directory needs a parent")?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".lenso-create-")
        .tempdir_in(parent)?;
    fs::create_dir(staging.path().join("app"))?;
    fs::create_dir(staging.path().join("plugins"))?;
    if !args.cli && (args.web || !matches!(args.runtime, Starter::Empty)) {
        let runtime = match args.runtime {
            Starter::Process => "process",
            Starter::Bun => "bun",
            Starter::Wasm => "wasm",
            Starter::Multi => "multi",
            Starter::Empty => unreachable!(),
        };
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["plugin", "new", "local.starter", "--repo-root"])
            .arg(staging.path())
            .args(["--dir", "app/local.starter"]);
        if args.web {
            command.arg("--web");
        } else {
            command.args(["--runtime", runtime]);
        }
        if args.no_install || args.web {
            command.arg("--no-install");
        }
        if !command.status()?.success() {
            bail!("App starter creation failed");
        }
        if args.web {
            prepare_web_starter(&staging.path().join("app/local.starter"), args.no_install)?;
        }
    }
    fs::write(
        staging.path().join(".gitignore"),
        ".lenso/\ndist/\ntarget/\nnode_modules/\n",
    )?;
    fs::write(
        staging.path().join("README.md"),
        "# Local Lenso App\n\nRun `lenso app dev` to build and watch this App. Run `lenso app build` to produce an offline executable, then `lenso app start --from dist`.\n\nAdd Plugin source projects under `app/`. Keep instance configuration and explicit dependency choices in `plugins/`. No App configuration file is required. Optional `plugin_sources` in `lenso.toml` adds shared local candidates; an explicit Plugin Root instance is required to select them.\n\nThe generated Host supports native Rust, Bun, Process and Wasm implementations. Existing custom Host authoring remains available through `lenso app build --source ... --target ...`.\n",
    )?;
    super::build::publish_new_output(staging.path(), &destination)?;
    if args.cli {
        for invocation in [
            vec!["app", "add", "@lenso/cli"],
            vec!["app", "plugin", "new", "local.hello"],
        ] {
            let mut command = Command::new(std::env::current_exe()?);
            command.args(invocation).arg("--root").arg(&destination);
            if args.no_install {
                command.arg("--no-install");
            }
            if !command.status()?.success() {
                bail!("CLI App scaffold is created; support setup failed");
            }
        }
    }
    println!(
        "Created App at {}. Run `lenso app dev --root {}`.",
        destination.display(),
        destination.display()
    );
    Ok(())
}

#[derive(Clone, Debug, Args)]
pub struct StartArgs {
    /// Built local App directory.
    #[arg(long, default_value = "dist")]
    from: PathBuf,
    /// External App root whose plugins/ intent replaces the build snapshot.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Start, validate readiness and shut down immediately.
    #[arg(long)]
    check: bool,
    /// Arguments passed to installed terminal support.
    #[arg(last = true, conflicts_with = "check")]
    args: Vec<String>,
}
pub fn start(args: StartArgs) -> anyhow::Result<()> {
    let executable = fs::canonicalize(args.from.join(".lenso/host"))
        .context("locate built local Host; run lenso app build first")?;
    let mut command = Command::new(executable);
    command.args(super::local_host::host_arguments(&args.from)?);
    if let Some(root) = args.root {
        command.arg("--root").arg(root);
    }
    if args.check {
        command.arg("--check");
    }
    if !args.args.is_empty() {
        command.arg("--").args(&args.args);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("execute local App")
    }
    #[cfg(not(unix))]
    {
        if !command.status()?.success() {
            bail!("local App exited unsuccessfully");
        }
        Ok(())
    }
}

fn prepare_web_starter(root: &std::path::Path, no_install: bool) -> anyhow::Result<()> {
    let manifest = root.join("Cargo.toml");
    let mut value: toml::Value = toml::from_str(&fs::read_to_string(&manifest)?)?;
    value["dependencies"]["lenso"] = toml::Value::String("=0.5.23".into());
    value["dependencies"]["lenso-capability-http-endpoint"] = toml::Value::String("=0.3.2".into());
    fs::write(manifest, toml::to_string_pretty(&value)?)?;
    let source = root.join("src/lib.rs");
    let code = fs::read_to_string(&source)?
        .replace("pub const fn link() {}", "pub fn link() { link_plugin(); }")
        .replace(
            "impl GreetingsHttp {",
            r#"impl GreetingsHttp {
    #[get("local.home", "/")]
    async fn home(&self) -> Result<HandleResponse, Problem> {
        let mut response = lenso_capability_http_endpoint::response::text(
            StatusCode::OK, include_str!("../public/index.html"));
        response.headers[0].value = "text/html; charset=utf-8".into();
        Ok(response)
    }
"#,
        );
    fs::write(source, code)?;
    fs::create_dir_all(root.join("public"))?;
    fs::write(
        root.join("public/index.html"),
        r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Local Lenso App</title>
<style>body{font:18px system-ui;max-width:40rem;margin:12vh auto;padding:2rem;background:#f6f7f9;color:#182132}input,button{font:inherit;padding:.65rem;border:1px solid #bac3d0;border-radius:.4rem}button{background:#182132;color:white}output{display:block;margin-top:1.5rem}</style>
<h1>Your Lenso App is running.</h1><p>This page and its HTTP routes belong to the starter Plugin.</p>
<form><label>Your name <input name="name" required value="Lenso"></label> <button>Create greeting</button></form><output aria-live="polite"></output>
<script>document.querySelector('form').onsubmit=async(event)=>{event.preventDefault();const response=await fetch('/greetings',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({name:new FormData(event.target).get('name')})});const body=await response.json();document.querySelector('output').textContent=body.message||body.detail||'Request failed';};</script></html>
"#,
    )?;
    if !no_install
        && !Command::new("cargo")
            .args(["check", "--manifest-path"])
            .arg(root.join("Cargo.toml"))
            .status()?
            .success()
    {
        bail!("Web starter compile check failed");
    }
    Ok(())
}

/// Build an App from a library host without parsing command-line arguments.
pub fn build_local(
    root: PathBuf,
    output: PathBuf,
    runtime_executable: PathBuf,
) -> anyhow::Result<()> {
    let mut engine = lenso_engine::Engine::default();
    engine.register(lenso_engine_runtime::RuntimeProcessor::new(
        super::preset::AppProject {
            root,
            output,
            runtime_executable,
        },
    ))?;
    let plan = engine.plan(lenso_engine::Snapshot::default())?;
    engine.execute(
        &plan,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?;
    Ok(())
}
/// Create an empty App; language and surface support are adopted independently.
pub fn create_empty(directory: PathBuf) -> anyhow::Result<()> {
    create(CreateArgs {
        directory,
        runtime: Starter::Empty,
        web: false,
        cli: false,
        no_install: true,
    })
}
/// Run a source-free distribution from an embedding host.
pub fn start_distribution(from: PathBuf, arguments: Vec<String>) -> anyhow::Result<()> {
    start(StartArgs {
        from,
        root: None,
        check: false,
        args: arguments,
    })
}
