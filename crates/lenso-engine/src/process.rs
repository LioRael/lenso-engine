//! Bounded subprocess execution. Signal policy belongs to the embedding host.
use anyhow::{Context, bail};
use std::sync::{Arc, atomic::AtomicI32};
use std::{
    io::Write,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub directory: PathBuf,
}

/// A host may observe the active process group for shutdown. Use a separate slot
/// for each concurrent invocation. This function never installs signal handlers.
pub fn execute(
    spec: &ProcessSpec,
    request: &serde_json::Value,
    active: Arc<AtomicI32>,
) -> anyhow::Result<Vec<u8>> {
    execute_cancellable(
        spec,
        request,
        active,
        &std::sync::atomic::AtomicBool::new(false),
    )
}

pub fn execute_cancellable(
    spec: &ProcessSpec,
    request: &serde_json::Value,
    active: Arc<AtomicI32>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<Vec<u8>> {
    if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        bail!("processing cancelled");
    }
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    let input = serde_json::to_vec(request)?;
    if input.len() > 128 * 1024 * 1024 {
        bail!("processor request exceeds 128 MiB");
    }
    let mut stdin = tempfile::tempfile()?;
    stdin.write_all(&input)?;
    std::io::Seek::rewind(&mut stdin)?;
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.directory)
        .stdin(stdin)
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("start processor {}", spec.program))?;
    #[cfg(unix)]
    let group = CompilerGroup::new(active.clone(), child.id());
    let mut child = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancelled.load(std::sync::atomic::Ordering::SeqCst)
            || Instant::now() >= deadline
            || stdout.metadata()?.len() > 1024 * 1024
            || stderr.metadata()?.len() > 1024 * 1024
        {
            #[cfg(unix)]
            {
                use nix::{
                    sys::signal::{Signal, killpg},
                    unistd::Pid,
                };
                let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                bail!("processing cancelled");
            }
            bail!("processor exceeded its 60 second / 1 MiB output budget");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    #[cfg(unix)]
    drop(group);
    if stdout.metadata()?.len() > 1024 * 1024 || stderr.metadata()?.len() > 1024 * 1024 {
        bail!("processor output exceeds 1 MiB");
    }
    use std::io::{Read, Seek};
    let mut stderr = stderr;
    stderr.rewind()?;
    let mut diagnostic = String::new();
    stderr.take(1024 * 1024).read_to_string(&mut diagnostic)?;
    if !status.success() {
        bail!("processor {} failed: {diagnostic}", spec.program);
    }
    let mut stdout = stdout;
    stdout.rewind()?;
    let mut response = Vec::new();
    stdout.take(1024 * 1024 + 1).read_to_end(&mut response)?;
    if response.len() > 1024 * 1024 {
        bail!("processor response exceeds 1 MiB");
    }
    Ok(response)
}

#[cfg(unix)]
struct CompilerGroup(std::sync::Arc<std::sync::atomic::AtomicI32>);
#[cfg(unix)]
impl CompilerGroup {
    fn new(active: std::sync::Arc<std::sync::atomic::AtomicI32>, pid: u32) -> Self {
        active.store(pid as i32, std::sync::atomic::Ordering::SeqCst);
        groups().lock().unwrap().insert(pid as i32);
        Self(active)
    }
}
#[cfg(unix)]
impl Drop for CompilerGroup {
    fn drop(&mut self) {
        let pid = self.0.swap(0, std::sync::atomic::Ordering::SeqCst);
        stop_group(pid);
        groups().lock().unwrap().remove(&pid);
    }
}
#[cfg(unix)]
fn stop_group(pid: i32) {
    if pid > 0 {
        use nix::{
            sys::signal::{Signal, killpg},
            unistd::Pid,
        };
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

struct ChildGuard(std::process::Child);
impl std::ops::Deref for ChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn groups() -> &'static std::sync::Mutex<std::collections::BTreeSet<i32>> {
    static GROUPS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<i32>>> =
        std::sync::OnceLock::new();
    GROUPS.get_or_init(Default::default)
}
/// Embedding-host shutdown hook. Never called implicitly by the library, and
/// never installs signal handlers or terminates the embedding process.
#[cfg(unix)]
pub fn terminate_active_processes() {
    for pid in groups().lock().unwrap().iter() {
        stop_group(*pid);
    }
}
