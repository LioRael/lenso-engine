//! Validated, atomic authoring operations for one Lenso App Plugin Root.
//!
//! Every mutation resolves the complete candidate against the Host Catalog
//! before changing visible App-owned files. Runtime Generation staging and
//! switching remain the responsibility of the running Host.

mod archive_download;

#[path = "archive.rs"]
pub mod bundle_archive;
pub mod discovery;
pub mod host_authoring;
pub mod identity;
pub mod signed_plugin_catalog;

use host_authoring::{GeneratedHostBuild, HOST_BUILD, HostInput};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use lenso_app_plan::authoring::{
    DependencyChoice, PluginDescriptor, PluginInstanceId, PluginRootInstance, PluginRootSnapshot,
    ResolvedApp,
};
use lenso_app_plan::{ExecutionClassId, PLUGIN_AUTHORING_V2_RUNTIME_PROFILE};
use lenso_plugin_bundle::{
    ImplementationPolicy, RuntimeAdmission, VerifiedBundle, read_bundle_manifest,
    resolve_implementation, verify_bundle_directory,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::identity::{
    classify_existing_plugin_id, validate_plugin_id_v1, validate_release_version,
};

/// Rust target triple of this CLI build.
///
/// Plugin admission uses the same canonical vocabulary as `rustc` and
/// `lenso app build --target`.
#[doc(hidden)]
#[must_use]
pub const fn native_host_target() -> &'static str {
    env!("LENSO_CLI_BUILD_TARGET")
}

mod configuration_authority;
mod root_transaction;
mod selection_authority;

pub use configuration_authority::{
    LocalPluginRootAuthority, PluginConfigurationApplication, PluginConfigurationAuthority,
    PluginConfigurationAuthoritySource, PluginConfigurationDiagnostic, PluginConfigurationProposal,
    PluginConfigurationProposalStatus, PluginConfigurationPublication,
    PluginConfigurationSourceConflict, PluginConfigurationSourceDigest, PluginRequirementMigration,
    PluginRootChangeProposal, PluginRootChangePublication, PluginRootChangeSet,
    PluginRootConfigurationChange, PluginRootRevision, PluginRootRevisionConflict,
    PluginRootRevisionParseError, PluginRootSourceDigest, propose_instance_configuration,
    propose_plugin_root_changes, publish_instance_configuration, publish_plugin_root_changes,
};
pub use selection_authority::{
    PluginSelectionAuthority, PluginSelectionPublication, set_instance_enabled_fenced,
};

const PLUGIN_ROOT: &str = "plugins";
const HOST_CATALOG: &str = ".lenso/host-catalog.json";
const BUNDLE_NAME: &str = "plugin.lenso-plugin";
const DEPENDENCY_SELECTIONS: &str = ".dependencies.json";
const LEGACY_DEPENDENCY_SELECTIONS: &str = "dependencies.json";
pub const DEPENDENCY_SELECTIONS_SCHEMA_VERSION: u32 = 1;
pub const DEPENDENCY_SELECTIONS_SCHEMA: &str = "lenso.plugin-dependencies.v1";
const AUTHORING_LOCK: &str = ".lenso/plugin-root-authoring.lock";
const TRANSACTION_GUARD: &str = ".transaction";
const MAX_DEPENDENCY_SELECTION_BYTES: u64 = 1024 * 1024;
const MAX_DEPENDENCY_SELECTIONS: usize = 4_096;
const MAX_CONFIGURATION_BYTES: u64 = 256 * 1024;
const MAX_RESOURCE_FILES: usize = 4_096;
const MAX_RESOURCE_FILE_BYTES: u64 = 1024 * 1024;
const MAX_RESOURCE_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RESOURCE_DEPTH: usize = 32;

/// Resolves the App selected by one project root's Host Catalog and Plugin Root.
pub fn load_resolved_app(root: &Path) -> anyhow::Result<ResolvedApp> {
    let _lock = lock_plugin_root_shared(root)?;
    let host = load_host_catalog(root)?;
    let snapshot = snapshot_plugin_root(root, &host)?;
    host.resolve(&snapshot).map_err(anyhow::Error::msg)
}

/// Exact runtime input resolved from immutable distribution authority and one external Root.
#[derive(Clone, Debug, Serialize)]
pub struct RuntimeAppResolution {
    schema: &'static str,
    app_id: String,
    authority_digest: String,
    host_build_digest: String,
    plugin_root_revision: String,
    plan: lenso_app_plan::ResolvedAppPlan,
}

/// Resolves an external Plugin Root without allowing it to replace distribution authority.
pub fn resolve_runtime_app(root: &Path, host_build: &Path) -> anyhow::Result<RuntimeAppResolution> {
    let root = fs::canonicalize(root).context("locate external App root")?;
    if !fs::metadata(&root)?.is_dir() {
        bail!("external App root must be a directory: {}", root.display());
    }
    for competing in [HOST_BUILD, HOST_CATALOG] {
        match fs::symlink_metadata(root.join(competing)) {
            Ok(_) => bail!(
                "external App root cannot replace distribution Host authority with `{competing}`"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect external Host authority"),
        }
    }
    let metadata = fs::symlink_metadata(host_build)
        .with_context(|| format!("inspect distribution Host build {}", host_build.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "distribution Host build must be a regular file: {}",
            host_build.display()
        );
    }
    let host_bytes = fs::read(host_build)
        .with_context(|| format!("read distribution Host build {}", host_build.display()))?;
    let host: GeneratedHostBuild =
        serde_json::from_slice(&host_bytes).context("invalid distribution Host build")?;
    host.validate()?;
    let _lock = lock_plugin_root_shared(&root)?;
    let snapshot = snapshot_plugin_root(&root, &HostInput::Generated(host.clone()))?;
    let plugin_root_revision = configuration_authority::revision_for_snapshot(&snapshot)?;
    let resolved = host.resolve(&snapshot).map_err(anyhow::Error::msg)?;
    let host_build_digest = runtime_sha256(&host_bytes);
    let authority = serde_json::to_vec(&serde_json::json!({
        "schema": "lenso.runtime-authority.v1",
        "host_build_digest": host_build_digest,
        "plugin_root_revision": plugin_root_revision.as_str(),
    }))?;
    Ok(RuntimeAppResolution {
        schema: "lenso.runtime-app-resolution.v1",
        app_id: host.host_id().to_owned(),
        authority_digest: runtime_sha256(&authority),
        host_build_digest,
        plugin_root_revision: plugin_root_revision.as_str().to_owned(),
        plan: resolved.plan().clone(),
    })
}

fn runtime_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut value = String::with_capacity(71);
    value.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

/// Read-only authoring state for one Plugin Instance.
///
/// This describes only the App-owned difference and the Host policy needed to
/// present it safely. The resolved Plan remains Host-owned runtime input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginInstanceAuthoringState {
    id: PluginInstanceId,
    origin: PluginInstanceOrigin,
    selection: PluginInstanceSelection,
    root_configuration_toml: Option<String>,
    source_digest: PluginConfigurationSourceDigest,
}

/// Authority that introduced one visible Plugin Instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginInstanceOrigin {
    HostDefault { disableable: bool },
    PluginRoot,
}

/// Current desired selection derived from the Plugin Root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginInstanceSelection {
    Enabled,
    DisabledByRoot,
}

impl PluginInstanceAuthoringState {
    pub const fn id(&self) -> &PluginInstanceId {
        &self.id
    }

    pub const fn is_enabled(&self) -> bool {
        matches!(self.selection, PluginInstanceSelection::Enabled)
    }

    pub const fn is_host_default(&self) -> bool {
        matches!(self.origin, PluginInstanceOrigin::HostDefault { .. })
    }

    pub const fn is_disableable(&self) -> bool {
        match self.origin {
            PluginInstanceOrigin::HostDefault { disableable } => disableable,
            PluginInstanceOrigin::PluginRoot => true,
        }
    }

    pub fn root_configuration_toml(&self) -> Option<&str> {
        self.root_configuration_toml.as_deref()
    }

    pub const fn source_digest(&self) -> &PluginConfigurationSourceDigest {
        &self.source_digest
    }

    pub const fn is_disabled_by_root(&self) -> bool {
        matches!(self.selection, PluginInstanceSelection::DisabledByRoot)
    }

    pub const fn has_root_difference(&self) -> bool {
        self.root_configuration_toml.is_some() || self.is_disabled_by_root()
    }
}

/// Read-only authoring state for one Plugin Release visible to the App owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginAuthoringState {
    configuration_defaults: Value,
    configuration_schema: Option<Value>,
    plugin_id: String,
    release_version: String,
    root_supplied: bool,
    instances: Vec<PluginInstanceAuthoringState>,
}

impl PluginAuthoringState {
    pub const fn configuration_schema(&self) -> Option<&Value> {
        self.configuration_schema.as_ref()
    }

