//! Bootstrap artifact: executable before any source convention is loaded.
use lenso_engine::{ContextView, Plugin, Resource, Step};
use std::{
    collections::BTreeMap,
    io::Read,
    sync::{Arc, atomic::AtomicBool},
};
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    step: Step,
    files: BTreeMap<String, Vec<u8>>,
    dependencies: BTreeMap<String, BTreeMap<String, Resource>>,
}
fn main() -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(128 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 * 1024 {
        anyhow::bail!("request too large");
    }
    let request: Request = serde_json::from_slice(&bytes)?;
    if request.schema != "lenso.engine-process.v1" {
        anyhow::bail!("unsupported request");
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let context = ContextView {
        step: &request.step,
        files: request
            .files
            .iter()
            .map(|(k, v)| (k.clone(), v.as_slice()))
            .collect(),
        dependencies: request
            .dependencies
            .iter()
            .map(|(k, v)| (k.clone(), v))
            .collect(),
        cancelled: &cancelled,
    };
    let outputs = lenso_engine_markdown::Markdown.process(&context)?;
    println!(
        "{}",
        serde_json::json!({"schema":"lenso.engine-processed.v1","outputs":outputs})
    );
    Ok(())
}
