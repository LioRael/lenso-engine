use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use lenso_app_authoring::identity::validate_plugin_id_v1;

use super::{PluginNewArgs, PluginRuntimeArg, WASM_TARGET, run_bun, run_cargo};

pub(super) const LENSO_APP_PLAN_REVISION: &str = "8599db7e4a214ed92f32089f81d14c833d4becf6";
pub(super) const LENSO_NATIVE_REVISION: &str = "89815107385475c8b5be378bdcf5e21aa74e02f0";
pub(super) const LENSO_WEB_REVISION: &str = "42efe4fe9aa249bdb19ed366b25b8358e30b68ab";

pub(super) fn create(args: PluginNewArgs) -> anyhow::Result<()> {
    validate_plugin_id_v1(&args.plugin_id)?;
    let base = args.repo_root.unwrap_or(env::current_dir()?);
    let target = args
        .dir
        .map_or_else(|| base.join(&args.plugin_id), |dir| base.join(dir));
    if target.exists() {
        bail!(
            "Plugin project directory already exists: {}",
            target.display()
        );
    }
    let files = if args.web {
        web_plugin_scaffold(&args.plugin_id)
    } else {
        match args.runtime {
            PluginRuntimeArg::Multi => multi_plugin_scaffold(&args.plugin_id),
            PluginRuntimeArg::Wasm => plugin_scaffold(&args.plugin_id),
            PluginRuntimeArg::Process => process_plugin_scaffold(&args.plugin_id),
            PluginRuntimeArg::Bun => bun_plugin_scaffold(&args.plugin_id),
        }
    };
    if args.dry_run {
        println!("Plugin dry run for {}:", target.display());
        for path in files.keys() {
            println!("  {}", path.display());
        }
        return Ok(());
    }
    fs::create_dir_all(&base)
        .with_context(|| format!("create Plugin project parent {}", base.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".lenso-plugin-new-")
        .tempdir_in(&base)
        .context("create Plugin scaffold staging directory")?;
    for (path, contents) in files {
        let destination = staging.path().join(path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(destination, contents)?;
    }
    fs::rename(staging.path(), &target)
        .with_context(|| format!("publish Plugin scaffold {}", target.display()))?;
    if !args.no_install {
        if args.web {
            run_cargo(
                &target,
                &["generate-lockfile"],
                "generate Web Plugin lockfile",
            )?;
            run_cargo(&target, &["test", "--locked"], "test generated Web Plugin")?;
        } else if args.runtime == PluginRuntimeArg::Bun {
            run_bun(&target, &["install"], "install Bun Plugin dependencies")?;
            run_bun(&target, &["run", "check"], "check generated Bun Plugin")?;
        } else {
            run_cargo(&target, &["generate-lockfile"], "generate Plugin lockfile")?;
        }
        if !args.web
            && matches!(
                args.runtime,
                PluginRuntimeArg::Multi | PluginRuntimeArg::Wasm
            )
        {
            run_cargo(
                &target,
                &["check", "--locked", "--lib", "--target", WASM_TARGET],
                "check generated Wasm implementation",
            )?;
        }
        if !args.web
            && matches!(
                args.runtime,
                PluginRuntimeArg::Multi | PluginRuntimeArg::Process
            )
        {
            run_cargo(
                &target,
                &[
                    "check",
                    "--locked",
                    "--bin",
                    &args.plugin_id.replace('.', "-"),
                ],
                "check generated Process implementation",
            )?;
        }
    }
    println!("Created Plugin project at {}.", target.display());
    Ok(())
}

#[allow(clippy::too_many_lines)] // The generated, copyable source is kept in one visible template.
pub(super) fn web_plugin_scaffold(plugin_id: &str) -> BTreeMap<PathBuf, String> {
    let package_name = plugin_id.replace('.', "-");
    let manifest = format!(
        r#"[package]
name = "{package_name}"
version = "0.1.0"
edition = "2024"
publish = false

[package.metadata.lenso]
plugin-id = "{plugin_id}"
root-slot = "web"

[dependencies]
lenso = {{ version = "0.5.0", git = "https://github.com/LioRael/lenso-runtime-rust", rev = "{LENSO_NATIVE_REVISION}" }}
lenso-capability-http-endpoint = {{ version = "0.2.8", git = "https://github.com/LioRael/lenso-web", rev = "{LENSO_WEB_REVISION}" }}
serde = {{ version = "1", features = ["derive"] }}

[dev-dependencies]
futures = "0.3"

[workspace]
"#
    );
    let source = r#"use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use lenso_capability_http_endpoint::{
    prelude::*,
    response::{Problem, StatusCode},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CreateGreeting {
    name: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct SearchGreetings {
    term: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Greeting {
    id: String,
    message: String,
}

#[lenso::plugin]
#[derive(Clone, Debug, Default)]
pub struct GreetingsHttp {
    next_id: Rc<Cell<u64>>,
    greetings: Rc<RefCell<BTreeMap<String, Greeting>>>,
}

/// Keeps this Plugin's generated factory linked into a native Host binary.
pub const fn link() {}

#[endpoint]
impl GreetingsHttp {
    #[post("greetings.create", "/greetings")]
    async fn create(
        &self,
        Json(input): Json<CreateGreeting>,
    ) -> Result<(StatusCode, Json<Greeting>), Problem> {
        // A real Plugin normally awaits its business Capability here.
        std::future::ready(()).await;
        let name = input.name.trim();
        if name.is_empty() {
            return Err(Problem::new(
                StatusCode::BAD_REQUEST,
                "invalid_name",
                "name must not be empty",
            ));
        }

        let sequence = self.next_id.get() + 1;
        self.next_id.set(sequence);
        let greeting = Greeting {
            id: format!("greeting-{sequence}"),
            message: format!("Hello, {name}!"),
        };
        self.greetings
            .borrow_mut()
            .insert(greeting.id.clone(), greeting.clone());
        Ok((StatusCode::CREATED, Json(greeting)))
    }

    #[query("greetings.search", "/greetings/search")]
    async fn search(
        &self,
        Json(input): Json<SearchGreetings>,
    ) -> Result<Json<Vec<Greeting>>, Problem> {
        std::future::ready(()).await;
        let greetings = self
            .greetings
            .borrow()
            .values()
            .filter(|greeting| greeting.message.contains(&input.term))
            .cloned()
            .collect();
        Ok(Json(greetings))
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use lenso_capability_http_endpoint::testing::EndpointTest;

    use super::*;

    #[test]
    fn creates_and_queries_a_greeting_without_opening_a_socket() {
        block_on(async {
            let endpoint = EndpointTest::new(GreetingsHttp::default());
            let created = endpoint
                .request("greetings.create")
                .json(&CreateGreeting {
                    name: "Lenso".to_owned(),
                })
                .unwrap()
                .send()
                .await
                .unwrap();
            assert_eq!(created.status(), StatusCode::CREATED);

            let found = endpoint
                .request("greetings.search")
                .json(&SearchGreetings {
                    term: "Lenso".to_owned(),
                })
                .unwrap()
                .send()
                .await
                .unwrap();
            assert_eq!(found.status(), StatusCode::OK);
            assert_eq!(found.json::<Vec<Greeting>>().unwrap().len(), 1);
        });
    }

    #[test]
    fn turns_business_rejections_into_problem_responses() {
        let response = block_on(async {
            EndpointTest::new(GreetingsHttp::default())
                .request("greetings.create")
                .json(&CreateGreeting {
                    name: String::new(),
                })
                .unwrap()
                .send()
                .await
                .unwrap()
        });

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.header("content-type"),
            Some("application/problem+json; charset=utf-8")
        );
    }
}
"#
    .to_owned();
    let readme = format!(
        "# {plugin_id}\n\nLinked native Rust Web Plugin using `#[lenso::plugin]` and `#[endpoint]`.\n\n```sh\ncargo test --locked\nlenso plugin dev\n```\n\nThe generated tests invoke typed Endpoint operations without opening a socket. `lenso plugin dev` builds a temporary native Host, mounts this Plugin through the `web` root slot, starts a loopback Web Ingress listener, and prints the real HTTP routes. Add `--watch` to rebuild and restart after source changes.\n"
    );

    BTreeMap::from([
        (PathBuf::from("Cargo.toml"), manifest),
        (PathBuf::from("src/lib.rs"), source),
        (PathBuf::from("README.md"), readme),
    ])
}

pub(super) fn bun_plugin_scaffold(plugin_id: &str) -> BTreeMap<PathBuf, String> {
    let package_name = plugin_id.replace('.', "-");
    BTreeMap::from([
        (
            PathBuf::from("package.json"),
            bun_package_manifest(plugin_id, &package_name),
        ),
        (
            PathBuf::from("tsconfig.json"),
            r#"{
  "compilerOptions": {
    "lib": ["ES2023"],
    "module": "Preserve",
    "moduleResolution": "bundler",
    "noEmit": true,
    "strict": true,
    "allowImportingTsExtensions": true,
    "types": ["bun"]
  },
  "include": ["src/**/*.ts"]
}
"#
            .to_owned(),
        ),
        (PathBuf::from("src/plugin.ts"), bun_author_source(plugin_id)),
        (
            PathBuf::from("README.md"),
            format!(
                "# {plugin_id}\n\nTyped Bun Plugin using the generic Lenso Plugin SDK and Agent-owned Tool declarations. Edit `src/plugin.ts`; the CLI compiles declarations and runtime bindings.\n\n```sh\nlenso plugin check\nlenso plugin dev --operation execute --request-json '{{\"name\":\"{plugin_id}\",\"arguments_json\":\"{{\\\"text\\\":\\\"hello\\\"}}\"}}'\nlenso plugin dev --watch\nlenso plugin pack\n```\n"
            ),
        ),
    ])
}

fn bun_package_manifest(plugin_id: &str, package_name: &str) -> String {
    format!(
        r#"{{
  "name": "{package_name}",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "scripts": {{
    "check": "tsc --noEmit"
  }},
  "dependencies": {{
    "@lenso/agent-tool-sdk": "0.1.0",
    "@lenso/bun-plugin": "0.2.2"
  }},
  "devDependencies": {{
    "@types/bun": "1.4.0",
    "typescript": "7.0.2"
  }},
  "lenso": {{
    "pluginId": "{plugin_id}",
    "rootSlot": "tool-providers",
    "runtime": "bun"
  }}
}}
"#
    )
}

fn bun_author_source(plugin_id: &str) -> String {
    format!(
        r#"import {{ definePlugin }} from "@lenso/bun-plugin";
import {{ tool, tools }} from "@lenso/agent-tool-sdk";
import * as schema from "@lenso/agent-tool-sdk/schema";

export default definePlugin({{
  providers: [
    tools([
      tool(
        {{
          name: "{plugin_id}",
          description: "Uppercase one UTF-8 string.",
          input: schema.object({{ text: schema.string() }}),
          output: schema.string(),
          execution: "parallel_safe",
        }},
        ({{ text }}) => ({{ ok: true, value: text.toUpperCase() }}),
      ),
    ]),
  ],
}});
"#
    )
}

pub(super) fn multi_plugin_scaffold(plugin_id: &str) -> BTreeMap<PathBuf, String> {
    let mut files = plugin_scaffold(plugin_id);
    let manifest = files
        .get_mut(Path::new("Cargo.toml"))
        .expect("Wasm scaffold has a manifest");
    *manifest = manifest.replace("runtime = \"wasm\"", "outputs = [\"wasm\", \"process\"]");
    files.insert(
        PathBuf::from("src/main.rs"),
        "// Cargo Process entrypoint; the SDK supplies main and protocol lowering.\ninclude!(\"lib.rs\");\n"
            .to_owned(),
    );
    files.insert(
        PathBuf::from("README.md"),
        format!(
            "# {plugin_id}\n\nOne ordinary Rust Plugin source with portable Wasm and trusted Process outputs. `lenso plugin pack` builds both implementations into one release.\n"
        ),
    );
    files
}

pub(super) fn plugin_scaffold(plugin_id: &str) -> BTreeMap<PathBuf, String> {
    let package_name = plugin_id.replace('.', "-");
    BTreeMap::from([
        (
            PathBuf::from("Cargo.toml"),
            format!(
                r#"[package]
name = "{package_name}"
version = "0.1.0"
edition = "2024"
publish = false

[package.metadata.lenso]
plugin-id = "{plugin_id}"
root-slot = "tool-providers"

[package.metadata.lenso-cli]
runtime = "wasm"

[lib]
crate-type = ["cdylib"]

[dependencies]
lenso = {{ package = "lenso-plugin-sdk", version = "0.4.1" }}
lenso-agent-tool-sdk = "0.3.0"
schemars = "1"
serde = {{ version = "1", features = ["derive"] }}

[workspace]
"#
            ),
        ),
        (
            PathBuf::from("src/lib.rs"),
            format!(
                r#"use lenso_agent_tool_sdk::prelude::*;
use schemars::JsonSchema;

#[derive(Debug, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Arguments {{
    #[schemars(length(max = 4096))]
    text: String,
}}

#[lenso::plugin]
#[derive(Clone, Copy, Debug, Default)]
struct Plugin {{}}

#[lenso_agent_tool_sdk::tool_provider]
impl Plugin {{
    #[tool(
        name = "{plugin_id}",
        description = "Process one UTF-8 string.",
        execution = "parallel_safe"
    )]
    fn execute(arguments: Arguments) -> Result<ExecuteResponse, ExecuteError> {{
        if arguments.text.is_empty() {{
            return Err(ExecuteError::InvalidArguments);
        }}
        Ok(ExecuteResponse {{
            content: arguments.text,
            content_blocks: None,
            content_type: ContentType::Text,
            metadata_json: "{{}}"
                .try_into()
                .expect("static Tool metadata must be valid JSON"),
        }})
    }}
}}
"#
            ),
        ),
        (
            PathBuf::from("README.md"),
            format!(
                "# {plugin_id}\n\nOrdinary Rust Plugin for Lenso Agent, packaged as an isolated Wasm Component. The SDK owns the execution bridge.\n\n```sh\nlenso plugin check\nlenso plugin dev --operation execute --request-json '{{\"name\":\"{plugin_id}\",\"arguments_json\":\"{{\\\"text\\\":\\\"hello\\\"}}\"}}'\nlenso plugin pack\n```\n\nCreate another project with `lenso plugin new <id>`.\n"
            ),
        ),
    ])
}

pub(super) fn process_plugin_scaffold(plugin_id: &str) -> BTreeMap<PathBuf, String> {
    let mut files = plugin_scaffold(plugin_id);
    let manifest = files
        .get_mut(Path::new("Cargo.toml"))
        .expect("Plugin scaffold has a manifest");
    *manifest = manifest.replace("runtime = \"wasm\"", "runtime = \"process\"");
    files.insert(
        PathBuf::from("src/main.rs"),
        "// Cargo Process entrypoint; the SDK supplies main and protocol lowering.\ninclude!(\"lib.rs\");\n"
            .to_owned(),
    );
    files.insert(
        PathBuf::from("README.md"),
        format!(
            "# {plugin_id}\n\nOrdinary Rust source compiled as a trusted native Process Plugin. The SDK owns the protocol bridge and runtime descriptor. Process Plugins are not sandboxed, so install only trusted bundles.\n\n```sh\nlenso plugin check\nlenso plugin dev --operation execute --request-json '{{\"name\":\"{plugin_id}\",\"arguments_json\":\"{{\\\"text\\\":\\\"hello\\\"}}\"}}'\nlenso plugin pack\n```\n"
        ),
    );
    files
}