    pub const fn configuration_defaults(&self) -> &Value {
        &self.configuration_defaults
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn release_version(&self) -> &str {
        &self.release_version
    }

    pub const fn is_root_supplied(&self) -> bool {
        self.root_supplied
    }

    pub fn instances(&self) -> &[PluginInstanceAuthoringState] {
        &self.instances
    }
}

/// Complete read-only management projection for the current Plugin Root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginRootAuthoringState {
    revision: PluginRootRevision,
    resolved: ResolvedApp,
    plugins: Vec<PluginAuthoringState>,
}

impl PluginRootAuthoringState {
    pub const fn revision(&self) -> &PluginRootRevision {
        &self.revision
    }

    pub const fn resolved(&self) -> &ResolvedApp {
        &self.resolved
    }

    pub fn plugins(&self) -> &[PluginAuthoringState] {
        &self.plugins
    }
}

/// Inspects the current Host Catalog and Plugin Root without changing either.
#[expect(
    clippy::too_many_lines,
    reason = "keeps one atomic read-only Root projection"
)]
pub fn inspect_plugin_root(root: &Path) -> anyhow::Result<PluginRootAuthoringState> {
    let _lock = lock_plugin_root_shared(root)?;
    let host = load_host_catalog(root)?;
    let snapshot = snapshot_plugin_root(root, &host)?;
    let revision = configuration_authority::revision_for_snapshot(&snapshot)?;
    let resolved = host.resolve(&snapshot).map_err(anyhow::Error::msg)?;
    let enabled = resolved
        .instances()
        .iter()
        .map(|instance| instance.id().clone())
        .collect::<BTreeSet<_>>();
    let disabled = snapshot.disabled().iter().cloned().collect::<BTreeSet<_>>();
    let root_instances = snapshot
        .instances()
        .iter()
        .map(|instance| instance.id().clone())
        .collect::<BTreeSet<_>>();
    let host_defaults = host
        .defaults()
        .iter()
        .map(|instance| (instance.id().clone(), instance.is_disableable()))
        .collect::<BTreeMap<_, _>>();

    let ids = root_instances
        .iter()
        .chain(disabled.iter())
        .chain(host_defaults.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let root_releases = snapshot
        .releases()
        .iter()
        .map(|release| release.plugin_id().to_owned())
        .collect::<BTreeSet<_>>();
    let mut releases = host
        .plugins()
        .iter()
        .map(|release| {
            let descriptor = release.descriptor();
            (
                descriptor.plugin_id().to_owned(),
                (
                    descriptor.release_version().to_owned(),
                    descriptor.configuration_schema().cloned(),
                    descriptor.configuration_defaults().clone(),
                ),
            )
        })
        .chain(snapshot.releases().iter().map(|release| {
            (
                release.plugin_id().to_owned(),
                (
                    release.release_version().to_owned(),
                    release.configuration_schema().cloned(),
                    release.configuration_defaults().clone(),
                ),
            )
        }))
        .collect::<BTreeMap<_, _>>();
    for id in &ids {
        releases
            .entry(id.plugin_id().to_owned())
            .or_insert_with(|| {
                (
                    String::new(),
                    None,
                    Value::Object(serde_json::Map::default()),
                )
            });
    }

    let mut plugins = Vec::with_capacity(releases.len());
    for (plugin_id, (release_version, configuration_schema, configuration_defaults)) in releases {
        let plugin_ids = ids
            .iter()
            .filter(|id| id.plugin_id() == plugin_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut instances = Vec::with_capacity(plugin_ids.len());
        for id in plugin_ids {
            let configuration_path = root
                .join(PLUGIN_ROOT)
                .join(id.plugin_id())
                .join(format!("{}.toml", id.instance_key()));
            let root_configuration_toml = if root_instances.contains(&id) {
                Some(fs::read_to_string(&configuration_path).with_context(|| {
                    format!(
                        "read Plugin configuration source {}",
                        configuration_path.display()
                    )
                })?)
            } else {
                None
            };
            let source_digest = instance_source_digest(&id, root_configuration_toml.as_deref());
            let host_disableable = host_defaults.get(&id).copied();
            instances.push(PluginInstanceAuthoringState {
                origin: host_disableable.map_or(PluginInstanceOrigin::PluginRoot, |disableable| {
                    PluginInstanceOrigin::HostDefault { disableable }
                }),
                selection: if enabled.contains(&id) {
                    PluginInstanceSelection::Enabled
                } else {
                    PluginInstanceSelection::DisabledByRoot
                },
                root_configuration_toml,
                source_digest,
                id,
            });
        }
        plugins.push(PluginAuthoringState {
            configuration_defaults,
            configuration_schema,
            root_supplied: root_releases.contains(&plugin_id),
            plugin_id,
            release_version,
            instances,
        });
    }
    Ok(authoring_state(revision, resolved, plugins))
}

fn instance_source_digest(
    id: &PluginInstanceId,
    source: Option<&str>,
) -> PluginConfigurationSourceDigest {
    configuration_authority::source_digest_for_bytes(
        id.plugin_id(),
        id.instance_key(),
        source.map(str::as_bytes),
    )
}

fn authoring_state(
    revision: PluginRootRevision,
    resolved: ResolvedApp,
    plugins: Vec<PluginAuthoringState>,
) -> PluginRootAuthoringState {
    PluginRootAuthoringState {
        revision,
        resolved,
        plugins,
    }
}

fn load_host_catalog(root: &Path) -> anyhow::Result<HostInput> {
    let generated = root.join(HOST_BUILD);
    match fs::symlink_metadata(&generated) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                bail!("Host build must be a regular file: {}", generated.display());
            }
            match fs::symlink_metadata(root.join(HOST_CATALOG)) {
                Ok(_) => bail!(
                    "competing Host authorities: install one complete Host build instead of mixing authority files"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect existing Host Catalog authority"),
            }
            let build: GeneratedHostBuild = serde_json::from_slice(&fs::read(&generated)?)
                .context("invalid generated Host build")?;
            build.validate()?;
            return Ok(HostInput::Generated(build));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect generated Host build"),
    }
    let path = root.join(HOST_CATALOG);
    let metadata = fs::symlink_metadata(&path).with_context(|| {
        format!(
            "Host Catalog is unavailable at {}; build or install the current Host first",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        bail!("Host Catalog must be a regular file: {}", path.display());
    }
    let bytes = fs::read(&path).with_context(|| format!("read Host Catalog {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map(HostInput::Legacy)
        .with_context(|| format!("Host Catalog is invalid: {}", path.display()))
}

fn snapshot_plugin_root(root: &Path, host: &HostInput) -> anyhow::Result<PluginRootSnapshot> {
    let plugin_root = root.join(PLUGIN_ROOT);
    match fs::symlink_metadata(&plugin_root) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => bail!(
            "Plugin Root must be a regular directory: {}",
            plugin_root.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PluginRootSnapshot::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", plugin_root.display()));
        }
    }

    let mut releases = Vec::new();
    let mut instances = Vec::new();
    let mut disabled = Vec::new();
    let mut dependency_selections = None;
    let mut legacy_dependency_selections = None;
    let mut plugin_names = BTreeMap::<String, String>::new();
    let mut directories = read_entries(&plugin_root)?;
    directories.sort_by_key(fs::DirEntry::file_name);
    for entry in directories {
        let name = utf8_name(&entry.path(), &entry.file_name())?;
        if is_ignored_os_metadata(&name) {
            continue;
        }
        let file_type = entry.file_type()?;
        if name == TRANSACTION_GUARD {
            bail!(
                "Plugin Root has an unresolved authoring transaction; run a configuration command to recover it: {}",
                entry.path().display()
            );
        }
        if name == DEPENDENCY_SELECTIONS || name == LEGACY_DEPENDENCY_SELECTIONS {
            let selections = read_dependency_selections(&entry.path(), &name, file_type)?;
            if name == DEPENDENCY_SELECTIONS {
                dependency_selections = Some(selections);
            } else {
                legacy_dependency_selections = Some(selections);
            }
            continue;
        }
        if !file_type.is_dir() {
            bail!("unknown Plugin Root entry: {}", entry.path().display());
        }
        let plugin_id = name;
        validate_existing_plugin_id(&plugin_id)?;
        reject_case_collision(&mut plugin_names, &plugin_id, "Plugin ID")?;
        scan_plugin_directory(
            &entry.path(),
            &plugin_id,
            &mut releases,
            &mut instances,
            &mut disabled,
            host,
        )?;
    }
    if dependency_selections.is_some() && legacy_dependency_selections.is_some() {
        bail!("Plugin Root contains both canonical and legacy dependency selection files");
    }
    let snapshot = PluginRootSnapshot::new(releases, instances, disabled);
    Ok(
        match dependency_selections.or(legacy_dependency_selections) {
            Some(selections) => snapshot.with_dependency_choices(selections),
            None => snapshot,
        },
    )
}

fn read_dependency_selections(
    path: &Path,
    name: &str,
    file_type: fs::FileType,
) -> anyhow::Result<Vec<DependencyChoice>> {
    if !file_type.is_file() {
        bail!(
            "Plugin dependency selections must be a regular file: {}",
            path.display()
        );
    }
    if fs::symlink_metadata(path)?.len() > MAX_DEPENDENCY_SELECTION_BYTES {
        bail!("Plugin dependency selections exceed 1 MiB");
    }
    let bytes = fs::read(path).context("read Plugin dependency selections")?;
    let selections = if name == DEPENDENCY_SELECTIONS {
        let document: DependencySelectionsDocument =
            serde_json::from_slice(&bytes).context("invalid Plugin dependency selections")?;
        if document.schema_version != DEPENDENCY_SELECTIONS_SCHEMA_VERSION {
            bail!(
                "unsupported Plugin dependency selection schema version `{}`",
                document.schema_version
            );
        }
        let mut sorted = document.choices.clone();
        sorted.sort_by(|left, right| {
            left.consumer
                .cmp(&right.consumer)
                .then_with(|| left.requirement_id.cmp(&right.requirement_id))
        });
        if document.choices != sorted {
            bail!("Plugin dependency selections must be sorted by consumer and requirement");
        }
        if document.choices.windows(2).any(|pair| {
            pair[0].consumer == pair[1].consumer && pair[0].requirement_id == pair[1].requirement_id
        }) {
            bail!("Plugin dependency selections contain a duplicate requirement key");
        }
        document.choices
    } else {
        let document: LegacyDependencySelectionsDocument = serde_json::from_slice(&bytes)
            .context("invalid legacy Plugin dependency selections")?;
        if document.schema != DEPENDENCY_SELECTIONS_SCHEMA {
            bail!(
                "unsupported legacy Plugin dependency selection schema `{}`",
                document.schema
            );
        }
        document.selections
    };
    if selections.len() > MAX_DEPENDENCY_SELECTIONS {
        bail!("Plugin dependency selections exceed {MAX_DEPENDENCY_SELECTIONS} entries");
    }
    Ok(selections)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DependencySelectionsDocument {
    pub schema_version: u32,
    pub choices: Vec<DependencyChoice>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyDependencySelectionsDocument {
    schema: String,
    selections: Vec<DependencyChoice>,
}

fn preserve_dependency_selections(
    candidate: PluginRootSnapshot,
    current: &PluginRootSnapshot,
) -> PluginRootSnapshot {
    if current.dependency_selection_adopted() {
        candidate.with_dependency_choices(current.dependency_choices().to_vec())
    } else {
        candidate
    }
}

fn scan_plugin_directory(
    directory: &Path,
    plugin_id: &str,
    releases: &mut Vec<PluginDescriptor>,
    instances: &mut Vec<PluginRootInstance>,
    disabled: &mut Vec<PluginInstanceId>,
    host: &HostInput,
) -> anyhow::Result<()> {
    let mut normalized = BTreeMap::<String, String>::new();
    let mut configured_instances = BTreeSet::new();
    let mut resource_directories = BTreeMap::<String, PathBuf>::new();
    let mut entries = read_entries(directory)?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = utf8_name(&entry.path(), &entry.file_name())?;
        if is_ignored_os_metadata(&name) {
            continue;
        }
        reject_case_collision(&mut normalized, &name, "Plugin filename")?;
        let file_type = entry.file_type()?;
        if name == BUNDLE_NAME {
            if !file_type.is_dir() {
                bail!(
                    "Plugin Bundle must be a regular directory: {}",
                    entry.path().display()
                );
            }
            releases.push(read_bundle_descriptor(&entry.path(), plugin_id, host)?);
            continue;
        }
        if file_type.is_dir() {
            validate_instance_filename(&name)?;
            resource_directories.insert(name, entry.path());
            continue;
        }
        if !file_type.is_file() {
            bail!(
                "Plugin entries cannot be symlinks or special files: {}",
                entry.path().display()
            );
        }
        if let Some(instance) = name.strip_suffix(".toml") {
            validate_instance_filename(instance)?;
            configured_instances.insert(instance.to_owned());
            instances.push(
                PluginRootInstance::new(plugin_id, instance)
                    .with_configuration(read_configuration(&entry.path())?),
            );
        } else if let Some(instance) = name.strip_suffix(".disabled") {
            validate_instance_filename(instance)?;
            if fs::metadata(entry.path())?.len() != 0 {
                bail!("disabled marker must be empty: {}", entry.path().display());
            }
            disabled.push(PluginInstanceId::new(plugin_id, instance));
        } else {
            bail!("unknown Plugin file: {}", entry.path().display());
        }
    }
    for (instance, resource_directory) in resource_directories {
        if !configured_instances.contains(&instance) {
            bail!(
                "orphan Plugin resource directory without `{instance}.toml`: {}",
                resource_directory.display()
            );
        }
        validate_resource_directory(&resource_directory)?;
    }
    Ok(())
}

fn validate_resource_directory(path: &Path) -> anyhow::Result<()> {
    let mut file_count = 0_usize;
    let mut total_size = 0_u64;
    let mut pending = vec![(path.to_path_buf(), 0_usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_RESOURCE_DEPTH {
            bail!(
                "Plugin resource directory exceeds {MAX_RESOURCE_DEPTH} levels: {}",
                directory.display()
            );
        }
        let mut entries = read_entries(&directory)?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let entry_path = entry.path();
            let name = utf8_name(&entry_path, &entry.file_name())?;
            if is_ignored_os_metadata(&name) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push((entry_path, depth + 1));
                continue;
            }
            if !file_type.is_file() {
                bail!(
                    "Plugin resources cannot contain symlinks or special files: {}",
                    entry_path.display()
                );
            }
            if file_count == MAX_RESOURCE_FILES {
                bail!(
                    "Plugin resources exceed {MAX_RESOURCE_FILES} files: {}",
                    path.display()
                );
            }
            let metadata = fs::symlink_metadata(&entry_path)?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                bail!(
                    "Plugin resources must be regular files: {}",
                    entry_path.display()
                );
            }
            if metadata.len() > MAX_RESOURCE_FILE_BYTES {
                bail!("Plugin resource exceeds 1 MiB: {}", entry_path.display());
            }
            let bytes = fs::read(&entry_path)?;
            let byte_count = u64::try_from(bytes.len()).with_context(|| {
                format!("Plugin resource is too large: {}", entry_path.display())
            })?;
            if byte_count > MAX_RESOURCE_FILE_BYTES {
                bail!("Plugin resource exceeds 1 MiB: {}", entry_path.display());
            }
            total_size = total_size
                .checked_add(byte_count)
                .with_context(|| format!("Plugin resource size overflow: {}", path.display()))?;
            if total_size > MAX_RESOURCE_TOTAL_BYTES {
                bail!("Plugin resources exceed 16 MiB: {}", path.display());
            }
            file_count += 1;
        }
    }
    Ok(())
}

fn is_ignored_os_metadata(name: &str) -> bool {
    name == ".DS_Store"
}

fn read_bundle_descriptor(
    path: &Path,
    plugin_id: &str,
    host: &HostInput,
) -> anyhow::Result<PluginDescriptor> {
    validate_existing_plugin_id(plugin_id)?;
    let verified = verify_bundle_directory(path)
        .with_context(|| format!("verify Plugin Bundle {}", path.display()))?;
    if verified.plugin_id != plugin_id {
        bail!("Plugin Bundle ID does not match its directory");
    }
    host.select_bundle(path, &verified)
}

fn read_verified_bundle_descriptor(
    path: &Path,
    plugin_id: &str,
    verified: &VerifiedBundle,
) -> anyhow::Result<PluginDescriptor> {
    if verified.plugin_id != plugin_id {
        bail!(
            "Plugin Bundle ID `{}` does not match directory `{plugin_id}`",
            verified.plugin_id
        );
    }
    let manifest = read_bundle_manifest(path)
        .with_context(|| format!("read Plugin Manifest {}", path.display()))?;
    let descriptor = resolve_implementation(
        &manifest,
        &ImplementationPolicy {
            host_target: native_host_target().to_owned(),
            runtimes: [
                ("lenso.quickjs@1", "lenso.quickjs@1"),
                ("lenso.process@1", "lenso.process-stdio@2"),
                ("lenso.process@1", "lenso.process@1"),
                ("lenso.wasm-component@1", "lenso.wasm-component@1"),
                ("lenso.bun-process@1", PLUGIN_AUTHORING_V2_RUNTIME_PROFILE),
                ("lenso.bun-process@1", "lenso.bun-process@1"),
            ]
            .into_iter()
            .map(|(execution_class, runtime_profile)| RuntimeAdmission {
                execution_class: ExecutionClassId::new(execution_class),
                runtime_profile: runtime_profile.to_owned(),
            })
            .collect(),
        },
    )?
    .descriptor;
    if descriptor.plugin_id() != plugin_id
        || descriptor.release_version() != verified.release_version
    {
        bail!("Plugin Descriptor identity does not match the verified Bundle");
    }
    Ok(descriptor)
}

fn read_configuration(path: &Path) -> anyhow::Result<serde_json::Value> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_CONFIGURATION_BYTES {
        bail!("Plugin configuration exceeds 256 KiB: {}", path.display());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("read Plugin configuration {}", path.display()))?;
    let table: toml::Table = toml::from_str(&text)
        .with_context(|| format!("parse Plugin configuration {}", path.display()))?;
    serde_json::to_value(table).context("convert Plugin configuration to portable values")
}

fn read_entries(path: &Path) -> anyhow::Result<Vec<fs::DirEntry>> {
    fs::read_dir(path)
        .with_context(|| format!("read directory {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("read directory entries {}", path.display()))
}

fn utf8_name(path: &Path, name: &std::ffi::OsStr) -> anyhow::Result<String> {
    name.to_str()
        .map(str::to_owned)
        .with_context(|| format!("Plugin path is not UTF-8: {}", path.display()))
}

fn validate_instance_filename(instance: &str) -> anyhow::Result<()> {
    validate_path_identity(instance, "Instance key")?;
    if instance.starts_with('.') || instance == "plugin" {
        bail!("reserved Plugin Instance key `{instance}`");
    }
    Ok(())
}

fn validate_existing_plugin_id(plugin_id: &str) -> anyhow::Result<()> {
    validate_path_identity(plugin_id, "Plugin ID")?;
    classify_existing_plugin_id(plugin_id).map(|_| ())
}

fn validate_path_identity(value: &str, label: &str) -> anyhow::Result<()> {
    if value.trim() != value
        || value.is_empty()
        || value == "."
        || value == ".."
        || value.contains(['/', '\0', '\\'])
    {
        bail!("invalid {label} `{value}`");
    }
    Ok(())
}

fn reject_case_collision(
    normalized: &mut BTreeMap<String, String>,
    value: &str,
    label: &str,
) -> anyhow::Result<()> {
    let key = value.to_lowercase();
    if let Some(previous) = normalized.insert(key, value.to_owned())
        && previous != value
    {
        bail!("case-colliding {label}s `{previous}` and `{value}`");
    }
    Ok(())
}

/// Adds one verified external Plugin Bundle after resolving the complete candidate App.
pub fn add_bundle(root: &Path, bundle: &Path) -> anyhow::Result<(String, String, ResolvedApp)> {
    prepare_bundle_mutation(root, bundle, BundleMutation::Add)?.commit()
}

/// Desired root-Bundle mutation validated before visible bytes change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BundleMutation {
    Add,
    Replace,
    /// Restore bytes already retained for a legacy or v1 root Plugin.
    Restore,
}

/// Stable staged bytes and candidate resolution for one pending Bundle mutation.
///
/// Callers may inspect the verified identity before committing, which lets a
/// catalog compare its signed metadata without re-reading or re-hashing the
/// Bundle. The staged directory is removed automatically unless `commit` is
/// called.
#[derive(Debug)]
pub struct PreparedBundleMutation {
    authority: fs::File,
    destination: PathBuf,
    mutation: BundleMutation,
    resolved: ResolvedApp,
    staging: tempfile::TempDir,
    verified: VerifiedBundle,
}

impl PreparedBundleMutation {
    pub const fn verified(&self) -> &VerifiedBundle {
        &self.verified
    }

    pub const fn resolved(&self) -> &ResolvedApp {
        &self.resolved
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Atomically makes the already-validated staged Bundle visible.
    pub fn commit(self) -> anyhow::Result<(String, String, ResolvedApp)> {
        let Self {
            authority,
            destination,
            mutation,
            resolved,
            staging,
            verified,
        } = self;
        let commit = commit_staged_bundle(&destination, mutation, staging);
        drop(authority);
        commit?;
        Ok((verified.plugin_id, verified.release_version, resolved))
    }
}

fn commit_staged_bundle(
    destination: &Path,
    mutation: BundleMutation,
    staging: tempfile::TempDir,
) -> anyhow::Result<()> {
    commit_staged_bundle_with(
        destination,
        mutation,
        staging,
        atomic_publish_bundle,
        tempfile::TempDir::close,
    )
}

fn commit_staged_bundle_with<Publish, Retire>(
    destination: &Path,
    mutation: BundleMutation,
    staging: tempfile::TempDir,
    publish: Publish,
    retire: Retire,
) -> anyhow::Result<()>
where
    Publish: FnOnce(&Path, &Path, BundleMutation) -> std::io::Result<()>,
    Retire: FnOnce(tempfile::TempDir) -> std::io::Result<()>,
{
    let parent = destination
        .parent()
        .context("Bundle destination has no parent")?;
    if mutation == BundleMutation::Add && destination.exists() {
        bail!("Plugin Bundle already exists: {}", destination.display());
    }
    let created_parent = mutation == BundleMutation::Add && !parent.exists();
    if mutation == BundleMutation::Add {
        fs::create_dir_all(parent)?;
    }
    let publication =
        publish(staging.path(), destination, mutation).with_context(|| match mutation {
            BundleMutation::Add => format!("commit Plugin Bundle {}", destination.display()),
            BundleMutation::Replace | BundleMutation::Restore => {
                format!("atomically replace Plugin Bundle {}", destination.display())
            }
        });
    if let Err(error) = publication {
        if created_parent
            && let Err(cleanup_error) = fs::remove_dir(parent)
            && cleanup_error.kind() != std::io::ErrorKind::NotFound
            && cleanup_error.kind() != std::io::ErrorKind::DirectoryNotEmpty
        {
            return Err(error.context(format!(
                "also failed to remove empty Plugin directory {}: {cleanup_error}",
                parent.display()
            )));
        }
        return Err(error);
    }

    if mutation != BundleMutation::Add
        && let Err(error) = retire(staging)
    {
        // EXCHANGE is the commit point: the new Bundle is already visible and
        // the old one is isolated at the hidden staging path. Cleanup failure
        // must not misreport a successfully committed mutation as rejected.
        eprintln!("warning: Plugin Bundle committed, but retired Bundle cleanup failed: {error}");
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn atomic_publish_bundle(
    staging: &Path,
    destination: &Path,
    mutation: BundleMutation,
) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    let flags = match mutation {
        BundleMutation::Add => RenameFlags::NOREPLACE,
        BundleMutation::Replace | BundleMutation::Restore => RenameFlags::EXCHANGE,
    };
    renameat_with(CWD, staging, CWD, destination, flags).map_err(std::io::Error::from)
}

#[cfg(windows)]
fn atomic_publish_bundle(
    staging: &Path,
    destination: &Path,
    mutation: BundleMutation,
) -> std::io::Result<()> {
    match mutation {
        // MoveFileW is intentionally used without a replacement flag: it is
        // one atomic rename and fails if a concurrent writer won the target.
        BundleMutation::Add => winsafe::MoveFile(
            staging.to_str().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Plugin Bundle staging path is not Unicode",
                )
            })?,
            destination.to_str().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Plugin Bundle destination path is not Unicode",
                )
            })?,
        )
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw() as i32)),
        BundleMutation::Replace | BundleMutation::Restore => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic Plugin Bundle replacement is unavailable on this platform",
        )),
    }
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
fn atomic_publish_bundle(
    _staging: &Path,
    _destination: &Path,
    _mutation: BundleMutation,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic Plugin Bundle publication is unavailable on this platform",
    ))
}

