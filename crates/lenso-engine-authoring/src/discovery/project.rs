use super::{Candidate, Implementation, SourceRole, read_metadata};
use crate::{
    bundle_archive::with_bundle_directory,
    identity::{classify_existing_plugin_id, validate_release_version},
};

pub(super) fn workspace_members(root: &Path) -> anyhow::Result<Option<Vec<std::path::PathBuf>>> {
    let mut members = BTreeSet::new();
    let mut found = false;
    for filename in ["Cargo.toml", "package.json"] {
        let path = root.join(filename);
        if !path.try_exists()? {
            continue;
        }
        let value = document(&path)?;
        let workspace = if filename == "Cargo.toml" {
            value.get("workspace")
        } else {
            value.get("workspaces")
        };
        let Some(workspace) = workspace else {
            continue;
        };
        found = true;
        let patterns = if filename == "Cargo.toml" {
            workspace.get("members")
        } else if workspace.is_array() {
            Some(workspace)
        } else {
            workspace.get("packages")
        };
        let excludes = if filename == "Cargo.toml" {
            workspace
                .get("exclude")
                .map(|value| {
                    value
                        .as_array()
                        .context("workspace.exclude must be an array")?
                        .iter()
                        .map(|value| {
                            Ok(glob::Pattern::new(
                                value
                                    .as_str()
                                    .context("workspace exclude must be a string")?,
                            )?)
                        })
                        .collect::<anyhow::Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if let Some(patterns) = patterns {
            for pattern in patterns
                .as_array()
                .context("workspace members must be an array")?
            {
                for member in super::expand(
                    root,
                    pattern
                        .as_str()
                        .context("workspace member must be a string")?,
                )? {
                    let relative = member
                        .strip_prefix(root)
                        .context("workspace member must be inside its workspace")?;
                    if excludes
                        .iter()
                        .any(|pattern| pattern.matches_path(relative))
                    {
                        continue;
                    }
                    let member = fs::canonicalize(member)?;
                    if !member.starts_with(root) {
                        bail!(
                            "workspace members outside {} require an explicit plugin_sources entry",
                            root.display()
                        );
                    }
                    if member == root {
                        continue;
                    }
                    members.insert(member);
                }
            }
        }
    }
    Ok(found.then(|| members.into_iter().collect()))
}
use anyhow::{Context, bail};
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path};

pub(super) fn read(root: &Path, role: SourceRole) -> anyhow::Result<Option<Candidate>> {
    let mut found = Vec::new();
    for (filename, format) in [("Cargo.toml", "cargo"), ("package.json", "bun")] {
        let path = root.join(filename);
        if !path.try_exists()? {
            continue;
        }
        let value = document(&path)?;
        let metadata = if format == "cargo" {
            value.pointer("/package/metadata/lenso")
        } else {
            value.get("lenso")
        };
        let Some(metadata) = metadata else {
            continue;
        };
        // SDK packages may expose lenso.build without declaring a business Plugin.
        if format == "bun"
            && metadata.get("pluginId").is_none()
            && (metadata.get("build").is_some() || metadata.get("contract").is_some())
        {
            continue;
        }
        if format == "cargo"
            && metadata.get("plugin-id").is_none()
            && metadata.get("contract").is_some()
        {
            continue;
        }
        let identity_key = if format == "cargo" {
            "plugin-id"
        } else {
            "pluginId"
        };
        let plugin_id = string(metadata, identity_key)?.to_owned();
        classify_existing_plugin_id(&plugin_id)?;
        let version = if format == "cargo" {
            cargo_version(root, &value)?
        } else {
            string(&value, "version")?.to_owned()
        };
        validate_release_version(&version)?;
        let implementations = if format == "cargo" {
            cargo_implementations(root, &value)?
        } else {
            let runtime = string(metadata, "runtime")?;
            if runtime != "bun" {
                bail!(
                    "unsupported package.json Plugin runtime `{runtime}` at {}; discovery does not imply Adapter support",
                    path.display()
                );
            }
            vec![implementation("bun", runtime, root)]
        };
        found.push(Candidate {
            composite: None,
            surface_owner: None,
            plugin_id,
            release_version: version,
            project: root.to_path_buf(),
            metadata: path,
            format: format.to_owned(),
            role,
            implementations,
            evidence: "source_metadata_only".to_owned(),
        });
    }
    if found.len() > 1 {
        bail!(
            "ambiguous Plugin project at {}: both Cargo.toml and package.json declare a Plugin; use an explicit composite project with separate implementation directories",
            root.display()
        );
    }
    Ok(found.pop())
}

pub(super) fn document(path: &Path) -> anyhow::Result<Value> {
    let text = read_metadata(path)?;
    if path
        .extension()
        .is_some_and(|extension| extension == "toml")
    {
        let value: toml::Value =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(serde_json::to_value(value)?)
    } else {
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }
}

fn string<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("Plugin metadata requires nonempty `{key}`"))
}

