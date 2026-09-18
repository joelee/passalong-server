//! TLS: the server's pair, read again when it changes on disk; pins; the
//! self-signed pair of `passalong-server tls self-signed`; and the client
//! side, which trusts one pinned public key and nothing else.
//!
//! A pin is `sha256/` and the base64 of the SHA-256 of the certificate's
//! SubjectPublicKeyInfo: the form of HPKP, of `curl --pinnedpubkey`, and of
//! the client's `tls_pin`. It names the key, not the certificate, so a
//! renewal that keeps the key keeps the pin.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use passalong_server_core::clock::rfc3339;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};

/// How long a certificate of `tls self-signed` is good for. Clients go by
/// the pin, which does not expire; this only has to outlast the machine.
const SELF_SIGNED_YEARS: i32 = 10;

/// What went wrong, in words for the operator. Never key material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsError(pub String);

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TlsError {}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

const VERSIONS: &[&rustls::SupportedProtocolVersion] =
    &[&rustls::version::TLS12, &rustls::version::TLS13];

// ---------- pins ----------

fn spki_sha256(certificate: &CertificateDer<'_>) -> Result<[u8; 32], TlsError> {
    let parsed = rustls::server::ParsedCertificate::try_from(certificate)
        .map_err(|err| TlsError(format!("not a certificate: {err}")))?;
    Ok(Sha256::digest(parsed.subject_public_key_info().as_ref()).into())
}

/// The pin of a certificate.
///
/// # Errors
///
/// When it is not an X.509 certificate.
pub fn pin_of(certificate: &CertificateDer<'_>) -> Result<String, TlsError> {
    Ok(format!(
        "sha256/{}",
        STANDARD.encode(spki_sha256(certificate)?)
    ))
}

/// The pin of the first certificate of a PEM file's text: the server's own,
/// which is what a client meets.
///
/// # Errors
///
/// When there is no certificate in it.
pub fn pin_of_pem(pem: &[u8]) -> Result<String, TlsError> {
    let first = CertificateDer::pem_slice_iter(pem)
        .next()
        .ok_or_else(|| TlsError("no certificate in it".to_owned()))?
        .map_err(|err| TlsError(format!("not PEM: {err}")))?;
    pin_of(&first)
}

fn parse_pin(pin: &str) -> Result<[u8; 32], TlsError> {
    let wrong = || TlsError("a pin is `sha256/` and 44 characters of base64".to_owned());
    let encoded = pin.trim().strip_prefix("sha256/").ok_or_else(wrong)?;
    let bytes = STANDARD.decode(encoded).map_err(|_| wrong())?;
    bytes.try_into().map_err(|_| wrong())
}

// ---------- a self-signed pair ----------

/// A certificate and its private key, as PEM.
pub struct Pair {
    /// The certificate.
    pub cert_pem: String,
    /// The private key. Whoever holds this writes it 0600 and never logs it.
    pub key_pem: String,
}

