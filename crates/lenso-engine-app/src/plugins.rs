use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
pub use lenso_app_authoring::load_resolved_app;
use lenso_app_authoring::{
    BundleMutation, DEPENDENCY_SELECTIONS_SCHEMA_VERSION, DependencySelectionsDocument,
    PluginRootChangeSet, add_bundle, configure_instance, inspect_plugin_root,
    prepare_bundle_mutation, propose_plugin_root_changes, publish_plugin_root_changes,
    remove_instance_difference, remove_plugin, set_dependency_selection, set_instance_disabled,
};
use lenso_app_plan::authoring::PluginInstanceId;
use lenso_plugin_bundle::verify_bundle_directory;

use crate::archive::with_bundle_directory;
use crate::{
    archive::archive_bundle,
    catalog::{
        CatalogPluginRelease, DEFAULT_CATALOG_URL, download_bundle, fetch_catalog_release,
        search_catalog,
    },
};
use lenso_app_authoring::identity::{validate_plugin_id_v1, validate_release_version};

#[derive(Clone, Debug, Subcommand)]
pub enum PluginsCommand {
    /// List the Plugin Instances in the derived App.
    List(ProjectArgs),
    /// Add one exact Plugin Bundle after candidate validation.
    Add(AddArgs),
    /// Write direct configuration for one Plugin Instance.
    Configure(ConfigureArgs),
    /// Save one exact provider choice for a named Plugin dependency.
    Bind(BindArgs),
    /// Disable one Plugin Instance without deleting its configuration.
    Disable(InstanceArgs),
    /// Re-enable one disabled Plugin Instance.
    Enable(InstanceArgs),
    /// Remove one Instance difference or an entire root-supplied Plugin.
    Remove(RemoveArgs),
    /// Search immutable Plugin Releases in a catalog.
    Search(SearchArgs),
    /// Install one exact catalog Release.
    Install(CatalogMutationArgs),
    /// Replace a root Bundle with one exact catalog Release.
    Update(CatalogMutationArgs),
    /// List retained exact Releases available for rollback.
    History(HistoryArgs),
    /// Restore one retained exact Release.
    Rollback(RollbackArgs),
}

