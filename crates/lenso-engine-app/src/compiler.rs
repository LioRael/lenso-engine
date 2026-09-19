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
    pub budget: process::ProcessBudget,
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
        let bytes = process::execute_cancellable_with_budget(
            &self.process,
            &context.step.options,
            self.active_group.clone(),
            context.cancelled,
            self.budget,
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, atomic::AtomicBool},
        time::Duration,
    };

    #[test]
    fn convention_compiler_applies_its_declared_budget() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = lenso_engine::Engine::default();
        engine
            .register(ConventionCompiler {
                identity: "test.convention.v1".into(),
                request: serde_json::json!({"schema":"lenso.convention-compile.v1"}),
                process: ProcessSpec {
                    program: "/bin/sh".into(),
                    args: vec!["-c".into(), "sleep 1".into()],
                    directory: directory.path().into(),
                },
                budget: process::ProcessBudget::new(Duration::from_millis(50), 1024).unwrap(),
                active_group: Arc::new(AtomicI32::new(0)),
            })
            .unwrap();
        let plan = engine.plan(Snapshot::default()).unwrap();
        let error = engine
            .execute(&plan, &Arc::new(AtomicBool::new(false)))
            .unwrap_err();
        assert!(format!("{error:#}").contains("50 millisecond execution budget"));
    }
}
