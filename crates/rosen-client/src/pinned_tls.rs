use rosen_protocol::BoxStream;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::{io, sync::Arc, time::Duration};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
type Fingerprint = [u8; 32];
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}
fn fingerprint_of(bytes: &[u8]) -> Fingerprint {
    Sha256::digest(bytes).into()
}
pub async fn connect(address: &str, pin: Fingerprint) -> io::Result<BoxStream> {
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertificate::new(pin)))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    // The name is only sent as SNI; the pin, not the name, decides trust.
    let name = ServerName::try_from("rosen-server").map_err(io::Error::other)?;
    let attempt = async {
        let tcp = TcpStream::connect(address).await?;
        tcp.set_nodelay(true)?;
        connector.connect(name, tcp).await
    };
    let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS connect timed out"))??;
    Ok(Box::new(stream))
}

/// Trusts one certificate by fingerprint. Names, issuers and expiry are not
/// checked: the certificate is self-signed and its fingerprint came with the
/// daemon's configuration, so the fingerprint alone identifies the server.
#[derive(Debug)]
struct PinnedCertificate {
    pin: Fingerprint,
    provider: Arc<CryptoProvider>,
}

impl PinnedCertificate {
    fn new(pin: Fingerprint) -> Self {
        Self {
            pin,
            provider: provider(),
        }
    }
}

impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint_of(end_entity.as_ref()) == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    // The handshake signatures are still verified, so a peer that replays
    // the pinned certificate without its private key is refused.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        verify_tls12_signature(message, cert, dss, algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        verify_tls13_signature(message, cert, dss, algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