/// Copies one candidate into stable staging, validates it once, and resolves
/// the complete candidate App before any Plugin Root bytes change.
pub fn prepare_bundle_mutation(
    root: &Path,
    bundle: &Path,
    mutation: BundleMutation,
) -> anyhow::Result<PreparedBundleMutation> {
    let staging = tempfile::Builder::new()
        .prefix(".plugin-bundle-")
        .tempdir_in(root)?;
    copy_directory(bundle, staging.path())?;
    let authority = lock_plugin_root(root)?;
    let host = load_host_catalog(root)?;
    let (verified, descriptor) = verify_bundle_mutation(staging.path(), mutation, &host)?;
    let resolved = resolve_bundle_mutation(root, mutation, &verified, descriptor, &host)?;
    let destination = root
        .join(PLUGIN_ROOT)
        .join(&verified.plugin_id)
        .join(BUNDLE_NAME);
    Ok(PreparedBundleMutation {
        authority,
        destination,
        mutation,
        resolved,
        staging,
        verified,
    })
}

/// Verifies one Bundle and resolves the complete candidate App for an add or replacement.
///
/// `prepare_bundle_mutation` is the preferred mutation boundary because it
/// also owns stable staged bytes and the atomic commit.
pub fn validate_bundle_mutation(
    root: &Path,
    bundle: &Path,
    mutation: BundleMutation,
) -> anyhow::Result<(lenso_plugin_bundle::VerifiedBundle, ResolvedApp)> {
    let _lock = lock_plugin_root(root)?;
    let host = load_host_catalog(root)?;
    let (verified, descriptor) = verify_bundle_mutation(bundle, mutation, &host)?;
    let resolved = resolve_bundle_mutation(root, mutation, &verified, descriptor, &host)?;
    Ok((verified, resolved))
}

