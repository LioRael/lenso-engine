use std::{
    error::Error,
    fmt,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    str,
    str::FromStr,
    sync::{Arc, Mutex},
};

use crate::host_authoring::{HOST_BUILD, HostInput};
use anyhow::{Context, bail};
use lenso_app_plan::authoring::{
    DependencyChoice, PluginInstanceId, PluginRootInstance, PluginRootResolutionError,
    PluginRootSnapshot, ResolvedApp,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{
    DEPENDENCY_SELECTIONS, DEPENDENCY_SELECTIONS_SCHEMA_VERSION, DependencySelectionsDocument,
    HOST_CATALOG, LEGACY_DEPENDENCY_SELECTIONS, MAX_CONFIGURATION_BYTES, PLUGIN_ROOT,
    PluginRootAuthoringState, inspect_plugin_root, load_host_catalog, lock_plugin_root,
    root_transaction, snapshot_plugin_root, validate_existing_plugin_id,
    validate_instance_filename, validate_requirement_id,
};

const PROPOSAL_SCHEMA: &str = "lenso.plugin-configuration-proposal.v1";
const PUBLICATION_SCHEMA: &str = "lenso.plugin-configuration-publication.v1";
const SOURCE_DIGEST_SCHEMA: &str = "lenso.plugin-configuration-source.v1";
const ROOT_CHANGE_PROPOSAL_SCHEMA: &str = "lenso.plugin-root-change-proposal.v1";
const ROOT_CHANGE_PUBLICATION_SCHEMA: &str = "lenso.plugin-root-change-publication.v1";
const ROOT_SOURCE_DIGEST_SCHEMA: &str = "lenso.plugin-root-source.v1";

/// Stable provenance for the authority that owns Plugin configuration publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginConfigurationAuthoritySource {
    kind: String,
    reference: String,
}

impl PluginConfigurationAuthoritySource {
    /// Creates one Host-trusted authority identity.
    pub fn new(kind: impl Into<String>, reference: impl Into<String>) -> anyhow::Result<Self> {
        let kind = kind.into();
        let reference = reference.into();
        if kind.is_empty()
            || kind.len() > 64
            || !kind.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'-' | b'.')
            })
        {
            bail!("Plugin configuration authority kind is invalid");
        }
        if reference.is_empty() || reference.len() > 256 || reference.chars().any(char::is_control)
        {
            bail!("Plugin configuration authority reference is invalid");
        }
        Ok(Self { kind, reference })
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn reference(&self) -> &str {
        &self.reference
    }
}

/// Host-side port for inspecting, proposing, and publishing Plugin configuration.
///
/// Implementations own authoring storage and compare-and-swap publication. They
/// do not own App Generation staging, routing, or Kernel execution.
pub trait PluginConfigurationAuthority: fmt::Debug + Send + Sync {
    fn source(&self) -> PluginConfigurationAuthoritySource;

    fn inspect(&self) -> anyhow::Result<PluginRootAuthoringState>;

    fn propose(
        &self,
        expected_revision: &PluginRootRevision,
        plugin_id: &str,
        instance: &str,
        bytes: &[u8],
    ) -> anyhow::Result<PluginConfigurationProposal>;

    fn publish(
        &self,
        proposal: &PluginConfigurationProposal,
    ) -> anyhow::Result<PluginConfigurationPublication>;

    fn propose_changes(
        &self,
        expected_revision: &PluginRootRevision,
        changes: PluginRootChangeSet,
    ) -> anyhow::Result<PluginRootChangeProposal>;

    fn publish_changes(
        &self,
        proposal: &PluginRootChangeProposal,
    ) -> anyhow::Result<PluginRootChangePublication>;
}

/// Default configuration authority backed by one visible App Plugin Root.
#[derive(Clone, Debug)]
pub struct LocalPluginRootAuthority {
    root: PathBuf,
    access: Arc<Mutex<()>>,
}

impl LocalPluginRootAuthority {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            access: Arc::new(Mutex::new(())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, ()>> {
        self.access
            .lock()
            .map_err(|_| anyhow::anyhow!("Plugin configuration authority lock is poisoned"))
    }
}

impl PluginConfigurationAuthority for LocalPluginRootAuthority {
    fn source(&self) -> PluginConfigurationAuthoritySource {
        PluginConfigurationAuthoritySource {
            kind: "local_plugin_root".to_owned(),
            reference: "app".to_owned(),
        }
    }

    fn inspect(&self) -> anyhow::Result<PluginRootAuthoringState> {
        let _guard = self.lock()?;
        inspect_plugin_root(&self.root)
    }

    fn propose(
        &self,
        expected_revision: &PluginRootRevision,
        plugin_id: &str,
        instance: &str,
        bytes: &[u8],
    ) -> anyhow::Result<PluginConfigurationProposal> {
        let _guard = self.lock()?;
        propose_instance_configuration(&self.root, expected_revision, plugin_id, instance, bytes)
    }

    fn publish(
        &self,
        proposal: &PluginConfigurationProposal,
    ) -> anyhow::Result<PluginConfigurationPublication> {
        let _guard = self.lock()?;
        publish_instance_configuration(&self.root, proposal)
    }

    fn propose_changes(
        &self,
        expected_revision: &PluginRootRevision,
        changes: PluginRootChangeSet,
    ) -> anyhow::Result<PluginRootChangeProposal> {
        let _guard = self.lock()?;
        propose_plugin_root_changes(&self.root, expected_revision, changes)
    }

    fn publish_changes(
        &self,
        proposal: &PluginRootChangeProposal,
    ) -> anyhow::Result<PluginRootChangePublication> {
        let _guard = self.lock()?;
        publish_plugin_root_changes(&self.root, proposal)
    }
}

/// A deterministic semantic revision of one App's Plugin Root.
///
/// Formatting and comments in TOML do not change this revision. The parsed
/// desired state is the authority used for compare-and-swap publication.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PluginRootRevision(String);

impl PluginRootRevision {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginRootRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for PluginRootRevision {
    type Err = PluginRootRevisionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(digest) = value.strip_prefix("sha256:") else {
            return Err(PluginRootRevisionParseError);
        };
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PluginRootRevisionParseError);
        }
        Ok(Self(format!("sha256:{}", digest.to_ascii_lowercase())))
    }
}

/// Text did not contain one complete SHA-256 Plugin Root revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginRootRevisionParseError;

impl fmt::Display for PluginRootRevisionParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Plugin Root revision must be `sha256:` followed by 64 hex digits")
    }
}

impl Error for PluginRootRevisionParseError {}

/// A proposal was published against a Plugin Root that has since changed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginRootRevisionConflict {
    expected: PluginRootRevision,
    current: PluginRootRevision,
}