#[derive(Clone, Debug, Args)]
pub struct ProjectArgs {
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Emit a stable JSON report.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, Args)]
pub struct AddArgs {
    /// Exact `.lenso-plugin` Bundle archive or legacy Bundle directory.
    bundle: PathBuf,
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct ConfigureArgs {
    /// Exact Plugin ID.
    plugin_id: String,
    /// App-local Instance key.
    #[arg(default_value = "default")]
    instance: String,
    /// TOML file to use. Omit to create an empty configuration.
    #[arg(long)]
    file: Option<PathBuf>,
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct InstanceArgs {
    /// Exact Plugin ID.
    plugin_id: String,
    /// App-local Instance key.
    #[arg(default_value = "default")]
    instance: String,
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct BindArgs {
    /// Consumer Plugin ID.
    consumer_plugin_id: Option<String>,
    /// Consumer-local requirement identity.
    requirement: Option<String>,
    /// Provider Plugin ID. Omit together with `--absent` for an optional dependency.
    provider_plugin_id: Option<String>,
    /// Consumer App-local Instance key.
    #[arg(long, default_value = "default")]
    consumer_instance: String,
    /// Provider App-local Instance key.
    #[arg(long, default_value = "default", requires = "provider_plugin_id")]
    provider_instance: String,
    /// Preserve an explicit absence for an optional dependency.
    #[arg(long, conflicts_with = "provider_plugin_id")]
    absent: bool,
    /// JSON selection document to apply atomically when several requirements are ambiguous.
    #[arg(long)]
    file: Option<PathBuf>,
    /// Validate and display the complete choice migration without publishing it.
    #[arg(long, requires = "file")]
    preview: bool,
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct RemoveArgs {
    /// Exact Plugin ID.
    plugin_id: String,
    /// Remove only this Instance difference; omit to remove the whole Plugin directory.
    instance: Option<String>,
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct SearchArgs {
    /// Text matched against Plugin ID, summary, and Capability IDs.
    query: Option<String>,
    /// Plugin catalog endpoint.
    #[arg(long, default_value = DEFAULT_CATALOG_URL)]
    catalog: String,
    /// Emit a stable JSON report.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, Args)]
#[command(disable_version_flag = true)]
pub struct CatalogMutationArgs {
    /// Exact Plugin ID.
    plugin_id: String,
    /// Exact immutable Release version; no implicit latest selection is performed.
    #[arg(long)]
    version: String,
    /// Plugin catalog endpoint.
    #[arg(long, default_value = DEFAULT_CATALOG_URL)]
    catalog: String,
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct HistoryArgs {
    /// Exact Plugin ID.
    plugin_id: String,
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Emit a stable JSON report.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, Args)]
#[command(disable_version_flag = true)]
pub struct RollbackArgs {
    /// Exact Plugin ID.
    plugin_id: String,
    /// Exact retained Release version.
    #[arg(long)]
    version: String,
    /// App project root. Defaults to the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
}

pub fn plugins(command: PluginsCommand) -> anyhow::Result<()> {
    match command {
        PluginsCommand::List(args) => list(args),
        PluginsCommand::Add(args) => add(args),
        PluginsCommand::Configure(args) => configure(args),
        PluginsCommand::Bind(args) => bind(args),
        PluginsCommand::Disable(args) => disable(args),
        PluginsCommand::Enable(args) => enable(args),
        PluginsCommand::Remove(args) => remove(args),
        PluginsCommand::Search(args) => search(args),
        PluginsCommand::Install(args) => install(args, false),
        PluginsCommand::Update(args) => install(args, true),
        PluginsCommand::History(args) => history(args),
        PluginsCommand::Rollback(args) => rollback(args),
    }
}

pub fn project_root(root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    root.map_or_else(
        || env::current_dir().context("read current directory"),
        |root| {
            Ok(if root.is_absolute() {
                root
            } else {
                env::current_dir()?.join(root)
            })
        },
    )
}

fn list(args: ProjectArgs) -> anyhow::Result<()> {
    let root = project_root(args.root)?;
    let resolved = load_resolved_app(&root)?;
    if args.json {
        let instances = resolved
            .instances()
            .iter()
            .map(|instance| {
                serde_json::json!({
                    "id": instance.id().to_string(),
                    "source": format!("{:?}", instance.source()),
                    "plan_key": instance.plan_key(),
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "kind": "lenso.plugins-list",
                "instances": instances,
            }))?
        );
    } else {
        for instance in resolved.instances() {
            println!("{}\t{:?}", instance.id(), instance.source());
        }
    }
    Ok(())
}

fn add(args: AddArgs) -> anyhow::Result<()> {
    let bundle = args.bundle.clone();
    with_bundle_directory(&bundle, |directory| add_from_directory(args, directory))
}

fn add_from_directory(args: AddArgs, bundle: &Path) -> anyhow::Result<()> {
    let root = project_root(args.root)?;
    let (plugin_id, release_version, _) = add_bundle(&root, bundle)?;
    println!("Added Plugin `{plugin_id}` {release_version}.");
    Ok(())
}

fn configure(args: ConfigureArgs) -> anyhow::Result<()> {
    let root = project_root(args.root)?;
    let bytes = args.file.map_or_else(
        || Ok(Vec::new()),
        |path| fs::read(&path).with_context(|| format!("read {}", path.display())),
    )?;
    configure_instance(&root, &args.plugin_id, &args.instance, &bytes)?;
    let id = PluginInstanceId::new(&args.plugin_id, &args.instance);
    println!("Configured Plugin Instance `{id}`.");
    Ok(())
}

fn bind(args: BindArgs) -> anyhow::Result<()> {
    if let Some(file) = args.file {
        if args.consumer_plugin_id.is_some()
            || args.requirement.is_some()
            || args.provider_plugin_id.is_some()
            || args.absent
        {
            bail!("use either positional dependency selection arguments or `--file`");
        }
        let document: DependencySelectionsDocument = serde_json::from_slice(
            &fs::read(&file).with_context(|| format!("read {}", file.display()))?,
        )
        .with_context(|| format!("invalid dependency selections: {}", file.display()))?;
        if document.schema_version != DEPENDENCY_SELECTIONS_SCHEMA_VERSION {
            bail!(
                "unsupported dependency selection schema version `{}`",
                document.schema_version
            );
        }
        let root = project_root(args.root)?;
        let current = inspect_plugin_root(&root)?;
        let base = current.revision().clone();
        let changes = PluginRootChangeSet::new().with_dependency_choices(document.choices);
        let proposal = propose_plugin_root_changes(&root, &base, changes)?;
        for migration in proposal.requirement_migrations() {
            let targets = if migration.new_requirement_ids().is_empty() {
                "removed".to_owned()
            } else {
                migration.new_requirement_ids().join(", ")
            };
            let provider = migration
                .provider()
                .map_or("absent".to_owned(), ToString::to_string);
            println!(
                "{}/{} -> {targets}; provider {provider}",
                migration.consumer(),
                migration.old_requirement_id()
            );
        }
        let reviewed_choices = proposal
            .materialized_dependency_choices()
            .or_else(|| proposal.changes().dependency_choices())
            .unwrap_or_default();
        print_choice_migration(current.resolved().dependency_choices(), reviewed_choices);
        if args.preview {
            println!("Migration status: {:?}.", proposal.status());
            for diagnostic in proposal.diagnostics() {
                println!("{}: {}", diagnostic.code(), diagnostic.detail());
            }
            return Ok(());
        }
        let count = proposal
            .materialized_dependency_choices()
            .map_or(0, <[lenso_app_plan::authoring::DependencyChoice]>::len);
        publish_plugin_root_changes(&root, &proposal)?;
        println!("Applied {count} dependency selections.");
        return Ok(());
    }
    if !args.absent && args.provider_plugin_id.is_none() {
        bail!("provide a provider Plugin ID or use `--absent`");
    }
    let consumer_plugin_id = args
        .consumer_plugin_id
        .as_deref()
        .context("provide a consumer Plugin ID or use `--file`")?;
    let requirement = args
        .requirement
        .as_deref()
        .context("provide a requirement identity or use `--file`")?;
    let root = project_root(args.root)?;
    let consumer = PluginInstanceId::new(consumer_plugin_id, &args.consumer_instance);
    let provider = args
        .provider_plugin_id
        .as_deref()
        .map(|plugin_id| PluginInstanceId::new(plugin_id, &args.provider_instance));
    set_dependency_selection(&root, consumer.clone(), requirement, provider.clone())?;
    match provider {
        Some(provider) => {
            println!("Bound `{consumer}` requirement `{requirement}` to `{provider}`.");
        }
        None => println!("Left `{consumer}` optional requirement `{requirement}` absent."),
    }
    Ok(())
}

fn print_choice_migration(
    current: &[lenso_app_plan::authoring::DependencyChoice],
    candidate: &[lenso_app_plan::authoring::DependencyChoice],
) {
    let key = |choice: &lenso_app_plan::authoring::DependencyChoice| {
        (choice.consumer.to_string(), choice.requirement_id.clone())
    };
    let current = current
        .iter()
        .map(|choice| (key(choice), choice))
        .collect::<std::collections::BTreeMap<_, _>>();
    let candidate = candidate
        .iter()
        .map(|choice| (key(choice), choice))
        .collect::<std::collections::BTreeMap<_, _>>();
    for identity in current
        .keys()
        .chain(candidate.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let describe = |choice: Option<&&lenso_app_plan::authoring::DependencyChoice>,
                        missing: &str| {
            choice.map_or_else(
                || missing.to_owned(),
                |choice| {
                    choice
                        .provider
                        .as_ref()
                        .map_or("absent".to_owned(), ToString::to_string)
                },
            )
        };
        let before = describe(current.get(identity), "unset");
        let after = describe(candidate.get(identity), "removed");
        if before != after {
            println!("{}/{}: {before} -> {after}", identity.0, identity.1);
        }
    }
}

fn disable(args: InstanceArgs) -> anyhow::Result<()> {
    set_disabled(args, true)
}

fn enable(args: InstanceArgs) -> anyhow::Result<()> {
    set_disabled(args, false)
}

fn set_disabled(args: InstanceArgs, disabled_state: bool) -> anyhow::Result<()> {
    let root = project_root(args.root)?;
    set_instance_disabled(&root, &args.plugin_id, &args.instance, disabled_state)?;
    let id = PluginInstanceId::new(&args.plugin_id, &args.instance);
    if disabled_state {
        println!("Disabled Plugin Instance `{id}`.");
    } else {
        println!("Enabled Plugin Instance `{id}`.");
    }
    Ok(())
}

fn remove(args: RemoveArgs) -> anyhow::Result<()> {
    let root = project_root(args.root)?;
    if let Some(instance) = args.instance {
        remove_instance_difference(&root, &args.plugin_id, &instance)?;
        println!(
            "Removed Plugin Instance difference `{}/{instance}`.",
            args.plugin_id
        );
    } else {
        let (_, trash) = remove_plugin(&root, &args.plugin_id)?;
        println!(
            "Removed Plugin `{}`; recoverable at {}.",
            args.plugin_id,
            trash.display()
        );
    }
    Ok(())
}

fn search(args: SearchArgs) -> anyhow::Result<()> {
    let query = args.query.unwrap_or_default().to_lowercase();
    let mut releases = search_catalog(&args.catalog, &query)?
        .into_iter()
        .filter(|release| {
            query.is_empty()
                || release.plugin_id.to_lowercase().contains(&query)
                || release.summary.to_lowercase().contains(&query)
                || release
                    .capabilities
                    .iter()
                    .any(|capability| capability.to_lowercase().contains(&query))
        })
        .collect::<Vec<_>>();
    releases.sort_by(|left, right| {
        (&left.plugin_id, &left.version).cmp(&(&right.plugin_id, &right.version))
    });
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "kind": "lenso.plugins-search",
                "catalog": args.catalog,
                "releases": releases.iter().map(catalog_release_json).collect::<Vec<_>>(),
            }))?
        );
    } else if releases.is_empty() {
        println!("No installable Plugin Releases matched.");
    } else {
        for release in releases {
            println!(
                "{}@{}\t{}",
                release.plugin_id, release.version, release.summary
            );
        }
    }
    Ok(())
}

fn install(args: CatalogMutationArgs, replace: bool) -> anyhow::Result<()> {
    validate_plugin_id_v1(&args.plugin_id)?;
    validate_release_version(&args.version)?;
    let root = project_root(args.root)?;
    let matches = fetch_catalog_release(&args.catalog, &args.plugin_id, &args.version)?
        .into_iter()
        .filter(|release| release.plugin_id == args.plugin_id && release.version == args.version)
        .collect::<Vec<_>>();
    let [release] = matches.as_slice() else {
        bail!(
            "catalog must contain exactly one `{}` Release at version `{}`; found {}",
            args.plugin_id,
            args.version,
            matches.len()
        );
    };
    let mut temporary = tempfile::NamedTempFile::new().context("stage catalog Plugin Bundle")?;
    download_bundle(release, temporary.as_file_mut())?;
    with_bundle_directory(temporary.path(), |bundle| {
        install_catalog_bundle(&root, release, bundle, temporary.path(), replace)
    })?;
    println!(
        "{} Plugin `{}` {}.",
        if replace { "Updated" } else { "Installed" },
        release.plugin_id,
        release.version
    );
    Ok(())
}

fn install_catalog_bundle(
    root: &Path,
    release: &CatalogPluginRelease,
    bundle: &Path,
    archive: &Path,
    replace: bool,
) -> anyhow::Result<()> {
    let mutation = if replace {
        BundleMutation::Replace
    } else {
        BundleMutation::Add
    };
    let prepared = prepare_bundle_mutation(root, bundle, mutation)?;
    let verified = prepared.verified();
    if verified.plugin_id != release.plugin_id || verified.release_version != release.version {
        bail!("catalog identity does not match the verified Plugin Bundle");
    }
    if verified.manifest_digest != release.manifest_digest {
        bail!("catalog manifest digest does not match the verified Plugin Bundle");
    }
    if replace {
        retain_current_bundle(root, &release.plugin_id, prepared.destination())?;
    }
    retain_archive(root, &release.plugin_id, &release.version, archive)?;
    prepared.commit()?;
    Ok(())
}

fn history(args: HistoryArgs) -> anyhow::Result<()> {
    validate_existing_cli_plugin_id(&args.plugin_id)?;
    let root = project_root(args.root)?;
    let directory = plugin_store(&root, &args.plugin_id);
    let mut versions = match fs::read_dir(&directory) {
        Ok(entries) => entries
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|entry| {
                entry
                    .path()
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    versions.sort();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "kind": "lenso.plugins-history",
                "plugin_id": args.plugin_id,
                "versions": versions,
            }))?
        );
    } else if versions.is_empty() {
        println!("No retained Releases for `{}`.", args.plugin_id);
    } else {
        for version in versions {
            println!("{}@{}", args.plugin_id, version);
        }
    }
    Ok(())
}