fn cargo_version(root: &Path, value: &Value) -> anyhow::Result<String> {
    let package = &value["package"];
    if let Some(version) = package["version"].as_str() {
        return Ok(version.to_owned());
    }
    if package
        .pointer("/version/workspace")
        .and_then(Value::as_bool)
        != Some(true)
    {
        bail!(
            "Plugin Cargo package requires a version at {}",
            root.display()
        );
    }
    let explicit = package
        .get("workspace")
        .and_then(Value::as_str)
        .map(|path| root.join(path));
    let roots = explicit.map_or_else(
        || root.ancestors().map(Path::to_path_buf).collect::<Vec<_>>(),
        |path| vec![path],
    );
    for parent in roots {
        let manifest = parent.join("Cargo.toml");
        if !manifest.try_exists()? {
            continue;
        }
        let workspace = document(&manifest)?;
        if workspace.get("workspace").is_some() {
            return workspace
                .pointer("/workspace/package/version")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .context("workspace.package.version is required for the inherited Plugin version");
        }
    }
    bail!(
        "cannot find workspace.package.version for {}",
        root.display()
    )
}

fn implementation(id: &str, runtime: &str, root: &Path) -> Implementation {
    Implementation {
        id: id.to_owned(),
        runtime: runtime.to_owned(),
        project: root.to_path_buf(),
    }
}