impl PluginRootRevisionConflict {
    pub const fn expected(&self) -> &PluginRootRevision {
        &self.expected
    }

    pub const fn current(&self) -> &PluginRootRevision {
        &self.current
    }
}

impl fmt::Display for PluginRootRevisionConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Plugin Root revision conflict: expected {}, current {}",
            self.expected, self.current
        )
    }
}

impl Error for PluginRootRevisionConflict {}

/// One exact TOML source to include in a coordinated Plugin Root change.
#[derive(Clone, Debug, Serialize)]
pub struct PluginRootConfigurationChange {
    plugin_id: String,
    instance_key: String,
    toml: Vec<u8>,
}

impl PluginRootConfigurationChange {
    pub fn new(
        plugin_id: impl Into<String>,
        instance: impl Into<String>,
        toml: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            instance_key: instance.into(),
            toml: toml.into(),
        }
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn instance_key(&self) -> &str {
        &self.instance_key
    }

    pub fn toml(&self) -> &[u8] {
        &self.toml
    }
}

/// A complete change request that can coordinate multiple configurations and choices.
#[derive(Clone, Debug, Default)]
pub struct PluginRootChangeSet {
    configurations: Vec<PluginRootConfigurationChange>,
    dependency_choices: Option<Vec<DependencyChoice>>,
}

impl PluginRootChangeSet {
    pub const fn new() -> Self {
        Self {
            configurations: Vec::new(),
            dependency_choices: None,
        }
    }

    #[must_use]
    pub fn with_configuration(mut self, change: PluginRootConfigurationChange) -> Self {
        self.configurations.push(change);
        self
    }

    /// Replaces the complete persisted choice set. An empty list adopts explicit empty intent.
    #[must_use]
    pub fn with_dependency_choices(
        mut self,
        choices: impl IntoIterator<Item = DependencyChoice>,
    ) -> Self {
        self.dependency_choices = Some(choices.into_iter().collect());
        self
    }

    pub fn configurations(&self) -> &[PluginRootConfigurationChange] {
        &self.configurations
    }

    pub fn dependency_choices(&self) -> Option<&[DependencyChoice]> {
        self.dependency_choices.as_deref()
    }
}

/// Digest of one exact Plugin Root source path, including absence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginRootSourceDigest {
    path: String,
    digest: String,
}

impl PluginRootSourceDigest {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// Exact old-to-new public requirement identity mapping in a reviewed migration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PluginRequirementMigration {
    consumer: PluginInstanceId,
    old_requirement_id: String,
    new_requirement_ids: Vec<String>,
    provider: Option<PluginInstanceId>,
}

impl PluginRequirementMigration {
    pub const fn consumer(&self) -> &PluginInstanceId {
        &self.consumer
    }

    pub fn old_requirement_id(&self) -> &str {
        &self.old_requirement_id
    }

    pub fn new_requirement_ids(&self) -> &[String] {
        &self.new_requirement_ids
    }

    pub const fn provider(&self) -> Option<&PluginInstanceId> {
        self.provider.as_ref()
    }
}

/// Immutable review evidence for one coordinated Plugin Root publication.
#[derive(Clone, Debug)]
pub struct PluginRootChangeProposal {
    schema: &'static str,
    base_revision: PluginRootRevision,
    host_catalog_digest: String,
    source_digests: Vec<PluginRootSourceDigest>,
    candidate_revision: PluginRootRevision,
    digest: String,
    status: PluginConfigurationProposalStatus,
    application: PluginConfigurationApplication,
    diagnostics: Vec<PluginConfigurationDiagnostic>,
    requirement_migrations: Vec<PluginRequirementMigration>,
    changes: PluginRootChangeSet,
    materialized_choices: Option<Vec<DependencyChoice>>,
}

impl PluginRootChangeProposal {
    pub const fn schema(&self) -> &str {
        self.schema
    }

    pub const fn base_revision(&self) -> &PluginRootRevision {
        &self.base_revision
    }

    pub fn host_catalog_digest(&self) -> &str {
        &self.host_catalog_digest
    }

    pub fn source_digests(&self) -> &[PluginRootSourceDigest] {
        &self.source_digests
    }

    pub const fn candidate_revision(&self) -> &PluginRootRevision {
        &self.candidate_revision
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub const fn status(&self) -> PluginConfigurationProposalStatus {
        self.status
    }

    pub const fn application(&self) -> PluginConfigurationApplication {
        self.application
    }

    pub fn diagnostics(&self) -> &[PluginConfigurationDiagnostic] {
        &self.diagnostics
    }

    pub fn requirement_migrations(&self) -> &[PluginRequirementMigration] {
        &self.requirement_migrations
    }

    pub const fn changes(&self) -> &PluginRootChangeSet {
        &self.changes
    }

    pub fn materialized_dependency_choices(&self) -> Option<&[DependencyChoice]> {
        self.materialized_choices.as_deref()
    }
}

/// Evidence returned after one coordinated Plugin Root transaction commits.
#[derive(Clone, Debug)]
pub struct PluginRootChangePublication {
    schema: &'static str,
    base_revision: PluginRootRevision,
    revision: PluginRootRevision,
    proposal_digest: String,
    resolved: ResolvedApp,
}

impl PluginRootChangePublication {
    pub const fn schema(&self) -> &str {
        self.schema
    }

    pub const fn base_revision(&self) -> &PluginRootRevision {
        &self.base_revision
    }

    pub const fn revision(&self) -> &PluginRootRevision {
        &self.revision
    }

    pub fn proposal_digest(&self) -> &str {
        &self.proposal_digest
    }

    pub const fn resolved(&self) -> &ResolvedApp {
        &self.resolved
    }

    pub fn into_resolved(self) -> ResolvedApp {
        self.resolved
    }
}

/// Read-only review evidence for one exact Plugin Instance configuration change.
#[derive(Clone, Debug)]
pub struct PluginConfigurationProposal {
    schema: &'static str,
    base_revision: PluginRootRevision,
    base_source_digest: PluginConfigurationSourceDigest,
    candidate_revision: PluginRootRevision,
    digest: String,
    status: PluginConfigurationProposalStatus,
    application: PluginConfigurationApplication,
    diagnostics: Vec<PluginConfigurationDiagnostic>,
    plugin_id: String,
    instance_key: String,
    toml: Vec<u8>,
    root_proposal: PluginRootChangeProposal,
}

impl PluginConfigurationProposal {
    pub const fn schema(&self) -> &str {
        self.schema
    }

    pub const fn base_revision(&self) -> &PluginRootRevision {
        &self.base_revision
    }

    pub const fn base_source_digest(&self) -> &PluginConfigurationSourceDigest {
        &self.base_source_digest
    }

