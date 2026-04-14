//! TLS support for SPICE link channels.
//!
//! SPICE TLS is straightforward at the protocol level — it's TLS from
//! byte 0, with the link handshake running *inside* the tunnel — but
//! cert verification is the awkward bit. The libvirt-managed CA used by
//! most QEMU deployments is self-signed, so users typically pin via
//! `--ca-file` (a CA bundle to verify against) or `--fingerprint` (a
//! SHA256 of the leaf cert DER, the same shape virt-viewer uses).
//!
//! The connector built here returns a `tokio_rustls::TlsConnector` you
//! can then drive against a freshly-opened `TcpStream`. Because
//! `link_client` already takes a generic `S: AsyncRead + AsyncWrite`,
//! the resulting `TlsStream<TcpStream>` plugs in unchanged.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::{NetError, Result};

/// Either a plain TCP stream or a TLS-wrapped one. SPICE channels are
/// generic over the byte stream, so this enum lets the client and CLI
/// pick the transport at connect time and store the result uniformly.
pub enum SpiceStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl SpiceStream {
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        match self {
            SpiceStream::Plain(s) => s.set_nodelay(nodelay),
            SpiceStream::Tls(s) => s.get_ref().0.set_nodelay(nodelay),
        }
    }
}

impl AsyncRead for SpiceStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            SpiceStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            SpiceStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for SpiceStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            SpiceStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            SpiceStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            SpiceStream::Plain(s) => Pin::new(s).poll_flush(cx),
            SpiceStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            SpiceStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            SpiceStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// How the client should verify the server certificate.
#[derive(Debug, Clone)]
pub enum TlsConfig {
    /// Verify against the OS's system root store. Right for SPICE
    /// servers behind a real CA; rare in practice — most deployments
    /// use the self-signed libvirt CA.
    SystemRoots,
    /// Verify against a CA bundle PEM file. The standard
    /// virt-viewer-style invocation.
    CaFile(String),
    /// Pin a specific SHA256 fingerprint of the server's leaf
    /// certificate DER. 32 bytes.
    Fingerprint([u8; 32]),
    /// Skip cert verification entirely. **Plaintext-equivalent.** A
    /// loud warning is logged. Useful for early bring-up against a
    /// brand-new VM whose cert you haven't grabbed yet.
    Insecure,
}

impl TlsConfig {
    /// Build a `TlsConnector` from this config. Lazily installs the
    /// `ring` crypto provider as the rustls process default the first
    /// time it's called.
    pub fn into_connector(self) -> Result<TlsConnector> {
        install_ring_default();
        let config = match self {
            TlsConfig::SystemRoots => {
                let mut roots = RootCertStore::empty();
                let certs = rustls_native_certs::load_native_certs().map_err(|e| {
                    NetError::Io(std::io::Error::other(format!("load native certs: {e}")))
                })?;
                for cert in certs {
                    let _ = roots.add(cert);
                }
                if roots.is_empty() {
                    return Err(NetError::RsaEncrypt(
                        "no system root certificates available".into(),
                    ));
                }
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth()
            }
            TlsConfig::CaFile(path) => {
                let roots = load_ca_file(&path)?;
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth()
            }
            TlsConfig::Fingerprint(fp) => ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(FingerprintVerifier(fp)))
                .with_no_client_auth(),
            TlsConfig::Insecure => {
                tracing::warn!(
                    "TLS configured with --insecure: certificate verification disabled. \
                     This is plaintext-equivalent against an active attacker."
                );
                ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(InsecureVerifier))
                    .with_no_client_auth()
            }
        };
        Ok(TlsConnector::from(Arc::new(config)))
    }
}

fn install_ring_default() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn load_ca_file(path: &str) -> Result<RootCertStore> {
    let pem = std::fs::read(Path::new(path))
        .map_err(|e| NetError::Io(std::io::Error::other(format!("read {path}: {e}"))))?;
    let mut roots = RootCertStore::empty();
    let mut found = 0usize;
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        let cert = cert
            .map_err(|e| NetError::Io(std::io::Error::other(format!("parse {path}: {e}"))))?;
        if roots.add(cert).is_ok() {
            found += 1;
        }
    }
    if found == 0 {
        return Err(NetError::Io(std::io::Error::other(format!(
            "no CA certificates parsed from {path}"
        ))));
    }
    Ok(roots)
}

#[derive(Debug)]
struct FingerprintVerifier([u8; 32]);

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let actual: [u8; 32] = hasher.finalize().into();
        if actual == self.0 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(RustlsError::General(format!(
                "TLS fingerprint mismatch: expected {}, got {}",
                hex(&self.0),
                hex(&actual)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        all_schemes()
    }
}

#[derive(Debug)]
struct InsecureVerifier;

impl ServerCertVerifier for InsecureVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        all_schemes()
    }
}

fn all_schemes() -> Vec<SignatureScheme> {
    use SignatureScheme::*;
    vec![
        RSA_PKCS1_SHA256,
        RSA_PKCS1_SHA384,
        RSA_PKCS1_SHA512,
        ECDSA_NISTP256_SHA256,
        ECDSA_NISTP384_SHA384,
        ECDSA_NISTP521_SHA512,
        RSA_PSS_SHA256,
        RSA_PSS_SHA384,
        RSA_PSS_SHA512,
        ED25519,
        ED448,
    ]
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Open a TCP connection to `addr` and perform a TLS handshake using
/// `tls`. SPICE TLS uses the bare hostname/IP for SNI, but rustls
/// rejects raw IPs as `ServerName`s when verification is on; for the
/// `Fingerprint` and `Insecure` modes we accept any name, so a literal
/// `"spice"` placeholder works.
pub async fn connect_tls(addr: &str, tls: TlsConfig) -> Result<TlsStream<TcpStream>> {
    let connector = tls.clone().into_connector()?;
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let host = sni_for(addr, &tls);
    let server_name = ServerName::try_from(host)
        .map_err(|e| NetError::Io(io::Error::other(format!("invalid SNI: {e}"))))?;
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| NetError::Io(io::Error::other(format!("tls handshake: {e}"))))?;
    Ok(tls_stream)
}

fn sni_for(addr: &str, tls: &TlsConfig) -> String {
    // For Fingerprint / Insecure modes the name is unchecked, so use a
    // placeholder that's always a valid DNS label. For SystemRoots /
    // CaFile we need the actual host so webpki can match the cert SAN.
    match tls {
        TlsConfig::Fingerprint(_) | TlsConfig::Insecure => "spice".to_string(),
        _ => addr
            .rsplit_once(':')
            .map(|(h, _)| h.trim_matches(['[', ']']).to_string())
            .unwrap_or_else(|| addr.to_string()),
    }
}

/// Parse a SHA256 fingerprint from `aa:bb:cc:..` or unseparated hex.
pub fn parse_fingerprint(s: &str) -> Result<[u8; 32]> {
    let cleaned: String = s.chars().filter(|c| !matches!(*c, ':' | ' ' | '-')).collect();
    if cleaned.len() != 64 {
        return Err(NetError::Io(std::io::Error::other(format!(
            "fingerprint must be 32 hex bytes (64 chars), got {} chars",
            cleaned.len()
        ))));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .map_err(|_| NetError::Io(std::io::Error::other("invalid hex in fingerprint")))?;
    }
    Ok(out)
}