fn cargo_implementations(root: &Path, value: &Value) -> anyhow::Result<Vec<Implementation>> {
    let metadata = value.pointer("/package/metadata/lenso-cli");
    let Some(metadata) = metadata else {
        // This is the existing native Web scaffold discriminator; do not infer
        // native linking for every Cargo package or business Capability.
        let native_sdk = value
            .pointer("/dependencies/lenso")
            .is_some_and(|dependency| {
                dependency
                    .get("package")
                    .and_then(Value::as_str)
                    .is_none_or(|name| name == "lenso")
            })
            || value
                .pointer("/dependencies/lenso-native-adapter")
                .is_some();
        let runtime = if native_sdk
            || value
                .pointer("/package/metadata/lenso/root-slot")
                .and_then(Value::as_str)
                == Some("web")
        {
            "native-linked"
        } else {
            "wasm"
        };
        return Ok(vec![implementation(runtime, runtime, root)]);
    };
    let declarations = metadata.get("implementations");
    let outputs = metadata.get("outputs");
    let runtime = metadata.get("runtime");
    if [declarations, outputs, runtime]
        .into_iter()
        .flatten()
        .count()
        > 1
    {
        bail!(
            "Plugin runtime, outputs, and implementations are mutually exclusive at {}",
            root.display()
        );
    }
    if let Some(declarations) = declarations {
        let declarations = declarations
            .as_array()
            .context("Plugin implementations must be an array")?;
        if declarations.len() < 2 {
            bail!("composite Plugin requires at least two implementations");
        }
        let mut ids = BTreeSet::new();
        let mut implementations = Vec::new();
        for declaration in declarations {
            let id = string(declaration, "id")?;
            if !ids.insert(id) {
                bail!("duplicate Plugin implementation `{id}`");
            }
            let runtime = string(declaration, "runtime")?;
            if !["wasm", "process", "bun"].contains(&runtime) {
                bail!("unsupported source implementation `{runtime}`");
            }
            let relative = Path::new(string(declaration, "path")?);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
            {
                bail!("Plugin implementation path must stay inside its project");
            }
            let path = fs::canonicalize(root.join(relative))
                .context("resolve declared Plugin implementation")?;
            if !path.starts_with(root) || !path.is_dir() {
                bail!("Plugin implementation path must be a directory inside its project");
            }
            let manifest = path.join(if runtime == "bun" {
                "package.json"
            } else {
                "Cargo.toml"
            });
            let child = document(&manifest)?;
            let child_id = if runtime == "bun" {
                child.pointer("/lenso/pluginId")
            } else {
                child.pointer("/package/metadata/lenso/plugin-id")
            };
            if child_id != value.pointer("/package/metadata/lenso/plugin-id") {
                bail!("composite implementation `{id}` has a different Plugin identity");
            }
            let child_version = if runtime == "bun" {
                string(&child, "version")?.to_owned()
            } else {
                cargo_version(&path, &child)?
            };
            if child_version != cargo_version(root, value)? {
                bail!("composite implementation `{id}` has a different release version");
            }
            implementations.push(implementation(id, runtime, &path));
        }
        return Ok(implementations);
    }
    let names = if let Some(outputs) = outputs {
        let names = outputs
            .as_array()
            .context("Plugin outputs must be an array")?
            .iter()
            .map(|value| value.as_str().context("Plugin output must be a string"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if !matches!(
            names.as_slice(),
            ["wasm"] | ["process"] | ["wasm", "process"]
        ) {
            bail!("unsupported Plugin outputs {names:?}");
        }
        names
    } else {
        vec![runtime.map_or(Ok("wasm"), |runtime| {
            runtime.as_str().context("Plugin runtime must be a string")
        })?]
    };
    names
        .into_iter()
        .map(|runtime| {
            if !["wasm", "process", "bun", "native-linked"].contains(&runtime) {
                bail!("unsupported Cargo Plugin runtime `{runtime}`");
            }
            Ok(implementation(runtime, runtime, root))
        })
        .collect()
}

pub(super) fn bundle(path: &Path, role: SourceRole) -> anyhow::Result<Candidate> {
    use lenso_plugin_bundle::{PluginManifest, read_bundle_manifest};
    let manifest = with_bundle_directory(path, |root| Ok(read_bundle_manifest(root)?))?;
    let implementations = match &manifest {
        PluginManifest::V2(value) => vec![implementation(
            "default",
            string(&value.entry.descriptor, "execution_class")?,
            path,
        )],
        PluginManifest::V3(value) => value
            .implementations
            .iter()
            .map(|item| implementation(&item.id, item.runtime.execution_class().as_str(), path))
            .collect(),
        PluginManifest::V4(value) => value
            .implementations
            .iter()
            .map(|item| implementation(&item.id, item.runtime.execution_class().as_str(), path))
            .collect(),
    };
    Ok(Candidate {
        composite: None,
        surface_owner: None,
        plugin_id: manifest.plugin_id().to_owned(),
        release_version: manifest.release_version().to_owned(),
        project: path.to_path_buf(),
        metadata: if path.is_dir() {
            path.join(lenso_plugin_bundle::MANIFEST_FILE)
        } else {
            path.to_path_buf()
        },
        format: "bundle".to_owned(),
        role,
        implementations,
        evidence: "verified_bundle_not_admitted".to_owned(),
    })
}
