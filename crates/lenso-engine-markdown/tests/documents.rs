use lenso_engine::{Engine, Snapshot};
use std::sync::atomic::AtomicBool;
#[test]
fn documents_are_optional_incremental_and_removed_with_inputs() {
    let mut input = Snapshot::default();
    input
        .insert("guide.md".into(), b"# Hello".to_vec())
        .unwrap();
    input
        .insert("plugin.rs".into(), b"deliberately invalid Rust".to_vec())
        .unwrap();
    let mut engine = Engine::default();
    assert!(engine.plan(input.clone()).unwrap().steps().is_empty());
    engine.register(lenso_engine_markdown::Markdown).unwrap();
    let plan = engine.plan(input).unwrap();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let result = engine.execute(&plan, &cancel).unwrap();
    assert_eq!(result.outputs.len(), 1);
    assert_eq!(
        result.outputs["markdown/guide.md"]["guide.md"].value["text"],
        "# Hello"
    );
    assert_eq!(engine.execute(&plan, &cancel).unwrap().cache_hits, 1);
    let empty = engine.plan(Snapshot::default()).unwrap();
    assert!(engine.execute(&empty, &cancel).unwrap().outputs.is_empty());
}
