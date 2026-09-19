use super::*;
use serde_json::json;

/// Compile just the contract source module using its declared build dependencies.
/// Building the owning library first would reject its stale generated projection.
pub(super) fn extract(
    app: &Path,
    contract: &Contract,
    source: Option<&Path>,
    output: &Path,
) -> anyhow::Result<()> {
    let (package, metadata) = contract
        .cargo
        .as_ref()
        .context("Rust contract source needs a Cargo package")?;
    let node = metadata["resolve"]["nodes"]
        .as_array()
        .context("Cargo graph")?
        .iter()
        .find(|n| n["id"] == package["id"])
        .context("contract dependency graph")?;
    let packages = metadata["packages"].as_array().context("Cargo packages")?;
    let mut dependencies = BTreeMap::new();
    let mut generator = None;
    for dep in node["deps"].as_array().context("Cargo dependencies")? {
        if !dep["dep_kinds"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind["kind"] == "build"))
        {
            continue;
        }
        let package = packages
            .iter()
            .find(|p| p["id"] == dep["pkg"])
            .context("build dependency")?;
        let name = dep["name"].as_str().context("dependency alias")?;
        let mut specification = super::super::local_host::dependency(package)?;
        if let Some(resolved) = metadata["resolve"]["nodes"]
            .as_array()
            .and_then(|nodes| nodes.iter().find(|n| n["id"] == package["id"]))
        {
            specification["features"] = resolved["features"].clone();
            specification["default-features"] = json!(false);
        }
        dependencies.insert(name.to_owned(), specification);
        if package["name"] == "lenso-contract-codegen" {
            generator = Some(name.to_owned());
        }
    }
    let generator = generator
        .context("source contract requires lenso-contract-codegen in build-dependencies")?;
    let cache = app
        .join(".lenso/contract-source")
        .join(super::super::local_host::digest_text(
            &contract.root.to_string_lossy(),
        ));
    fs::create_dir_all(cache.join("src"))?;
    write_changed(
        cache.join("Cargo.toml"),
        toml::to_string_pretty(
            &json!({"package":{"name":"lenso-contract-source-extractor","version":"0.0.0","edition":"2024"},"workspace":{},"dependencies":dependencies}),
        )?,
    )?;
    let mut languages = contract
        .declaration
        .projections
        .iter()
        .map(|p| p.projection.as_str())
        .collect::<Vec<_>>();
    if let Some(language) = &contract.declaration.projection {
        languages.push(language);
    }
    let mut generate = String::new();
    for (index, language) in languages.into_iter().enumerate() {
        let variant = match language {
            "rust" => "Rust",
            "rust-runtime" => "RustRuntime",
            "rust-plugin" => "RustPlugin",
            "typescript" => "TypeScript",
            "wit" => "Wit",
            other => bail!("unsupported projection {other}"),
        };
        generate.push_str(&format!("std::fs::write(output.parent().unwrap().join(\"projection-{index}.txt\"), {generator}::generate_projection(output, {generator}::ProjectionLanguage::{variant}).expect(\"generate projection\").source).unwrap();\n"));
    }
    let module = match source {
        Some(source) => format!(
            "#[allow(dead_code)]\n#[path = {:?}]\nmod contract_source;\n",
            source.to_str().context("source path must be UTF-8")?
        ),
        None => String::new(),
    };
    let extract = if source.is_some() {
        format!(
            "{generator}::write_source_snapshot(&contract_source::__lenso_capability_snapshot(), output).expect(\"extract Capability source\");"
        )
    } else {
        String::new()
    };
    write_changed(
        cache.join("src/main.rs"),
        format!(
            "{module}fn main() {{ let argument = std::env::args_os().nth(1).expect(\"output\"); let output = std::path::Path::new(&argument); {extract} {generate} }}\n"
        ),
    )?;
    let status = Command::new("cargo")
        .args(["run", "--quiet", "--manifest-path"])
        .arg(cache.join("Cargo.toml"))
        .arg("--")
        .arg(output)
        .status()?;
    if !status.success() {
        bail!(
            "Capability source extraction/generation failed: {}",
            contract.root.display()
        );
    }
    Ok(())
}

fn write_changed(path: PathBuf, content: String) -> anyhow::Result<()> {
    if fs::read_to_string(&path).ok().as_ref() != Some(&content) {
        fs::write(path, content)?;
    }
    Ok(())
}
