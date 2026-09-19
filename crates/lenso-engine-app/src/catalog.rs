use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    time::Duration,
};

use anyhow::{Context, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use lenso_app_authoring::identity::{validate_plugin_id_v1, validate_release_version};

pub const DEFAULT_CATALOG_URL: &str = "https://catalog.lenso.dev/v1/plugins.json";
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_BUNDLE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CatalogPluginRelease {
    pub plugin_id: String,
    pub version: String,
    pub summary: String,
    pub bundle_url: String,
    pub bundle_digest: String,
    pub manifest_digest: String,
    pub host_targets: Vec<String>,
    pub execution_classes: Vec<String>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginCatalog {
    schema: String,
    plugins: Vec<CatalogPluginRelease>,
}

pub fn fetch_catalog(url: &str) -> anyhow::Result<Vec<CatalogPluginRelease>> {
    validate_url(url, "Plugin catalog")?;
    let bytes = fetch(url, MAX_CATALOG_BYTES)?;
    parse_catalog(&bytes)
}

pub fn search_catalog(url: &str, query: &str) -> anyhow::Result<Vec<CatalogPluginRelease>> {
    let url = catalog_search_url(url, query)?;
    fetch_catalog(url.as_str())
}

pub fn fetch_catalog_release(
    url: &str,
    plugin_id: &str,
    version: &str,
) -> anyhow::Result<Vec<CatalogPluginRelease>> {
    validate_plugin_id_v1(plugin_id)?;
    validate_release_version(version)?;
    let url = catalog_release_url(url, plugin_id, version)?;
    fetch_catalog(url.as_str())
}

fn catalog_search_url(url: &str, query: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(url).context("Plugin catalog URL is invalid")?;
    if !query.is_empty() {
        url.query_pairs_mut().append_pair("q", query);
    }
    Ok(url)
}

fn catalog_release_url(url: &str, plugin_id: &str, version: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(url).context("Plugin catalog URL is invalid")?;
    url.query_pairs_mut()
        .append_pair("pluginId", plugin_id)
        .append_pair("version", version);
    Ok(url)
}

fn parse_catalog(bytes: &[u8]) -> anyhow::Result<Vec<CatalogPluginRelease>> {
    let catalog: PluginCatalog = serde_json::from_slice(bytes).context("decode Plugin catalog")?;
    if catalog.schema != "lenso.plugin-catalog.v1" {
        bail!("unsupported Plugin catalog schema `{}`", catalog.schema);
    }
    let mut identities = BTreeSet::new();
    for release in &catalog.plugins {
        validate_plugin_id_v1(&release.plugin_id)
            .with_context(|| format!("catalog Plugin id `{}` is invalid", release.plugin_id))?;
        validate_release_version(&release.version).with_context(|| {
            format!(
                "catalog Release version `{}@{}` is invalid",
                release.plugin_id, release.version
            )
        })?;
        if release.summary.trim().is_empty() {
            bail!("Plugin catalog contains an incomplete Release identity");
        }
        if !identities.insert((&release.plugin_id, &release.version)) {
            bail!(
                "Plugin catalog contains duplicate `{}@{}` Releases",
                release.plugin_id,
                release.version
            );
        }
        validate_digest(&release.bundle_digest, "Bundle")?;
        validate_digest(&release.manifest_digest, "Manifest")?;
        validate_url(&release.bundle_url, "Plugin Bundle")?;
        if release.host_targets.is_empty()
            || release.execution_classes.is_empty()
            || release.capabilities.is_empty()
        {
            bail!(
                "Plugin catalog Release `{}@{}` has incomplete compatibility metadata",
                release.plugin_id,
                release.version
            );
        }
    }
    Ok(catalog.plugins)
}

pub fn download_bundle(release: &CatalogPluginRelease, output: &mut File) -> anyhow::Result<()> {
    validate_url(&release.bundle_url, "Plugin Bundle")?;
    let response = request(&release.bundle_url, "Plugin Bundle")?;
    let digest = copy_bounded_and_hash(response.into_reader(), &mut *output, MAX_BUNDLE_BYTES)
        .with_context(|| format!("download {}@{}", release.plugin_id, release.version))?;
    if digest != release.bundle_digest {
        bail!(
            "downloaded Plugin Bundle digest `{digest}` does not match catalog `{}`",
            release.bundle_digest
        );
    }
    output.flush().context("flush downloaded Plugin Bundle")
}

#[cfg(test)]
fn sha256_digest(bytes: &[u8]) -> String {
    render_sha256(Sha256::digest(bytes))
}

fn render_sha256(digest: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = digest.as_ref();
    let mut output = String::with_capacity(71);
    output.push_str("sha256:");
    for &byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn copy_bounded_and_hash(
    mut reader: impl Read,
    mut writer: impl Write,
    limit: u64,
) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = reader.read(&mut buffer).context("read response body")?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(read)?)
            .context("response size overflow")?;
        if total > limit {
            bail!("response exceeds {} MiB", limit / 1024 / 1024);
        }
        hasher.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .context("write response body")?;
    }
    Ok(render_sha256(hasher.finalize()))
}