    pub const fn candidate_revision(&self) -> &PluginRootRevision {
        &self.candidate_revision
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub const fn status(&self) -> PluginConfigurationProposalStatus {
        self.status
    }

    pub const fn application(&self) -> PluginConfigurationApplication {
        self.application
    }

    pub fn diagnostics(&self) -> &[PluginConfigurationDiagnostic] {
        &self.diagnostics
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn instance_key(&self) -> &str {
        &self.instance_key
    }
}

/// Whether a configuration proposal can proceed to publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginConfigurationProposalStatus {
    Ready,
    NeedsDecision,
    Rejected,
}

/// The Host action required after publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginConfigurationApplication {
    Noop,
    AppGeneration,
    Blocked,
}

/// One stable reason a configuration proposal cannot proceed automatically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginConfigurationDiagnostic {
    code: &'static str,
    detail: String,
}

impl PluginConfigurationDiagnostic {
    pub const fn code(&self) -> &str {
        self.code
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Evidence returned after compare-and-swap publication succeeds.
#[derive(Clone, Debug)]
pub struct PluginConfigurationPublication {
    schema: &'static str,
    base_revision: PluginRootRevision,
    base_source_digest: PluginConfigurationSourceDigest,
    revision: PluginRootRevision,
    proposal_digest: String,
    resolved: ResolvedApp,
}

impl PluginConfigurationPublication {
    pub const fn schema(&self) -> &str {
        self.schema
    }

    pub const fn base_revision(&self) -> &PluginRootRevision {
        &self.base_revision
    }

    pub const fn base_source_digest(&self) -> &PluginConfigurationSourceDigest {
        &self.base_source_digest
    }

    pub const fn revision(&self) -> &PluginRootRevision {
        &self.revision
    }

    pub fn proposal_digest(&self) -> &str {
        &self.proposal_digest
    }

    pub const fn resolved(&self) -> &ResolvedApp {
        &self.resolved
    }

    pub fn into_resolved(self) -> ResolvedApp {
        self.resolved
    }
}

/// Builds review evidence for a coordinated set of Plugin Root changes without publishing it.
pub fn propose_plugin_root_changes(
    root: &Path,
    expected_revision: &PluginRootRevision,
    changes: PluginRootChangeSet,
) -> anyhow::Result<PluginRootChangeProposal> {
    let _lock = lock_plugin_root(root)?;
    let host = load_host_catalog(root)?;
    let current = snapshot_plugin_root(root, &host)?;
    let current_revision = revision_for_snapshot(&current)?;
    ensure_revision(expected_revision, &current_revision)?;
    build_root_change_proposal(root, &host, &current, current_revision, changes)
}

/// Publishes one reviewed multi-file change with revision, source, and Host fencing.
pub fn publish_plugin_root_changes(
    root: &Path,
    proposal: &PluginRootChangeProposal,
) -> anyhow::Result<PluginRootChangePublication> {
    let _lock = lock_plugin_root(root)?;
    let host = load_host_catalog(root)?;
    let current = snapshot_plugin_root(root, &host)?;
    let current_revision = revision_for_snapshot(&current)?;
    ensure_revision(&proposal.base_revision, &current_revision)?;
    let current_host_digest = host_catalog_digest(root)?;
    if current_host_digest != proposal.host_catalog_digest {
        bail!("Host Catalog changed after the Plugin Root proposal was reviewed");
    }
    let current_sources = source_digests_for_changes(root, &proposal.changes)?;
    if current_sources != proposal.source_digests {
        bail!("Plugin Root source bytes changed after the proposal was reviewed");
    }
    let verified = build_root_change_proposal(
        root,
        &host,
        &current,
        current_revision,
        proposal.changes.clone(),
    )?;
    if proposal.candidate_revision != verified.candidate_revision
        || proposal.digest != verified.digest
        || proposal.materialized_choices != verified.materialized_choices
    {
        bail!("Plugin Root proposal no longer matches its reviewed candidate");
    }
    ensure_ready(
        verified.status,
        verified.application,
        &verified.diagnostics,
        "Plugin Root proposal",
    )?;

    let mut files = verified
        .changes
        .configurations
        .iter()
        .map(|change| {
            root_transaction::RootFileChange::write(
                PathBuf::from(&change.plugin_id).join(format!("{}.toml", change.instance_key)),
                change.toml.clone(),
            )
        })
        .collect::<Vec<_>>();
    if let Some(choices) = &verified.materialized_choices {
        let document = DependencySelectionsDocument {
            schema_version: DEPENDENCY_SELECTIONS_SCHEMA_VERSION,
            choices: choices.clone(),
        };
        files.push(root_transaction::RootFileChange::write(
            DEPENDENCY_SELECTIONS,
            serde_json::to_vec_pretty(&document).context("encode dependency selections")?,
        ));
        files.push(root_transaction::RootFileChange::remove(
            LEGACY_DEPENDENCY_SELECTIONS,
        ));
    }
    root_transaction::publish_root_files(root, files)?;

    let published = snapshot_plugin_root(root, &host)?;
    let revision = revision_for_snapshot(&published)?;
    if revision != proposal.candidate_revision {
        bail!("published Plugin Root does not match the reviewed candidate revision");
    }
    let resolved = host.resolve(&published).map_err(anyhow::Error::msg)?;
    Ok(PluginRootChangePublication {
        schema: ROOT_CHANGE_PUBLICATION_SCHEMA,
        base_revision: proposal.base_revision.clone(),
        revision,
        proposal_digest: proposal.digest.clone(),
        resolved,
    })
}

fn candidate_configuration_instances(
    current: &PluginRootSnapshot,
    changes: &PluginRootChangeSet,
) -> anyhow::Result<Vec<PluginRootInstance>> {
    let mut instances = current.instances().to_vec();
    for change in &changes.configurations {
        let id = PluginInstanceId::new(&change.plugin_id, &change.instance_key);
        let instance = instances
            .iter()
            .find(|instance| instance.id() == &id)
            .cloned()
            .unwrap_or_else(|| PluginRootInstance::new(&change.plugin_id, &change.instance_key));
        instances.retain(|instance| instance.id() != &id);
        instances.push(instance.with_configuration(parse_configuration(&change.toml)?));
    }
    instances.sort_by(|left, right| left.id().cmp(right.id()));
    Ok(instances)
}

fn build_root_change_proposal(
    root: &Path,
    host: &HostInput,
    current: &PluginRootSnapshot,
    base_revision: PluginRootRevision,
    changes: PluginRootChangeSet,
) -> anyhow::Result<PluginRootChangeProposal> {
    let changes = normalize_change_set(changes)?;
    let instances = candidate_configuration_instances(current, &changes)?;
    let mut candidate = PluginRootSnapshot::new(
        current.releases().iter().cloned(),
        instances,
        current.disabled().iter().cloned(),
    );
    candidate = match &changes.dependency_choices {
        Some(choices) => candidate.with_dependency_choices(choices.clone()),
        None => crate::preserve_dependency_selections(candidate, current),
    };

    let mut materialized_choices = None;
    let resolution = if changes.dependency_choices.is_some() {
        match host.propose(&candidate) {
            Ok(proposed) => {
                let choices = proposed.dependency_choices().to_vec();
                candidate = PluginRootSnapshot::new(
                    candidate.releases().iter().cloned(),
                    candidate.instances().iter().cloned(),
                    candidate.disabled().iter().cloned(),
                )
                .with_dependency_choices(choices.clone());
                materialized_choices = Some(choices);
                host.resolve(&candidate)
            }
            Err(error) => Err(error),
        }
    } else {
        host.resolve(&candidate)
    };
    if let Some(choices) = &materialized_choices {
        validate_dependency_choices(choices)?;
        let document = DependencySelectionsDocument {
            schema_version: DEPENDENCY_SELECTIONS_SCHEMA_VERSION,
            choices: choices.clone(),
        };
        let bytes = serde_json::to_vec(&document).context("encode dependency selections")?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > super::MAX_DEPENDENCY_SELECTION_BYTES {
            bail!("Plugin dependency selections exceed 1 MiB");
        }
    }
    let candidate_revision = revision_for_snapshot(&candidate)?;
    let (mut status, mut application, mut diagnostics) =
        classify_resolution(resolution, &candidate_revision, &base_revision);
    let requirement_migrations = materialized_choices
        .as_ref()
        .map_or_else(Vec::new, |choices| {
            requirement_migrations(current.dependency_choices(), choices)
        });
    if changes
        .dependency_choices
        .as_ref()
        .is_some_and(|requested| has_unreviewed_split(&requirement_migrations, requested))
    {
        status = PluginConfigurationProposalStatus::NeedsDecision;
        application = PluginConfigurationApplication::Blocked;
        diagnostics.push(PluginConfigurationDiagnostic {
            code: "migration_mapping_required",
            detail: "a split requirement migration needs every new requirement mapped explicitly"
                .to_owned(),
        });
    }
    let host_catalog_digest = host_catalog_digest(root)?;
    let source_digests = source_digests_for_changes(root, &changes)?;
    let authority = serde_json::to_vec(&(
        ROOT_CHANGE_PROPOSAL_SCHEMA,
        base_revision.as_str(),
        &host_catalog_digest,
        source_digests
            .iter()
            .map(|source| (&source.path, &source.digest))
            .collect::<Vec<_>>(),
        candidate_revision.as_str(),
        &changes.configurations,
        &materialized_choices,
        &requirement_migrations,
    ))
    .context("encode Plugin Root proposal authority")?;
    Ok(PluginRootChangeProposal {
        schema: ROOT_CHANGE_PROPOSAL_SCHEMA,
        base_revision,
        host_catalog_digest,
        source_digests,
        candidate_revision,
        digest: sha256_digest(&authority),
        status,
        application,
        diagnostics,
        requirement_migrations,
        changes,
        materialized_choices,
    })
}

fn has_unreviewed_split(
    migrations: &[PluginRequirementMigration],
    requested: &[DependencyChoice],
) -> bool {
    let requested_keys = requested
        .iter()
        .map(|choice| (&choice.consumer, choice.requirement_id.as_str()))
        .collect::<std::collections::BTreeSet<_>>();
    migrations.iter().any(|migration| {
        migration.new_requirement_ids.len() > 1
            && migration.new_requirement_ids.iter().any(|requirement_id| {
                !requested_keys.contains(&(&migration.consumer, requirement_id.as_str()))
            })
    })
}

fn normalize_change_set(mut changes: PluginRootChangeSet) -> anyhow::Result<PluginRootChangeSet> {
    if changes.configurations.is_empty() && changes.dependency_choices.is_none() {
        bail!("Plugin Root proposal must contain at least one change");
    }
    let mut identities = std::collections::BTreeSet::new();
    for change in &changes.configurations {
        validate_existing_plugin_id(&change.plugin_id)?;
        validate_instance_filename(&change.instance_key)?;
        parse_configuration(&change.toml)?;
        if !identities.insert((change.plugin_id.clone(), change.instance_key.clone())) {
            bail!(
                "duplicate Plugin configuration change for `{}/{}`",
                change.plugin_id,
                change.instance_key
            );
        }
    }
    changes.configurations.sort_by(|left, right| {
        left.plugin_id
            .cmp(&right.plugin_id)
            .then_with(|| left.instance_key.cmp(&right.instance_key))
    });
    if let Some(choices) = &mut changes.dependency_choices {
        validate_dependency_choices(choices)?;
        choices.sort_by(|left, right| {
            left.consumer
                .cmp(&right.consumer)
                .then_with(|| left.requirement_id.cmp(&right.requirement_id))
        });
    }
    Ok(changes)
}

fn requirement_migrations(
    current: &[DependencyChoice],
    candidate: &[DependencyChoice],
) -> Vec<PluginRequirementMigration> {
    let current_keys = current
        .iter()
        .map(|choice| (&choice.consumer, choice.requirement_id.as_str()))
        .collect::<std::collections::BTreeSet<_>>();
    let candidate_keys = candidate
        .iter()
        .map(|choice| (&choice.consumer, choice.requirement_id.as_str()))
        .collect::<std::collections::BTreeSet<_>>();
    current
        .iter()
        .filter(|choice| {
            !candidate_keys.contains(&(&choice.consumer, choice.requirement_id.as_str()))
        })
        .map(|old| {
            let mut new_requirement_ids = candidate
                .iter()
                .filter(|new| {
                    !current_keys.contains(&(&new.consumer, new.requirement_id.as_str()))
                        && new.consumer == old.consumer
                        && new.provider == old.provider
                })
                .map(|choice| choice.requirement_id.clone())
                .collect::<Vec<_>>();
            new_requirement_ids.sort();
            PluginRequirementMigration {
                consumer: old.consumer.clone(),
                old_requirement_id: old.requirement_id.clone(),
                new_requirement_ids,
                provider: old.provider.clone(),
            }
        })
        .collect()
}

fn validate_dependency_choices(choices: &[DependencyChoice]) -> anyhow::Result<()> {
    if choices.len() > super::MAX_DEPENDENCY_SELECTIONS {
        bail!(
            "Plugin dependency selections exceed {} entries",
            super::MAX_DEPENDENCY_SELECTIONS
        );
    }
    let mut keys = std::collections::BTreeSet::new();
    for choice in choices {
        validate_existing_plugin_id(choice.consumer.plugin_id())?;
        validate_instance_filename(choice.consumer.instance_key())?;
        validate_requirement_id(&choice.requirement_id)?;
        if let Some(provider) = &choice.provider {
            validate_existing_plugin_id(provider.plugin_id())?;
            validate_instance_filename(provider.instance_key())?;
        }
        if !keys.insert((choice.consumer.clone(), choice.requirement_id.clone())) {
            bail!("duplicate dependency choice for `{}`", choice.consumer);
        }
    }
    Ok(())
}

fn host_catalog_digest(root: &Path) -> anyhow::Result<String> {
    let generated = root.join(HOST_BUILD);
    let path = match fs::symlink_metadata(&generated) {
        Ok(_) => generated,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => root.join(HOST_CATALOG),
        Err(error) => return Err(error).context("inspect generated Host authority source"),
    };
    let metadata = fs::symlink_metadata(&path).context("inspect Host Catalog source")?;
    if !metadata.file_type().is_file() {
        bail!(
            "Host Catalog source must be a regular file: {}",
            path.display()
        );
    }
    Ok(sha256_digest(
        &fs::read(path).context("read Host Catalog source")?,
    ))
}

fn source_digests_for_changes(
    root: &Path,
    changes: &PluginRootChangeSet,
) -> anyhow::Result<Vec<PluginRootSourceDigest>> {
    let mut paths = changes
        .configurations
        .iter()
        .map(|change| format!("{}/{}.toml", change.plugin_id, change.instance_key))
        .collect::<Vec<_>>();
    if changes.dependency_choices.is_some() {
        paths.extend([
            DEPENDENCY_SELECTIONS.to_owned(),
            LEGACY_DEPENDENCY_SELECTIONS.to_owned(),
        ]);
    }
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| root_source_digest(root, path))
        .collect()
}

fn root_source_digest(root: &Path, path: String) -> anyhow::Result<PluginRootSourceDigest> {
    let source = root.join(PLUGIN_ROOT).join(&path);
    let bytes = match fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.file_type().is_file() => {
            Some(fs::read(&source).with_context(|| format!("read {}", source.display()))?)
        }
        Ok(_) => bail!(
            "Plugin Root source must be a regular file: {}",
            source.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("inspect {}", source.display())),
    };
    let mut digest = Sha256::new();
    update_digest_component(&mut digest, ROOT_SOURCE_DIGEST_SCHEMA.as_bytes());
    update_digest_component(&mut digest, path.as_bytes());
    match bytes {
        Some(bytes) => {
            digest.update([1]);
            update_digest_component(&mut digest, &bytes);
        }
        None => digest.update([0]),
    }
    Ok(PluginRootSourceDigest {
        path,
        digest: encode_sha256(digest.finalize()),
    })
}