fn rollback(args: RollbackArgs) -> anyhow::Result<()> {
    validate_existing_cli_plugin_id(&args.plugin_id)?;
    validate_release_version(&args.version)?;
    let root = project_root(args.root)?;
    let archive =
        plugin_store(&root, &args.plugin_id).join(format!("{}.lenso-plugin", args.version));
    if !archive.is_file() {
        bail!(
            "retained Plugin Release is unavailable: {}",
            archive.display()
        );
    }
    with_bundle_directory(&archive, |bundle| {
        let prepared = prepare_bundle_mutation(&root, bundle, BundleMutation::Restore)?;
        let verified = prepared.verified();
        if verified.plugin_id != args.plugin_id || verified.release_version != args.version {
            bail!("retained Plugin Release identity is invalid");
        }
        retain_current_bundle(&root, &args.plugin_id, prepared.destination())?;
        prepared.commit()?;
        Ok(())
    })?;
    println!(
        "Rolled back Plugin `{}` to {}.",
        args.plugin_id, args.version
    );
    Ok(())
}

fn catalog_release_json(release: &CatalogPluginRelease) -> serde_json::Value {
    serde_json::json!({
        "plugin_id": release.plugin_id,
        "version": release.version,
        "summary": release.summary,
        "bundle_url": release.bundle_url,
        "bundle_digest": release.bundle_digest,
        "manifest_digest": release.manifest_digest,
        "host_targets": release.host_targets,
        "execution_classes": release.execution_classes,
        "capabilities": release.capabilities,
    })
}

