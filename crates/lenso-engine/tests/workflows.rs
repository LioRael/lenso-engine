use lenso_engine::{
    ContextView, Engine, Generation, Plugin, Resource, Snapshot, Step,
    bootstrap::BootstrapLock,
    publication::{publish, verify},
    session::Session,
};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, atomic::AtomicBool},
};
#[derive(Debug)]
struct Text;
impl Plugin for Text {
    fn identity(&self) -> &str {
        "text.v1"
    }
    fn cacheable(&self) -> bool {
        true
    }
    fn plan(&self, snapshot: &Snapshot) -> anyhow::Result<Vec<Step>> {
        Ok(snapshot
            .files()
            .keys()
            .map(|p| Step {
                id: p.clone(),
                inputs: vec![p.clone()],
                after: vec![],
                options: serde_json::Value::Null,
            })
            .collect())
    }
    fn process(&self, context: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>> {
        let text = std::str::from_utf8(context.files.values().next().unwrap())?;
        Ok(BTreeMap::from([(
            "file".into(),
            Resource::file(context.step.id.clone(), text.as_bytes().to_vec())?,
        )]))
    }
}
#[test]
fn refresh_preserves_success_on_failure_and_removes_disabled_contributions() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("readme.md");
    fs::write(&file, "first").unwrap();
    let mut engine = Engine::default();
    engine.register(Text).unwrap();
    let mut session = Session::new(engine, vec![root.path().into()]).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let first = session.refresh(&cancelled).unwrap().unwrap();
    assert!(session.refresh(&cancelled).unwrap().is_none());
    fs::write(&file, [255]).unwrap();
    assert!(session.refresh(&cancelled).is_err());
    assert_eq!(session.current().unwrap().outputs, first.outputs);
    fs::write(&file, "second").unwrap();
    assert!(session.refresh(&cancelled).unwrap().is_some());
    session.replace_engine(Engine::default());
    assert!(
        session
            .refresh(&cancelled)
            .unwrap()
            .unwrap()
            .outputs
            .is_empty()
    );
    fs::remove_file(file).unwrap();
    assert!(
        session
            .refresh(&cancelled)
            .unwrap()
            .unwrap()
            .outputs
            .is_empty()
    );
}
#[test]
fn publication_is_atomic_on_conflict_and_detects_tampering() {
    let root = tempfile::tempdir().unwrap();
    let mut generation = Generation::default();
    generation.outputs.insert(
        "first".into(),
        BTreeMap::from([(
            "page".into(),
            Resource::file("index.md".into(), b"hello".to_vec()).unwrap(),
        )]),
    );
    let published = publish(root.path(), &generation).unwrap();
    let selected = fs::read(root.path().join("current.json")).unwrap();
    let output = verify(root.path(), &published).unwrap();
    assert_eq!(fs::read(output.join("files/index.md")).unwrap(), b"hello");
    generation
        .outputs
        .insert("conflict".into(), generation.outputs["first"].clone());
    assert!(publish(root.path(), &generation).is_err());
    assert_eq!(
        selected,
        fs::read(root.path().join("current.json")).unwrap()
    );
    fs::write(output.join("files/index.md"), "tampered").unwrap();
    assert!(verify(root.path(), &published).is_err());
    assert!(Resource::file("../escape".into(), vec![]).is_err());
}
#[cfg(unix)]
#[test]
fn presets_discover_local_plugins_without_execution_and_lock_exact_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path();
    fs::create_dir(base.join("content")).unwrap();
    fs::create_dir_all(base.join("plugins/reader")).unwrap();
    fs::write(base.join("content/readme.md"), "hello").unwrap();
    fs::write(base.join("plugins/reader/engine-plugin.json"),serde_json::to_vec(&serde_json::json!({"schema":"lenso.engine-plugin.v1","identity":"test.reader.v1","entries":["*.md"],"program":"/bin/sh","args":["reader.sh"],"artifacts":["reader.sh"]})).unwrap()).unwrap();
    fs::write(base.join("plugins/reader/reader.sh"),"cat >/dev/null; touch executed; printf '%s' '{\"schema\":\"lenso.engine-processed.v1\",\"outputs\":{}}'").unwrap();
    fs::write(base.join("preset.json"),r#"{"schema":"lenso.engine-workflow.v1","plugin_sources":["plugins"],"plugins":["test.reader.v1"]}"#).unwrap();
    let config = base.join("engine.json");
    fs::write(
        &config,
        r#"{"schema":"lenso.engine-workflow.v1","sources":["content"],"presets":["preset.json"]}"#,
    )
    .unwrap();
    let lock = BootstrapLock::resolve(&config).unwrap();
    let lockpath = base.join("engine.lock.json");
    lock.save(&lockpath).unwrap();
    assert!(!base.join("plugins/reader/executed").exists());
    let lock = BootstrapLock::load(&lockpath, &config).unwrap();
    let mut engine = Engine::default();
    for plugin in lock.processors().unwrap() {
        engine.register(plugin).unwrap();
    }
    let plan = engine
        .plan(Snapshot::read(&base.join("content")).unwrap())
        .unwrap();
    engine
        .execute(&plan, &Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert!(base.join("plugins/reader/executed").exists());
    fs::write(base.join("plugins/reader/reader.sh"), "exit 0").unwrap();
    assert!(BootstrapLock::load(&lockpath, &config).is_err());
    assert!(
        engine
            .execute(&plan, &Arc::new(AtomicBool::new(false)))
            .is_err()
    );
}
#[test]
fn preset_cycles_are_rejected_without_recursing_forever() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("engine.json");
    fs::write(
        &file,
        r#"{"schema":"lenso.engine-workflow.v1","presets":["engine.json"]}"#,
    )
    .unwrap();
    assert!(
        BootstrapLock::resolve(&file)
            .unwrap_err()
            .to_string()
            .contains("cyclic")
    );
}
