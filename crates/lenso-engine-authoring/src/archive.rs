pub use crate::archive_download::PluginArchiveDownloadPolicy;

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, bail};
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

const MAX_ARCHIVE_FILES: usize = 4_096;
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

/// Writes one immutable archive from regular files. Does not grant execution authority.
pub fn archive_bundle(source: &Path, output: &Path) -> anyhow::Result<()> {
    if output.exists() {
        bail!("Plugin Bundle output already exists: {}", output.display());
    }
    let parent = output
        .parent()
        .context("Plugin Bundle output has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut writer = ZipWriter::new(temporary.reopen()?);
    let mut files = Vec::new();
    collect_files(source, source, &mut files)?;
    files.sort();
    for relative in files {
        let source_file = source.join(&relative);
        let metadata = fs::symlink_metadata(&source_file)?;
        if !metadata.file_type().is_file() {
            bail!(
                "Plugin Bundle archives accept regular files only: {}",
                source_file.display()
            );
        }
        let name = portable_path(&relative)?;
        #[cfg(unix)]
        let permissions = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode()
        };
        #[cfg(not(unix))]
        let permissions = 0o644;
        writer.start_file(
            name,
            SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(permissions),
        )?;
        io::copy(&mut File::open(&source_file)?, &mut writer)?;
    }
    writer.finish()?.sync_all()?;
    temporary
        .persist(output)
        .map_err(|error| error.error)
        .with_context(|| format!("publish Plugin Bundle archive {}", output.display()))?;
    Ok(())
}

fn extract_bundle(archive: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(destination)?;
    let mut zip = ZipArchive::new(File::open(archive)?)
        .with_context(|| format!("open Plugin Bundle archive {}", archive.display()))?;
    if zip.len() > MAX_ARCHIVE_FILES {
        bail!("Plugin Bundle archive exceeds {MAX_ARCHIVE_FILES} files");
    }
    let mut total = 0_u64;
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        let relative = entry
            .enclosed_name()
            .context("Plugin Bundle archive contains an unsafe path")?;
        validate_relative_path(&relative)?;
        if entry.is_dir() {
            fs::create_dir_all(destination.join(relative))?;
            continue;
        }
        if !entry.is_file()
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170_000 == 0o120_000)
        {
            bail!(
                "Plugin Bundle archive contains a non-file entry: {}",
                entry.name()
            );
        }
        let size = entry.size();
        let mode = entry.unix_mode();
        total = total
            .checked_add(size)
            .context("Plugin Bundle archive size overflow")?;
        if total > MAX_ARCHIVE_BYTES {
            bail!("Plugin Bundle archive exceeds 256 MiB");
        }
        let output = destination.join(relative);
        let parent = output.parent().context("archive entry has no parent")?;
        fs::create_dir_all(parent)?;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&output)?;
        io::copy(&mut entry.take(size), &mut file)?;
        file.flush()?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&output, fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    Ok(())
}

/// Uses a local directory or privately extracts a bounded local archive.
/// This authoring helper does not check a remote digest or verify Bundle contents.
/// Use [`VerifiedPluginArchive`] for an untrusted downloaded release.
pub fn with_bundle_directory<T>(
    bundle: &Path,
    use_directory: impl FnOnce(&Path) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if bundle.is_dir() {
        return use_directory(bundle);
    }
    let temporary = tempfile::tempdir().context("extract Plugin Bundle archive")?;
    extract_bundle(bundle, temporary.path())?;
    use_directory(temporary.path())
}

/// Expected immutable transport identity supplied by an independent trusted source.
#[derive(Clone, Debug)]
pub struct PluginArchiveIdentity {
    pub size: u64,
    pub sha256: String,
}

/// Exact release identity admitted by the caller, independently of transport bytes.
#[derive(Clone, Debug)]
pub struct PluginReleaseIdentity {
    pub plugin_id: String,
    pub release_version: String,
    pub manifest_digest: String,
}

impl PluginReleaseIdentity {
    fn verify(&self, bundle: &lenso_plugin_bundle::VerifiedBundle) -> anyhow::Result<()> {
        if self.plugin_id != bundle.plugin_id
            || self.release_version != bundle.release_version
            || self.manifest_digest != bundle.manifest_digest
        {
            bail!("Plugin archive does not match the admitted release identity");
        }
        Ok(())
    }
}

