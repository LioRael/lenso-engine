//! Read-only surface selection. No compiler, package manager or business code runs here.
use super::{Candidate, DiscoveryReport, SourceRole, project, read_metadata};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Composite {
    schema: String,
    core: String,
    #[serde(default)]
    surfaces: Vec<Surface>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Surface {
    entry: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Convention {
    id: String,
    entries: Vec<String>,
    #[serde(default)]
    compiler: Option<Compiler>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Compiler {
    pub program: String,
    pub args: Vec<String>,
    /// Optional execution budget for this compiler only. Omitted values use
    /// the Engine defaults, preserving the ordinary processor boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_limit_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Compilation {
    pub owner: String,
    pub version: String,
    pub role: SourceRole,
    pub owner_project: PathBuf,
    pub entry: PathBuf,
    pub plugin_id: String,
    pub convention: String,
    pub compiler_project: PathBuf,
    pub compiler: Compiler,
}

#[derive(Debug, Serialize)]
pub struct SurfaceSelection {
    pub owner: String,
    pub entry: PathBuf,
    pub support: Option<String>,
    pub convention: Option<String>,
    pub plugin_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ConventionPlan {
    pub schema: &'static str,
    pub candidates: Vec<Candidate>,
    pub surfaces: Vec<SurfaceSelection>,
    pub compilations: Vec<Compilation>,
}

pub(super) fn composite(root: &Path, role: SourceRole) -> anyhow::Result<Candidate> {
    let manifest = root.join("plugin.json");
    let composite: Composite = serde_json::from_str(&read_metadata(&manifest)?)?;
    if composite.schema != "lenso.plugin-project.v1" {
        bail!("unsupported composite Plugin project schema");
    }
    let core = inside(root, &composite.core)?;
    if core == root {
        bail!("composite core must be a nested package");
    }
    let mut candidate =
        project::read(&core, role)?.context("composite core must declare a Plugin")?;
    candidate.evidence = format!("composite:{}", manifest.display());
    // Keep the compiler project and its metadata unchanged. The separate source
    // root is recovered from this manifest by the selection phase.
    candidate.composite = Some(manifest);
    Ok(candidate)
}

fn metadata(candidate: &Candidate) -> anyhow::Result<serde_json::Value> {
    if candidate.format == "bundle" || candidate.format == "convention-owner" {
        return Ok(serde_json::Value::Null);
    }
    let document = project::document(&candidate.metadata)?;
    Ok(if candidate.format == "cargo" {
        document.pointer("/package/metadata/lenso")
    } else {
        document.get("lenso")
    }
    .cloned()
    .unwrap_or_default())
}

/// Count authored active instances without activating or compiling a Plugin.
pub fn active_instances(root: &Path, candidate: &Candidate) -> anyhow::Result<usize> {
    let directory = root.join("plugins").join(&candidate.plugin_id);
    if !directory.try_exists()? {
        return Ok(usize::from(candidate.role == SourceRole::AppOwned));
    }
    if !fs::symlink_metadata(&directory)?.file_type().is_dir() {
        bail!("Plugin Root directory must not be a symbolic link");
    }
    let mut normalized = BTreeMap::new();
    let mut instances = BTreeSet::new();
    if candidate.role == SourceRole::AppOwned {
        instances.insert("default".to_owned());
    }
    let mut disabled = BTreeSet::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!("Plugin Root entries cannot be symbolic links");
        }
        if !kind.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name
            .to_str()
            .context("Plugin Root filename must be UTF-8")?;
        crate::reject_case_collision(&mut normalized, name, "Plugin filename")?;
        if let Some(instance) = name.strip_suffix(".toml") {
            crate::validate_instance_filename(instance)?;
            crate::read_configuration(&entry.path())?;
            instances.insert(instance.to_owned());
        }
        if let Some(instance) = name.strip_suffix(".disabled") {
            crate::validate_instance_filename(instance)?;
            if entry.metadata()?.len() != 0 {
                bail!("disabled marker must be empty");
            }
            disabled.insert(instance.to_owned());
        }
    }
    Ok(instances.difference(&disabled).count())
}

/// Select contributions from adopted support packages, without inspecting inactive
/// package manifests. The ordinary runtime resolver still owns final admission.
pub fn plan(report: &DiscoveryReport) -> anyhow::Result<ConventionPlan> {
    let mut recognition = BTreeMap::<String, (String, String, PathBuf, Option<Compiler>)>::new();
    let mut conventions = BTreeSet::new();
    let mut known_entries = BTreeSet::new();
    for candidate in &report.candidates {
        let adopted = active_instances(&report.root, candidate)? != 0;
        let meta = metadata(candidate)?;
        let declarations: Vec<Convention> = serde_json::from_value(
            meta.get("conventions")
                .cloned()
                .unwrap_or(serde_json::json!([])),
        )?;
        for declaration in declarations {
            known_entries.extend(declaration.entries.iter().cloned());
            if !adopted {
                continue;
            }
            crate::identity::classify_existing_plugin_id(&declaration.id)?;
            if !conventions.insert(declaration.id.clone()) {
                bail!("duplicate convention identity {}", declaration.id);
            }
            if declaration.entries.is_empty() || declaration.entries.len() > 64 {
                bail!("convention entries must contain 1..64 filenames");
            }
            for entry in declaration.entries {
                if entry.is_empty()
                    || Path::new(&entry).components().count() != 1
                    || !matches!(
                        Path::new(&entry).components().next(),
                        Some(Component::Normal(_))
                    )
                {
                    bail!("convention entry must be a filename: {entry}");
                }
                if let Some(previous) = recognition.insert(
                    entry.clone(),
                    (
                        candidate.plugin_id.clone(),
                        declaration.id.clone(),
                        candidate.project.clone(),
                        declaration.compiler.clone(),
                    ),
                ) {
                    bail!(
                        "conflicting convention entry {entry}: {} and {}",
                        previous.0,
                        candidate.plugin_id
                    );
                }
            }
        }
    }
    let mut result = ConventionPlan {
        schema: "lenso.convention-plan.v1",
        candidates: report.candidates.clone(),
        surfaces: Vec::new(),
        compilations: Vec::new(),
    };
    let mut ids = report
        .candidates
        .iter()
        .map(|c| c.plugin_id.clone())
        .collect::<BTreeSet<_>>();
    let mut owners = report.candidates.clone();
    let app = report.root.join("app");
    if app.is_dir() {
        discover_bare_owners(&app, &app, &known_entries, &mut owners, &mut 0, 0)?;
    }
    for owner in &owners {
        let (base, mut surfaces) = if let Some(manifest) = &owner.composite {
            let composite: Composite = serde_json::from_str(&read_metadata(manifest)?)?;
            (
                manifest.parent().context("composite parent")?.to_path_buf(),
                composite.surfaces,
            )
        } else {
            let meta = metadata(owner)?;
            (
                owner.project.clone(),
                serde_json::from_value::<Vec<Surface>>(
                    meta.get("surfaces")
                        .cloned()
                        .unwrap_or(serde_json::json!([])),
                )?,
            )
        };
        if surfaces.is_empty() {
            discover_entries(&base, &base, &known_entries, &mut surfaces, &mut 0, 0)?;
        }
        if surfaces.len() > 256 {
            bail!("Plugin accepts at most 256 surfaces");
        }
        let owner_instances = active_instances(&report.root, owner)?;
        if !surfaces.is_empty() && owner_instances > 1 {
            bail!(
                "surface composition requires one active owner instance: {}",
                owner.plugin_id
            );
        }
        let selected_owner = owner_instances == 1;
        let mut entries = BTreeSet::new();
        for surface in surfaces {
            let entry = inside(&base, &surface.entry)?;
            if !entry.is_file() && !entry.is_dir() {
                bail!(
                    "surface entry must be a file or directory: {}",
                    entry.display()
                );
            }
            if !entries.insert(entry.clone()) {
                bail!("duplicate surface entry {}", entry.display());
            }
            let filename = entry
                .file_name()
                .and_then(|s| s.to_str())
                .context("surface filename must be UTF-8")?;
            let support = recognition.get(filename);
            let active = selected_owner && support.is_some();
            if selected_owner && surface.required && support.is_none() {
                bail!(
                    "required surface {} has no adopted convention support",
                    entry.display()
                );
            }
            let mut selection = SurfaceSelection {
                owner: owner.plugin_id.clone(),
                entry: entry.clone(),
                support: support.map(|s| s.0.clone()),
                convention: support.map(|s| s.1.clone()),
                plugin_id: None,
                reason: if !selected_owner {
                    "owner_not_adopted"
                } else if !active {
                    "support_not_adopted"
                } else {
                    "selected"
                }
                .into(),
            };
            if active && surface.project.is_none() {
                use sha2::{Digest, Sha256};
                let support = support.context("selected convention")?;
                let compiler = support
                    .3
                    .clone()
                    .context("standalone entry requires a convention compiler")?;
                if compiler.program.is_empty() || compiler.args.len() > 32 {
                    bail!("invalid convention compiler command");
                }
                if compiler
                    .timeout_seconds
                    .is_some_and(|seconds| !(1..=300).contains(&seconds))
                {
                    bail!("convention compiler timeout_seconds must be between 1 and 300");
                }
                if compiler
                    .output_limit_bytes
                    .is_some_and(|bytes| !(1..=16 * 1024 * 1024).contains(&bytes))
                {
                    bail!(
                        "convention compiler output_limit_bytes must be between 1 and {}",
                        16 * 1024 * 1024
                    );
                }
                let digest = Sha256::digest(surface.entry.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                let plugin_id = format!("{}.surface-{}", owner.plugin_id, &digest[..12]);
                if !ids.insert(plugin_id.clone()) {
                    bail!("duplicate generated surface identity {plugin_id}");
                }
                selection.plugin_id = Some(plugin_id.clone());
                result.compilations.push(Compilation {
                    owner: owner.plugin_id.clone(),
                    version: owner.release_version.clone(),
                    role: owner.role,
                    owner_project: base.clone(),
                    entry: entry.clone(),
                    plugin_id,
                    convention: support.1.clone(),
                    compiler_project: support.2.clone(),
                    compiler,
                });
            } else if active {
                let package = inside(
                    &base,
                    surface.project.as_deref().context("surface package")?,
                )?;
                if package == owner.project || !entry.starts_with(&package) {
                    bail!("surface requires an independent package containing its entry");
                }
                let mut candidate = project::read(&package, owner.role)?
                    .context("selected surface package must declare a Plugin")?;
                if candidate.release_version != owner.release_version {
                    bail!("surface release must match its logical owner");
                }
                if !ids.insert(candidate.plugin_id.clone()) {
                    bail!("duplicate surface Plugin identity {}", candidate.plugin_id);
                }
                let meta = metadata(&candidate)?;
                if meta.get("conventions").is_some() || meta.get("surfaces").is_some() {
                    bail!("surface packages cannot recursively activate conventions or surfaces");
                }
                candidate.surface_owner = Some(owner.plugin_id.clone());
                candidate.evidence = format!("surface:{}:{}", owner.plugin_id, support.unwrap().1);
                selection.plugin_id = Some(candidate.plugin_id.clone());
                result.candidates.push(candidate);
            }
            result.surfaces.push(selection);
        }
    }
    result
        .candidates
        .sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    result
        .surfaces
        .sort_by(|a, b| (&a.owner, &a.entry).cmp(&(&b.owner, &b.entry)));
    Ok(result)
}

fn inside(root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("surface paths must be relative and stay inside their owner");
    }
    let mut current = root.to_path_buf();
    for part in relative.components() {
        current.push(part);
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            bail!("surface paths cannot traverse symbolic links");
        }
    }
    let path = fs::canonicalize(current)?;
    if !path.starts_with(fs::canonicalize(root)?) {
        bail!("surface path escapes owner");
    }
    Ok(path)
}

fn discover_entries(
    root: &Path,
    directory: &Path,
    names: &BTreeSet<String>,
    surfaces: &mut Vec<Surface>,
    count: &mut usize,
    depth: usize,
) -> anyhow::Result<()> {
    if depth > 32 {
        bail!("convention discovery exceeds 32 levels");
    }
    let mut children = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        *count += 1;
        if *count > 10000 {
            bail!("convention discovery exceeds 10000 entries");
        }
        let name = child.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || super::EXCLUDED.contains(&name.as_str()) {
            continue;
        }
        let kind = child.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        // A selected directory is one processing boundary. Its processor owns
        // nested routes/files and private manifests, including dependency setup.
        if (kind.is_file() || kind.is_dir()) && names.contains(&name) {
            surfaces.push(Surface {
                entry: child
                    .path()
                    .strip_prefix(root)?
                    .to_str()
                    .context("entry path must be UTF-8")?
                    .replace('\\', "/"),
                project: None,
                required: false,
            });
        } else if kind.is_dir() {
            if child.path().join("package.json").exists()
                || child.path().join("Cargo.toml").exists()
                || child.path().join("plugin.json").exists()
            {
                continue;
            }
            discover_entries(root, &child.path(), names, surfaces, count, depth + 1)?;
        }
    }
    Ok(())
}

