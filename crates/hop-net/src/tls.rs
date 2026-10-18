//! TLS for Hop's QUIC connections: mutual TLS 1.3 with Ed25519 certificates
//! checked against pinned fingerprints, with no resumption and no early
//! data (docs/M2-DESIGN.md, "Transport").
//!
//! The verifiers here are the security boundary of the whole link. Each one
//! checks two things, and both are needed:
//!
//! 1. the certificate's fingerprint is the expected pin, and
//! 2. the TLS 1.3 handshake signature verifies against that same
//!    certificate.
//!
//! Certificates are public (any device can fetch one by connecting), so the
//! first check alone would let anyone replay a paired laptop's certificate.
//! The signature is what proves the peer holds the pinned private key.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hop_types::DeviceId;
use quinn::crypto::rustls::{HandshakeData, QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, PeerMisbehaved};
use rustls::{SignatureScheme, client::Resumption, server::NoServerSessionStorage};

use crate::fingerprint::cert_is_ed25519;
use crate::{Fingerprint, Identity};

/// ALPN of session connections. It names the kind of connection, not the
/// protocol version, so it stays `hop/1` when `PROTO_MAJOR` changes and
/// mismatched builds still reach `Hello` to tell the user to update.
pub(crate) const ALPN_SESSION: &[u8] = b"hop/1";

/// ALPN of pairing connections, offered only by `hop pair`.
pub(crate) const ALPN_PAIRING: &[u8] = b"hop-pair/1";

/// The server name passed to quinn when dialling. SNI is off, so it never
/// leaves this device, and the verifiers ignore names entirely.
pub(crate) const SERVER_NAME: &str = "hop";

/// The pinned peers a session listener accepts, by full fingerprint.
///
/// It is built once and never changes while the link runs, so the TLS
/// verifiers (which run inside quinn's connection locks) never take a lock
/// of Hop's.
#[derive(Debug, Default)]
pub(crate) struct PinSet {
    by_fingerprint: HashMap<Fingerprint, DeviceId>,
}

impl PinSet {
    /// A pin set of `(fingerprint, id)` pairs.
    pub(crate) fn new(pins: impl IntoIterator<Item = (Fingerprint, DeviceId)>) -> PinSet {
        PinSet {
            by_fingerprint: pins.into_iter().collect(),
        }
    }

    /// The pinned device with exactly this fingerprint.
    pub(crate) fn lookup(&self, fingerprint: &Fingerprint) -> Option<&DeviceId> {
        self.by_fingerprint.get(fingerprint)
    }
}

/// Counts of verifier calls. The tests use them to prove that every
/// connection runs both checks (so no session was resumed) and that
/// rejected handshakes were rejected by the pin.
#[derive(Debug, Default)]
pub(crate) struct VerifierStats {
    /// Certificate checks.
    pub(crate) certificates: AtomicUsize,
    /// Handshake signature checks.
    pub(crate) signatures: AtomicUsize,
    /// Checks of either kind that failed.
    pub(crate) rejected: AtomicUsize,
}

impl VerifierStats {
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> (usize, usize, usize) {
        (
            self.certificates.load(Ordering::Relaxed),
            self.signatures.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
        )
    }
}

/// Which peer certificates a verifier accepts.
#[derive(Clone, Debug)]
pub(crate) enum Accept {
    /// The session listener: any pinned peer.
    Pinned(Arc<PinSet>),
    /// The session dialler: exactly the peer being dialled, never just any
    /// pinned peer.
    Exactly(Fingerprint),
    /// Pairing only: any single Ed25519 certificate. Its fingerprint is
    /// read after the handshake and becomes the SPAKE2 identity, and the
    /// signature check still proves the peer holds that key.
    AnyEd25519,
}

impl Accept {
    fn allows(&self, fingerprint: &Fingerprint) -> bool {
        match self {
            Accept::Pinned(pins) => pins.lookup(fingerprint).is_some(),
            Accept::Exactly(expected) => expected == fingerprint,
            Accept::AnyEd25519 => true,
        }
    }
}

/// The one verifier type, used for both directions.
#[derive(Debug)]
pub(crate) struct PinVerifier {
    accept: Accept,
    /// The ring provider's signature algorithms. Only the Ed25519 entry is
    /// ever used, because every other scheme is refused before lookup.
    algorithms: WebPkiSupportedAlgorithms,
    stats: Arc<VerifierStats>,
}

