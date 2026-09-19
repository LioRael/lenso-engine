use std::{fmt, path::Path};

use lenso_app_plan::authoring::ResolvedApp;

use super::{
    LocalPluginRootAuthority, PluginConfigurationAuthoritySource, PluginRootRevision,
    set_instance_disabled_inner,
};

/// Host-side port for enabling or disabling one Plugin Instance.
///
/// Implementations own the visible selection state and compare-and-swap mutation. They do not
/// own App Generation staging, routing, or Kernel execution.
pub trait PluginSelectionAuthority: fmt::Debug + Send + Sync {
    fn source(&self) -> PluginConfigurationAuthoritySource;

    fn set_enabled(
        &self,
        expected_revision: &PluginRootRevision,
        plugin_id: &str,
        instance: &str,
        enabled: bool,
    ) -> anyhow::Result<PluginSelectionPublication>;
}

/// One committed Plugin Instance selection change.
#[derive(Debug)]
pub struct PluginSelectionPublication {
    base_revision: PluginRootRevision,
    enabled: bool,
    instance: String,
    plugin_id: String,
    revision: PluginRootRevision,
    resolved: ResolvedApp,
}

impl PluginSelectionPublication {
    pub const fn base_revision(&self) -> &PluginRootRevision {
        &self.base_revision
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub const fn revision(&self) -> &PluginRootRevision {
        &self.revision
    }

    pub const fn resolved(&self) -> &ResolvedApp {
        &self.resolved
    }

    pub fn into_resolved(self) -> ResolvedApp {
        self.resolved
    }
}

/// Atomically changes one Instance selection after checking the exact current revision.
pub fn set_instance_enabled_fenced(
    root: &Path,
    expected_revision: &PluginRootRevision,
    plugin_id: &str,
    instance: &str,
    enabled: bool,
) -> anyhow::Result<PluginSelectionPublication> {
    let (base_revision, revision, resolved) =
        set_instance_disabled_inner(root, plugin_id, instance, !enabled, Some(expected_revision))?;
    Ok(PluginSelectionPublication {
        base_revision,
        enabled,
        instance: instance.to_owned(),
        plugin_id: plugin_id.to_owned(),
        revision,
        resolved,
    })
}

impl PluginSelectionAuthority for LocalPluginRootAuthority {
    fn source(&self) -> PluginConfigurationAuthoritySource {
        PluginConfigurationAuthoritySource::new("local_plugin_root", "app")
            .expect("built-in Plugin selection authority source is valid")
    }

    fn set_enabled(
        &self,
        expected_revision: &PluginRootRevision,
        plugin_id: &str,
        instance: &str,
        enabled: bool,
    ) -> anyhow::Result<PluginSelectionPublication> {
        let _guard = self.lock()?;
        set_instance_enabled_fenced(self.root(), expected_revision, plugin_id, instance, enabled)
    }
}