fn fetch(url: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
    let response = request(url, "Plugin catalog")?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .context("read Plugin catalog response")?;
    if u64::try_from(bytes.len())? > limit {
        bail!(
            "Plugin catalog response exceeds {} MiB",
            limit / 1024 / 1024
        );
    }
    Ok(bytes)
}

fn request(url: &str, label: &str) -> anyhow::Result<ureq::Response> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .redirects(0)
        .build();
    let mut current = Url::parse(url).with_context(|| format!("{label} URL is invalid"))?;
    validate_url(current.as_str(), label)?;
    for redirect_count in 0..=MAX_REDIRECTS {
        let response = agent
            .get(current.as_str())
            .call()
            .map_err(|_| anyhow::anyhow!("{label} request failed"))?;
        validate_url(response.get_url(), "Response")?;
        if !matches!(response.status(), 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        if redirect_count == MAX_REDIRECTS {
            bail!("{label} request exceeded the redirect limit");
        }
        let location = response
            .header("location")
            .context("redirect response omitted Location")?;
        current = current
            .join(location)
            .context("redirect Location is invalid")?;
        validate_url(current.as_str(), "Redirect target")?;
    }
    unreachable!("bounded redirect loop returns or fails")
}

fn validate_digest(value: &str, label: &str) -> anyhow::Result<()> {
    if value.len() != 71
        || !value.starts_with("sha256:")
        || !value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
        || value[7..].bytes().any(|byte| byte.is_ascii_uppercase())
    {
        bail!("{label} digest must be one lowercase SHA-256 digest");
    }
    Ok(())
}

fn validate_url(value: &str, label: &str) -> anyhow::Result<()> {
    let url = Url::parse(value).with_context(|| format!("{label} URL is invalid"))?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("{label} URL must not include user information");
    }
    let is_https = url.scheme() == "https";
    let is_loopback_http = url.scheme() == "http"
        && url.host().is_some_and(|host| match host {
            url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
            url::Host::Ipv4(address) => address.is_loopback(),
            url::Host::Ipv6(address) => address.is_loopback(),
        });
    if is_https || is_loopback_http {
        return Ok(());
    }
    bail!("{label} URL must use HTTPS (loopback HTTP is allowed for development)")
}

