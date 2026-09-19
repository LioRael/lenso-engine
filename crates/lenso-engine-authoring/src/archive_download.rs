//! Target-owned transport policy. Catalog URLs do not grant network authority.
use crate::bundle_archive::{PluginArchiveIdentity, PluginReleaseIdentity, VerifiedPluginArchive};
use anyhow::{Context, bail, ensure};
use std::{
    collections::BTreeSet,
    io,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    time::Duration,
};
use url::Url;

/// Explicit HTTPS origins admitted by the target operator, independently of a
/// catalog. Redirects, proxies and private-network destinations are forbidden.
#[derive(Clone, Debug)]
pub struct PluginArchiveDownloadPolicy {
    origins: BTreeSet<String>,
}

impl PluginArchiveDownloadPolicy {
    pub fn new(origins: &[String]) -> anyhow::Result<Self> {
        ensure!(
            !origins.is_empty() && origins.len() <= 16,
            "expected 1 to 16 archive origins"
        );
        let mut allowed = BTreeSet::new();
        for origin in origins {
            let url = checked_url(origin)?;
            ensure!(
                url.path() == "/" && url.query().is_none(),
                "archive policy requires an origin, not a path or query"
            );
            allowed.insert(url.origin().ascii_serialization());
        }
        Ok(Self { origins: allowed })
    }

    /// Fetches into private verified storage, without mutating a target App.
    /// Callers verify catalog trust/freshness and target authorization first.
    /// HTTP transport has a 60s deadline; system DNS resolution is OS-managed.
    pub fn download_release(
        &self,
        url: &str,
        transport: &PluginArchiveIdentity,
        release: &PluginReleaseIdentity,
    ) -> anyhow::Result<VerifiedPluginArchive> {
        let agent = agent_builder().resolver(public_resolve).build();
        self.download_with_agent(url, transport, release, &agent)
    }

    fn download_with_agent(
        &self,
        input: &str,
        transport: &PluginArchiveIdentity,
        release: &PluginReleaseIdentity,
        agent: &ureq::Agent,
    ) -> anyhow::Result<VerifiedPluginArchive> {
        let url = checked_url(input)?;
        ensure!(
            self.origins.contains(&url.origin().ascii_serialization()),
            "archive origin is not admitted by the target"
        );
        // Reject invalid size/digest before any network request. The shared
        // reader performs the same validation again while hashing the stream.
        ensure!(
            transport.size > 0 && transport.size <= 256 * 1024 * 1024,
            "archive size is outside supported bounds"
        );
        let hash = transport
            .sha256
            .strip_prefix("sha256:")
            .context("archive requires SHA-256")?;
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "archive digest is invalid"
        );
        let response = agent
            .get(url.as_str())
            .set("Accept-Encoding", "identity")
            .call()
            .map_err(|_| anyhow::anyhow!("archive HTTPS request failed"))?;
        ensure!(
            response.status() == 200,
            "archive response must be HTTP 200; redirects are not followed"
        );
        ensure!(
            response
                .header("Content-Encoding")
                .is_none_or(|v| v.eq_ignore_ascii_case("identity")),
            "encoded archive responses are not accepted"
        );
        if let Some(length) = response.header("Content-Length") {
            ensure!(
                length.parse::<u64>().ok() == Some(transport.size),
                "archive response length differs from the admitted size"
            );
        }
        VerifiedPluginArchive::read_release(response.into_reader(), transport, release)
    }
}

fn agent_builder() -> ureq::AgentBuilder {
    ureq::AgentBuilder::new()
        .https_only(true)
        .redirects(0)
        .try_proxy_from_env(false)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
}

fn checked_url(input: &str) -> anyhow::Result<Url> {
    ensure!(
        input.len() <= 2048 && !input.chars().any(char::is_control),
        "archive URL exceeds bounds or contains controls"
    );
    let url = Url::parse(input).context("invalid archive URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "expected credential-free HTTPS archive URL"
    );
    if let Some(host) = url.host() {
        let address = match host {
            url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
            url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
            url::Host::Domain(_) => None,
        };
        if address.is_some_and(|ip| !public_address(ip)) {
            bail!("archive URL names a non-public network address");
        }
    }
    Ok(url)
}

