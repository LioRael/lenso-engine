// JSON stays erased only when the entire Capability boundary is portable.
#[derive(Clone, Debug)]
struct PortableCodec {
    id: &'static str,
    version: &'static str,
    digest: &'static str,
    operations: &'static [&'static str],
}
impl PortableCodec {
    fn check(&self, operation: &str) -> Result<(), lenso_kernel::RuntimeFailure> {
        if self.operations.contains(&operation) {
            Ok(())
        } else {
            Err(lenso_kernel::RuntimeFailure::UnknownOperation {
                capability: self.id,
                operation: operation.into(),
            })
        }
    }
    fn value(
        &self,
        operation: &str,
        value: &dyn std::any::Any,
    ) -> Result<serde_json::Value, lenso_kernel::RuntimeFailure> {
        self.check(operation)?;
        value.downcast_ref::<serde_json::Value>().cloned().ok_or(
            lenso_kernel::RuntimeFailure::ProtocolViolation {
                capability: self.id,
            },
        )
    }
}
impl lenso_runtime_codec::JsonCapabilityCodec for PortableCodec {
    fn capability_id(&self) -> &'static str {
        self.id
    }
    fn descriptor_version(&self) -> &'static str {
        self.version
    }
    fn descriptor_digest(&self) -> &'static str {
        self.digest
    }
    fn request_operations(&self) -> &'static [&'static str] {
        self.operations
    }
    fn encode_request(
        &self,
        operation: &str,
        request: &dyn std::any::Any,
    ) -> Result<serde_json::Value, lenso_kernel::RuntimeFailure> {
        self.value(operation, request)
    }
    fn decode_response(
        &self,
        operation: &str,
        value: serde_json::Value,
    ) -> Result<Box<dyn std::any::Any>, lenso_kernel::RuntimeFailure> {
        self.check(operation)?;
        Ok(Box::new(value))
    }
    fn decode_domain_error(
        &self,
        operation: &str,
        value: serde_json::Value,
    ) -> Result<Box<dyn std::any::Any>, lenso_kernel::RuntimeFailure> {
        self.check(operation)?;
        Ok(Box::new(value))
    }
    fn invoke_host_request(
        &self,
        dependency: lenso_kernel::PluginDependencyHandle,
        operation: String,
        request: serde_json::Value,
        context: lenso_kernel::InvocationContext,
    ) -> lenso_runtime_codec::JsonHostRequestFuture {
        let codec = self.clone();
        Box::pin(async move {
            codec.check(&operation)?;
            match dependency
                .invoke_erased(&operation, Box::new(request), context)
                .await?
            {
                Ok(value) => Ok(lenso_runtime_codec::JsonInvocationOutcome::Success(
                    codec.value(&operation, value.as_ref())?,
                )),
                Err(value) => Ok(lenso_runtime_codec::JsonInvocationOutcome::DomainError(
                    codec.value(&operation, value.as_ref())?,
                )),
            }
        })
    }
}
fn portable_codecs(
    plan: &ResolvedAppPlan,
    typed: &std::collections::BTreeSet<&str>,
    evidence: &std::collections::BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<Vec<PortableCodec>> {
    let mut endpoints = std::collections::BTreeMap::new();
    let portable = plan
        .plugin_instances()
        .iter()
        .filter(|i| i.execution_class().as_str() != "lenso.native-rust@1")
        .flat_map(|i| {
            i.provided_capabilities()
                .iter()
                .map(|c| c.capability_id())
                .chain(i.required_capabilities().iter().map(|c| c.capability_id()))
        })
        .collect::<std::collections::BTreeSet<_>>();
    for instance in plan.plugin_instances() {
        if instance.execution_class().as_str() == "lenso.native-rust@1" {
            for id in instance
                .provided_capabilities()
                .iter()
                .map(|c| c.capability_id())
                .chain(
                    instance
                        .required_capabilities()
                        .iter()
                        .map(|c| c.capability_id()),
                )
            {
                if portable.contains(id) && !typed.contains(id) {
                    bail!(
                        "native Capability {id} requires a rust-runtime contract projection in the Plugin's normal Cargo dependencies"
                    );
                }
            }
            continue;
        }
        for endpoint in instance.provided_capabilities() {
            if typed.contains(endpoint.capability_id()) {
                continue;
            }
            if !endpoint.stream_operations().is_empty() || !endpoint.event_operations().is_empty() {
                bail!(
                    "Capability {} needs a typed codec for Stream/Event interaction",
                    endpoint.capability_id()
                );
            }
            let metadata = evidence
                .get(instance.package_id())
                .and_then(|d| d["capabilities"].as_array())
                .and_then(|capabilities| {
                    capabilities.iter().find(|c| {
                        c["capability_id"] == endpoint.capability_id()
                            && c["descriptor_version"] == endpoint.descriptor_version()
                    })
                });
            let digest = metadata
                .and_then(|c| c["descriptor_digest"].as_str())
                .unwrap_or("");
            if instance.authoring_version() == 2 && digest.is_empty() {
                bail!(
                    "Capability {} needs generated Descriptor digest evidence; rebuild/repack this Plugin with the current source compiler",
                    endpoint.capability_id()
                );
            }
            let identity = (
                endpoint.descriptor_version().to_owned(),
                endpoint.operations().to_vec(),
                digest.to_owned(),
            );
            if endpoints
                .insert(endpoint.capability_id().to_owned(), identity.clone())
                .is_some_and(|old| old != identity)
            {
                bail!(
                    "conflicting portable Capability {}",
                    endpoint.capability_id()
                );
            }
        }
    }
    // One bounded immutable codec table per process, never per invocation.
    Ok(endpoints
        .into_iter()
        .map(|(id, (version, operations, digest))| PortableCodec {
            digest: Box::leak(digest.into_boxed_str()),
            id: Box::leak(id.into_boxed_str()),
            version: Box::leak(version.into_boxed_str()),
            operations: Box::leak(
                operations
                    .into_iter()
                    .map(|s| &*Box::leak(s.into_boxed_str()))
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
        })
        .collect())
}

#[derive(Clone, Debug)]
struct LegacyBunCodec<C>(C);
impl<C: lenso_runtime_codec::JsonCapabilityCodec> lenso_bun_adapter::BunCapabilityCodec
    for LegacyBunCodec<C>
{
    fn capability_id(&self) -> &'static str {
        self.0.capability_id()
    }
    fn descriptor_version(&self) -> &'static str {
        self.0.descriptor_version()
    }
    fn operations(&self) -> &'static [&'static str] {
        self.0.request_operations()
    }
    fn encode_request(
        &self,
        operation: &str,
        request: &dyn std::any::Any,
    ) -> Result<serde_json::Value, lenso_kernel::RuntimeFailure> {
        self.0.encode_request(operation, request)
    }
    fn decode_response(
        &self,
        operation: &str,
        value: serde_json::Value,
    ) -> Result<Box<dyn std::any::Any>, lenso_kernel::RuntimeFailure> {
        self.0.decode_response(operation, value)
    }
    fn decode_domain_error(
        &self,
        operation: &str,
        value: serde_json::Value,
    ) -> Result<Box<dyn std::any::Any>, lenso_kernel::RuntimeFailure> {
        self.0.decode_domain_error(operation, value)
    }
}