fn verify_bundle_mutation(
    bundle: &Path,
    mutation: BundleMutation,
    host: &HostInput,
) -> anyhow::Result<(VerifiedBundle, PluginDescriptor)> {
    let verified = verify_bundle_directory(bundle)
        .with_context(|| format!("verify Plugin Bundle {}", bundle.display()))?;
    match mutation {
        BundleMutation::Add | BundleMutation::Replace => {
            validate_plugin_id_v1(&verified.plugin_id)?;
        }
        BundleMutation::Restore => {
            classify_existing_plugin_id(&verified.plugin_id)?;
        }
    }
    validate_release_version(&verified.release_version)?;
    let descriptor = host.select_bundle(bundle, &verified)?;
    Ok((verified, descriptor))
}

fn resolve_bundle_mutation(
    root: &Path,
    mutation: BundleMutation,
    verified: &VerifiedBundle,
    descriptor: PluginDescriptor,
    host: &HostInput,
) -> anyhow::Result<ResolvedApp> {
    let current = snapshot_plugin_root(root, host)?;
    let has_current = current
        .releases()
        .iter()
        .any(|release| release.plugin_id() == verified.plugin_id);
    match (mutation, has_current) {
        (BundleMutation::Add, true) => {
            bail!("Plugin `{}` already has a root Bundle", verified.plugin_id)
        }
        (BundleMutation::Replace | BundleMutation::Restore, false) => {
            bail!(
                "Plugin `{}` has no root Bundle to update",
                verified.plugin_id
            )
        }
        _ => {}
    }
    let candidate = preserve_dependency_selections(
        PluginRootSnapshot::new(
            current
                .releases()
                .iter()
                .filter(|release| release.plugin_id() != verified.plugin_id)
                .cloned()
                .chain([descriptor]),
            current.instances().iter().cloned(),
            current.disabled().iter().cloned(),
        ),
        &current,
    );
    let resolved = host.resolve(&candidate).map_err(anyhow::Error::msg)?;
    Ok(resolved)
}

