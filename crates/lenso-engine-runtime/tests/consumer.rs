use lenso_engine::{ContextView, Engine, Plugin, Resource, Snapshot, Step};
use lenso_engine_runtime::RuntimeProcessor;
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
};
#[derive(Debug)]
struct Reader;
impl Plugin for Reader {
    fn identity(&self) -> &str {
        "test.reader.v1"
    }
    fn plan(&self, _: &Snapshot) -> anyhow::Result<Vec<Step>> {
        Ok(vec![Step {
            id: "read".into(),
            inputs: vec!["hello.md".into()],
            after: vec![],
            options: serde_json::Value::Null,
        }])
    }
    fn process(&self, ctx: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>> {
        let text = std::str::from_utf8(ctx.files["hello.md"])?;
        if text == "fail" {
            anyhow::bail!("reader rejection details");
        }
        Ok(BTreeMap::from([(
            "text".into(),
            Resource {
                schema: "test.text.v1".into(),
                value: serde_json::json!(text),
            },
        )]))
    }
}
#[test]
fn processor_uses_real_plan_bound_kernel_requests_and_shuts_down_on_errors() {
    let mut engine = Engine::default();
    engine.register(RuntimeProcessor::new(Reader)).unwrap();
    for text in ["hello", "fail", "again"] {
        let mut snapshot = Snapshot::default();
        snapshot
            .insert("hello.md".into(), text.as_bytes().to_vec())
            .unwrap();
        let plan = engine.plan(snapshot).unwrap();
        let result = engine.execute(&plan, &Arc::new(AtomicBool::new(false)));
        if text == "fail" {
            assert!(format!("{:#}", result.unwrap_err()).contains("reader rejection details"));
        } else {
            assert_eq!(result.unwrap().outputs["read"]["text"].value, text);
        }
    }
}
#[test]
fn generated_processor_contract_is_fresh() {
    let descriptor =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/capability.json");
    for (language, file) in [
        (
            lenso_contract_codegen::ProjectionLanguage::RustRuntime,
            "src/generated.rs",
        ),
        (
            lenso_contract_codegen::ProjectionLanguage::TypeScript,
            "processor.ts",
        ),
    ] {
        assert_eq!(
            lenso_contract_codegen::generate_projection(&descriptor, language)
                .unwrap()
                .source,
            std::fs::read_to_string(descriptor.parent().unwrap().parent().unwrap().join(file))
                .unwrap()
        );
    }
}
