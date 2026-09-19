//! Compatibility adapter for existing App compiler extensions. Domain protocol
//! interpretation stays outside the generic Engine. Artifact admission remains
//! with App assembly, after compilation.
use lenso_engine::{
    ContextView, Plugin, Resource, Snapshot, Step,
    process::{self, ProcessSpec},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicI32},
};

#[derive(Debug)]
pub struct ConventionCompiler {
    pub identity: String,
    pub request: serde_json::Value,
    pub process: ProcessSpec,
    pub active_group: Arc<AtomicI32>,
}
impl Plugin for ConventionCompiler {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn plan(&self, _snapshot: &Snapshot) -> anyhow::Result<Vec<Step>> {
        if self
            .request
            .get("schema")
            .and_then(serde_json::Value::as_str)
            != Some("lenso.convention-compile.v1")
        {
            anyhow::bail!("invalid App compilation request");
        }
        if serde_json::to_vec(&self.request)?.len() > 16384 {
            anyhow::bail!("compiler request exceeds 16 KiB");
        }
        Ok(vec![Step {
            id: "app/compile".into(),
            inputs: vec![],
            after: vec![],
            options: self.request.clone(),
        }])
    }
    fn process(&self, context: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>> {
        let bytes = process::execute_cancellable(
            &self.process,
            &context.step.options,
            self.active_group.clone(),
            context.cancelled,
        )?;
        let response: serde_json::Value = serde_json::from_slice(&bytes)?;
        if response != serde_json::json!({"schema":"lenso.convention-compiled.v1"}) {
            anyhow::bail!("unsupported compiler response");
        }
        Ok(BTreeMap::from([(
            "compilation".into(),
            Resource {
                schema: "lenso.convention-compiled.v1".into(),
                value: response,
            },
        )]))
    }
}