/// Builds review evidence without changing the Plugin Root.
pub fn propose_instance_configuration(
    root: &Path,
    expected_revision: &PluginRootRevision,
    plugin_id: &str,
    instance: &str,
    bytes: &[u8],
) -> anyhow::Result<PluginConfigurationProposal> {
    validate_existing_plugin_id(plugin_id)?;
    validate_instance_filename(instance)?;
    let _lock = lock_plugin_root(root)?;
    let host = load_host_catalog(root)?;
    let current = snapshot_plugin_root(root, &host)?;
    let current_revision = revision_for_snapshot(&current)?;
    ensure_revision(expected_revision, &current_revision)?;
    let base_source_digest = source_digest_for_instance(root, plugin_id, instance)?;
    let root_proposal = build_root_change_proposal(
        root,
        &host,
        &current,
        current_revision.clone(),
        PluginRootChangeSet::new().with_configuration(PluginRootConfigurationChange::new(
            plugin_id,
            instance,
            bytes.to_vec(),
        )),
    )?;
    Ok(PluginConfigurationProposal {
        schema: PROPOSAL_SCHEMA,
        base_revision: current_revision,
        base_source_digest,
        candidate_revision: root_proposal.candidate_revision.clone(),
        digest: root_proposal.digest.clone(),
        status: root_proposal.status,
        application: root_proposal.application,
        diagnostics: root_proposal.diagnostics.clone(),
        plugin_id: plugin_id.to_owned(),
        instance_key: instance.to_owned(),
        toml: bytes.to_vec(),
        root_proposal,
    })
}

