use super::*;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::json;

#[derive(Clone, Debug, Subcommand)]
pub enum ContractCommand {
    /// Create one owned Capability package and its normal generated projections.
    New(NewArgs),
}
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Source {
    Schema,
    Rust,
}
#[derive(Clone, Debug, Args)]
pub struct NewArgs {
    /// Stable Capability name without @major, for example example.text.
    id: String,
    /// Contract authority: portable JSON Schema or a Rust trait.
    #[arg(long, value_enum, default_value = "schema")]
    source: Source,
    /// App root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
}

pub fn run(command: ContractCommand) -> anyhow::Result<()> {
    let ContractCommand::New(args) = command;
    lenso_app_authoring::identity::validate_plugin_id_v1(&args.id)?;
    let root = crate::plugins::project_root(args.root)?;
    let destination = root.join("contracts").join(&args.id);
    if fs::symlink_metadata(&destination).is_ok() {
        bail!("contract package already exists: {}", destination.display());
    }
    fs::create_dir_all(destination.parent().context("contract parent")?)?;
    let staging = tempfile::Builder::new()
        .prefix(".lenso-contract-new-")
        .tempdir_in(destination.parent().unwrap())?;
    let package_name = args.id.replace('.', "-");
    match args.source {
        Source::Schema => {
            fs::write(
                staging.path().join("package.json"),
                serde_json::to_vec_pretty(&json!({
                    "name":format!("{package_name}-contract"),"version":"1.0.0","private":true,"type":"module",
                "exports":"./generated/contract.ts", "dependencies":{"@lenso/contract-runtime":"0.3.0"},
                    "lenso":{"contract":{"descriptor":"capability.json","projection":"typescript","output":"generated/contract.ts"}}
                }))?,
            )?;
            fs::write(
                staging.path().join("capability.json"),
                serde_json::to_vec_pretty(&json!({
                    "id":format!("{}@1",args.id),"version":"1.0.0","portable":true,"cross_lane_transfer":false,
                    "operations":[{"name":"execute","interaction":"request","request_schema":"schemas/execute-request.schema.json","response_schema":"schemas/execute-response.schema.json","domain_error_schema":"schemas/execute-error.schema.json"}]
                }))?,
            )?;
            fs::create_dir(staging.path().join("schemas"))?;
            for (name, title) in [
                ("request", "ExecuteRequest"),
                ("response", "ExecuteResponse"),
            ] {
                fs::write(
                    staging
                        .path()
                        .join(format!("schemas/execute-{name}.schema.json")),
                    serde_json::to_vec_pretty(&json!({
                        "$schema":"https://json-schema.org/draft/2020-12/schema","title":title,"type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"type":"string"}}
                    }))?,
                )?;
            }
            fs::write(
                staging.path().join("schemas/execute-error.schema.json"),
                r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","oneOf":[{"const":"unavailable"}]}"#,
            )?;
        }
        Source::Rust => {
            fs::create_dir(staging.path().join("src"))?;
            fs::write(
                staging.path().join("Cargo.toml"),
                format!(
                    r#"[package]
name = "{package_name}-contract"
version = "1.0.0"
edition = "2024"
[workspace]
[package.metadata.lenso.contract]
source = "src/contract.rs"
descriptor = "capability.json"
projection = "rust-runtime"
output = "src/generated.rs"
[dependencies]
futures = "0.3"
lenso-kernel = "0.3.5"
lenso-contract-runtime = "0.2.0"
lenso-plugin-authoring = "0.2.0"
lenso-guest-sdk = "0.5.0"
lenso-runtime-codec = "0.4.1"
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
[build-dependencies]
lenso-contract-authoring = "=0.1.1"
lenso-contract-codegen = "=0.9.0"
schemars = "1.2"
"#
                ),
            )?;
            fs::write(
                staging.path().join("src/lib.rs"),
                "include!(\"generated.rs\");\n",
            )?;
            fs::write(
                staging.path().join("src/contract.rs"),
                format!(
                    r#"use lenso_contract_authoring as lenso;

#[derive(lenso::JsonSchema)]
#[schemars(deny_unknown_fields)]
struct ExecuteRequest {{ text: String }}

#[derive(lenso::JsonSchema)]
#[schemars(deny_unknown_fields)]
struct ExecuteResponse {{ text: String }}

#[derive(lenso::DomainError)]
enum OperationError {{ Unavailable }}

#[lenso::capability(id = "{}", major = 1, version = "1.0.0", portable = true, cross_lane_transfer = false)]
trait Contract {{
    async fn execute(&self, context: lenso::Ctx<'_>, request: ExecuteRequest) -> Result<ExecuteResponse, OperationError>;
}}
"#,
                    args.id
                ),
            )?;
            fs::write(
                staging.path().join("build.rs"),
                r#"#[allow(dead_code)]
#[path = "src/contract.rs"]
mod contract;
fn main() {
    for path in ["src/contract.rs", "capability.json", "schemas", "src/generated.rs"] { println!("cargo:rerun-if-changed={path}"); }
    lenso_contract_codegen::check_source_snapshot(&contract::__lenso_capability_snapshot(), std::path::Path::new("capability.json")).expect("run lenso app build/dev to synchronize the contract");
    lenso_contract_codegen::check_projection(std::path::Path::new("capability.json"), lenso_contract_codegen::ProjectionLanguage::RustRuntime, std::path::Path::new("src/generated.rs")).expect("stale generated projection");
}
"#,
            )?;
        }
    }
    fs::write(
        staging.path().join("README.md"),
        "# Local Capability\n\nEdit the contract source, then run `lenso app build` or `lenso app dev` from the App root. Generated projections update before Plugins compile. Keep identity and version explicit; incompatible changes require a new Capability series. Implement the generated Provider and declare the generated Client as a Plugin dependency.\n",
    )?;
    super::super::build::publish_new_output(staging.path(), &destination)?;
    println!(
        "Created Capability {}@1 at {}. app build/dev generates its projections.",
        args.id,
        destination.display()
    );
    Ok(())
}