/// Owns a private copy of the exact verified archive and its verified extraction.
/// No candidate App is resolved or changed. Drop removes temporary files.
#[derive(Debug)]
pub struct VerifiedPluginArchive {
    temporary: tempfile::TempDir,
    bundle: lenso_plugin_bundle::VerifiedBundle,
}

impl VerifiedPluginArchive {
    /// Reads at most the expected byte count plus one, verifies the transport
    /// identity, then extracts and verifies the complete Bundle. Source changes
    /// after this call cannot alter the retained bytes used by installation.
    pub fn read(reader: impl Read, expected: &PluginArchiveIdentity) -> anyhow::Result<Self> {
        use sha2::{Digest as _, Sha256};
        if expected.size == 0 || expected.size > MAX_ARCHIVE_BYTES {
            bail!("Plugin archive expected size is outside supported bounds");
        }
        let digest = expected
            .sha256
            .strip_prefix("sha256:")
            .context("Plugin archive requires a SHA-256 digest")?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            bail!("Plugin archive SHA-256 digest is invalid");
        }
        let temporary = tempfile::tempdir().context("stage verified Plugin archive")?;
        let archive_path = temporary.path().join("plugin.lenso-plugin");
        let mut archive = File::create(&archive_path)?;
        let mut reader = reader.take(expected.size + 1);
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += u64::try_from(count)?;
            if total > expected.size {
                bail!("Plugin archive exceeds expected size");
            }
            hasher.update(&buffer[..count]);
            archive.write_all(&buffer[..count])?;
        }
        let mut actual = String::with_capacity(64);
        for byte in hasher.finalize() {
            use std::fmt::Write as _;
            write!(actual, "{byte:02x}")?;
        }
        if total != expected.size || actual != digest {
            bail!("Plugin archive size or digest mismatch");
        }
        archive.sync_all()?;
        drop(archive);
        let directory = temporary.path().join("bundle");
        extract_bundle(&archive_path, &directory)?;
        let bundle = lenso_plugin_bundle::verify_bundle_directory(&directory)?;
        Ok(Self { temporary, bundle })
    }

    /// Verifies both the transport bytes and exact release before handing a
    /// downloaded archive to a target. The caller authenticates the expected
    /// identities (for example, by verifying and selecting a signed catalog).
    pub fn read_release(
        reader: impl Read,
        transport: &PluginArchiveIdentity,
        release: &PluginReleaseIdentity,
    ) -> anyhow::Result<Self> {
        let verified = Self::read(reader, transport)?;
        release.verify(verified.bundle())?;
        Ok(verified)
    }

    /// Prepares an add/update in an explicitly chosen authoring root, using
    /// the existing Host admission, complete-App resolution and atomic commit.
    /// Rechecks the copied candidate against the originally verified identity,
    /// so even modification of the exposed temporary directory cannot substitute
    /// another valid Bundle. Dropping the proposal leaves the Plugin Root intact.
    ///
    /// This is an authoring primitive, not online installation authority: a live
    /// Host must use its private candidate root and own revision checks, user
    /// approval, readiness, durable receipts and publication to the active App.
    pub fn prepare_mutation(
        &self,
        root: &Path,
        mutation: crate::BundleMutation,
    ) -> anyhow::Result<crate::PreparedBundleMutation> {
        let proposal = crate::prepare_bundle_mutation(root, &self.directory(), mutation)?;
        PluginReleaseIdentity {
            plugin_id: self.bundle.plugin_id.clone(),
            release_version: self.bundle.release_version.clone(),
            manifest_digest: self.bundle.manifest_digest.clone(),
        }
        .verify(proposal.verified())?;
        Ok(proposal)
    }

    /// Source-derived release facts, after full Bundle verification.
    pub fn bundle(&self) -> &lenso_plugin_bundle::VerifiedBundle {
        &self.bundle
    }

    /// Private verified extraction, for immediate downstream installation.
    /// The owning process must not modify it between verification and use.
    pub fn directory(&self) -> PathBuf {
        self.temporary.path().join("bundle")
    }

    /// Opens the retained archive, rather than a mutable caller-supplied path.
    pub fn open_archive(&self) -> anyhow::Result<File> {
        Ok(File::open(
            self.temporary.path().join("plugin.lenso-plugin"),
        )?)
    }
}