/// Publishes an exact reviewed proposal when its base revision is still current.
pub fn publish_instance_configuration(
    root: &Path,
    proposal: &PluginConfigurationProposal,
) -> anyhow::Result<PluginConfigurationPublication> {
    if proposal.plugin_id != proposal.root_proposal.changes.configurations[0].plugin_id
        || proposal.instance_key != proposal.root_proposal.changes.configurations[0].instance_key
        || proposal.toml != proposal.root_proposal.changes.configurations[0].toml
        || proposal.digest != proposal.root_proposal.digest
    {
        bail!("Plugin configuration proposal no longer matches its reviewed candidate");
    }
    let current_revision = inspect_plugin_root(root)?.revision().clone();
    ensure_revision(&proposal.base_revision, &current_revision)?;
    let current_source_digest =
        source_digest_for_instance(root, &proposal.plugin_id, &proposal.instance_key)?;
    ensure_source_digest(&proposal.base_source_digest, &current_source_digest)?;
    let publication = publish_plugin_root_changes(root, &proposal.root_proposal)?;
    Ok(PluginConfigurationPublication {
        schema: PUBLICATION_SCHEMA,
        base_revision: proposal.base_revision.clone(),
        base_source_digest: proposal.base_source_digest.clone(),
        revision: publication.revision,
        proposal_digest: proposal.digest.clone(),
        resolved: publication.resolved,
    })
}

fn classify_resolution(
    resolution: Result<ResolvedApp, PluginRootResolutionError>,
    candidate_revision: &PluginRootRevision,
    base_revision: &PluginRootRevision,
) -> (
    PluginConfigurationProposalStatus,
    PluginConfigurationApplication,
    Vec<PluginConfigurationDiagnostic>,
) {
    match resolution {
        Ok(_) => (
            PluginConfigurationProposalStatus::Ready,
            if candidate_revision == base_revision {
                PluginConfigurationApplication::Noop
            } else {
                PluginConfigurationApplication::AppGeneration
            },
            Vec::new(),
        ),
        Err(error) => {
            let status = if matches!(
                error,
                PluginRootResolutionError::AmbiguousSlot { .. }
                    | PluginRootResolutionError::AmbiguousCapability { .. }
            ) {
                PluginConfigurationProposalStatus::NeedsDecision
            } else {
                PluginConfigurationProposalStatus::Rejected
            };
            (
                status,
                PluginConfigurationApplication::Blocked,
                vec![PluginConfigurationDiagnostic {
                    code: resolution_error_code(&error),
                    detail: error.to_string(),
                }],
            )
        }
    }
}

