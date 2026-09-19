//! Optional Markdown reading convention, independent of App and Runtime.
use lenso_engine::{ContextView, Plugin, Resource, Snapshot, Step};
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct Markdown;
impl Plugin for Markdown {
    fn cacheable(&self) -> bool {
        true
    }
    fn identity(&self) -> &str {
        "lenso.markdown.v1"
    }
    fn plan(&self, snapshot: &Snapshot) -> anyhow::Result<Vec<Step>> {
        Ok(snapshot
            .files()
            .keys()
            .filter(|p| p.ends_with(".md"))
            .map(|path| Step {
                id: format!("markdown/{path}"),
                inputs: vec![path.clone()],
                after: vec![],
                options: serde_json::Value::Null,
            })
            .collect())
    }
    fn process(&self, context: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>> {
        let (path, bytes) = context
            .files
            .first_key_value()
            .ok_or_else(|| anyhow::anyhow!("missing Markdown input"))?;
        let text = std::str::from_utf8(bytes)?;
        Ok(BTreeMap::from([(
            path.clone(),
            Resource {
                schema: "lenso.markdown-document.v1".into(),
                value: serde_json::json!({"source":path,"text":text}),
            },
        )]))
    }
}