fn plugin_store(root: &Path, plugin_id: &str) -> PathBuf {
    root.join(".lenso/plugin-store").join(plugin_id)
}

fn retain_archive(
    root: &Path,
    plugin_id: &str,
    version: &str,
    archive: &Path,
) -> anyhow::Result<()> {
    let destination = plugin_store(root, plugin_id).join(format!("{version}.lenso-plugin"));
    fs::create_dir_all(destination.parent().expect("store path has a parent"))?;
    if destination.exists() {
        if !archives_equal(&destination, archive)? {
            bail!("retained Plugin Release `{plugin_id}@{version}` has different immutable bytes");
        }
        return Ok(());
    }
    fs::copy(archive, &destination)?;
    Ok(())
}

fn archives_equal(left: &Path, right: &Path) -> anyhow::Result<bool> {
    let length = fs::metadata(left)?.len();
    if length != fs::metadata(right)?.len() {
        return Ok(false);
    }
    readers_equal_exact(fs::File::open(left)?, fs::File::open(right)?, length)
}

fn readers_equal_exact(
    mut left: impl Read,
    mut right: impl Read,
    mut remaining: u64,
) -> anyhow::Result<bool> {
    let mut left_buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    let mut right_buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    while remaining != 0 {
        let chunk = usize::try_from(remaining.min(left_buffer.len() as u64))?;
        left.read_exact(&mut left_buffer[..chunk])?;
        right.read_exact(&mut right_buffer[..chunk])?;
        if left_buffer[..chunk] != right_buffer[..chunk] {
            return Ok(false);
        }
        remaining -= u64::try_from(chunk)?;
    }
    let mut left_extra = [0_u8; 1];
    let mut right_extra = [0_u8; 1];
    Ok(left.read(&mut left_extra)? == 0 && right.read(&mut right_extra)? == 0)
}

