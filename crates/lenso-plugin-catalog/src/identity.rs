//! Canonical identities shared by Plugin authoring and catalog consumers.

use anyhow::bail;

/// Versioned shape accepted for a Plugin ID already present in a project.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginIdVersion {
    /// Namespaced Plugin identity used by new authoring and catalogs.
    V1,
    /// Pre-v1 unnamespaced identity retained only for opening existing projects.
    Legacy,
}

/// Validates the canonical namespaced Plugin ID v1 grammar.
///
/// A v1 ID contains at least two dot-separated labels. Every label starts with
/// a lowercase ASCII letter, ends with a lowercase letter or digit, and may
/// contain lowercase letters, digits, or hyphens between them. Labels contain
/// at most 63 bytes and the complete ID contains at most 253 bytes.
pub fn validate_plugin_id_v1(plugin_id: &str) -> anyhow::Result<()> {
    if plugin_id.len() > 253 {
        bail!("Plugin id v1 must not exceed 253 bytes");
    }
    let labels = plugin_id.split('.').collect::<Vec<_>>();
    if labels.len() < 2 || !labels.iter().all(valid_plugin_label) {
        bail!(
            "Plugin id v1 must contain at least two lowercase dot-separated labels; labels start with a letter, end with a letter or digit, and may contain hyphens"
        );
    }
    Ok(())
}

/// Classifies an existing Plugin ID without silently breaking pre-v1 projects.
pub fn classify_existing_plugin_id(plugin_id: &str) -> anyhow::Result<PluginIdVersion> {
    if validate_plugin_id_v1(plugin_id).is_ok() {
        return Ok(PluginIdVersion::V1);
    }
    if plugin_id.is_empty()
        || plugin_id.contains('.')
        || !plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
        || !plugin_id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
    {
        bail!("Plugin id is neither a canonical v1 identity nor a supported legacy identity");
    }
    Ok(PluginIdVersion::Legacy)
}

/// Validates one exact Semantic Version rather than a range or tag.
pub fn validate_release_version(version: &str) -> anyhow::Result<()> {
    semver::Version::parse(version)
        .map(|_| ())
        .map_err(|error| {
            anyhow::anyhow!("Release version must be an exact Semantic Version: {error}")
        })
}

fn valid_plugin_label(label: &&str) -> bool {
    let bytes = label.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}
