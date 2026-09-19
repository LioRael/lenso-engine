//! Native filesystem integration, excluded from the portable default feature set.

use anyhow::{Result, ensure};
use std::path::Path;

use crate::Release;

impl Release {
    /// Reuse the framework's executable Bundle verifier; metadata cannot override it.
    /// The caller must supply a private, immutable extraction of the checked archive.
    pub fn verify_bundle_directory(
        &self,
        directory: &Path,
    ) -> Result<lenso_plugin_bundle::VerifiedBundle> {
        self.validate()?;
        let verified = lenso_plugin_bundle::verify_bundle_directory(directory)?;
        ensure!(
            verified.plugin_id == self.plugin_id
                && verified.release_version == self.version
                && verified.manifest_digest == self.artifact.manifest_digest,
            "Bundle identity differs from signed release"
        );
        Ok(verified)
    }
}