fn retain_current_bundle(root: &Path, plugin_id: &str, bundle: &Path) -> anyhow::Result<()> {
    if !bundle.is_dir() {
        bail!("current Plugin Bundle is unavailable: {}", bundle.display());
    }
    let verified = verify_bundle_directory(bundle)?;
    let staging = tempfile::tempdir().context("stage retained Plugin Release")?;
    let archive = staging.path().join("current.lenso-plugin");
    archive_bundle(bundle, &archive)?;
    retain_archive(root, plugin_id, &verified.release_version, &archive)
}

fn validate_existing_cli_plugin_id(plugin_id: &str) -> anyhow::Result<()> {
    lenso_app_authoring::identity::classify_existing_plugin_id(plugin_id).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use super::{archives_equal, readers_equal_exact};

    struct ShortReader {
        inner: Cursor<Vec<u8>>,
        maximum: usize,
    }

    impl Read for ShortReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let length = buffer.len().min(self.maximum);
            self.inner.read(&mut buffer[..length])
        }
    }

    #[test]
    fn immutable_archive_comparison_is_exact() {
        let directory = tempfile::tempdir().unwrap();
        let left = directory.path().join("left.bundle");
        let right = directory.path().join("right.bundle");
        std::fs::write(&left, vec![b'a'; 128 * 1024]).unwrap();
        std::fs::write(&right, vec![b'a'; 128 * 1024]).unwrap();
        assert!(archives_equal(&left, &right).unwrap());

        std::fs::write(&right, vec![b'a'; 128 * 1024 - 1]).unwrap();
        assert!(!archives_equal(&left, &right).unwrap());
        let mut changed = vec![b'a'; 128 * 1024];
        changed[96 * 1024] = b'b';
        std::fs::write(&right, changed).unwrap();
        assert!(!archives_equal(&left, &right).unwrap());
    }

    #[test]
    fn archive_comparison_tolerates_different_short_read_boundaries() {
        let bytes = vec![b'a'; 128 * 1024 + 17];
        let left = ShortReader {
            inner: Cursor::new(bytes.clone()),
            maximum: 7,
        };
        let right = ShortReader {
            inner: Cursor::new(bytes.clone()),
            maximum: 113,
        };

        assert!(readers_equal_exact(left, right, bytes.len() as u64).unwrap());
    }
}
