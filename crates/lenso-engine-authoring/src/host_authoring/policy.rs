use anyhow::{Context, bail};
use lenso_app_plan::authoring::PluginDescriptor;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One exact independently installable release/implementation admitted by the Host.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedRelease {
    pub descriptor: PluginDescriptor,
    pub manifest_digest: String,
}

/// Host-owned admission and effective-configuration limits for an explicit Slot.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlotAdmission {
    pub slot: String,
    pub max_instances: usize,
    pub releases: Vec<AdmittedRelease>,
    pub configuration_schema: Option<Value>,
}

/// A local, bounded JSON Schema profile. No retrievers, regexes, or format callbacks.
pub(super) fn compile_ceiling(schema: &Value) -> anyhow::Result<jsonschema::Validator> {
    check_schema(schema, 0, &mut 0)?;
    jsonschema::draft202012::options()
        .build(schema)
        .map_err(|_| anyhow::anyhow!("invalid Host configuration ceiling schema"))
}

fn check_schema(schema: &Value, depth: usize, count: &mut usize) -> anyhow::Result<()> {
    *count += 1;
    if depth > 64 || *count > 4096 {
        bail!("Host configuration schema exceeds depth/node limits");
    }
    if schema.is_boolean() {
        return Ok(());
    }
    let object = schema
        .as_object()
        .context("Host configuration schema must be an object or boolean")?;
    for (key, value) in object {
        match key.as_str() {
            "properties" => {
                for child in value
                    .as_object()
                    .context("schema properties must be an object")?
                    .values()
                {
                    check_schema(child, depth + 1, count)?;
                }
            }
            "items" | "additionalProperties" | "not" | "if" | "then" | "else" => {
                check_schema(value, depth + 1, count)?;
            }
            "allOf" | "anyOf" | "oneOf" => {
                for child in value
                    .as_array()
                    .context("schema combinator must be an array")?
                {
                    check_schema(child, depth + 1, count)?;
                }
            }
            "type" | "const" | "enum" | "required" | "minimum" | "maximum" | "minItems"
            | "maxItems" | "uniqueItems" | "minLength" | "maxLength" | "title" | "description"
            | "$comment" => {}
            _ => bail!("unsupported Host configuration schema keyword `{key}`"),
        }
    }
    Ok(())
}
