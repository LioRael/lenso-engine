use lenso_engine::{Engine, Snapshot};
use std::{path::PathBuf, sync::atomic::AtomicBool};
fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("usage: documents INPUT_DIRECTORY"))?,
    );
    let mut engine = Engine::default();
    engine.register(lenso_engine_markdown::Markdown)?;
    let plan = engine.plan(Snapshot::read(&root)?)?;
    let generation = engine.execute(&plan, &std::sync::Arc::new(AtomicBool::new(false)))?;
    println!("{}", serde_json::to_string_pretty(&generation)?);
    Ok(())
}