#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, net::TcpListener, thread};

    use super::*;

    #[test]
    fn rejects_unknown_catalog_schema() {
        let error = parse_catalog(br#"{"schema":"old","plugins":[]}"#).unwrap_err();
        assert!(error.to_string().contains("unsupported"));
    }

    #[test]
    fn rejects_duplicate_releases() {
        let release = r#"{"pluginId":"example.echo","version":"1.0.0","summary":"Echo","bundleUrl":"https://example.com/echo.lenso-plugin","bundleDigest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","manifestDigest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","hostTargets":["*"],"executionClasses":["lenso.wasm-component@1"],"capabilities":["example.echo@1"]}"#;
        let document =
            format!(r#"{{"schema":"lenso.plugin-catalog.v1","plugins":[{release},{release}]}}"#);
        let error = parse_catalog(document.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn search_url_preserves_custom_parameters_and_encodes_the_query() {
        let url = catalog_search_url(
            "https://example.com/v1/plugins.json?channel=preview",
            "agent tools/zh",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.com/v1/plugins.json?channel=preview&q=agent+tools%2Fzh"
        );
    }

    #[test]
    fn exact_release_url_preserves_custom_parameters_and_encodes_identity() {
        let url = catalog_release_url(
            "https://example.com/v1/plugins.json?channel=preview",
            "company.support-bot",
            "1.2.3-rc.1+build.7",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.com/v1/plugins.json?channel=preview&pluginId=company.support-bot&version=1.2.3-rc.1%2Bbuild.7"
        );
    }

    #[test]
    fn rejects_noncanonical_catalog_identity() {
        let release = r#"{"pluginId":"uppercase","version":"1.0","summary":"Echo","bundleUrl":"https://example.com/echo.lenso-plugin","bundleDigest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","manifestDigest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","hostTargets":["*"],"executionClasses":["lenso.wasm-component@1"],"capabilities":["example.echo@1"]}"#;
        let document = format!(r#"{{"schema":"lenso.plugin-catalog.v1","plugins":[{release}]}}"#);
        let error = parse_catalog(document.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("catalog Plugin id"));
    }

    #[test]
    fn loopback_http_validation_uses_the_parsed_host() {
        assert!(validate_url("http://localhost:8787/catalog", "Catalog").is_ok());
        assert!(validate_url("http://127.0.0.2:8787/catalog", "Catalog").is_ok());
        assert!(validate_url("http://[::1]:8787/catalog", "Catalog").is_ok());
        let deceptive_user_info = format!("http://localhost:8787{}evil.example/catalog", '@');
        assert!(validate_url(&deceptive_user_info, "Catalog").is_err());
        assert!(validate_url("http://127.0.0.1.evil.example/catalog", "Catalog").is_err());
    }

    #[test]
    fn catalog_urls_reject_user_information_without_echoing_credentials() {
        for url in [
            "https://alice:secret@example.com/catalog",
            "http://alice:secret@localhost:8787/catalog",
        ] {
            let error = validate_url(url, "Catalog").unwrap_err().to_string();
            assert!(error.contains("must not include user information"));
            assert!(!error.contains("alice"));
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn redirect_user_info_is_rejected_before_the_target_is_contacted() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let source = TcpListener::bind("127.0.0.1:0").unwrap();
        let source_address = source.local_addr().unwrap();
        let target_address = target.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = source.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: http://user:super-secret@{target_address}/bundle\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let error = request(&format!("http://{source_address}/catalog"), "Catalog")
            .unwrap_err()
            .to_string();
        server.join().unwrap();

        assert!(!error.contains("super-secret"));
        assert!(!error.contains(&target_address.to_string()));
        assert!(matches!(target.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock));
    }

    #[test]
    fn bounded_copy_streams_and_hashes_without_retaining_the_body() {
        let bytes = b"streamed Plugin Bundle";
        let mut output = Vec::new();
        let digest = copy_bounded_and_hash(bytes.as_slice(), &mut output, bytes.len() as u64)
            .expect("bounded copy should succeed");
        assert_eq!(output, bytes);
        assert_eq!(digest, sha256_digest(bytes));
    }

    #[test]
    fn bounded_copy_rejects_the_first_chunk_over_the_limit() {
        let mut output = Vec::new();
        let error = copy_bounded_and_hash(b"oversized".as_slice(), &mut output, 4).unwrap_err();
        assert!(error.to_string().contains("response exceeds"));
        assert!(output.len() <= 4);
    }
}