impl PinVerifier {
    pub(crate) fn new(accept: Accept, stats: Arc<VerifierStats>) -> PinVerifier {
        PinVerifier {
            accept,
            algorithms: provider().signature_verification_algorithms,
            stats,
        }
    }

    /// Exactly one Ed25519 certificate whose fingerprint is accepted.
    /// Names, intermediates, dates and extensions are ignored: the pin is
    /// the whole trust decision.
    fn check_certificate(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), rustls::Error> {
        self.stats.certificates.fetch_add(1, Ordering::Relaxed);
        let result = if !intermediates.is_empty() || !cert_is_ed25519(end_entity) {
            Err(rejected())
        } else {
            match Fingerprint::of_cert(end_entity) {
                Ok(fingerprint) if self.accept.allows(&fingerprint) => Ok(()),
                Ok(_) => Err(rejected()),
                Err(_) => Err(rustls::Error::InvalidCertificate(
                    CertificateError::BadEncoding,
                )),
            }
        };
        if result.is_err() {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Verifies the TLS 1.3 handshake signature against `cert`, which rustls
    /// guarantees is the certificate `check_certificate` accepted. The pin
    /// is checked again anyway, so this function is safe on its own.
    fn check_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.stats.signatures.fetch_add(1, Ordering::Relaxed);
        let result = if dss.scheme != SignatureScheme::ED25519 {
            Err(PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme.into())
        } else {
            match Fingerprint::of_cert(cert) {
                Ok(fingerprint) if self.accept.allows(&fingerprint) => {
                    verify_tls13_signature(message, cert, dss, &self.algorithms)
                }
                _ => Err(rejected()),
            }
        };
        if result.is_err() {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check_certificate(end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // TLS 1.2 is not even compiled in; refuse it here as well.
        Err(tls12_refused())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.check_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

impl ClientCertVerifier for PinVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // No CA hints: there is no CA, and the hints would only tell a
        // stranger what Hop expects.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check_certificate(end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(tls12_refused())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.check_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

fn rejected() -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

fn tls12_refused() -> rustls::Error {
    rustls::Error::General("TLS 1.2 is not supported".to_owned())
}

/// The ring crypto provider. aws-lc-rs, rustls's default, would need CMake
/// and NASM to build on the PC. Without rustls's `tls12` feature, its cipher
/// suites are TLS 1.3 only.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A rustls server config: TLS 1.3 only, a mandatory client certificate
/// checked by `accept`, one ALPN, and no resumption or early data.
pub(crate) fn server_config(
    identity: &Identity,
    accept: Accept,
    alpn: &[u8],
    stats: Arc<VerifierStats>,
) -> Result<rustls::ServerConfig, rustls::Error> {
    let verifier = Arc::new(PinVerifier::new(accept, stats));
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert()], identity.key())?;
    config.alpn_protocols = vec![alpn.to_vec()];
    // A resumed handshake skips both verifiers, so a pin removed while Hop
    // runs would not stop a laptop that connected before: store no
    // sessions and send no tickets. Early data could be replayed.
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    config.max_early_data_size = 0;
    // `key_log` keeps its default, which logs nothing. rustls's
    // `KeyLogFile` would honour SSLKEYLOGFILE and write session secrets to
    // disk, so it is never used.
    Ok(config)
}

/// A rustls client config: TLS 1.3 only, this device's certificate, the
/// server checked by `accept`, one ALPN, no SNI, no resumption and no early
/// data.
pub(crate) fn client_config(
    identity: &Identity,
    accept: Accept,
    alpn: &[u8],
    stats: Arc<VerifierStats>,
) -> Result<rustls::ClientConfig, rustls::Error> {
    let verifier = Arc::new(PinVerifier::new(accept, stats));
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert()], identity.key())?;
    config.alpn_protocols = vec![alpn.to_vec()];
    config.resumption = Resumption::disabled();
    config.enable_early_data = false;
    // No host name travels in the clear.
    config.enable_sni = false;
    Ok(config)
}

/// Wraps a rustls server config for quinn.
pub(crate) fn quic_server(
    tls: rustls::ServerConfig,
    transport: Arc<quinn::TransportConfig>,
    max_incoming: usize,
) -> Result<quinn::ServerConfig, rustls::Error> {
    let crypto =
        QuicServerConfig::try_from(tls).map_err(|err| rustls::Error::General(err.to_string()))?;
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    server
        .transport_config(transport)
        .max_incoming(max_incoming)
        // Packets that arrive for a connection attempt before Hop accepts
        // it are buffered; quinn's defaults (10 MiB each, 100 MiB in all)
        // are far more than a handshake needs.
        .incoming_buffer_size(64 * 1024)
        .incoming_buffer_size_total(1024 * 1024);
    Ok(server)
}

/// Wraps a rustls client config for quinn.
pub(crate) fn quic_client(
    tls: rustls::ClientConfig,
    transport: Arc<quinn::TransportConfig>,
) -> Result<quinn::ClientConfig, rustls::Error> {
    let crypto =
        QuicClientConfig::try_from(tls).map_err(|err| rustls::Error::General(err.to_string()))?;
    let mut client = quinn::ClientConfig::new(Arc::new(crypto));
    client.transport_config(transport);
    Ok(client)
}

/// The fingerprint of a connected peer, read again from the finished
/// handshake as a second line of defence: exactly one Ed25519 certificate,
/// or `None`.
pub(crate) fn peer_fingerprint(conn: &quinn::Connection) -> Option<Fingerprint> {
    let identity = conn.peer_identity()?;
    let certs = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    let [cert] = certs.as_slice() else {
        return None;
    };
    if !cert_is_ed25519(cert) {
        return None;
    }
    Fingerprint::of_cert(cert).ok()
}

/// Whether the finished handshake negotiated exactly `alpn`.
pub(crate) fn negotiated_alpn_is(conn: &quinn::Connection, alpn: &[u8]) -> bool {
    conn.handshake_data()
        .and_then(|data| data.downcast::<HandshakeData>().ok())
        .and_then(|data| data.protocol)
        .is_some_and(|protocol| protocol == alpn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_build_for_every_mode() {
        let identity = Identity::generate().unwrap();
        let stats = Arc::new(VerifierStats::default());
        let pins = Arc::new(PinSet::new([(
            identity.fingerprint(),
            identity.device_id(),
        )]));
        for accept in [
            Accept::Pinned(pins),
            Accept::Exactly(identity.fingerprint()),
            Accept::AnyEd25519,
        ] {
            let server =
                server_config(&identity, accept.clone(), ALPN_SESSION, stats.clone()).unwrap();
            assert_eq!(server.max_early_data_size, 0);
            assert_eq!(server.send_tls13_tickets, 0);
            assert_eq!(server.alpn_protocols, vec![ALPN_SESSION.to_vec()]);
            let client = client_config(&identity, accept, ALPN_PAIRING, stats.clone()).unwrap();
            assert!(!client.enable_early_data);
            assert!(!client.enable_sni);
            assert_eq!(client.alpn_protocols, vec![ALPN_PAIRING.to_vec()]);
        }
    }

    #[test]
    fn the_provider_offers_only_tls13_suites() {
        for suite in &provider().cipher_suites {
            assert!(suite.tls13().is_some(), "{:?}", suite.suite());
        }
    }

    #[test]
    fn certificate_checks_follow_the_pin() {
        let pinned = Identity::generate().unwrap();
        let stranger = Identity::generate().unwrap();
        let stats = Arc::new(VerifierStats::default());
        let verifier = PinVerifier::new(Accept::Exactly(pinned.fingerprint()), stats.clone());
        assert!(verifier.check_certificate(&pinned.cert(), &[]).is_ok());
        assert!(verifier.check_certificate(&stranger.cert(), &[]).is_err());
        assert!(
            verifier
                .check_certificate(&pinned.cert(), &[stranger.cert()])
                .is_err(),
            "exactly one certificate"
        );
        let any = PinVerifier::new(Accept::AnyEd25519, stats.clone());
        assert!(any.check_certificate(&stranger.cert(), &[]).is_ok());
        assert_eq!(stats.snapshot(), (4, 0, 2));
    }

    #[test]
    fn non_ed25519_certificates_are_refused_even_in_pairing_mode() {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = rcgen::CertificateParams::default()
            .self_signed(&key)
            .unwrap()
            .der()
            .clone();
        let verifier = PinVerifier::new(Accept::AnyEd25519, Arc::default());
        assert!(verifier.check_certificate(&cert, &[]).is_err());
    }
}
