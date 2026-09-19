use lenso_engine::{Engine, Snapshot};
use lenso_engine_app::app::{AppProject, create_empty};
use std::sync::{Arc, atomic::AtomicBool};
#[test]
fn app_planning_never_probes_the_host_and_rejects_changed_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("app");
    create_empty(root.clone()).unwrap();
    let output = temp.path().join("dist");
    let mut engine = Engine::default();
    engine
        .register(AppProject {
            root: root.clone(),
            output: output.clone(),
            runtime_executable: temp.path().join("not-an-executable"),
        })
        .unwrap();
    let plan = engine.plan(Snapshot::default()).unwrap();
    std::fs::write(root.join("app/input.txt"), "changed").unwrap();
    let error = engine
        .execute(&plan, &Arc::new(AtomicBool::new(false)))
        .unwrap_err();
    assert!(format!("{error:#}").contains("App inputs changed after planning"));
    assert!(!output.exists());
}
