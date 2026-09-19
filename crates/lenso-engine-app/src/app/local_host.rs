//! Generate a native Host using normal Cargo package identities for linked
//! Plugins and their typed contract projections. Never regenerate native types.
use anyhow::{Context, bail};
use lenso_app_authoring::discovery::Candidate;
use lenso_app_plan::authoring::{HostCatalog, PluginDescriptor};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub(super) fn generate(
    stage: &Path,
    cache: &Path,
    candidates: &[Candidate],
) -> anyhow::Result<Vec<PluginDescriptor>> {
    fs::create_dir_all(cache)?;
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache.join("build.lock"))?;
    lock.lock().context("lock generated Host build cache")?;
    let generated = cache.join("source");
    fs::create_dir_all(generated.join("src"))?;
    let mut dependencies = BTreeMap::<String, Value>::new();
    for (name, version) in [
        ("anyhow", "1"),
        ("futures", "0.3"),
        ("serde_json", "1"),
        ("sha2", "0.10"),
        ("lenso-app-plan", "0.4.1"),
        ("lenso-kernel", "0.3.5"),
        ("lenso-native-adapter", "=0.3.14"),
        ("lenso-runner", "0.2.14"),
        ("lenso-bun-adapter", "0.1.7"),
        ("lenso-process-adapter", "0.3.0"),
        ("lenso-wasm-component-adapter", "0.2.5"),
        ("lenso-runtime-codec", "0.3.2"),
    ] {
        dependencies.insert(name.into(), json!(version));
    }
    dependencies.insert(
        "serde".into(),
        json!({"version":"1", "features":["derive"]}),
    );
    dependencies.insert(
        "tokio".into(),
        json!({"version":"1.52", "features":["rt-multi-thread","macros","signal","time","net"]}),
    );
    let mut linked = String::new();
    let mut codecs = BTreeMap::<String, (String, Value)>::new();
    let mut seen_packages = BTreeSet::new();
    let mut codec_cohorts = BTreeSet::new();
    let mut web_contract = None;
    let mut watch_roots = BTreeSet::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.format != "cargo" {
            continue;
        }
        let output = Command::new("cargo")
            .args(["metadata", "--format-version=1", "--filter-platform"])
            .arg(lenso_app_authoring::native_host_target())
            .arg("--manifest-path")
            .arg(candidate.project.join("Cargo.toml"))
            .output()
            .context("read native Cargo graph")?;
        if !output.status.success() {
            bail!(
                "Cargo metadata for {}: {}",
                candidate.project.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let metadata: Value = serde_json::from_slice(&output.stdout)?;
        let packages = metadata["packages"].as_array().context("Cargo packages")?;
        for package in packages
            .iter()
            .filter(|package| package["source"].is_null())
        {
            if let Some(path) = package["manifest_path"]
                .as_str()
                .and_then(|path| Path::new(path).parent())
            {
                watch_roots.insert(path.to_path_buf());
            }
        }

        let manifest = fs::canonicalize(candidate.project.join("Cargo.toml"))?;
        let package = packages
            .iter()
            .find(|p| {
                p["manifest_path"]
                    .as_str()
                    .is_some_and(|p| Path::new(p) == manifest)
            })
            .context("selected Cargo package is missing")?;
        if is_native(candidate) {
            if !package["targets"]
                .as_array()
                .context("Cargo targets")?
                .iter()
                .any(|t| {
                    t["kind"]
                        .as_array()
                        .is_some_and(|k| k.iter().any(|k| k == "lib" || k == "rlib"))
                })
            {
                bail!(
                    "native Plugin {} needs a Rust library target",
                    candidate.plugin_id
                );
            }
            let alias = format!("local_plugin_{index}");
            dependencies.insert(alias.clone(), dependency(package)?);
            linked.push_str(&format!("{alias}::link_plugin();\n"));
        }
        // Only normal reachable dependencies are eligible: test/build helper
        // contracts must not introduce runtime identities or competing versions.
        let nodes = metadata
            .pointer("/resolve/nodes")
            .and_then(Value::as_array)
            .context("resolved Cargo graph")?;
        let mut pending = vec![
            package["id"]
                .as_str()
                .context("Cargo package ID")?
                .to_owned(),
        ];
        while let Some(id) = pending.pop() {
            if !seen_packages.insert(id.clone()) {
                continue;
            }
            if let Some(node) = nodes.iter().find(|n| n["id"] == id) {
                for dep in node["deps"].as_array().context("Cargo dependencies")? {
                    if dep["dep_kinds"]
                        .as_array()
                        .is_some_and(|k| k.iter().any(|k| k["kind"].is_null()))
                    {
                        pending.push(dep["pkg"].as_str().context("Cargo dependency ID")?.into());
                    }
                }
            }
            let package = packages
                .iter()
                .find(|p| p["id"] == id)
                .context("reachable Cargo package")?;
            if is_native(candidate) && package["name"] == "lenso-capability-http-endpoint" {
                let dependency = dependency(package)?;
                if web_contract
                    .as_ref()
                    .is_some_and(|previous| previous != &dependency)
                {
                    bail!("Web endpoints use different Cargo contract identities");
                }
                web_contract = Some(dependency);
            }
            let Some(contract) = package.pointer("/metadata/lenso/contract") else {
                continue;
            };
            if contract["projection"] != "rust-runtime" {
                continue;
            }
            let node = nodes
                .iter()
                .find(|n| n["id"] == id)
                .context("contract Cargo node")?;
            let codec_id = node["deps"]
                .as_array()
                .context("contract dependencies")?
                .iter()
                .find(|d| d["name"] == "lenso_runtime_codec")
                .context("rust-runtime projection must depend on lenso-runtime-codec")?["pkg"]
                .clone();
            let codec = packages
                .iter()
                .find(|p| p["id"] == codec_id)
                .context("contract Codec package")?;
            let version = codec["version"].as_str().context("Codec version")?;
            codec_cohorts.insert(version.split('.').take(2).collect::<Vec<_>>().join("."));
            let manifest = Path::new(
                package["manifest_path"]
                    .as_str()
                    .context("contract manifest")?,
            );
            let descriptor: Value = serde_json::from_slice(&fs::read(
                manifest.parent().context("contract directory")?.join(
                    contract["descriptor"]
                        .as_str()
                        .context("contract descriptor")?,
                ),
            )?)?;
            let capability = descriptor["id"].as_str().context("contract id")?.to_owned();
            let dep = dependency(package)?;
            if let Some((previous, _)) = codecs.get(&capability) {
                if previous != &id {
                    bail!(
                        "Capability {capability} has competing Rust package identities; align native contract dependencies"
                    );
                }
            } else {
                codecs.insert(capability, (id, dep));
            }
        }
    }
    if codec_cohorts.len() > 1 {
        bail!("native contracts use incompatible Codec cohorts: {codec_cohorts:?}");
    }
    // Adapters have historically changed the Codec cohort in patch releases.
    // Pin a tested set instead of allowing Cargo to mix distinct traits.
    let cohort = codec_cohorts.first().map_or("0.4", String::as_str);
    let versions = match cohort {
        "0.3" => [
            ("lenso-bun-adapter", "=0.1.8"),
            ("lenso-process-adapter", "=0.3.5"),
            ("lenso-wasm-component-adapter", "=0.2.8"),
            ("lenso-runtime-codec", "=0.3.4"),
        ],
        "0.4" => [
            ("lenso-bun-adapter", "=0.1.10"),
            ("lenso-process-adapter", "=0.3.11"),
            ("lenso-wasm-component-adapter", "=0.2.13"),
            ("lenso-runtime-codec", "=0.4.1"),
        ],
        other => bail!("unsupported typed Codec cohort {other}; use a custom Host"),
    };
    for (name, version) in versions {
        dependencies.insert(name.into(), json!(version));
    }
    if cohort == "0.3" {
        dependencies.insert(
            "native-resources".into(),
            json!({"package":"lenso-runtime-codec","version":"=0.4.1"}),
        );
    }
    fs::write(
        cache.join("watch-roots.json"),
        serde_json::to_vec_pretty(&watch_roots)?,
    )?;
    let local_inputs = watch_roots
        .iter()
        .map(|path| Ok((path.clone(), input_digest(path)?)))
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let ids = codecs
        .keys()
        .map(|id| format!("{id:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let terminal_enabled = candidates
        .iter()
        .any(|c| c.plugin_id == "lenso.terminal.cli");
    if terminal_enabled && cohort != "0.4" {
        bail!("bundled terminal support requires runtime-codec 0.4 contracts");
    }
    let mut terminal_aliases = BTreeMap::new();
    let mut register = format!("let typed = std::collections::BTreeSet::<&str>::from([{ids}]);\n");
    for (index, (capability, (_, dependency))) in codecs.into_iter().enumerate() {
        let alias = format!("local_contract_{index}");
        dependencies.insert(alias.clone(), dependency);
        if capability == "lenso.terminal.command@1"
            || capability == "lenso.terminal.command-provider@1"
        {
            terminal_aliases.insert(capability.clone(), alias.clone());
        }
        let name = codec_name(&capability)?;
        register.push_str(&format!("let bun = bun.with_codec(LegacyBunCodec({alias}::{name})).with_authoring_codec({alias}::{name});\nlet process = process.with_codec({alias}::{name});\nlet wasm = wasm.with_codec({alias}::{name});\n"));
    }
    if terminal_enabled {
        for (name, version) in [
            ("lenso-contract-runtime", "0.2.0"),
            ("lenso-plugin-authoring", "0.2.0"),
            ("lenso-guest-sdk", "0.5.0"),
            ("shell-words", "1.1"),
        ] {
            dependencies.insert(name.into(), json!(version));
        }
        dependencies.insert("clap".into(), json!({"version":"4", "features":["string"]}));
        fs::create_dir_all(generated.join("src/terminal"))?;
        let mut module = include_str!("terminal/mod.rs").to_owned();
        for (id, name, codec, body) in [
            (
                "lenso.terminal.command@1",
                "command",
                "CommandJsonCodec",
                include_str!("terminal/command.rs"),
            ),
            (
                "lenso.terminal.command-provider@1",
                "provider",
                "CommandProviderJsonCodec",
                include_str!("terminal/provider.rs"),
            ),
        ] {
            if let Some(alias) = terminal_aliases.get(id) {
                module = module.replace(
                    &format!("pub mod {name};"),
                    &format!("pub use {alias} as {name};"),
                );
            } else {
                fs::write(generated.join(format!("src/terminal/{name}.rs")), body)?;
                register.push_str(&format!("let bun = bun.with_authoring_codec(terminal::{name}::{codec});\nlet process = process.with_codec(terminal::{name}::{codec});\nlet wasm = wasm.with_codec(terminal::{name}::{codec});\n"));
            }
        }
        register.push_str("let mut typed = typed; typed.insert(terminal::command::CAPABILITY_ID); typed.insert(terminal::provider::CAPABILITY_ID);\n");
        fs::write(generated.join("src/terminal/mod.rs"), module)?;
        fs::write(
            generated.join("src/terminal/parser.rs"),
            include_str!("terminal/parser.rs"),
        )?;
    }
    let web = web_contract.is_some();
    if let Some(contract) = web_contract {
        dependencies.insert("local-web-contract".into(), contract);
        dependencies.insert("lenso-web-ingress-plugin".into(), json!("=0.4.5"));
    }
    let mut source = (include_str!("local_runtime_template.rs").to_owned()
        + include_str!("local_json_template.rs"))
    .replace("// LENSO_REGISTER_CODECS", &register)
    .replace("// LENSO_LINK_PLUGINS", &linked);
    source = source.replace("// LENSO_TERMINAL_RUN", if terminal_enabled { "if let Some(args) = &command_args { command_result = terminal::run(&app, args).await; }" } else { "if command_args.is_some() { command_result = Err(anyhow::anyhow!(\"CLI support is not adopted\")); }" });
    if terminal_enabled {
        source.push_str("\n#[allow(dead_code)] mod terminal;\n");
    }
    source = source.replace(
        "// LENSO_NATIVE_RESOURCES",
        if cohort == "0.3" {
            ""
        } else {
            "use lenso_runtime_codec as native_resources;"
        },
    );
    source = source.replace("// LENSO_DESCRIBE_WEB", if web { r#"
        let mut releases = catalog.plugins().to_vec();
        if releases.iter().any(|r| r.descriptor().provided_capabilities().iter().any(|c| c.capability_id() == local_web_contract::CAPABILITY_ID)) {
            releases.push(lenso_app_plan::authoring::HostPluginRelease::new(lenso_web_ingress_plugin::WebIngressFactory::plugin_descriptor()));
        }
        let catalog = HostCatalog::new([], releases, []);
"# } else { "" });
    source = source.replace(
        "// LENSO_RUNTIME_WEB",
        if web {
            r#"
    let ingress = lenso_web_ingress_plugin::WebIngressFactory::new();
    let native = native.with_factory(ingress.clone());
"#
        } else {
            ""
        },
    );
    source = source.replace("// LENSO_WEB_READY", if web { r#"
            if let Some(address) = ingress.local_address() { eprintln!("Listening on http://{address}"); }
"# } else { "" });
    let manifest = json!({"package":{"name":"lenso-generated-local-host", "version":"0.0.0", "edition":"2024"}, "workspace":{}, "dependencies": dependencies});
    fs::write(
        generated.join("Cargo.toml"),
        toml::to_string_pretty(&manifest)?,
    )?;
    fs::write(generated.join("src/main.rs"), source)?;
    fs::write(
        generated.join("build.rs"),
        "fn main() { println!(\"cargo:rustc-check-cfg=cfg(generated_native_host)\"); println!(\"cargo:rustc-cfg=generated_native_host\"); }\n",
    )?;

    let output = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--message-format=json-render-diagnostics",
            "--manifest-path",
        ])
        .arg(generated.join("Cargo.toml"))
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("build generated local Host")?;
    if !output.status.success() {
        bail!("generated local Host build failed");
    }
    for (path, before) in &local_inputs {
        if &input_digest(path)? != before {
            bail!(
                "native path dependency changed during build: {}; retry after edits settle",
                path.display()
            );
        }
    }
    let binary = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| {
            message["reason"] == "compiler-artifact"
                && message["target"]["name"] == "lenso-generated-local-host"
        })
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
        .context("Cargo did not report the generated Host executable")?;
    let output = Command::new(&binary).arg("--describe").output()?;
    if !output.status.success() {
        bail!(
            "linked Host Descriptor failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let catalog: HostCatalog = serde_json::from_slice(&output.stdout)?;
    fs::copy(binary, stage.join(".lenso/host"))?;
    let provenance = stage.join(".lenso/generated-host");
    fs::create_dir_all(provenance.join("src"))?;
    fs::write(
        provenance.join("local-inputs.json"),
        serde_json::to_vec_pretty(&local_inputs)?,
    )?;
    for file in ["Cargo.toml", "Cargo.lock", "src/main.rs", "build.rs"] {
        fs::copy(generated.join(file), provenance.join(file))?;
    }

    let descriptors = catalog
        .plugins()
        .iter()
        .map(|r| r.descriptor().clone())
        .collect::<Vec<_>>();
    let mut expected = candidates
        .iter()
        .filter(|c| is_native(c))
        .map(|c| c.plugin_id.as_str())
        .collect::<BTreeSet<_>>();
    if web
        && descriptors
            .iter()
            .any(|d| d.plugin_id() == "lenso.web-ingress")
    {
        expected.insert("lenso.web-ingress");
    }
    let actual = descriptors
        .iter()
        .map(|d| d.plugin_id())
        .collect::<BTreeSet<_>>();
    if expected != actual || descriptors.len() != actual.len() {
        bail!(
            "linked Plugin registry differs from selected local sources: expected {expected:?}, linked {actual:?}"
        );
    }
    Ok(descriptors)
}

pub(super) fn is_native(candidate: &Candidate) -> bool {
    candidate
        .implementations
        .iter()
        .any(|i| i.runtime == "native-linked")
}

pub(super) fn dependency(package: &Value) -> anyhow::Result<Value> {
    let name = package["name"].as_str().context("Cargo name")?;
    let version = package["version"].as_str().context("Cargo version")?;
    match package["source"].as_str() {
        None => Ok(
            json!({"package":name,"path":Path::new(package["manifest_path"].as_str().context("Cargo manifest")?).parent().context("Cargo directory")?}),
        ),
        Some("registry+https://github.com/rust-lang/crates.io-index") => {
            Ok(json!({"package":name,"version":format!("={version}")}))
        }
        Some(source) if source.starts_with("git+") => {
            let (url, rev) = source[4..]
                .rsplit_once('#')
                .context("Cargo git source needs exact commit")?;
            let url = url.split('?').next().context("Cargo git URL")?;
            Ok(json!({"package":name,"git":url,"rev":rev}))
        }
        Some(source) => bail!(
            "unsupported contract registry {source}; use a custom Host for alternate registry dependencies"
        ),
    }
}

fn codec_name(capability: &str) -> anyhow::Result<String> {
    let name = capability
        .split('@')
        .next()
        .and_then(|s| s.rsplit('.').next())
        .context("Capability name")?;
    let mut output = String::new();
    for part in name.split(|c: char| !c.is_ascii_alphanumeric()) {
        if part.is_empty() || part.chars().all(char::is_numeric) {
            continue;
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            output.extend(first.to_uppercase());
            output.push_str(chars.as_str());
        }
    }
    if output.is_empty() {
        output.push_str("Value");
    }
    Ok(format!("{output}JsonCodec"))
}

pub(super) fn digest(path: &Path) -> anyhow::Result<String> {
    Ok(format!(
        "sha256:{}",
        Sha256::digest(fs::read(path)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

pub(super) fn finalize(stage: &Path, runtime_artifacts: Vec<Value>) -> anyhow::Result<()> {
    fs::create_dir_all(stage.join("runtime"))?;
    let native = stage.join(".lenso/host").is_file();
    if !native {
        fs::copy(
            super::preset::runtime_executable()?,
            stage.join(".lenso/host"),
        )?;
    }
    fs::write(
        stage.join(".lenso/host-mode"),
        if native { "native" } else { "portable" },
    )?;

    fs::copy(
        super::preset::runtime_executable()?,
        stage.join("runtime/lenso-resolver"),
    )?;
    if runtime_artifacts
        .iter()
        .any(|a| a["execution_class"] == "lenso.bun-process@1")
    {
        let bun = executable_on_path("bun")?;
        fs::copy(bun, stage.join("runtime/bun"))?;
    }
    fs::create_dir_all(stage.join("intent/.lenso"))?;
    fs::write(stage.join("intent/.lenso/plugin-root-authoring.lock"), [])?;
    if stage.join("plugins").exists() {
        super::assemble::copy_root(
            &stage.join("plugins"),
            &stage.join("intent/plugins"),
            0,
            &mut 0,
        )?;
    }
    let mut files = vec![
        ".lenso/host",
        ".lenso/host-mode",
        ".lenso/host-build.json",
        "runtime/lenso-resolver",
        "bundles.json",
        "runtime-codecs.json",
        "local-sources.json",
        ".lenso/precompiled-host.json",
        ".lenso/generated-host/Cargo.lock",
        ".lenso/generated-host/Cargo.toml",
        ".lenso/generated-host/src/main.rs",
        ".lenso/generated-host/build.rs",
        ".lenso/generated-host/local-inputs.json",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if stage.join("runtime/bun").exists() {
        files.push("runtime/bun".into());
    }
    files.extend(
        runtime_artifacts
            .iter()
            .filter_map(|a| a["path"].as_str().map(str::to_owned)),
    );
    let mut proofs = files
        .into_iter()
        .filter(|path| stage.join(path).is_file())
        .map(|path| {
            let role = match path.as_str() {
                ".lenso/host" => "host_runtime",
                ".lenso/host-build.json" => "host_authority",
                ".lenso/host-mode" => "host_entrypoint",
                "runtime/lenso-resolver" => "runtime_resolver",
                "runtime/bun" => "javascript_runtime",
                "bundles.json" => "bundle_inventory",
                "local-sources.json" => "source_provenance",
                _ if path.starts_with("runtime/artifacts/") => "plugin_artifact",
                _ => "build_provenance",
            };
            Ok(super::prepare::DistributionFile {
                sha256: digest(&stage.join(&path))?,
                size: fs::metadata(stage.join(&path))?.len(),
                executable: matches!(
                    role,
                    "host_runtime" | "runtime_resolver" | "javascript_runtime"
                ),
                role: role.into(),
                path,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let inventory: Vec<Value> = serde_json::from_slice(&fs::read(stage.join("bundles.json"))?)?;
    for bundle in inventory {
        let path = bundle["path"].as_str().context("bundle inventory path")?;
        proofs.push(super::prepare::DistributionFile {
            path: path.into(),
            role: "plugin_bundle".into(),
            sha256: digest(&stage.join(path))?,
            size: fs::metadata(stage.join(path))?.len(),
            executable: false,
        });
    }
    let authority: lenso_app_authoring::host_authoring::GeneratedHostBuild =
        serde_json::from_slice(&fs::read(stage.join(".lenso/host-build.json"))?)?;
    let target = lenso_app_authoring::native_host_target();
    let (platform, arch) = super::prepare::target_platform(target)?;
    let lock = super::prepare::DistributionLock {
        schema: "lenso.local-host-distribution.v1",
        app_id: authority.host_id().into(),
        target: target.into(),
        platform,
        arch,
        files: proofs,
    };
    fs::write(
        stage.join(".lenso/distribution.lock.json"),
        serde_json::to_vec_pretty(&lock)?,
    )?;
    Ok(())
}

fn executable_on_path(name: &str) -> anyhow::Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .find(|p| p.is_file())
        .with_context(|| format!("{name} executable is required to prepare an offline local Host"))
}

/// Content evidence for authored inputs; generated dependency lockfiles are
/// captured separately by Cargo/the package builder and may be created on first build.
pub(super) fn input_digest(root: &Path) -> anyhow::Result<String> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        visited += 1;
        if visited > 50_000 {
            bail!("source input exceeds 50,000 entries");
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_str().is_some_and(|name| {
                    [
                        ".git",
                        ".lenso",
                        "target",
                        "node_modules",
                        "dist",
                        "build",
                        ".next",
                        ".venv",
                        "__pycache__",
                        "Cargo.lock",
                        "bun.lock",
                        "bun.lockb",
                        "package-lock.json",
                        "pnpm-lock.yaml",
                    ]
                    .contains(&name)
                }) {
                    continue;
                }
                pending.push(entry.path());
            }
        } else if metadata.is_file() {
            files.push(path);
        } else {
            bail!(
                "source input contains a symlink or special file: {}",
                path.display()
            );
        }
        if pending.len() + files.len() > 50_000 {
            bail!("source input exceeds 50,000 entries");
        }
    }
    files.sort();
    let mut hasher = Sha256::new();
    let mut size = 0;
    for path in files {
        let relative = path.strip_prefix(root)?.to_string_lossy();
        size += fs::metadata(&path)?.len();
        if size > 256 * 1024 * 1024 {
            bail!("source input exceeds 256 MiB");
        }
        let bytes = fs::read(&path)?;
        hasher.update((relative.len() as u64).to_be_bytes());
        hasher.update(relative.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    Ok(format!(
        "sha256:{}",
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}

pub(super) fn host_arguments(root: &Path) -> anyhow::Result<Vec<&'static str>> {
    match fs::read_to_string(root.join(".lenso/host-mode"))?.as_str() {
        "native" => Ok(Vec::new()),
        "portable" => Ok(vec!["app", "__run-local", "--"]),
        _ => bail!("unsupported local Host entrypoint"),
    }
}

pub(super) fn digest_text(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