fn ensure_ready(
    status: PluginConfigurationProposalStatus,
    application: PluginConfigurationApplication,
    diagnostics: &[PluginConfigurationDiagnostic],
    subject: &str,
) -> anyhow::Result<()> {
    if status == PluginConfigurationProposalStatus::Ready
        && application != PluginConfigurationApplication::Blocked
    {
        return Ok(());
    }
    let detail = diagnostics
        .first()
        .map_or("candidate did not pass the Ready Gate", |diagnostic| {
            diagnostic.detail()
        });
    bail!("{subject} cannot be published: {detail}")
}

fn resolution_error_code(error: &PluginRootResolutionError) -> &'static str {
    match error {
        PluginRootResolutionError::InvalidHostConfiguration(_) => "host_admission_denied",
        PluginRootResolutionError::AmbiguousSlot { .. } => "ambiguous_slot",
        PluginRootResolutionError::AmbiguousCapability { .. } => "ambiguous_capability",
        PluginRootResolutionError::InvalidConfiguration { .. } => "invalid_configuration",
        PluginRootResolutionError::MissingRequiredSlot(_) => "missing_required_slot",
        PluginRootResolutionError::MissingCapability { .. } => "missing_capability",
        PluginRootResolutionError::RequiredInstanceDisabled(_) => "required_instance_disabled",
        PluginRootResolutionError::UnknownPlugin(_) => "unknown_plugin",
        PluginRootResolutionError::UnknownDisabledInstance(_) => "unknown_disabled_instance",
        _ => "invalid_plugin_root",
    }
}

fn parse_configuration(bytes: &[u8]) -> anyhow::Result<serde_json::Value> {
    let byte_count = u64::try_from(bytes.len()).context("Plugin configuration is too large")?;
    if byte_count > MAX_CONFIGURATION_BYTES {
        bail!("Plugin configuration exceeds 256 KiB");
    }
    let text = str::from_utf8(bytes).context("Plugin configuration must be UTF-8 TOML")?;
    let table: toml::Table = toml::from_str(text).context("parse Plugin configuration TOML")?;
    serde_json::to_value(table).context("convert Plugin configuration to portable values")
}

pub(crate) fn ensure_revision(
    expected: &PluginRootRevision,
    current: &PluginRootRevision,
) -> anyhow::Result<()> {
    if expected == current {
        return Ok(());
    }
    Err(PluginRootRevisionConflict {
        expected: expected.clone(),
        current: current.clone(),
    }
    .into())
}

/// A digest of the exact target configuration source, including absence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginConfigurationSourceDigest(String);

impl PluginConfigurationSourceDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Digests one exact target configuration source or its explicit absence.
    pub fn for_source(
        plugin_id: &str,
        instance: &str,
        bytes: Option<&[u8]>,
    ) -> anyhow::Result<Self> {
        validate_existing_plugin_id(plugin_id)?;
        validate_instance_filename(instance)?;
        Ok(source_digest_for_bytes(plugin_id, instance, bytes))
    }
}

impl fmt::Display for PluginConfigurationSourceDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Exact configuration source changed after a proposal was reviewed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginConfigurationSourceConflict {
    expected: PluginConfigurationSourceDigest,
    current: PluginConfigurationSourceDigest,
}

impl PluginConfigurationSourceConflict {
    pub const fn expected(&self) -> &PluginConfigurationSourceDigest {
        &self.expected
    }

    pub const fn current(&self) -> &PluginConfigurationSourceDigest {
        &self.current
    }
}

impl fmt::Display for PluginConfigurationSourceConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Plugin configuration source conflict: expected {}, current {}",
            self.expected, self.current
        )
    }
}

impl Error for PluginConfigurationSourceConflict {}

pub(crate) fn source_digest_for_bytes(
    plugin_id: &str,
    instance: &str,
    bytes: Option<&[u8]>,
) -> PluginConfigurationSourceDigest {
    let mut authority = Sha256::new();
    update_digest_component(&mut authority, SOURCE_DIGEST_SCHEMA.as_bytes());
    update_digest_component(&mut authority, plugin_id.as_bytes());
    update_digest_component(&mut authority, instance.as_bytes());
    match bytes {
        Some(bytes) => {
            authority.update([1]);
            update_digest_component(&mut authority, bytes);
        }
        None => authority.update([0]),
    }
    PluginConfigurationSourceDigest(encode_sha256(authority.finalize()))
}

fn source_digest_for_instance(
    root: &Path,
    plugin_id: &str,
    instance: &str,
) -> anyhow::Result<PluginConfigurationSourceDigest> {
    let path = root
        .join(PLUGIN_ROOT)
        .join(plugin_id)
        .join(format!("{instance}.toml"));
    let bytes = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => Some(
            fs::read(&path)
                .with_context(|| format!("read Plugin configuration source {}", path.display()))?,
        ),
        Ok(_) => bail!(
            "Plugin configuration source must be a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect Plugin configuration source {}", path.display())
            });
        }
    };
    Ok(source_digest_for_bytes(
        plugin_id,
        instance,
        bytes.as_deref(),
    ))
}

fn ensure_source_digest(
    expected: &PluginConfigurationSourceDigest,
    current: &PluginConfigurationSourceDigest,
) -> anyhow::Result<()> {
    if expected == current {
        return Ok(());
    }
    Err(PluginConfigurationSourceConflict {
        expected: expected.clone(),
        current: current.clone(),
    }
    .into())
}

fn update_digest_component(authority: &mut Sha256, bytes: &[u8]) {
    authority.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    authority.update(bytes);
}

pub(crate) fn revision_for_snapshot(
    snapshot: &PluginRootSnapshot,
) -> anyhow::Result<PluginRootRevision> {
    let canonical =
        serde_json::to_vec(snapshot).context("encode Plugin Root revision authority")?;
    Ok(PluginRootRevision(sha256_digest(&canonical)))
}

fn sha256_digest(bytes: &[u8]) -> String {
    encode_sha256(Sha256::digest(bytes))
}

