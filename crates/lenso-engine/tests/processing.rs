use lenso_engine::{ContextView, Engine, Plugin, Resource, Snapshot, Step};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
#[derive(Debug)]
struct Pipeline {
    calls: Arc<AtomicUsize>,
    cycle: bool,
}
impl Plugin for Pipeline {
    fn identity(&self) -> &str {
        "test.pipeline.v1"
    }
    fn cacheable(&self) -> bool {
        true
    }
    fn plan(&self, _: &Snapshot) -> anyhow::Result<Vec<Step>> {
        Ok(vec![
            Step {
                id: "read".into(),
                inputs: vec!["article.md".into()],
                after: if self.cycle {
                    vec!["index".into()]
                } else {
                    vec![]
                },
                options: serde_json::Value::Null,
            },
            Step {
                id: "index".into(),
                inputs: vec![],
                after: vec!["read".into()],
                options: serde_json::Value::Null,
            },
        ])
    }
    fn process(&self, context: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let value = if context.step.id == "read" {
            serde_json::json!(std::str::from_utf8(context.files["article.md"])?)
        } else {
            context.dependencies["read"]["text"].value.clone()
        };
        Ok(BTreeMap::from([(
            "text".into(),
            Resource {
                schema: "test.text.v1".into(),
                value,
            },
        )]))
    }
}
fn snapshot(text: &str) -> Snapshot {
    let mut s = Snapshot::default();
    s.insert("article.md".into(), text.as_bytes().to_vec())
        .unwrap();
    s
}
#[test]
fn planning_is_read_only_and_dependencies_invalidate_transitively() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut engine = Engine::default();
    engine
        .register(Pipeline {
            calls: calls.clone(),
            cycle: false,
        })
        .unwrap();
    let first = engine.plan(snapshot("one")).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    assert_eq!(engine.execute(&first, &cancel).unwrap().cache_hits, 0);
    assert_eq!(engine.execute(&first, &cancel).unwrap().cache_hits, 2);
    let changed = engine.plan(snapshot("two")).unwrap();
    let result = engine.execute(&changed, &cancel).unwrap();
    assert_eq!(result.cache_hits, 0);
    assert_eq!(result.outputs["index"]["text"].value, "two");
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(
        engine
            .execute(&first, &std::sync::Arc::new(AtomicBool::new(true)))
            .is_err()
    );
}
#[test]
fn cycles_and_duplicate_plugins_fail_before_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut engine = Engine::default();
    engine
        .register(Pipeline {
            calls: calls.clone(),
            cycle: true,
        })
        .unwrap();
    assert!(
        engine
            .register(Pipeline {
                calls: calls.clone(),
                cycle: false
            })
            .is_err()
    );
    assert!(engine.plan(snapshot("hello")).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
#[test]
fn no_plugins_means_no_conventions_and_paths_are_checked() {
    let mut engine = Engine::default();
    let plan = engine.plan(snapshot("hello")).unwrap();
    assert!(
        engine
            .execute(&plan, &std::sync::Arc::new(AtomicBool::new(false)))
            .unwrap()
            .outputs
            .is_empty()
    );
    assert!(
        Snapshot::default()
            .insert("../escape".into(), vec![])
            .is_err()
    );
}
#[cfg(unix)]
#[test]
fn local_process_manifest_does_not_execute_until_selected() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("executed");
    let manifest = dir.path().join("processor.json");
    std::fs::write(&manifest, serde_json::to_vec(&serde_json::json!({
        "schema":"lenso.engine-plugin.v1", "identity":"test.shell.v1", "entries":["*.md"],
        "program":"/bin/sh", "args":["-c", "cat >/dev/null; touch executed; printf '%s' '{\"schema\":\"lenso.engine-processed.v1\",\"outputs\":{\"document\":{\"schema\":\"test.document.v1\",\"value\":\"read\"}}}'"]
    })).unwrap()).unwrap();
    let mut engine = Engine::default();
    engine
        .register(lenso_engine::external::ProcessPlugin::load(&manifest).unwrap())
        .unwrap();
    let plan = engine.plan(snapshot("read without compilation")).unwrap();
    assert!(!marker.exists());
    let generation = engine
        .execute(&plan, &std::sync::Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert!(marker.exists());
    assert_eq!(
        generation.outputs["test.shell.v1/article.md"]["document"].value,
        "read"
    );
}
#[cfg(unix)]
#[test]
fn cancelling_a_processor_kills_its_process_group() {
    use lenso_engine::process::{ProcessSpec, execute_cancellable};
    use std::sync::atomic::AtomicI32;
    let dir = tempfile::tempdir().unwrap();
    let cancelled = AtomicBool::new(false);
    let active = Arc::new(AtomicI32::new(0));
    let spec = ProcessSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 30 & wait".into()],
        directory: dir.path().into(),
    };
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while active.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            cancelled.store(true, Ordering::SeqCst);
        });
        assert!(
            execute_cancellable(&spec, &serde_json::json!({}), active.clone(), &cancelled)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
    });
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[test]
fn custom_process_budget_terminates_a_slow_processor() {
    use lenso_engine::process::{ProcessBudget, ProcessSpec, execute_cancellable_with_budget};
    use std::sync::atomic::AtomicI32;
    let dir = tempfile::tempdir().unwrap();
    let spec = ProcessSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 1".into()],
        directory: dir.path().into(),
    };
    let budget = ProcessBudget::new(std::time::Duration::from_millis(50), 1024).unwrap();
    let error = execute_cancellable_with_budget(
        &spec,
        &serde_json::json!({}),
        Arc::new(AtomicI32::new(0)),
        &AtomicBool::new(false),
        budget,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("50 millisecond execution budget")
    );
}
