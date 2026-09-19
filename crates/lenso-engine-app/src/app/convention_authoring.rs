//! Bundled support and ordinary local sources use the same discovery/adoption path.
use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
include!(concat!(env!("OUT_DIR"), "/terminal_assets.rs"));

#[derive(Clone, Debug, Args)]
pub struct AddArgs {
    /// Local Plugin source directory, or bundled @lenso/cli support.
    source: String,
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long)]
    no_install: bool,
}
#[derive(Clone, Debug, Subcommand)]
pub enum PluginCommand {
    New(NewArgs),
}
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Language {
    Ts,
    Rust,
}
#[derive(Clone, Debug, Args)]
pub struct NewArgs {
    id: String,
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "ts")]
    language: Language,
    #[arg(long)]
    no_install: bool,
}
fn write(root: &Path, name: &str, bytes: impl AsRef<[u8]>) -> anyhow::Result<()> {
    let path = root.join(name);
    fs::create_dir_all(path.parent().context("file parent")?)?;
    fs::write(path, bytes)?;
    Ok(())
}
fn writable_path(root: &Path, relative: &Path) -> anyhow::Result<()> {
    let mut path = root.to_path_buf();
    for part in relative.components() {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("refusing to modify a symlink: {}", path.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn install(root: &Path) -> anyhow::Result<()> {
    if !Command::new("bun")
        .args(["install", "--ignore-scripts"])
        .current_dir(root)
        .status()?
        .success()
    {
        bail!("Bun dependency installation failed at {}", root.display());
    }
    Ok(())
}
fn package(id: &str, source: &str) -> serde_json::Value {
    json!({"name":id,"version":"1.0.0","private":true,"type":"module",
        "scripts":{"check":"tsc --noEmit"},
        "dependencies":{"@lenso/bun-plugin":"0.4.1","@lenso/contract-runtime":"0.3.0"},
        "devDependencies":{"typescript":"7.0.2","@types/bun":"1.4.0"},
        "lenso":{"pluginId":id,"runtime":"bun","rootSlot":"tools","source":source}})
}
fn tsconfig(root: &Path, source: &str) -> anyhow::Result<()> {
    write(
        root,
        "tsconfig.json",
        serde_json::to_vec_pretty(
            &json!({"compilerOptions":{"strict":true,"noEmit":true,"module":"Preserve","moduleResolution":"bundler","allowImportingTsExtensions":true,"types":["bun"]},"include":[source]}),
        )?,
    )
}

pub fn add(args: AddArgs) -> anyhow::Result<()> {
    let root = crate::plugins::project_root(args.root)?;
    writable_path(&root, Path::new("app"))?;
    if args.source == "@lenso/cli" {
        let destination = root.join("app/lenso-terminal-cli");
        if destination.exists() {
            bail!("CLI support already exists at {}", destination.display());
        }
        fs::create_dir_all(root.join("app"))?;
        let stage = tempfile::Builder::new()
            .prefix(".lenso-support-")
            .tempdir_in(root.join("app"))?;
        for (name, bytes) in TERMINAL_ASSETS {
            write(
                stage.path(),
                name.strip_suffix(".template").unwrap_or(name),
                bytes,
            )?;
        }
        let mut metadata = package("lenso.terminal.cli", "consumer.ts");
        metadata["name"] = "@lenso/cli".into();
        metadata["exports"] = json!({".":"./sdk.ts"});
        metadata["lenso"]["conventions"] = json!([
            {"id":"lenso.cli.typescript","entries":["cli.ts"],"compiler":{"program":"bun","args":["compiler.mjs"]}},
            {"id":"lenso.cli.rust","entries":["cli.rs"],"compiler":{"program":"bun","args":["compiler.mjs"]}},
            {"id":"lenso.cli.router","entries":["router.ts"]}
        ]);
        metadata["lenso"]["surfaces"] =
            json!([{"entry":"router/router.ts","project":"router","required":true}]);
        write(
            stage.path(),
            "package.json",
            serde_json::to_vec_pretty(&metadata)?,
        )?;
        tsconfig(stage.path(), "consumer.ts")?;
        for name in ["router.ts", "provider.ts", "command.ts"] {
            write(
                stage.path(),
                &format!("router/{name}"),
                fs::read(stage.path().join(name))?,
            )?;
        }
        write(
            stage.path(),
            "router/package.json",
            serde_json::to_vec_pretty(&package("lenso.terminal.command", "router.ts"))?,
        )?;
        tsconfig(&stage.path().join("router"), "router.ts")?;
        write(
            stage.path(),
            "rust-sdk/src/generated.rs",
            include_str!("terminal/provider.rs"),
        )?;
        for (name, bytes) in TERMINAL_ASSETS {
            if let Some(relative) = name.strip_prefix("contracts/provider/") {
                write(stage.path(), &format!("rust-sdk/{relative}"), bytes)?;
            }
        }
        super::build::publish_new_output(stage.path(), &destination)?;
        if !args.no_install {
            install(&destination)?;
            install(&destination.join("router"))?;
        }
        println!("Adopted bundled CLI support at {}", destination.display());
        return Ok(());
    }
    let source = fs::canonicalize(&args.source)
        .context("app add accepts a local source path or @lenso/cli")?;
    let probe = tempfile::tempdir()?;
    write(
        probe.path(),
        "lenso.toml",
        toml::to_string(&json!({"plugin_sources":[source]}))?,
    )?;
    let report = lenso_app_authoring::discovery::discover(probe.path())?;
    let [candidate] = report.candidates.as_slice() else {
        bail!("select one Plugin source package, not a workspace of candidates");
    };
    writable_path(&root, Path::new("lenso.toml"))?;
    let intent_relative = Path::new("plugins").join(&candidate.plugin_id);
    for name in ["default.toml", "default.disabled"] {
        writable_path(&root, &intent_relative.join(name))?;
    }
    let config = root.join("lenso.toml");
    let mut document: toml::Value = if config.exists() {
        toml::from_str(&fs::read_to_string(&config)?)?
    } else {
        toml::Value::Table(Default::default())
    };
    let sources = document
        .as_table_mut()
        .context("lenso.toml table")?
        .entry("plugin_sources")
        .or_insert_with(|| toml::Value::Array(vec![]))
        .as_array_mut()
        .context("plugin_sources array")?;
    let value = toml::Value::String(source.to_str().context("source path UTF-8")?.to_owned());
    if !source.starts_with(root.join("app")) && !sources.contains(&value) {
        sources.push(value);
    }
    if !source.starts_with(root.join("app")) {
        let mut staged = tempfile::NamedTempFile::new_in(&root)?;
        use std::io::Write;
        staged.write_all(toml::to_string_pretty(&document)?.as_bytes())?;
        staged.persist(&config)?;
    }
    let intent = root.join("plugins").join(&candidate.plugin_id);
    fs::create_dir_all(&intent)?;
    let default = intent.join("default.toml");
    if !default.exists() {
        fs::write(default, "# Explicit local Plugin adoption\n")?;
    }
    if intent.join("default.disabled").exists() {
        fs::remove_file(intent.join("default.disabled"))?;
    }
    if !args.no_install && candidate.format == "bun" {
        install(&candidate.project)?;
    }
    println!("Adopted {} from {}", candidate.plugin_id, source.display());
    Ok(())
}

pub fn new(command: PluginCommand) -> anyhow::Result<()> {
    let PluginCommand::New(args) = command;
    lenso_app_authoring::identity::validate_plugin_id_v1(&args.id)?;
    let root = crate::plugins::project_root(args.root)?;
    writable_path(&root, Path::new("app"))?;
    let support = root.join("app/lenso-terminal-cli");
    if !support.is_dir() {
        bail!("install CLI support first: lenso app add @lenso/cli");
    }
    let destination = root.join("app").join(&args.id);
    if destination.exists() {
        bail!("Plugin project already exists");
    }
    let stage = tempfile::Builder::new()
        .prefix(".lenso-plugin-")
        .tempdir_in(root.join("app"))?;
    match args.language {
        Language::Ts => {
            let mut metadata = package(&args.id, "plugin.ts");
            metadata["dependencies"]["@lenso/cli"] = "file:../lenso-terminal-cli".into();
            write(
                stage.path(),
                "package.json",
                serde_json::to_vec_pretty(&metadata)?,
            )?;
            write(
                stage.path(),
                "plugin.ts",
                "import { definePlugin } from '@lenso/bun-plugin';\nexport default definePlugin({ provides: [], create() { return {}; } });\n",
            )?;
            write(
                stage.path(),
                "cli.ts",
                "import { command } from '@lenso/cli';\nexport default command({\n  name: 'hello',\n  description: 'Say hello',\n  args: { name: { type: 'string', default: 'world' } },\n  run({ args, output }) { output.text(`Hello, ${args.name}!`); },\n});\n",
            )?;
            tsconfig(stage.path(), "*.ts")?;
        }
        Language::Rust => {
            write(
                stage.path(),
                "Cargo.toml",
                format!(
                    "[package]\nname = {:?}\nversion = \"1.0.0\"\nedition = \"2024\"\n[workspace]\n[package.metadata.lenso]\nplugin-id = {:?}\nroot-slot = \"tools\"\n[dependencies]\nlenso = \"=0.5.23\"\n",
                    args.id.replace('.', "-"),
                    args.id
                ),
            )?;
            write(
                stage.path(),
                "src/lib.rs",
                "#[lenso::plugin(consumer)]\n#[derive(Clone, Debug)]\nstruct Core {}\n",
            )?;
            write(
                stage.path(),
                "cli.rs",
                "use lenso_cli_support::command;\n\n/// Say hello from Rust\n#[command(name = \"hello-rust\")]\nasync fn hello(#[arg(long, default = \"world\")] name: String) -> anyhow::Result<String> {\n    Ok(format!(\"Hello, {name}!\"))\n}\n",
            )?;
        }
    }
    super::build::publish_new_output(stage.path(), &destination)?;
    if !args.no_install && matches!(args.language, Language::Ts) {
        install(&destination)?;
    }
    if !args.no_install
        && matches!(args.language, Language::Rust)
        && !Command::new("cargo")
            .arg("check")
            .current_dir(&destination)
            .status()?
            .success()
    {
        bail!("initial Rust Plugin check failed");
    }
    println!("Created {} at {}", args.id, destination.display());
    Ok(())
}

pub fn adopt(root: PathBuf, source: String, install_dependencies: bool) -> anyhow::Result<()> {
    add(AddArgs {
        root: Some(root),
        source,
        no_install: !install_dependencies,
    })
}
pub fn create_plugin(
    root: PathBuf,
    id: String,
    language: Language,
    install_dependencies: bool,
) -> anyhow::Result<()> {
    new(PluginCommand::New(NewArgs {
        root: Some(root),
        id,
        language,
        no_install: !install_dependencies,
    }))
}
