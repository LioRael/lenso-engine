//! The complete local App workflow is an optional Engine processor.
use lenso_engine::{ContextView, Plugin, Resource, Snapshot, Step};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug)]
pub struct AppProject {
    pub root: PathBuf,
    pub output: PathBuf,
    pub runtime_executable: PathBuf,
}
impl Plugin for AppProject {
    fn identity(&self) -> &str {
        "lenso.app.v1"
    }
    fn plan(&self, _: &Snapshot) -> anyhow::Result<Vec<Step>> {
        let report = lenso_app_authoring::discovery::discover(&self.root)?;
        // Domain-specific source inspection belongs to this optional preset.
        let conventions = lenso_app_authoring::discovery::conventions::plan(&report)?;
        let mut fingerprints = BTreeMap::new();
        fingerprints.insert(
            self.root.clone(),
            super::local_host::input_digest(&self.root)?,
        );
        for compilation in &conventions.compilations {
            for root in [&compilation.owner_project, &compilation.compiler_project] {
                fingerprints.insert(root.clone(), super::local_host::input_digest(root)?);
            }
        }
        Ok(vec![Step {
            id: "app/build".into(),
            inputs: vec![],
            after: vec![],
            options: serde_json::json!({"root":self.root,"output":self.output,"conventions":conventions,"runtime_executable":self.runtime_executable,"fingerprints":fingerprints}),
        }])
    }
    fn process(&self, context: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>> {
        if context.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("App build cancelled");
        }
        let fingerprints: BTreeMap<PathBuf, String> =
            serde_json::from_value(context.step.options["fingerprints"].clone())?;
        for (root, expected) in fingerprints {
            if super::local_host::input_digest(&root)? != expected {
                anyhow::bail!("App inputs changed after planning; replan before execution");
            }
        }
        let _cancellation = CancellationGuard::enter(context.cancelled.clone());
        let _runtime = RuntimeGuard::enter(self.runtime_executable.clone());
        let info = std::process::Command::new(&self.runtime_executable)
            .arg("--engine-host-info")
            .output()?;
        if !info.status.success() {
            anyhow::bail!("precompiled Engine Host rejected its protocol probe");
        }
        let info: serde_json::Value = serde_json::from_slice(&info.stdout)?;
        if info["schema"] != "lenso.engine-host.v1"
            || info["target"] != lenso_app_authoring::native_host_target()
        {
            anyhow::bail!("incompatible precompiled Engine Host");
        }
        super::assemble::assemble(super::assemble::AssembleArgs {
            root: Some(self.root.clone()),
            id: "local.app".into(),
            out: self.output.clone(),
            json: false,
            executable: true,
        })?;
        Ok(BTreeMap::from([(
            "distribution".into(),
            Resource {
                schema: "lenso.app-distribution.v1".into(),
                value: serde_json::json!({"directory":self.output}),
            },
        )]))
    }
}

thread_local! {
    static CANCELLATION: std::cell::RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> = const { std::cell::RefCell::new(None) };
}
struct CancellationGuard(Option<std::sync::Arc<std::sync::atomic::AtomicBool>>);
impl CancellationGuard {
    fn enter(token: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self(CANCELLATION.with(|slot| slot.replace(Some(token))))
    }
}
impl Drop for CancellationGuard {
    fn drop(&mut self) {
        CANCELLATION.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}
pub(super) fn cancellation() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    CANCELLATION
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))
}
pub(super) fn checkpoint() -> anyhow::Result<()> {
    if cancellation().load(std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("App build cancelled before publication");
    }
    Ok(())
}

thread_local! { static RUNTIME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) }; }
struct RuntimeGuard(Option<PathBuf>);
impl RuntimeGuard {
    fn enter(path: PathBuf) -> Self {
        Self(RUNTIME.with(|slot| slot.replace(Some(path))))
    }
}
impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        RUNTIME.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}
pub(super) fn runtime_executable() -> anyhow::Result<PathBuf> {
    RUNTIME
        .with(|slot| slot.borrow().clone())
        .map_or_else(|| Ok(std::env::current_exe()?), Ok)
}