fn collect_files(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(
                "Plugin Bundle cannot archive symlinks: {}",
                entry.path().display()
            );
        }
        if metadata.file_type().is_dir() {
            collect_files(root, &entry.path(), output)?;
        } else if metadata.file_type().is_file() {
            output.push(entry.path().strip_prefix(root)?.to_path_buf());
        } else {
            bail!(
                "Plugin Bundle cannot archive special files: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn portable_path(path: &Path) -> anyhow::Result<String> {
    validate_relative_path(path)?;
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .context("Plugin Bundle path is not UTF-8"),
            _ => unreachable!("validated relative path contains only normal components"),
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

fn validate_relative_path(path: &Path) -> anyhow::Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!(
            "Plugin Bundle archive path is not a safe relative path: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(bytes: &[u8]) -> PluginArchiveIdentity {
        PluginArchiveIdentity {
            size: bytes.len() as u64,
            sha256: crate::runtime_sha256(bytes),
        }
    }

    #[test]
    fn rejects_transport_changes_before_parsing_an_archive() {
        let expected = identity(b"not a zip");
        let mut reader = io::Cursor::new(vec![42; 1000]);
        let error = VerifiedPluginArchive::read(&mut reader, &expected).unwrap_err();
        assert!(error.to_string().contains("expected size"));
        assert_eq!(reader.position(), expected.size + 1);
        let error = VerifiedPluginArchive::read(&b"different"[..], &expected).unwrap_err();
        assert!(error.to_string().contains("digest mismatch"));
        let error = VerifiedPluginArchive::read(&b"short"[..], &expected).unwrap_err();
        assert!(error.to_string().contains("digest mismatch"));
    }

    #[test]
    fn rejects_invalid_expectations_without_reading_source() {
        for expected in [
            PluginArchiveIdentity {
                size: 0,
                sha256: crate::runtime_sha256(b""),
            },
            PluginArchiveIdentity {
                size: MAX_ARCHIVE_BYTES + 1,
                sha256: crate::runtime_sha256(b""),
            },
            PluginArchiveIdentity {
                size: 1,
                sha256: "sha256:INVALID".into(),
            },
        ] {
            let mut reader = io::Cursor::new(b"x");
            assert!(VerifiedPluginArchive::read(&mut reader, &expected).is_err());
            assert_eq!(reader.position(), 0);
        }
    }

    #[test]
    fn rejects_unsafe_archive_paths_even_when_transport_identity_matches() {
        let mut zip = ZipWriter::new(io::Cursor::new(Vec::new()));
        zip.start_file("../escape", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"unexpected").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let error = VerifiedPluginArchive::read(bytes.as_slice(), &identity(&bytes)).unwrap_err();
        assert!(error.to_string().contains("unsafe path"));
    }

    #[test]
    fn valid_zip_with_invalid_bundle_cannot_produce_verified_evidence() {
        let mut zip = ZipWriter::new(io::Cursor::new(Vec::new()));
        zip.start_file("lenso-plugin.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"{}").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        assert!(VerifiedPluginArchive::read(bytes.as_slice(), &identity(&bytes)).is_err());
    }

    #[test]
    #[ignore = "requires LENSO_TEST_PLUGIN_ARCHIVE from a real CLI pack"]
    fn verified_archive_retains_validated_bytes_after_source_changes() {
        let bytes = fs::read(
            std::env::var("LENSO_TEST_PLUGIN_ARCHIVE").expect("provide CLI-built archive"),
        )
        .unwrap();
        let source_root = tempfile::tempdir().unwrap();
        let source = source_root.path().join("source.lenso-plugin");
        fs::write(&source, &bytes).unwrap();
        let verified =
            VerifiedPluginArchive::read(File::open(&source).unwrap(), &identity(&bytes)).unwrap();
        assert_eq!(verified.bundle().plugin_id, "lenso.marketplace.echo");
        fs::write(&source, b"replaced after acquisition").unwrap();
        let mut retained = Vec::new();
        verified
            .open_archive()
            .unwrap()
            .read_to_end(&mut retained)
            .unwrap();
        assert_eq!(retained, bytes);
        let directory = verified.directory();
        assert!(directory.join("lenso-plugin.json").exists());
        drop(verified);
        assert!(!directory.exists());
    }

    #[test]
    #[ignore = "requires LENSO_TEST_PLUGIN_ARCHIVE from a real CLI pack"]
    #[expect(
        clippy::too_many_lines,
        reason = "one exact-release add, substitution rejection and replacement journey"
    )]
    fn verified_release_prepares_add_and_rejects_substitution_without_changing_root() {
        use lenso_app_plan::authoring::{HostCatalog, HostSlot};
        let bytes = fs::read(std::env::var("LENSO_TEST_PLUGIN_ARCHIVE").unwrap()).unwrap();
        let archive = VerifiedPluginArchive::read(bytes.as_slice(), &identity(&bytes)).unwrap();
        let expected = PluginReleaseIdentity {
            plugin_id: archive.bundle().plugin_id.clone(),
            release_version: archive.bundle().release_version.clone(),
            manifest_digest: archive.bundle().manifest_digest.clone(),
        };
        for wrong in [
            PluginReleaseIdentity {
                plugin_id: "example.other".into(),
                ..expected.clone()
            },
            PluginReleaseIdentity {
                release_version: "999.0.0".into(),
                ..expected.clone()
            },
            PluginReleaseIdentity {
                manifest_digest: format!("sha256:{}", "0".repeat(64)),
                ..expected.clone()
            },
        ] {
            assert!(
                VerifiedPluginArchive::read_release(bytes.as_slice(), &identity(&bytes), &wrong)
                    .is_err()
            );
        }
        let archive =
            VerifiedPluginArchive::read_release(bytes.as_slice(), &identity(&bytes), &expected)
                .unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".lenso")).unwrap();
        let host = HostCatalog::new([HostSlot::many("tool-providers")], [], []);
        fs::write(
            root.path().join(".lenso/host-catalog.json"),
            serde_json::to_vec(&host).unwrap(),
        )
        .unwrap();
        let destination = root
            .path()
            .join("plugins/lenso.marketplace.echo/plugin.lenso-plugin");
        let preview = archive
            .prepare_mutation(root.path(), crate::BundleMutation::Add)
            .unwrap();
        assert!(!destination.exists());
        assert_eq!(preview.verified().manifest_digest, expected.manifest_digest);
        drop(preview);
        assert!(!destination.exists());
        archive
            .prepare_mutation(root.path(), crate::BundleMutation::Add)
            .unwrap()
            .commit()
            .unwrap();
        assert!(destination.exists());
        assert!(
            archive
                .prepare_mutation(root.path(), crate::BundleMutation::Add)
                .is_err()
        );
        let original = fs::read(destination.join("lenso-plugin.json")).unwrap();
        // A valid but different release in the public temporary directory must
        // not inherit the original archive handle's admission.
        let manifest_path = archive.directory().join("lenso-plugin.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["release_version"] = "0.2.0".into();
        manifest["entry"]["descriptor"]["release_version"] = "0.2.0".into();
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        lenso_plugin_bundle::verify_bundle_directory(&archive.directory()).unwrap();
        let error = archive
            .prepare_mutation(root.path(), crate::BundleMutation::Replace)
            .unwrap_err();
        assert!(error.to_string().contains("admitted release identity"));
        assert_eq!(
            fs::read(destination.join("lenso-plugin.json")).unwrap(),
            original
        );
        // Independently admit the actual second release, then use the same
        // candidate/commit path to update the authoring root.
        let updated_path = root.path().join("updated.lenso-plugin");
        archive_bundle(&archive.directory(), &updated_path).unwrap();
        let updated_bytes = fs::read(updated_path).unwrap();
        let updated_bundle =
            lenso_plugin_bundle::verify_bundle_directory(&archive.directory()).unwrap();
        let updated_identity = PluginReleaseIdentity {
            plugin_id: updated_bundle.plugin_id,
            release_version: updated_bundle.release_version,
            manifest_digest: updated_bundle.manifest_digest,
        };
        let updated = VerifiedPluginArchive::read_release(
            updated_bytes.as_slice(),
            &identity(&updated_bytes),
            &updated_identity,
        )
        .unwrap();
        let proposal = updated
            .prepare_mutation(root.path(), crate::BundleMutation::Replace)
            .unwrap();
        assert_eq!(
            fs::read(destination.join("lenso-plugin.json")).unwrap(),
            original
        );
        assert_eq!(proposal.verified().release_version, "0.2.0");
        proposal.commit().unwrap();
        assert_eq!(
            lenso_plugin_bundle::verify_bundle_directory(&destination)
                .unwrap()
                .release_version,
            "0.2.0"
        );
    }

    #[test]
    fn archives_and_extracts_regular_bundle_files() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("source")).unwrap();
        fs::write(root.path().join("source/lenso-plugin.json"), "{}").unwrap();
        let archive = root.path().join("fixture.lenso-plugin");
        archive_bundle(&root.path().join("source"), &archive).unwrap();
        let output = root.path().join("output");
        extract_bundle(&archive, &output).unwrap();
        assert_eq!(fs::read(output.join("lenso-plugin.json")).unwrap(), b"{}");
    }
}