/// Read one compiler output using the ordinary source metadata rules.
pub fn generated_candidate(project: &Path, compilation: &Compilation) -> anyhow::Result<Candidate> {
    let project = fs::canonicalize(project)?;
    let mut candidate = if project.join(lenso_plugin_bundle::MANIFEST_FILE).exists() {
        project::bundle(&project, compilation.role)?
    } else {
        project::read(&project, compilation.role)?
            .context("compiler output must declare a Plugin")?
    };
    if candidate.plugin_id != compilation.plugin_id
        || candidate.release_version != compilation.version
    {
        bail!("convention compiler changed its assigned Plugin identity or version");
    }
    let meta = metadata(&candidate)?;
    if meta.get("conventions").is_some() || meta.get("surfaces").is_some() {
        bail!("compiler output cannot recursively activate conventions");
    }
    candidate.surface_owner = Some(compilation.owner.clone());
    candidate.evidence = format!("compiler:{}:{}", compilation.owner, compilation.convention);
    Ok(candidate)
}

fn discover_bare_owners(
    root: &Path,
    directory: &Path,
    names: &BTreeSet<String>,
    owners: &mut Vec<Candidate>,
    count: &mut usize,
    depth: usize,
) -> anyhow::Result<()> {
    if depth > 32 {
        bail!("bare entry discovery exceeds 32 levels");
    }
    if ["package.json", "Cargo.toml", "plugin.json"]
        .iter()
        .any(|name| directory.join(name).exists())
    {
        return Ok(());
    }
    let mut children = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    let mut found = false;
    for child in &children {
        *count += 1;
        if *count > 10000 {
            bail!("bare entry discovery exceeds 10000 entries");
        }
        if (child.file_type()?.is_file() || child.file_type()?.is_dir())
            && names.contains(&child.file_name().to_string_lossy().into_owned())
        {
            found = true;
        }
    }
    if found {
        use sha2::{Digest, Sha256};
        let relative = directory
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        let digest = Sha256::digest(relative.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        owners.push(Candidate {
            plugin_id: format!("local.files-{digest}"),
            release_version: "1.0.0".into(),
            project: fs::canonicalize(directory)?,
            metadata: directory.to_path_buf(),
            format: "convention-owner".into(),
            role: SourceRole::AppOwned,
            implementations: vec![],
            evidence: "bare_app_entries".into(),
            surface_owner: None,
            composite: None,
        });
        return Ok(());
    }
    for child in children {
        let name = child.file_name().to_string_lossy().into_owned();
        if child.file_type()?.is_dir()
            && !name.starts_with('.')
            && !super::EXCLUDED.contains(&name.as_str())
        {
            discover_bare_owners(root, &child.path(), names, owners, count, depth + 1)?;
        }
    }
    Ok(())
}