fn public_resolve(netloc: &str) -> io::Result<Vec<SocketAddr>> {
    let addresses: Vec<_> = netloc.to_socket_addrs()?.take(65).collect();
    checked_addresses(addresses)
}

fn checked_addresses(addresses: Vec<SocketAddr>) -> io::Result<Vec<SocketAddr>> {
    if addresses.is_empty()
        || addresses.len() > 64
        || addresses
            .iter()
            .any(|address| !public_address(address.ip()))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "archive DNS resolved to an empty or non-public address set",
        ));
    }
    // These exact checked addresses go to the connector: no second DNS lookup.
    Ok(addresses)
}

// Conservative public-address subset, reviewed against the IANA IPv4/IPv6
// special-purpose registries. Some globally reachable protocol exceptions are
// intentionally denied; this is a download policy, not a routing classifier.
fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(matches!(a, 0 | 10 | 127)
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192
                    && (b == 168 || (b == 0 && matches!(c, 0 | 2)) || (b == 88 && c == 99)))
                || (a == 198 && (matches!(b, 18 | 19) || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            (0x2000..0x4000).contains(&a)
                && !(a == 0x2001 && (b < 0x200 || b == 0xdb8))
                && a != 0x2002
                && a != 0x3fff
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Arc,
        thread,
    };

    fn identity(bytes: &[u8]) -> PluginArchiveIdentity {
        PluginArchiveIdentity {
            size: bytes.len() as u64,
            sha256: {
                use std::fmt::Write as _;
                let mut digest = String::from("sha256:");
                for byte in Sha256::digest(bytes) {
                    write!(digest, "{byte:02x}").unwrap();
                }
                digest
            },
        }
    }
    fn release() -> PluginReleaseIdentity {
        PluginReleaseIdentity {
            plugin_id: "example.echo".into(),
            release_version: "1.0.0".into(),
            manifest_digest: format!("sha256:{}", "a".repeat(64)),
        }
    }

    #[test]
    fn origin_policy_and_dns_reject_private_and_ambiguous_destinations() {
        for url in [
            "http://archive.example",
            "https://user:secret@archive.example",
            "https://archive.example/#fragment",
            "https://archive.example/path",
            "https://archive.example/?q=x",
            "https://127.0.0.1",
            "https://[::ffff:127.0.0.1]",
            "https://2130706433",
            "https://archive.example\n",
        ] {
            assert!(
                PluginArchiveDownloadPolicy::new(&[url.into()]).is_err(),
                "{url}"
            );
        }
        for address in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.31.1.1",
            "192.168.1.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2002:7f00:1::1",
            "3fff::1",
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(!public_address(ip), "{address}");
            assert!(checked_addresses(vec![SocketAddr::new(ip, 443)]).is_err());
        }
        let public = "8.8.8.8:443".parse().unwrap();
        assert_eq!(checked_addresses(vec![public]).unwrap(), vec![public]);
        assert!(checked_addresses(vec![]).is_err());
        assert!(checked_addresses(vec![public; 65]).is_err());
        assert!(checked_addresses(vec![public, "127.0.0.1:443".parse().unwrap()]).is_err());
        assert!(public_address("2606:4700:4700::1111".parse().unwrap()));
        let policy = PluginArchiveDownloadPolicy::new(&["https://archive.example".into()]).unwrap();
        let error = policy
            .download_release(
                "https://other.example/file",
                &identity(b"archive"),
                &release(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("not admitted"));
        assert!(
            policy
                .download_release(
                    "https://archive.example:444/file",
                    &identity(b"archive"),
                    &release()
                )
                .unwrap_err()
                .to_string()
                .contains("not admitted")
        );
    }

    struct Server {
        url: String,
        agent: ureq::Agent,
        worker: thread::JoinHandle<()>,
    }

    // Test-only transport pins archive.example to this TLS fixture and trusts
    // its ephemeral CA. Production always uses checked public DNS + system roots.
    fn serve(status: &str, headers: &str, bytes: Vec<u8>, delay: Duration) -> Server {
        let certificate =
            rcgen::generate_simple_self_signed(vec!["archive.example".into()]).unwrap();
        let cert = certificate.cert.der().clone();
        let key =
            rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())
        .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("https://archive.example:{}/archive", addr.port());
        let response = format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n");
        let worker = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(Arc::new(config)).unwrap(),
                socket,
            );
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                let mut byte = [0];
                if stream.read_exact(&mut byte).is_err() {
                    return;
                }
                request.push(byte[0]);
            }
            thread::sleep(delay);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
            stream.conn.send_close_notify();
            let _ = stream.flush();
        });
        Server {
            url,
            agent: agent_builder()
                .resolver(move |_: &str| Ok(vec![addr]))
                .tls_config(Arc::new(client))
                .timeout(Duration::from_millis(300))
                .build(),
            worker,
        }
    }

    fn fixture_policy(url: &str) -> PluginArchiveDownloadPolicy {
        PluginArchiveDownloadPolicy::new(&[Url::parse(url).unwrap().origin().ascii_serialization()])
            .unwrap()
    }

    #[test]
    fn https_download_rejects_redirects_wrong_lengths_encodings_and_modified_bytes() {
        let cases = [
            (
                "302 Found",
                "Location: https://other.example/redirect\r\n",
                b"archive".to_vec(),
                "redirects",
            ),
            (
                "200 OK",
                "Content-Length: 99\r\n",
                b"archive".to_vec(),
                "length",
            ),
            (
                "200 OK",
                "Content-Encoding: gzip\r\n",
                b"archive".to_vec(),
                "encoded",
            ),
            (
                "200 OK",
                "",
                b"too many bytes".to_vec(),
                "exceeds expected size",
            ),
            ("200 OK", "", b"changed".to_vec(), "digest mismatch"),
        ];
        for (status, headers, bytes, message) in cases {
            let server = serve(status, headers, bytes, Duration::ZERO);
            let error = fixture_policy(&server.url)
                .download_with_agent(
                    &server.url,
                    &identity(b"archive"),
                    &release(),
                    &server.agent,
                )
                .unwrap_err();
            server.worker.join().unwrap();
            assert!(error.to_string().contains(message), "{error:#}");
        }
        // A valid TLS connection is required; production trust must not accept
        // the ephemeral test CA merely because its origin was allowlisted.
        let server = serve("200 OK", "", b"archive".to_vec(), Duration::ZERO);
        let port = Url::parse(&server.url).unwrap().port().unwrap();
        let untrusted_agent = agent_builder()
            .resolver(move |_: &str| Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))]))
            .build();
        assert!(
            fixture_policy(&server.url)
                .download_with_agent(
                    &server.url,
                    &identity(b"archive"),
                    &release(),
                    &untrusted_agent
                )
                .is_err()
        );
        server.worker.join().unwrap();
        let server = serve(
            "200 OK",
            "",
            b"archive".to_vec(),
            Duration::from_millis(500),
        );
        assert!(
            fixture_policy(&server.url)
                .download_with_agent(
                    &server.url,
                    &identity(b"archive"),
                    &release(),
                    &server.agent
                )
                .is_err()
        );
        server.worker.join().unwrap();
    }

    #[test]
    #[ignore = "requires LENSO_TEST_PLUGIN_ARCHIVE from current CLI pack"]
    fn https_download_returns_only_the_exact_verified_release() {
        let bytes = std::fs::read(std::env::var("LENSO_TEST_PLUGIN_ARCHIVE").unwrap()).unwrap();
        let archive = VerifiedPluginArchive::read(bytes.as_slice(), &identity(&bytes)).unwrap();
        let expected = PluginReleaseIdentity {
            plugin_id: archive.bundle().plugin_id.clone(),
            release_version: archive.bundle().release_version.clone(),
            manifest_digest: archive.bundle().manifest_digest.clone(),
        };
        let server = serve(
            "200 OK",
            &format!("Content-Length: {}\r\n", bytes.len()),
            bytes.clone(),
            Duration::ZERO,
        );
        let result = fixture_policy(&server.url)
            .download_with_agent(&server.url, &identity(&bytes), &expected, &server.agent)
            .unwrap();
        server.worker.join().unwrap();
        assert_eq!(result.bundle().manifest_digest, expected.manifest_digest);
        let mut retained = Vec::new();
        result
            .open_archive()
            .unwrap()
            .read_to_end(&mut retained)
            .unwrap();
        assert_eq!(retained, bytes);
    }
}