/// Atomically writes one typed Instance patch after resolving the complete candidate App.
pub fn configure_instance(
    root: &Path,
    plugin_id: &str,
    instance: &str,
    bytes: &[u8],
) -> anyhow::Result<ResolvedApp> {
    let base_revision = inspect_plugin_root(root)?.revision().clone();
    let proposal =
        propose_instance_configuration(root, &base_revision, plugin_id, instance, bytes)?;
    let publication = publish_instance_configuration(root, &proposal)?;
    Ok(publication.into_resolved())
}

/// Atomically changes one Instance selection marker after candidate resolution.
pub fn set_instance_disabled(
    root: &Path,
    plugin_id: &str,
    instance: &str,
    disabled_state: bool,
) -> anyhow::Result<ResolvedApp> {
    set_instance_disabled_inner(root, plugin_id, instance, disabled_state, None)
        .map(|(_, _, resolved)| resolved)
}

/// Saves one exact App-owned dependency choice after validating the complete candidate App.
pub fn set_dependency_selection(
    root: &Path,
    consumer: PluginInstanceId,
    requirement_id: &str,
    provider: Option<PluginInstanceId>,
) -> anyhow::Result<ResolvedApp> {
    let selection = DependencyChoice {
        consumer,
        requirement_id: requirement_id.to_owned(),
        provider,
    };
    set_dependency_selections(root, [selection])
}

/// Atomically applies one or more App-owned dependency choices as one validated candidate.
pub fn set_dependency_selections(
    root: &Path,
    replacements: impl IntoIterator<Item = DependencyChoice>,
) -> anyhow::Result<ResolvedApp> {
    apply_dependency_selections(root, replacements.into_iter().collect(), false)
}

/// Replaces the complete saved choice set after validating one complete candidate App.
///
/// This is the explicit migration boundary for renamed or removed requirement identities.
pub fn replace_dependency_selections(
    root: &Path,
    selections: impl IntoIterator<Item = DependencyChoice>,
) -> anyhow::Result<ResolvedApp> {
    apply_dependency_selections(root, selections.into_iter().collect(), true)
}

fn apply_dependency_selections(
    root: &Path,
    replacements: Vec<DependencyChoice>,
    replace_all: bool,
) -> anyhow::Result<ResolvedApp> {
    if replacements.is_empty() && !replace_all {
        bail!("at least one dependency selection is required");
    }
    let mut replacement_keys = BTreeSet::new();
    for selection in &replacements {
        validate_existing_plugin_id(selection.consumer.plugin_id())?;
        validate_instance_filename(selection.consumer.instance_key())?;
        validate_requirement_id(&selection.requirement_id)?;
        if let Some(provider) = &selection.provider {
            validate_existing_plugin_id(provider.plugin_id())?;
            validate_instance_filename(provider.instance_key())?;
        }
        if !replacement_keys.insert((selection.consumer.clone(), selection.requirement_id.clone()))
        {
            bail!(
                "duplicate dependency selection for `{}` requirement `{}`",
                selection.consumer,
                selection.requirement_id
            );
        }
    }
    let _lock = lock_plugin_root(root)?;
    let host = load_host_catalog(root)?;
    let current = snapshot_plugin_root(root, &host)?;
    let mut selections = if replace_all {
        replacements
    } else {
        let mut selections = current.dependency_choices().to_vec();
        selections.retain(|selection| {
            !replacement_keys
                .contains(&(selection.consumer.clone(), selection.requirement_id.clone()))
        });
        selections.extend(replacements);
        selections
    };
    selections.sort_by(|left, right| {
        left.consumer
            .cmp(&right.consumer)
            .then_with(|| left.requirement_id.cmp(&right.requirement_id))
    });
    let (resolved, selections) = resolve_adopted_dependencies(&host, &current, selections)?;
    let document = DependencySelectionsDocument {
        schema_version: DEPENDENCY_SELECTIONS_SCHEMA_VERSION,
        choices: selections,
    };
    let bytes = serde_json::to_vec_pretty(&document).context("encode dependency selections")?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_DEPENDENCY_SELECTION_BYTES {
        bail!("Plugin dependency selections exceed 1 MiB");
    }
    let legacy = root.join(PLUGIN_ROOT).join(LEGACY_DEPENDENCY_SELECTIONS);
    let legacy_exists = match fs::symlink_metadata(&legacy) {
        Ok(metadata) if metadata.file_type().is_file() => true,
        Ok(_) => bail!(
            "legacy Plugin dependency selections must be a regular file: {}",
            legacy.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("inspect legacy Plugin dependency selections"),
    };
    if legacy_exists {
        root_transaction::publish_root_files(
            root,
            vec![
                root_transaction::RootFileChange::write(DEPENDENCY_SELECTIONS, bytes),
                root_transaction::RootFileChange::remove(LEGACY_DEPENDENCY_SELECTIONS),
            ],
        )?;
    } else {
        atomic_write(&root.join(PLUGIN_ROOT).join(DEPENDENCY_SELECTIONS), &bytes)?;
    }
    Ok(resolved)
}

fn validate_requirement_id(value: &str) -> anyhow::Result<()> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_lowercase()
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
    {
        bail!("dependency requirement identity `{value}` is invalid");
    }
    Ok(())
}

fn resolve_adopted_dependencies(
    host: &HostInput,
    current: &PluginRootSnapshot,
    mut selections: Vec<DependencyChoice>,
) -> anyhow::Result<(ResolvedApp, Vec<DependencyChoice>)> {
    selections.sort_by(|left, right| {
        left.consumer
            .cmp(&right.consumer)
            .then_with(|| left.requirement_id.cmp(&right.requirement_id))
    });
    let candidate = PluginRootSnapshot::new(
        current.releases().iter().cloned(),
        current.instances().iter().cloned(),
        current.disabled().iter().cloned(),
    )
    .with_dependency_choices(selections);
    let proposed = host.propose(&candidate).map_err(anyhow::Error::msg)?;
    let selections = proposed.dependency_choices().to_vec();
    let materialized = PluginRootSnapshot::new(
        current.releases().iter().cloned(),
        current.instances().iter().cloned(),
        current.disabled().iter().cloned(),
    )
    .with_dependency_choices(selections.clone());
    let resolved = host.resolve(&materialized).map_err(anyhow::Error::msg)?;
    Ok((resolved, selections))
}