fn encode_sha256(digest: impl AsRef<[u8]>) -> String {
    let digest = digest.as_ref();
    let mut encoded = String::with_capacity(7 + digest.len() * 2);
    encoded.push_str("sha256:");
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use lenso_app_plan::authoring::{
        HostDefaultPlugin, HostPluginRelease, HostSlot, PluginDescriptor,
    };

    use super::*;
    use crate::{HOST_CATALOG, inspect_plugin_root};

    fn fixture_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lenso")).unwrap();
        let descriptor = PluginDescriptor::new("example.agent", "1.0.0", "agent")
            .with_configuration_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "greeting": { "type": "string" }
                },
                "additionalProperties": false
            }));
        let host = lenso_app_plan::authoring::HostCatalog::new(
            [HostSlot::one("agent")],
            [HostPluginRelease::new(descriptor)],
            [HostDefaultPlugin::new("example.agent", "default")],
        );
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();
        root
    }

    fn coordinated_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lenso")).unwrap();
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "additionalProperties": false
        });
        let host = lenso_app_plan::authoring::HostCatalog::new(
            [HostSlot::one("source"), HostSlot::one("target")],
            [
                HostPluginRelease::new(
                    PluginDescriptor::new("example.source", "1.0.0", "source")
                        .with_configuration_schema(schema.clone()),
                ),
                HostPluginRelease::new(
                    PluginDescriptor::new("example.target", "1.0.0", "target")
                        .with_configuration_schema(schema),
                ),
            ],
            [
                HostDefaultPlugin::new("example.source", "default"),
                HostDefaultPlugin::new("example.target", "default"),
            ],
        );
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();
        root
    }

    #[test]
    fn single_configuration_keeps_the_materialized_instance_order() {
        let root = coordinated_root();
        for (plugin, value) in [("example.source", "old"), ("example.target", "unchanged")] {
            let directory = root.path().join("plugins").join(plugin);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("default.toml"),
                format!("value = \"{value}\"\n"),
            )
            .unwrap();
        }
        let resources = root.path().join("plugins/example.source/default");
        fs::create_dir_all(&resources).unwrap();
        fs::write(resources.join("note.txt"), "keep this resource").unwrap();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let proposal = propose_instance_configuration(
            root.path(),
            &base,
            "example.source",
            "default",
            b"value = \"new\"\n",
        )
        .unwrap();
        let publication = publish_instance_configuration(root.path(), &proposal).unwrap();
        assert_eq!(publication.revision(), proposal.candidate_revision());
    }

    #[test]
    fn proposal_is_read_only_and_publication_advances_the_revision() {
        let root = fixture_root();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let proposal = propose_instance_configuration(
            root.path(),
            &base,
            "example.agent",
            "default",
            b"greeting = \"hello\"\n",
        )
        .unwrap();

        assert_eq!(proposal.status(), PluginConfigurationProposalStatus::Ready);
        assert_eq!(
            proposal.application(),
            PluginConfigurationApplication::AppGeneration
        );
        assert_eq!(proposal.base_revision(), &base);
        assert!(
            proposal
                .base_source_digest()
                .as_str()
                .starts_with("sha256:")
        );
        assert_ne!(proposal.candidate_revision(), &base);
        assert!(proposal.digest().starts_with("sha256:"));
        assert!(!configuration_path(root.path()).exists());

        let publication = publish_instance_configuration(root.path(), &proposal).unwrap();
        assert_eq!(publication.base_revision(), &base);
        assert_eq!(
            publication.base_source_digest(),
            proposal.base_source_digest()
        );
        assert_eq!(publication.revision(), proposal.candidate_revision());
        assert_eq!(publication.proposal_digest(), proposal.digest());
        assert_eq!(
            fs::read_to_string(configuration_path(root.path())).unwrap(),
            "greeting = \"hello\"\n"
        );
    }

    #[test]
    fn coordinated_proposal_publishes_two_configurations_and_choices_together() {
        let root = coordinated_root();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let changes = PluginRootChangeSet::new()
            .with_configuration(PluginRootConfigurationChange::new(
                "example.source",
                "default",
                b"value = \"source\"\n".to_vec(),
            ))
            .with_configuration(PluginRootConfigurationChange::new(
                "example.target",
                "default",
                b"value = \"target\"\n".to_vec(),
            ))
            .with_dependency_choices([]);
        let proposal = propose_plugin_root_changes(root.path(), &base, changes).unwrap();

        assert_eq!(proposal.status(), PluginConfigurationProposalStatus::Ready);
        assert_eq!(proposal.source_digests().len(), 4);
        assert!(proposal.host_catalog_digest().starts_with("sha256:"));
        assert!(
            !root
                .path()
                .join("plugins/example.source/default.toml")
                .exists()
        );
        assert!(!root.path().join("plugins/.dependencies.json").exists());

        let publication = publish_plugin_root_changes(root.path(), &proposal).unwrap();

        assert_eq!(publication.revision(), proposal.candidate_revision());
        assert_eq!(
            fs::read_to_string(root.path().join("plugins/example.source/default.toml")).unwrap(),
            "value = \"source\"\n"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("plugins/example.target/default.toml")).unwrap(),
            "value = \"target\"\n"
        );
        let choices: DependencySelectionsDocument = serde_json::from_slice(
            &fs::read(root.path().join("plugins/.dependencies.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(choices.schema_version, DEPENDENCY_SELECTIONS_SCHEMA_VERSION);
        assert!(choices.choices.is_empty());
    }

    #[test]
    fn coordinated_publication_rejects_a_byte_changed_host_catalog() {
        let root = fixture_root();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let changes =
            PluginRootChangeSet::new().with_configuration(PluginRootConfigurationChange::new(
                "example.agent",
                "default",
                b"greeting = \"hello\"\n".to_vec(),
            ));
        let proposal = propose_plugin_root_changes(root.path(), &base, changes).unwrap();
        let catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join(HOST_CATALOG)).unwrap()).unwrap();
        fs::write(
            root.path().join(HOST_CATALOG),
            serde_json::to_vec_pretty(&catalog).unwrap(),
        )
        .unwrap();

        let error = publish_plugin_root_changes(root.path(), &proposal).unwrap_err();

        assert!(error.to_string().contains("Host Catalog changed"));
        assert!(!configuration_path(root.path()).exists());
    }

    #[test]
    fn requirement_migration_preserves_exact_provider_and_exposes_splits() {
        let consumer = PluginInstanceId::new("example.copy", "default");
        let provider = PluginInstanceId::new("example.store", "account-a");
        let current = [DependencyChoice {
            consumer: consumer.clone(),
            requirement_id: "~example.store@1".to_owned(),
            provider: Some(provider.clone()),
        }];
        let candidate = ["source", "archive"].map(|requirement_id| DependencyChoice {
            consumer: consumer.clone(),
            requirement_id: requirement_id.to_owned(),
            provider: Some(provider.clone()),
        });

        let migrations = requirement_migrations(&current, &candidate);

        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0].provider(), Some(&provider));
        assert_eq!(
            migrations[0].new_requirement_ids(),
            &["archive".to_owned(), "source".to_owned()]
        );
        assert!(has_unreviewed_split(&migrations, &[]));
        assert!(!has_unreviewed_split(&migrations, &candidate));
    }

    #[test]
    fn local_authority_dispatches_through_the_host_port() {
        let root = fixture_root();
        let authority: Arc<dyn PluginConfigurationAuthority> =
            Arc::new(LocalPluginRootAuthority::new(root.path()));
        let source = authority.source();
        let base = authority.inspect().unwrap().revision().clone();

        let proposal = authority
            .propose(
                &base,
                "example.agent",
                "default",
                b"greeting = \"authority\"\n",
            )
            .unwrap();
        assert!(!configuration_path(root.path()).exists());

        let publication = authority.publish(&proposal).unwrap();

        assert_eq!(source.kind(), "local_plugin_root");
        assert_eq!(source.reference(), "app");
        assert_eq!(publication.revision(), proposal.candidate_revision());
        assert_eq!(
            authority.inspect().unwrap().revision(),
            publication.revision()
        );
    }

    #[test]
    fn stale_publication_fails_with_a_typed_conflict_and_preserves_the_winner() {
        let root = fixture_root();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let first = proposal(root.path(), &base, b"greeting = \"first\"\n");
        let stale = proposal(root.path(), &base, b"greeting = \"stale\"\n");
        let first_publication = publish_instance_configuration(root.path(), &first).unwrap();

        let error = publish_instance_configuration(root.path(), &stale).unwrap_err();
        let conflict = error.downcast_ref::<PluginRootRevisionConflict>().unwrap();

        assert_eq!(conflict.expected(), &base);
        assert_eq!(conflict.current(), first_publication.revision());
        assert_eq!(
            fs::read_to_string(configuration_path(root.path())).unwrap(),
            "greeting = \"first\"\n"
        );
    }

    #[test]
    fn concurrent_publications_allow_exactly_one_winner() {
        let root = fixture_root();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let first = proposal(root.path(), &base, b"greeting = \"first\"\n");
        let second = proposal(root.path(), &base, b"greeting = \"second\"\n");
        let path = root.path().to_path_buf();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = [first, second].map(|proposal| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                publish_instance_configuration(&path, &proposal)
            })
        });
        barrier.wait();
        let outcomes = handles.map(|handle| handle.join().unwrap());

        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        let error = outcomes.into_iter().find_map(Result::err).unwrap();
        assert!(error.downcast_ref::<PluginRootRevisionConflict>().is_some());
        let contents = fs::read_to_string(configuration_path(&path)).unwrap();
        assert!(contents == "greeting = \"first\"\n" || contents == "greeting = \"second\"\n");
    }

    #[test]
    fn rejected_proposal_keeps_structured_diagnostics_without_writing() {
        let root = fixture_root();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let proposal = proposal(root.path(), &base, b"unexpected = true\n");

        assert_eq!(
            proposal.status(),
            PluginConfigurationProposalStatus::Rejected
        );
        assert_eq!(
            proposal.application(),
            PluginConfigurationApplication::Blocked
        );
        assert_eq!(proposal.diagnostics()[0].code(), "invalid_configuration");
        assert!(!configuration_path(root.path()).exists());
        assert!(publish_instance_configuration(root.path(), &proposal).is_err());
    }

    #[test]
    fn plugin_root_revision_is_semantic_not_toml_formatting() {
        let root = fixture_root();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let proposal = proposal(root.path(), &base, b"greeting = \"hello\"\n");
        let publication = publish_instance_configuration(root.path(), &proposal).unwrap();
        fs::write(
            configuration_path(root.path()),
            b"# human note\n\ngreeting=\"hello\"\n",
        )
        .unwrap();

        let reformatted = inspect_plugin_root(root.path()).unwrap();
        assert_eq!(reformatted.revision(), publication.revision());
    }

    #[test]
    fn formatting_only_source_change_rejects_stale_publication_without_overwrite() {
        let root = fixture_root();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let initial = proposal(root.path(), &base, b"greeting = \"hello\"\n");
        let initial = publish_instance_configuration(root.path(), &initial).unwrap();
        let stale = proposal(root.path(), initial.revision(), b"greeting = \"goodbye\"\n");
        let external = b"# keep this human note\n\ngreeting=\"hello\"\n";
        fs::write(configuration_path(root.path()), external).unwrap();

        assert_eq!(
            inspect_plugin_root(root.path()).unwrap().revision(),
            initial.revision()
        );
        let error = publish_instance_configuration(root.path(), &stale).unwrap_err();
        let conflict = error
            .downcast_ref::<PluginConfigurationSourceConflict>()
            .unwrap();

        assert_eq!(conflict.expected(), stale.base_source_digest());
        assert_ne!(conflict.current(), stale.base_source_digest());
        assert_eq!(fs::read(configuration_path(root.path())).unwrap(), external);
    }

    #[test]
    fn proposal_digest_closes_the_exact_reviewed_toml() {
        let root = fixture_root();
        let base = inspect_plugin_root(root.path()).unwrap().revision().clone();
        let compact = propose_instance_configuration(
            root.path(),
            &base,
            "example.agent",
            "default",
            b"greeting=\"hello\"\n",
        )
        .unwrap();
        let formatted = propose_instance_configuration(
            root.path(),
            &base,
            "example.agent",
            "default",
            b"greeting = \"hello\"\n",
        )
        .unwrap();

        assert_eq!(compact.candidate_revision(), formatted.candidate_revision());
        assert_ne!(compact.digest(), formatted.digest());
    }

    #[test]
    fn source_digest_domains_raw_bytes_absence_and_instance_identity() {
        let absent =
            PluginConfigurationSourceDigest::for_source("example.agent", "default", None).unwrap();
        let empty =
            PluginConfigurationSourceDigest::for_source("example.agent", "default", Some(b""))
                .unwrap();
        let other_instance =
            PluginConfigurationSourceDigest::for_source("example.agent", "secondary", None)
                .unwrap();

        assert_ne!(absent, empty);
        assert_ne!(absent, other_instance);
    }

    #[test]
    fn plugin_root_revision_round_trips_for_http_preconditions() {
        let root = fixture_root();
        let revision = inspect_plugin_root(root.path()).unwrap().revision().clone();

        assert_eq!(
            revision.as_str().parse::<PluginRootRevision>().unwrap(),
            revision
        );
        assert!("sha256:not-a-digest".parse::<PluginRootRevision>().is_err());
    }

    fn proposal(
        root: &Path,
        base: &PluginRootRevision,
        toml: &[u8],
    ) -> PluginConfigurationProposal {
        propose_instance_configuration(root, base, "example.agent", "default", toml).unwrap()
    }

    fn configuration_path(root: &Path) -> std::path::PathBuf {
        root.join("plugins/example.agent/default.toml")
    }
}