/// A self-signed pair for `names`: host names and IP addresses, as clients
/// will type them. Valid from the day before `now`, for clocks that differ.
///
/// # Errors
///
/// When a name is not one, or there is none.
pub fn self_signed(names: &[String], now: u64) -> Result<Pair, TlsError> {
    if names.is_empty() {
        return Err(TlsError("a certificate needs at least one name".to_owned()));
    }
    if let Some(odd) = names.iter().find(|name| !is_host(name)) {
        return Err(TlsError(format!(
            "{odd:?} is neither a host name nor an IP address"
        )));
    }
    let failed = |err: rcgen::Error| TlsError(format!("cannot make the certificate: {err}"));
    let mut params = rcgen::CertificateParams::new(names.to_vec()).map_err(failed)?;
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, names[0].as_str());
    params.distinguished_name = name;
    let (year, month, day) = ymd(now.saturating_sub(86_400));
    params.not_before = rcgen::date_time_ymd(year, month, day);
    // The 29th of February has no anniversary in most years.
    params.not_after = rcgen::date_time_ymd(year + SELF_SIGNED_YEARS, month, day.min(28));
    let key = rcgen::KeyPair::generate().map_err(failed)?;
    let certificate = params.self_signed(&key).map_err(failed)?;
    Ok(Pair {
        cert_pem: certificate.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// An IP address, or a DNS name as clients type one: labels of letters,
/// digits, and inner hyphens. The certificate library takes any text.
fn is_host(name: &str) -> bool {
    let label = |label: &str| {
        (1..=63).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    name.parse::<std::net::IpAddr>().is_ok() || (name.len() <= 253 && name.split('.').all(label))
}

/// The date of a Unix time, by way of the one formatter the server has.
fn ymd(unix: u64) -> (i32, u8, u8) {
    let text = rfc3339(unix);
    let part = |range: std::ops::Range<usize>| text.get(range).and_then(|t| t.parse::<i32>().ok());
    let year = part(0..4).unwrap_or(1970);
    let month = part(5..7).and_then(|m| u8::try_from(m).ok()).unwrap_or(1);
    let day = part(8..10).and_then(|d| u8::try_from(d).ok()).unwrap_or(1);
    (year, month, day)
}

// ---------- the server's side ----------

/// Reads a pair into a server configuration: TLS 1.2 and 1.3, HTTP/1.1.
///
/// # Errors
///
/// When a file cannot be read, holds nothing usable, or the key is not the
/// certificate's.
pub fn load(cert_file: &Path, key_file: &Path) -> Result<Arc<ServerConfig>, TlsError> {
    let certificates = CertificateDer::pem_file_iter(cert_file)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|err| TlsError(format!("{}: {err}", cert_file.display())))?;
    if certificates.is_empty() {
        return Err(TlsError(format!(
            "{}: no certificate in it",
            cert_file.display()
        )));
    }
    let key = PrivateKeyDer::from_pem_file(key_file)
        .map_err(|err| TlsError(format!("{}: {err}", key_file.display())))?;
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(VERSIONS)
        .map_err(|err| TlsError(err.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|err| {
            TlsError(format!(
                "{} and {} are not a pair: {err}",
                cert_file.display(),
                key_file.display()
            ))
        })?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// What tells that a file was replaced.
type Stamp = [Option<(SystemTime, u64)>; 2];

/// The server's pair, read again when the files change (PLAN-00004 D-06):
/// certbot and its kind need no hook. A pair that does not load is logged,
/// and the one before it stays in use.
pub struct Reloading {
    files: [PathBuf; 2],
    current: RwLock<Arc<ServerConfig>>,
    seen: Mutex<Stamp>,
}

impl Reloading {
    /// Loads the pair.
    ///
    /// # Errors
    ///
    /// As [`load`].
    pub fn open(cert_file: &Path, key_file: &Path) -> Result<Self, TlsError> {
        let files = [cert_file.to_owned(), key_file.to_owned()];
        let seen = stamp(&files);
        Ok(Self {
            current: RwLock::new(load(cert_file, key_file)?),
            seen: Mutex::new(seen),
            files,
        })
    }

    /// The configuration new connections get.
    pub fn current(&self) -> Arc<ServerConfig> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Looks at the files and, if they changed, reads them. Reads files: to
    /// be called from the blocking pool.
    pub fn look(&self) {
        let now = stamp(&self.files);
        {
            let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            if *seen == now {
                return;
            }
            // Remembered whether or not it loads: a broken pair is reported
            // once, not every half minute. A renewal writes two files, and
            // seen between them they are no pair; the second write changes
            // the stamp again.
            *seen = now;
        }
        match load(&self.files[0], &self.files[1]) {
            Ok(config) => {
                *self.current.write().unwrap_or_else(PoisonError::into_inner) = config;
                tracing::info!(target: "passalong_server::tls", "the certificate changed on disk and was read again");
            }
            Err(err) => {
                tracing::warn!(target: "passalong_server::tls", %err, "the certificate changed on disk and cannot be used; the one before it stays");
            }
        }
    }
}

fn stamp(files: &[PathBuf; 2]) -> Stamp {
    let of = |file: &PathBuf| {
        let meta = std::fs::metadata(file).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    };
    [of(&files[0]), of(&files[1])]
}

// ---------- the client's side ----------

/// Trusts the server whose public key has the pin, and no other: no
/// authority, no name, no date. The handshake's signature is still checked,
/// so the server has the private key and not just the certificate.
#[derive(Debug)]
struct Pinned {
    pin: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented =
            spki_sha256(end_entity).map_err(|err| rustls::Error::General(err.to_string()))?;
        if presented == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the server's key is not the pinned one".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// A client configuration that connects by `pin` alone.
///
/// # Errors
///
/// When `pin` is not a pin.
pub fn client_config(pin: &str) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = provider();
    let verifier = Pinned {
        pin: parse_pin(pin)?,
        algorithms: provider.signature_verification_algorithms,
    };
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(VERSIONS)
        .map_err(|err| TlsError(err.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::PublicKeyData;

    const NOW: u64 = 1_800_000_000;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_pin_is_the_hash_of_the_public_key_info() {
        let pair = self_signed(&names(&["nas.example", "192.0.2.4", "2001:db8::4"]), NOW).unwrap();
        let pin = pin_of_pem(pair.cert_pem.as_bytes()).unwrap();
        // By another road: the key pair's own account of its public key.
        let key = rcgen::KeyPair::from_pem(&pair.key_pem).unwrap();
        let expected = STANDARD.encode(Sha256::digest(key.subject_public_key_info()));
        assert_eq!(pin, format!("sha256/{expected}"));
        assert_eq!(pin.len(), "sha256/".len() + 44);
        assert_eq!(
            parse_pin(&pin).unwrap().to_vec(),
            Sha256::digest(key.subject_public_key_info()).to_vec()
        );
        assert_eq!(
            parse_pin(&format!("  {pin}\n")).unwrap(),
            parse_pin(&pin).unwrap()
        );
    }

    #[test]
    fn what_is_not_a_pin_or_not_a_certificate_is_refused() {
        for bad in [
            "",
            "sha256/",
            "sha256/AAAA",
            "md5/AAAA",
            "sha256/not base64 at all!",
            &"A".repeat(51),
        ] {
            assert!(parse_pin(bad).is_err(), "{bad:?}");
            assert!(client_config(bad).is_err(), "{bad:?}");
        }
        assert!(pin_of_pem(b"").is_err());
        assert!(
            pin_of_pem(b"-----BEGIN CERTIFICATE-----\nbm8=\n-----END CERTIFICATE-----\n").is_err()
        );
        assert!(self_signed(&[], NOW).is_err());
        for odd in [
            "not a host name",
            "",
            "nas..example",
            "-nas",
            "nas.example.",
            "https://nas",
            "nas:8443",
        ] {
            assert!(
                self_signed(&names(&["ok.example", odd]), NOW).is_err(),
                "{odd:?}"
            );
        }
        for fine in [
            "localhost",
            "nas",
            "my-nas.home.arpa",
            "192.0.2.4",
            "::1",
            "xn--bcher-kva.example",
        ] {
            assert!(is_host(fine), "{fine}");
        }
    }

    #[test]
    fn the_pair_is_valid_from_the_day_before_for_ten_years() {
        assert_eq!(ymd(NOW), (2027, 1, 15));
        assert_eq!(ymd(0), (1970, 1, 1));
        // 2028-02-29T12:00:00Z
        assert_eq!(ymd(1_835_438_400), (2028, 2, 29));
        let pair = self_signed(&names(&["localhost"]), 1_835_438_400 + 86_400).unwrap();
        assert!(pair.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(pair.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn a_pair_loads_and_half_a_pair_or_a_mixed_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        assert!(load(&cert, &key).unwrap_err().0.contains("cert.pem"));
        let pair = self_signed(&names(&["localhost"]), NOW).unwrap();
        std::fs::write(&cert, &pair.cert_pem).unwrap();
        assert!(load(&cert, &key).unwrap_err().0.contains("key.pem"));
        std::fs::write(&key, &pair.key_pem).unwrap();
        let config = load(&cert, &key).unwrap();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);

        let other = self_signed(&names(&["localhost"]), NOW).unwrap();
        std::fs::write(&key, &other.key_pem).unwrap();
        let mixed = load(&cert, &key).unwrap_err();
        assert!(mixed.0.contains("not a pair"), "{mixed}");
        assert!(!mixed.0.contains("PRIVATE"), "{mixed}");
        std::fs::write(&cert, "").unwrap();
        assert!(load(&cert, &key).unwrap_err().0.contains("no certificate"));
    }
}