fn set_instance_disabled_inner(
    root: &Path,
    plugin_id: &str,
    instance: &str,
    disabled_state: bool,
    expected_revision: Option<&PluginRootRevision>,
) -> anyhow::Result<(PluginRootRevision, PluginRootRevision, ResolvedApp)> {
    validate_existing_plugin_id(plugin_id)?;
    validate_instance_filename(instance)?;
    let _lock = lock_plugin_root(root)?;
    let host = load_host_catalog(root)?;
    let current = snapshot_plugin_root(root, &host)?;
    let base_revision = configuration_authority::revision_for_snapshot(&current)?;
    if let Some(expected_revision) = expected_revision {
        configuration_authority::ensure_revision(expected_revision, &base_revision)?;
    }
    let id = PluginInstanceId::new(plugin_id, instance);
    let mut disabled = current.disabled().iter().cloned().collect::<BTreeSet<_>>();
    if disabled_state {
        disabled.insert(id.clone());
    } else if !disabled.remove(&id) {
        bail!("Plugin Instance `{id}` is not disabled");
    }
    let candidate = preserve_dependency_selections(
        PluginRootSnapshot::new(
            current.releases().iter().cloned(),
            current.instances().iter().cloned(),
            disabled,
        ),
        &current,
    );
    let candidate_revision = configuration_authority::revision_for_snapshot(&candidate)?;
    let resolved = host.resolve(&candidate).map_err(anyhow::Error::msg)?;
    let marker = root
        .join(PLUGIN_ROOT)
        .join(plugin_id)
        .join(format!("{instance}.disabled"));
    if disabled_state {
        atomic_write(&marker, &[])?;
    } else {
        fs::remove_file(&marker)
            .with_context(|| format!("remove disabled marker {}", marker.display()))?;
    }
    Ok((base_revision, candidate_revision, resolved))
}

/// Removes one App-owned Instance difference after validating the remaining App.
pub fn remove_instance_difference(
    root: &Path,
    plugin_id: &str,
    instance: &str,
) -> anyhow::Result<ResolvedApp> {
    validate_existing_plugin_id(plugin_id)?;
    validate_instance_filename(instance)?;
    let _lock = lock_plugin_root(root)?;
    let host = load_host_catalog(root)?;
    let current = snapshot_plugin_root(root, &host)?;
    let id = PluginInstanceId::new(plugin_id, instance);
    let candidate = preserve_dependency_selections(
        PluginRootSnapshot::new(
            current.releases().iter().cloned(),
            current
                .instances()
                .iter()
                .filter(|item| item.id() != &id)
                .cloned(),
            current
                .disabled()
                .iter()
                .filter(|item| *item != &id)
                .cloned(),
        ),
        &current,
    );
    let resolved = host.resolve(&candidate).map_err(anyhow::Error::msg)?;
    let plugin_directory = root.join(PLUGIN_ROOT).join(plugin_id);
    remove_if_exists(&plugin_directory.join(format!("{instance}.toml")))?;
    remove_if_exists(&plugin_directory.join(format!("{instance}.disabled")))?;
    Ok(resolved)
}

