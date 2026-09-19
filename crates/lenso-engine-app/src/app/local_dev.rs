//! Local development rebuilds complete App generations. A failed build leaves
//! the running generation alone; replacement first drains the previous Host.
use anyhow::{Context, bail};
use clap::Args;
use notify::{RecursiveMode, Watcher};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    sync::mpsc,
};

#[derive(Clone, Debug, Args)]
pub struct DevArgs {
    /// App source root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Arguments for installed terminal support, rerun after each successful rebuild.
    #[arg(last = true)]
    args: Vec<String>,
}

pub async fn dev(args: DevArgs) -> anyhow::Result<()> {
    let root = crate::plugins::project_root(args.root)?;
    fs::create_dir_all(root.join(".lenso"))?;
    let generations = tempfile::Builder::new()
        .prefix("dev-")
        .tempdir_in(root.join(".lenso"))?;
    let (mut watcher, mut events) = watch(&root)?;
    let mut host: Option<Child> = None;
    let mut revision = 0;
    let mut current_output: Option<PathBuf> = None;
    loop {
        revision += 1;
        let output = generations.path().join(format!("generation-{revision}"));
        let mut build = command(std::env::current_exe()?);
        build
            .args(["app", "build", "--root"])
            .arg(&root)
            .arg("--out")
            .arg(&output);
        let mut child = build.spawn().context("start local App build")?;
        let status = tokio::select! {
            status = child.wait() => status?,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                stop(&mut child, true).await?;
                if let Some(host) = &mut host { stop(host, false).await?; }
                return Ok(());
            }
        };
        watch_dependencies(&root, &mut watcher)?;
        if status.success() {
            if let Some(host) = &mut host {
                stop(host, false).await?;
            }
            host = Some(
                command(output.join(".lenso/host"))
                    .args(super::local_host::host_arguments(&output)?)
                    .args(if args.args.is_empty() {
                        vec![]
                    } else {
                        std::iter::once("--".to_owned())
                            .chain(args.args.clone())
                            .collect()
                    })
                    .spawn()
                    .context("start generated local Host")?,
            );
            if let Some(previous) = current_output.replace(output.clone()) {
                fs::remove_dir_all(previous)?;
            }
            eprintln!(
                "Watching {} for App changes. Press Ctrl-C to stop.",
                root.display()
            );
        } else {
            eprintln!("App rebuild failed; edit the source to retry.");
        }
        loop {
            tokio::select! {
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    if let Some(host) = &mut host { stop(host, false).await?; }
                    return Ok(());
                }
                event = events.recv() => {
                    match event.context("App watcher closed")? {
                        Ok(event) if event.paths.iter().any(|p| relevant(p)) => {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            while events.try_recv().is_ok() {}
                            // Configuration edits may add a previously unwatched shared source.
                            match watch(&root) {
                                Ok((next, receiver)) => { watcher = next; events = receiver; }
                                Err(error) => eprintln!("App source configuration is invalid: {error:#}"),
                            }
                            break;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            if let Some(host) = &mut host { stop(host, false).await?; }
                            return Err(error).context("App watcher failed");
                        }
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(200)) => {
                    if let Some(process) = &mut host
                        && let Some(status) = process.try_wait()? {
                            eprintln!("Local Host exited ({status}); edit the source to restart.");
                            host = None;
                        }
                }
            }
        }
        // Retain the OS watcher while building, so edits during compilation are queued.
        let _ = &watcher;
    }
}

fn command(executable: PathBuf) -> Command {
    let mut command = Command::new(executable);
    command.kill_on_drop(true);
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    command
}

async fn stop(child: &mut Child, whole_group: bool) -> anyhow::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use nix::{
            sys::signal::{Signal, kill, killpg},
            unistd::Pid,
        };
        let id = i32::try_from(child.id().context("child process ID")?)?;
        let pid = Pid::from_raw(id);
        let result = if whole_group {
            killpg(pid, Signal::SIGTERM)
        } else {
            kill(pid, Signal::SIGTERM)
        };
        if let Err(error) = result
            && error != nix::errno::Errno::ESRCH
        {
            return Err(error.into());
        }
        match tokio::time::timeout(Duration::from_secs(12), child.wait()).await {
            Ok(result) => {
                let status = result?;
                if !whole_group && !status.success() {
                    bail!("local Host shutdown failed: {status}");
                }
            }
            Err(_) => {
                let _ = killpg(pid, Signal::SIGKILL);
                child.wait().await?;
                bail!("local process did not stop within its shutdown budget");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = whole_group;
        child.kill().await?;
        child.wait().await?;
    }
    Ok(())
}

fn watch(
    root: &Path,
) -> anyhow::Result<(
    notify::RecommendedWatcher,
    mpsc::Receiver<notify::Result<notify::Event>>,
)> {
    let (sender, receiver) = mpsc::channel(128);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event
            .as_ref()
            .is_ok_and(|event| !event.paths.iter().any(|path| relevant(path)))
        {
            return;
        }
        // A full queue already guarantees a rebuild; coalesce further events.
        let _ = sender.try_send(event);
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    let config = root.join("lenso.toml");
    if config.is_file() {
        let document: toml::Value = toml::from_str(&fs::read_to_string(config)?)?;
        if let Some(sources) = document
            .get("plugin_sources")
            .and_then(toml::Value::as_array)
        {
            for source in sources {
                let source = source
                    .as_str()
                    .context("plugin_sources entries must be paths")?;
                let mut path = root.to_path_buf();
                for part in Path::new(source).components() {
                    if part.as_os_str().to_string_lossy().contains(['*', '?', '[']) {
                        break;
                    }
                    path.push(part);
                }
                while !path.is_dir() {
                    if !path.pop() {
                        bail!("cannot watch local source {source}");
                    }
                }
                let path = fs::canonicalize(path)?;
                if !path.starts_with(root) {
                    watcher.watch(&path, RecursiveMode::Recursive)?;
                }
            }
        }
    }
    watch_dependencies(root, &mut watcher)?;
    Ok((watcher, receiver))
}
fn watch_dependencies(root: &Path, watcher: &mut notify::RecommendedWatcher) -> anyhow::Result<()> {
    let path = root.join(".lenso/host-cache/watch-roots.json");
    if path.is_file() {
        let paths: Vec<PathBuf> = serde_json::from_slice(&fs::read(path)?)?;
        for path in paths {
            if !path.starts_with(root) && path.is_dir() {
                watcher.watch(&path, RecursiveMode::Recursive)?;
            }
        }
    }
    Ok(())
}

fn relevant(path: &Path) -> bool {
    !path.components().any(|part| {
        part.as_os_str().to_str().is_some_and(|name| {
            name.starts_with(".lenso-")
                || [
                    ".git",
                    ".lenso",
                    "target",
                    "node_modules",
                    "dist",
                    "build",
                    ".next",
                    ".venv",
                    "__pycache__",
                ]
                .contains(&name)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_watch_includes_discovery_and_intent_but_excludes_generated_output() {
        for path in [
            "/app/app/new/Cargo.toml",
            "/shared/plugins/new/src/lib.rs",
            "/app/plugins/a/instance.json",
            "/app/lenso.toml",
            "/app/app/web/public/icon.svg",
        ] {
            assert!(relevant(Path::new(path)));
        }
        for path in [
            "/app/.lenso/dev/generation/runtime/bun",
            "/app/app/p/target/debug/a",
            "/app/node_modules/a",
            "/app/dist/plugins/a",
        ] {
            assert!(!relevant(Path::new(path)));
        }
    }
}
