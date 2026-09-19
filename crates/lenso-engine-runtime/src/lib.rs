//! Lowers Engine processors to an ordinary, Plan-bound Lenso Request Capability.
//! This is an authoring adapter, not another plugin scheduler or execution class.
#[rustfmt::skip]
#[allow(dead_code, clippy::all)]
mod generated;
pub use generated::*;
use lenso_app_plan::{
    AppComposition, CapabilityBinding, CapabilityEndpointPlan, CapabilityRequirementPlan,
    PluginInstancePlan,
};
use lenso_engine::{ContextView, Plugin, Resource, Snapshot, Step};
use lenso_kernel::{
    InvocationContext, Kernel, NativeRequestFuture, RuntimeFailure, ShutdownOutcome,
};
use lenso_native_adapter::{
    NativePluginFactory, NativePluginFactoryContext, NativePluginInstance, NativePluginRegistry,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

/// Public SDK lowering used identically by bundled and third-party processors.
#[derive(Debug)]
pub struct RuntimeProcessor {
    processor: Arc<dyn Plugin>,
}
impl RuntimeProcessor {
    pub fn new(processor: impl Plugin + 'static) -> Self {
        Self {
            processor: Arc::new(processor),
        }
    }
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Input {
    step: Step,
    files: BTreeMap<String, Vec<u8>>,
    dependencies: BTreeMap<String, BTreeMap<String, Resource>>,
}
#[derive(Debug)]
struct Provider {
    processor: Arc<dyn Plugin>,
    cancelled: Arc<AtomicBool>,
    diagnostic: Rc<RefCell<Option<String>>>,
}
impl ProcessorProvider for Provider {
    fn process(
        &self,
        _: InvocationContext,
        request: ProcessRequest,
    ) -> NativeRequestFuture<Processor> {
        let processor = self.processor.clone();
        let cancelled = self.cancelled.clone();
        let diagnostic = self.diagnostic.clone();
        Box::pin(async move {
            let result = (|| -> anyhow::Result<ProcessResponse> {
                let input: Input = serde_json::from_str(request.input_json.as_str())?;
                let context = ContextView {
                    step: &input.step,
                    files: input
                        .files
                        .iter()
                        .map(|(key, value)| (key.clone(), value.as_slice()))
                        .collect(),
                    dependencies: input
                        .dependencies
                        .iter()
                        .map(|(key, value)| (key.clone(), value))
                        .collect(),
                    cancelled: &cancelled,
                };
                Ok(ProcessResponse {
                    output_json: serde_json::to_string(&processor.process(&context)?)?
                        .try_into()?,
                })
            })();
            Ok(result.map_err(|error| {
                *diagnostic.borrow_mut() = Some(format!("{error:#}"));
                ProcessError::Rejected
            }))
        })
    }
}
#[derive(Debug)]
struct Factory {
    processor: Option<Arc<dyn Plugin>>,
    cancelled: Arc<AtomicBool>,
    diagnostic: Rc<RefCell<Option<String>>>,
}
impl NativePluginFactory for Factory {
    fn package_id(&self) -> &'static str {
        if self.processor.is_some() {
            "lenso.engine.processor"
        } else {
            "lenso.engine.consumer"
        }
    }
    fn package_version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    fn instantiate(
        &self,
        _: NativePluginFactoryContext<'_>,
    ) -> Result<NativePluginInstance, RuntimeFailure> {
        Ok(match &self.processor {
            Some(processor) => {
                NativePluginInstance::new(vec![Rc::new(ProcessorEndpoint::new(Provider {
                    processor: processor.clone(),
                    cancelled: self.cancelled.clone(),
                    diagnostic: self.diagnostic.clone(),
                }))])
            }
            None => NativePluginInstance::default(),
        })
    }
}
impl Plugin for RuntimeProcessor {
    fn identity(&self) -> &str {
        self.processor.identity()
    }
    fn cacheable(&self) -> bool {
        self.processor.cacheable()
    }
    fn plan(&self, snapshot: &Snapshot) -> anyhow::Result<Vec<Step>> {
        self.processor.plan(snapshot)
    }
    fn process(&self, context: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>> {
        // Each Plugin generation owns its runtime thread. Nested authoring
        // workflows can invoke processors without nesting an executor.
        std::thread::scope(|scope| scope.spawn(|| self.invoke(context)).join())
            .map_err(|_| anyhow::anyhow!("processor runtime thread panicked"))?
    }
}
impl RuntimeProcessor {
    fn invoke(&self, context: &ContextView<'_>) -> anyhow::Result<BTreeMap<String, Resource>> {
        let input = Input {
            step: context.step.clone(),
            files: context
                .files
                .iter()
                .map(|(k, v)| (k.clone(), v.to_vec()))
                .collect(),
            dependencies: context
                .dependencies
                .iter()
                .map(|(k, v)| (k.clone(), (*v).clone()))
                .collect(),
        };
        let plan = AppComposition::new(
            vec![
                PluginInstancePlan::new("consumer", "lenso.engine.consumer")
                    .with_package_revision(env!("CARGO_PKG_VERSION"))
                    .with_requirement(CapabilityRequirementPlan::one(
                        CAPABILITY_ID,
                        DESCRIPTOR_VERSION,
                    )),
                PluginInstancePlan::new("processor", "lenso.engine.processor")
                    .with_package_revision(env!("CARGO_PKG_VERSION"))
                    .with_capability(CapabilityEndpointPlan::new(
                        CAPABILITY_ID,
                        DESCRIPTOR_VERSION,
                        ["process"],
                    )),
            ],
            vec![CapabilityBinding::new(
                "consumer",
                CAPABILITY_ID,
                DESCRIPTOR_VERSION,
                "processor",
            )],
        )
        .resolve()
        .map_err(|e| anyhow::anyhow!("processor admission: {e:?}"))?;
        let diagnostic = Rc::new(RefCell::new(None));
        let registry = NativePluginRegistry::new()
            .with_factory(Factory {
                processor: Some(self.processor.clone()),
                cancelled: context.cancelled.clone(),
                diagnostic: diagnostic.clone(),
            })
            .with_factory(Factory {
                processor: None,
                cancelled: context.cancelled.clone(),
                diagnostic: diagnostic.clone(),
            });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let _entered = runtime.enter();
        let result = futures::executor::block_on(tokio::task::LocalSet::new().run_until(async {
            let app = Kernel::start_native(plan, lenso_runner::TokioDriver::new(), registry)
                .await
                .map_err(|e| anyhow::anyhow!("processor startup: {e:?}"))?;
            // Capture all invocation failures, then always close the generation.
            let result = async {
                let handle = app
                    .handle::<Processor>("consumer")
                    .map_err(|e| anyhow::anyhow!("processor binding: {e:?}"))?;
                let response = handle
                    .invoke(
                        "process",
                        ProcessRequest {
                            input_json: serde_json::to_string(&input)?.try_into()?,
                        },
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("processor runtime: {e:?}"))?
                    .map_err(|e| anyhow::anyhow!("processor rejected input: {e:?}"))?;
                Ok::<_, anyhow::Error>(serde_json::from_str(response.output_json.as_str())?)
            }
            .await;
            let outcome = app.shutdown(Duration::from_secs(5)).await;
            if outcome != ShutdownOutcome::Clean {
                anyhow::bail!("processor shutdown: {outcome:?}");
            }
            result
        }));
        drop(_entered);
        runtime.shutdown_background();
        if let Some(message) = diagnostic.borrow_mut().take() {
            anyhow::bail!("processor {}: {message}", self.identity());
        }
        result
    }
}