/// Moves one root-supplied Plugin to recoverable trash after validating the remaining App.
pub fn remove_plugin(root: &Path, plugin_id: &str) -> anyhow::Result<(ResolvedApp, PathBuf)> {
    validate_existing_plugin_id(plugin_id)?;
    let _lock = lock_plugin_root(root)?;
    let host = load_host_catalog(root)?;
    let current = snapshot_plugin_root(root, &host)?;
    let candidate = preserve_dependency_selections(
        PluginRootSnapshot::new(
            current
                .releases()
                .iter()
                .filter(|release| release.plugin_id() != plugin_id)
                .cloned(),
            current
                .instances()
                .iter()
                .filter(|instance| instance.id().plugin_id() != plugin_id)
                .cloned(),
            current
                .disabled()
                .iter()
                .filter(|instance| instance.plugin_id() != plugin_id)
                .cloned(),
        ),
        &current,
    );
    let resolved = host.resolve(&candidate).map_err(anyhow::Error::msg)?;
    let plugin_directory = root.join(PLUGIN_ROOT).join(plugin_id);
    if !plugin_directory.exists() {
        bail!("Plugin `{plugin_id}` has no Plugin Root directory");
    }
    let trash = root
        .join(".lenso/trash")
        .join(format!("{plugin_id}-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(trash.parent().expect("trash has a parent"))?;
    fs::rename(&plugin_directory, &trash)?;
    Ok((resolved, trash))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("Plugin file has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    fs::write(temporary.path(), bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("commit Plugin file {}", path.display()))?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn lock_plugin_root(root: &Path) -> anyhow::Result<fs::File> {
    let path = root.join(AUTHORING_LOCK);
    let parent = path.parent().context("Plugin Root lock has no parent")?;
    fs::create_dir_all(parent)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open Plugin Root authoring lock {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock Plugin Root authoring authority {}", path.display()))?;
    root_transaction::recover_plugin_root_transaction(root)?;
    Ok(file)
}

fn lock_plugin_root_shared(root: &Path) -> anyhow::Result<Option<fs::File>> {
    let path = root.join(AUTHORING_LOCK);
    let file = match fs::OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if root.join(PLUGIN_ROOT).join(DEPENDENCY_SELECTIONS).exists() {
                bail!(
                    "adopted Plugin Root is missing its authoring lock: {}",
                    path.display()
                );
            }
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("open Plugin Root authoring lock {}", path.display()));
        }
    };
    file.lock_shared()
        .with_context(|| format!("lock Plugin Root for reading {}", path.display()))?;
    Ok(Some(file))
}
fn copy_directory(source: &Path, destination: &Path) -> anyhow::Result<()> {
    for entry in read_entries(source)? {
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let child = destination.join(entry.file_name());
            fs::create_dir_all(&child)?;
            copy_directory(&entry.path(), &child)?;
            continue;
        }
        if !file_type.is_file() {
            bail!(
                "Plugin Bundle contains a non-file entry: {}",
                entry.path().display()
            );
        }
        fs::copy(entry.path(), destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lenso_app_plan::authoring::{
        HostBinding, HostCatalog, HostDefaultPlugin, HostPluginRelease, HostSlot,
    };
    use lenso_app_plan::{CapabilityEndpointPlan, CapabilityRequirementPlan};

    fn fixture_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lenso")).unwrap();
        let host = HostCatalog::new(
            [HostSlot::one("agent")],
            [HostPluginRelease::new(PluginDescriptor::new(
                "example.agent",
                "1.0.0",
                "agent",
            ))],
            [HostDefaultPlugin::new("example.agent", "default")],
        );
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();
        root
    }

    #[test]
    fn missing_plugin_root_resolves_the_host_default_app() {
        let root = fixture_root();
        let resolved = load_resolved_app(root.path()).unwrap();

        assert_eq!(resolved.instances().len(), 1);
        assert_eq!(
            resolved.instances()[0].id().to_string(),
            "example.agent/default"
        );
    }

    #[test]
    fn dependency_choice_is_materialized_and_survives_a_new_compatible_provider() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lenso")).unwrap();
        let consumer = PluginDescriptor::new("example.copy", "1.0.0", "copy")
            .with_authoring(2, "lenso.test-authoring@2")
            .with_requirement(
                CapabilityRequirementPlan::one("example.store@1", "1.0.0")
                    .with_requirement_id("source"),
            );
        let store = |plugin_id: &str| {
            PluginDescriptor::new(plugin_id, "1.0.0", "store").with_capability(
                CapabilityEndpointPlan::new("example.store@1", "1.0.0", ["get"]),
            )
        };
        let host = HostCatalog::new(
            [HostSlot::one("copy"), HostSlot::many("store")],
            [
                HostPluginRelease::new(consumer.clone()),
                HostPluginRelease::new(store("example.store.a")),
            ],
            [
                HostDefaultPlugin::new("example.copy", "default"),
                HostDefaultPlugin::new("example.store.a", "default"),
            ],
        )
        .with_bindings([HostBinding::new(
            PluginInstanceId::new("example.copy", "default"),
            "example.store@1",
            "store",
        )
        .with_requirement_id("source")
        .selectable(None)]);
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();

        set_dependency_selection(
            root.path(),
            PluginInstanceId::new("example.copy", "default"),
            "source",
            Some(PluginInstanceId::new("example.store.a", "default")),
        )
        .unwrap();
        assert!(root.path().join("plugins/.dependencies.json").is_file());

        let canonical: DependencySelectionsDocument = serde_json::from_slice(
            &fs::read(root.path().join("plugins/.dependencies.json")).unwrap(),
        )
        .unwrap();
        fs::write(
            root.path().join("plugins/dependencies.json"),
            serde_json::to_vec_pretty(&LegacyDependencySelectionsDocument {
                schema: DEPENDENCY_SELECTIONS_SCHEMA.to_owned(),
                selections: canonical.choices,
            })
            .unwrap(),
        )
        .unwrap();
        fs::remove_file(root.path().join("plugins/.dependencies.json")).unwrap();
        set_dependency_selection(
            root.path(),
            PluginInstanceId::new("example.copy", "default"),
            "source",
            Some(PluginInstanceId::new("example.store.a", "default")),
        )
        .unwrap();
        assert!(root.path().join("plugins/.dependencies.json").is_file());
        assert!(!root.path().join("plugins/dependencies.json").exists());

        let expanded = HostCatalog::new(
            [HostSlot::one("copy"), HostSlot::many("store")],
            [
                HostPluginRelease::new(consumer),
                HostPluginRelease::new(store("example.store.a")),
                HostPluginRelease::new(store("example.store.b")),
            ],
            [
                HostDefaultPlugin::new("example.copy", "default"),
                HostDefaultPlugin::new("example.store.a", "default"),
                HostDefaultPlugin::new("example.store.b", "default"),
            ],
        )
        .with_bindings([HostBinding::new(
            PluginInstanceId::new("example.copy", "default"),
            "example.store@1",
            "store",
        )
        .with_requirement_id("source")
        .selectable(None)]);
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&expanded).unwrap(),
        )
        .unwrap();

        let resolved = load_resolved_app(root.path()).unwrap();
        assert_eq!(
            resolved.plan().capability_bindings()[0].provider_instance(),
            "example.store.a/default"
        );
    }

    #[test]
    fn first_bind_repairs_the_requested_legacy_ambiguity_and_materializes_unique_choices() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lenso")).unwrap();
        let consumer = PluginDescriptor::new("example.copy", "1.0.0", "copy")
            .with_authoring(2, "lenso.test-authoring@2")
            .with_requirement(
                CapabilityRequirementPlan::one("example.store@1", "1.0.0")
                    .with_requirement_id("source"),
            )
            .with_requirement(
                CapabilityRequirementPlan::one("example.audit@1", "1.0.0")
                    .with_requirement_id("audit"),
            );
        let store = |plugin_id: &str| {
            PluginDescriptor::new(plugin_id, "1.0.0", "store").with_capability(
                CapabilityEndpointPlan::new("example.store@1", "1.0.0", ["get"]),
            )
        };
        let audit = PluginDescriptor::new("example.audit", "1.0.0", "audit").with_capability(
            CapabilityEndpointPlan::new("example.audit@1", "1.0.0", ["record"]),
        );
        let host = HostCatalog::new(
            [
                HostSlot::one("copy"),
                HostSlot::many("store"),
                HostSlot::one("audit"),
            ],
            [
                HostPluginRelease::new(consumer),
                HostPluginRelease::new(store("example.store.a")),
                HostPluginRelease::new(store("example.store.b")),
                HostPluginRelease::new(audit),
            ],
            [
                HostDefaultPlugin::new("example.copy", "default"),
                HostDefaultPlugin::new("example.store.a", "default"),
                HostDefaultPlugin::new("example.store.b", "default"),
                HostDefaultPlugin::new("example.audit", "default"),
            ],
        )
        .with_bindings([HostBinding::new(
            PluginInstanceId::new("example.copy", "default"),
            "example.store@1",
            "store",
        )
        .with_requirement_id("source")
        .selectable(None)]);
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();

        let resolved = set_dependency_selection(
            root.path(),
            PluginInstanceId::new("example.copy", "default"),
            "source",
            Some(PluginInstanceId::new("example.store.b", "default")),
        )
        .unwrap();
        let bindings = resolved
            .plan()
            .capability_bindings()
            .iter()
            .map(|binding| (binding.requirement_id(), binding.provider_instance()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(bindings["source"], "example.store.b/default");
        assert_eq!(bindings["audit"], "example.audit/default");
        let mut document: DependencySelectionsDocument = serde_json::from_slice(
            &fs::read(root.path().join("plugins/.dependencies.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(document.choices.len(), 1);
        document.choices.insert(
            0,
            DependencyChoice {
                consumer: PluginInstanceId::new("example.copy", "default"),
                requirement_id: "retired".to_owned(),
                provider: Some(PluginInstanceId::new("example.store.a", "default")),
            },
        );
        fs::write(
            root.path().join("plugins/.dependencies.json"),
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();

        replace_dependency_selections(
            root.path(),
            [DependencyChoice {
                consumer: PluginInstanceId::new("example.copy", "default"),
                requirement_id: "source".to_owned(),
                provider: Some(PluginInstanceId::new("example.store.b", "default")),
            }],
        )
        .unwrap();
        let repaired: DependencySelectionsDocument = serde_json::from_slice(
            &fs::read(root.path().join("plugins/.dependencies.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(repaired.choices.len(), 1);
        assert_eq!(repaired.choices[0].requirement_id, "source");
    }

    #[test]
    fn batch_bind_adopts_two_ambiguous_requirements_atomically() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lenso")).unwrap();
        let consumer = PluginDescriptor::new("example.copy", "1.0.0", "copy")
            .with_authoring(2, "lenso.test-authoring@2")
            .with_requirement(
                CapabilityRequirementPlan::one("example.store@1", "1.0.0")
                    .with_requirement_id("source"),
            )
            .with_requirement(
                CapabilityRequirementPlan::one("example.store@1", "1.0.0")
                    .with_requirement_id("destination"),
            );
        let store = |plugin_id: &str| {
            PluginDescriptor::new(plugin_id, "1.0.0", "store").with_capability(
                CapabilityEndpointPlan::new("example.store@1", "1.0.0", ["get"]),
            )
        };
        let host = HostCatalog::new(
            [HostSlot::one("copy"), HostSlot::many("store")],
            [
                HostPluginRelease::new(consumer),
                HostPluginRelease::new(store("example.store.a")),
                HostPluginRelease::new(store("example.store.b")),
            ],
            [
                HostDefaultPlugin::new("example.copy", "default"),
                HostDefaultPlugin::new("example.store.a", "default"),
                HostDefaultPlugin::new("example.store.b", "default"),
            ],
        )
        .with_bindings([
            HostBinding::new(
                PluginInstanceId::new("example.copy", "default"),
                "example.store@1",
                "store",
            )
            .with_requirement_id("source")
            .selectable(None),
            HostBinding::new(
                PluginInstanceId::new("example.copy", "default"),
                "example.store@1",
                "store",
            )
            .with_requirement_id("destination")
            .selectable(None),
        ]);
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();
        let consumer = PluginInstanceId::new("example.copy", "default");

        let resolved = set_dependency_selections(
            root.path(),
            [
                DependencyChoice {
                    consumer: consumer.clone(),
                    requirement_id: "source".to_owned(),
                    provider: Some(PluginInstanceId::new("example.store.a", "default")),
                },
                DependencyChoice {
                    consumer,
                    requirement_id: "destination".to_owned(),
                    provider: Some(PluginInstanceId::new("example.store.b", "default")),
                },
            ],
        )
        .unwrap();
        let bindings = resolved
            .plan()
            .capability_bindings()
            .iter()
            .map(|binding| (binding.requirement_id(), binding.provider_instance()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(bindings["source"], "example.store.a/default");
        assert_eq!(bindings["destination"], "example.store.b/default");
    }

    #[test]
    fn inspection_separates_host_defaults_from_root_differences() {
        let root = fixture_root();
        let plugin = root.path().join("plugins/example.agent");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(plugin.join("default.toml"), "").unwrap();

        let state = inspect_plugin_root(root.path()).unwrap();
        let plugin = state
            .plugins()
            .iter()
            .find(|plugin| plugin.plugin_id() == "example.agent")
            .unwrap();
        let instance = &plugin.instances()[0];

        assert_eq!(plugin.release_version(), "1.0.0");
        assert!(!plugin.is_root_supplied());
        assert!(instance.is_enabled());
        assert!(instance.is_host_default());
        assert!(!instance.is_disableable());
        assert_eq!(instance.root_configuration_toml(), Some(""));
        assert!(instance.source_digest().as_str().starts_with("sha256:"));
        assert!(instance.has_root_difference());
    }

    #[test]
    fn inspection_reports_disabled_host_default_without_losing_the_instance() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lenso")).unwrap();
        let host = HostCatalog::new(
            [HostSlot::optional("optional")],
            [HostPluginRelease::new(PluginDescriptor::new(
                "example.optional",
                "1.0.0",
                "optional",
            ))],
            [HostDefaultPlugin::new("example.optional", "default").disableable()],
        );
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();
        let plugin = root.path().join("plugins/example.optional");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(plugin.join("default.disabled"), "").unwrap();

        let state = inspect_plugin_root(root.path()).unwrap();
        let instance = &state.plugins()[0].instances()[0];

        assert!(!instance.is_enabled());
        assert!(instance.is_host_default());
        assert!(instance.is_disableable());
        assert!(instance.is_disabled_by_root());
    }

    #[test]
    fn local_selection_authority_disables_and_enables_one_instance() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lenso")).unwrap();
        let host = HostCatalog::new(
            [HostSlot::optional("optional")],
            [HostPluginRelease::new(PluginDescriptor::new(
                "example.optional",
                "1.0.0",
                "optional",
            ))],
            [HostDefaultPlugin::new("example.optional", "default").disableable()],
        );
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();
        let authority = LocalPluginRootAuthority::new(root.path());
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();

        let disabled = authority
            .set_enabled(&base, "example.optional", "default", false)
            .unwrap();
        assert_eq!(disabled.base_revision(), &base);
        assert!(!disabled.enabled());
        assert_eq!(disabled.plugin_id(), "example.optional");
        assert_eq!(disabled.instance(), "default");
        assert!(
            root.path()
                .join("plugins/example.optional/default.disabled")
                .is_file()
        );

        let enabled = authority
            .set_enabled(disabled.revision(), "example.optional", "default", true)
            .unwrap();
        assert!(enabled.enabled());
        assert!(
            !root
                .path()
                .join("plugins/example.optional/default.disabled")
                .exists()
        );
    }

    #[test]
    fn local_selection_authority_rejects_a_stale_revision_without_mutating() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lenso")).unwrap();
        let host = HostCatalog::new(
            [HostSlot::optional("optional")],
            [HostPluginRelease::new(PluginDescriptor::new(
                "example.optional",
                "1.0.0",
                "optional",
            ))],
            [HostDefaultPlugin::new("example.optional", "default").disableable()],
        );
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();
        let authority = LocalPluginRootAuthority::new(root.path());
        let stale = inspect_plugin_root(root.path()).unwrap().revision().clone();
        authority
            .set_enabled(&stale, "example.optional", "default", false)
            .unwrap();

        let error = authority
            .set_enabled(&stale, "example.optional", "default", true)
            .unwrap_err();

        assert!(error.downcast_ref::<PluginRootRevisionConflict>().is_some());
        assert!(
            root.path()
                .join("plugins/example.optional/default.disabled")
                .is_file()
        );
    }

    #[test]
    fn macos_metadata_at_plugin_root_is_ignored() {
        let root = fixture_root();
        fs::create_dir(root.path().join("plugins")).unwrap();
        fs::write(root.path().join("plugins/.DS_Store"), b"Finder metadata").unwrap();

        let resolved = load_resolved_app(root.path()).unwrap();

        assert_eq!(resolved.instances().len(), 1);
    }

    #[test]
    fn readers_reject_an_unresolved_transaction_guard() {
        let root = fixture_root();
        fs::create_dir_all(root.path().join("plugins")).unwrap();
        fs::write(root.path().join("plugins/.transaction"), "broken").unwrap();

        let error = load_resolved_app(root.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unresolved authoring transaction")
        );
    }

    #[test]
    fn macos_metadata_inside_plugin_directory_is_ignored() {
        let root = fixture_root();
        let plugin = root.path().join("plugins/example.agent");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(plugin.join(".DS_Store"), b"Finder metadata").unwrap();

        let resolved = load_resolved_app(root.path()).unwrap();

        assert_eq!(resolved.instances().len(), 1);
    }

    #[test]
    fn accepts_a_bounded_resource_directory_paired_with_an_instance() {
        let root = fixture_root();
        let plugin = root.path().join("plugins/example.agent");
        fs::create_dir_all(plugin.join("default/prompts")).unwrap();
        fs::write(plugin.join("default.toml"), "").unwrap();
        fs::write(plugin.join("default/prompts/system.md"), "hello").unwrap();
        fs::write(plugin.join("default/prompts/.DS_Store"), "metadata").unwrap();

        let resolved = load_resolved_app(root.path()).unwrap();

        assert!(
            resolved
                .instances()
                .iter()
                .any(|instance| instance.id().to_string() == "example.agent/default")
        );
    }

    #[test]
    fn rejects_an_orphan_resource_directory() {
        let root = fixture_root();
        let resources = root.path().join("plugins/example.agent/custom");
        fs::create_dir_all(&resources).unwrap();
        fs::write(resources.join("prompt.md"), "orphan").unwrap();

        let error = load_resolved_app(root.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("orphan Plugin resource directory")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_resource_symlink() {
        use std::os::unix::fs::symlink;

        let root = fixture_root();
        let plugin = root.path().join("plugins/example.agent");
        fs::create_dir_all(plugin.join("custom")).unwrap();
        fs::write(plugin.join("custom.toml"), "").unwrap();
        fs::write(root.path().join("secret"), "not admitted").unwrap();
        symlink(root.path().join("secret"), plugin.join("custom/secret")).unwrap();

        let error = load_resolved_app(root.path()).unwrap_err();

        assert!(error.to_string().contains("cannot contain symlinks"));
    }

    #[test]
    fn failed_configuration_candidate_does_not_write_the_plugin_root() {
        let root = fixture_root();

        let error = configure_instance(
            root.path(),
            "example.agent",
            "default",
            b"unexpected = true\n",
        )
        .unwrap_err();

        assert!(error.to_string().contains("non-empty configuration"));
        assert!(
            !root
                .path()
                .join("plugins/example.agent/default.toml")
                .exists()
        );
    }

    #[test]
    fn required_default_disable_fails_before_writing_a_marker() {
        let root = fixture_root();

        let error =
            set_instance_disabled(root.path(), "example.agent", "default", true).unwrap_err();

        assert!(error.to_string().contains("cannot be disabled"));
        assert!(
            !root
                .path()
                .join("plugins/example.agent/default.disabled")
                .exists()
        );
    }

    #[test]
    fn case_colliding_plugin_identities_fail_closed() {
        let mut normalized = BTreeMap::new();
        reject_case_collision(&mut normalized, "Example.Agent", "Plugin ID").unwrap();
        let error =
            reject_case_collision(&mut normalized, "example.agent", "Plugin ID").unwrap_err();

        assert!(error.to_string().contains("case-colliding Plugin IDs"));
    }

    #[test]
    fn add_replace_and_restore_publish_failures_leave_visible_bytes_unchanged() {
        for mutation in [
            BundleMutation::Add,
            BundleMutation::Replace,
            BundleMutation::Restore,
        ] {
            let root = tempfile::tempdir().unwrap();
            let destination = root
                .path()
                .join("plugins/example.agent/plugin.lenso-plugin");
            if mutation == BundleMutation::Add {
                fs::create_dir(root.path().join("plugins")).unwrap();
            } else {
                fs::create_dir_all(&destination).unwrap();
                fs::write(destination.join("marker"), "old").unwrap();
            }
            let staging = tempfile::tempdir_in(root.path()).unwrap();
            fs::write(staging.path().join("marker"), "new").unwrap();

            let error = commit_staged_bundle_with(
                &destination,
                mutation,
                staging,
                |_, _, _| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected publish failure",
                    ))
                },
                |_| panic!("retirement cannot run before publication succeeds"),
            )
            .unwrap_err();

            assert!(error.to_string().contains("Plugin Bundle"));
            if mutation == BundleMutation::Add {
                assert!(!destination.exists());
                assert!(!destination.parent().unwrap().exists());
            } else {
                assert_eq!(
                    fs::read_to_string(destination.join("marker")).unwrap(),
                    "old"
                );
            }
        }
    }

    #[test]
    fn portable_bundle_add_publishes_with_one_atomic_rename() {
        let root = tempfile::tempdir().unwrap();
        let destination = root
            .path()
            .join("plugins/example.agent/plugin.lenso-plugin");
        let staging = tempfile::tempdir_in(root.path()).unwrap();
        fs::write(staging.path().join("marker"), "new").unwrap();

        commit_staged_bundle(&destination, BundleMutation::Add, staging).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("marker")).unwrap(),
            "new"
        );
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
    #[test]
    fn portable_bundle_add_never_replaces_a_concurrent_destination() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("marker"), "old").unwrap();
        let staging = tempfile::tempdir_in(root.path()).unwrap();
        fs::write(staging.path().join("marker"), "new").unwrap();

        atomic_publish_bundle(staging.path(), &destination, BundleMutation::Add).unwrap_err();

        assert_eq!(
            fs::read_to_string(destination.join("marker")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(staging.path().join("marker")).unwrap(),
            "new"
        );
    }

    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    #[test]
    fn portable_bundle_replace_fails_closed_when_exchange_is_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let destination = root
            .path()
            .join("plugins/example.agent/plugin.lenso-plugin");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("marker"), "old").unwrap();
        let staging = tempfile::tempdir_in(root.path()).unwrap();
        fs::write(staging.path().join("marker"), "new").unwrap();

        let error =
            commit_staged_bundle(&destination, BundleMutation::Replace, staging).unwrap_err();

        assert_eq!(
            error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::Unsupported
        );
        assert_eq!(
            fs::read_to_string(destination.join("marker")).unwrap(),
            "old"
        );
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn replace_and_restore_commit_atomically_even_when_retirement_cleanup_fails() {
        for mutation in [BundleMutation::Replace, BundleMutation::Restore] {
            let root = tempfile::tempdir().unwrap();
            let destination = root
                .path()
                .join("plugins/example.agent/plugin.lenso-plugin");
            fs::create_dir_all(&destination).unwrap();
            fs::write(destination.join("marker"), "old").unwrap();
            let staging = tempfile::tempdir_in(root.path()).unwrap();
            fs::write(staging.path().join("marker"), "new").unwrap();
            let mut retired = None;

            commit_staged_bundle_with(
                &destination,
                mutation,
                staging,
                atomic_publish_bundle,
                |staging| {
                    retired = Some(staging.keep());
                    Err(std::io::Error::other("injected cleanup failure"))
                },
            )
            .unwrap();

            assert_eq!(
                fs::read_to_string(destination.join("marker")).unwrap(),
                "new"
            );
            let retired = retired.unwrap();
            assert_eq!(fs::read_to_string(retired.join("marker")).unwrap(), "old");
            fs::remove_dir_all(retired).unwrap();
        }
    }
}
