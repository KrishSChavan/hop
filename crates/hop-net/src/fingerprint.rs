//! Device fingerprints: the SHA-256 pin of a device's public key.

use std::fmt;

use hop_types::DeviceId;
use rustls::pki_types::CertificateDer;
use rustls::server::ParsedCertificate;
use sha2::{Digest, Sha256};

/// Hex characters in a [`DeviceId`]: the first half of the fingerprint.
const ID_HEX_LEN: usize = 32;

/// The DER prefix of every Ed25519 SubjectPublicKeyInfo (RFC 8410): a
/// SEQUENCE of the algorithm identifier 1.3.101.112 and a 32-byte BIT
/// STRING. Ed25519 has no parameters, so this prefix is the only valid
/// encoding.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// The length of an Ed25519 SubjectPublicKeyInfo: the prefix plus the key.
const ED25519_SPKI_LEN: usize = ED25519_SPKI_PREFIX.len() + 32;

/// A device's fingerprint: SHA-256 over the DER SubjectPublicKeyInfo of its
/// certificate, outer SEQUENCE included (the standard SPKI pin of RFC 7469).
///
/// This is what pairing stores and what every connection is checked
/// against. It is written as 64 lowercase hex characters, and the device's
/// [`DeviceId`] is the first 32 of them ([`Fingerprint::device_id`]).
///
/// Fingerprints are public: any device on the network can fetch a
/// certificate by connecting. They identify a key, they are not a secret,
/// and they may be logged.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// The fingerprint of a DER-encoded X.509 certificate.
    ///
    /// This one function computes the fingerprint for this device and for
    /// peers alike, so the two can never disagree about the encoding.
    ///
    /// # Errors
    ///
    /// [`FingerprintError::BadCertificate`] if the bytes are not a
    /// certificate rustls can parse.
    pub fn of_certificate(der: &[u8]) -> Result<Fingerprint, FingerprintError> {
        let cert = CertificateDer::from(der);
        Self::of_cert(&cert).map_err(|_| FingerprintError::BadCertificate)
    }

    /// [`Fingerprint::of_certificate`] for rustls's certificate type,
    /// keeping rustls's error for the TLS verifiers.
    pub(crate) fn of_cert(cert: &CertificateDer<'_>) -> Result<Fingerprint, rustls::Error> {
        let parsed = ParsedCertificate::try_from(cert)?;
        let spki = parsed.subject_public_key_info();
        Ok(Fingerprint(Sha256::digest(spki.as_ref()).into()))
    }

    /// Parses the 64 lowercase hex characters a config file stores.
    ///
    /// # Errors
    ///
    /// [`FingerprintError::BadHex`] for anything else, uppercase included:
    /// the config format allows exactly one spelling, so two spellings can
    /// never name the same key.
    pub fn from_hex(hex: &str) -> Result<Fingerprint, FingerprintError> {
        let bytes = hex.as_bytes();
        if bytes.len() != 64 {
            return Err(FingerprintError::BadHex);
        }
        let mut out = [0u8; 32];
        let (pairs, _) = bytes.as_chunks::<2>();
        for (byte, [high, low]) in out.iter_mut().zip(pairs) {
            *byte = (hex_value(*high)? << 4) | hex_value(*low)?;
        }
        Ok(Fingerprint(out))
    }

    /// The fingerprint as 64 lowercase hex characters.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The device id this fingerprint gives: its first 32 hex characters.
    pub fn device_id(&self) -> DeviceId {
        let mut hex = self.to_hex();
        hex.truncate(ID_HEX_LEN);
        DeviceId::new(hex)
    }

    /// Whether `id` is the device id of this fingerprint.
    pub fn matches_id(&self, id: &DeviceId) -> bool {
        self.device_id() == *id
    }

    /// The 32 raw bytes, as pairing feeds them to SPAKE2.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Builds a fingerprint from raw bytes. Tests use it to make keys that
    /// are not pinned.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Fingerprint {
        Fingerprint(bytes)
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.to_hex())
    }
}

/// Why a fingerprint could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FingerprintError {
    /// The text is not 64 lowercase hex characters.
    #[error("a fingerprint must be 64 lowercase hex characters")]
    BadHex,
    /// The bytes are not a certificate rustls can parse.
    #[error("not a valid X.509 certificate")]
    BadCertificate,
}

/// Whether `spki` (DER, outer SEQUENCE included) holds an Ed25519 key, the
/// only key type Hop accepts.
pub(crate) fn is_ed25519_spki(spki: &[u8]) -> bool {
    spki.len() == ED25519_SPKI_LEN && spki.starts_with(&ED25519_SPKI_PREFIX)
}

/// Whether a certificate's key is Ed25519. A certificate that does not
/// parse is not.
pub(crate) fn cert_is_ed25519(cert: &CertificateDer<'_>) -> bool {
    ParsedCertificate::try_from(cert)
        .is_ok_and(|parsed| is_ed25519_spki(parsed.subject_public_key_info().as_ref()))
}

fn hex_value(digit: u8) -> Result<u8, FingerprintError> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        _ => Err(FingerprintError::BadHex),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_gives_the_id() {
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(i * 7).unwrap();
        }
        let fingerprint = Fingerprint::from_bytes(bytes);
        let hex = fingerprint.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_eq!(Fingerprint::from_hex(&hex), Ok(fingerprint));
        assert_eq!(fingerprint.device_id().as_str(), &hex[..32]);
        assert!(fingerprint.matches_id(&DeviceId::new(&hex[..32])));
        assert!(!fingerprint.matches_id(&DeviceId::new(&hex[..31])));
    }

    #[test]
    fn only_lowercase_64_hex_parses() {
        let good = "ab".repeat(32);
        assert!(Fingerprint::from_hex(&good).is_ok());
        for bad in [
            "AB".repeat(32),
            "ab".repeat(31),
            "ab".repeat(33),
            format!("{}g", "a".repeat(63)),
            format!("{}é", "a".repeat(62)),
            String::new(),
        ] {
            assert_eq!(
                Fingerprint::from_hex(&bad),
                Err(FingerprintError::BadHex),
                "{bad}"
            );
        }
    }

    #[test]
    fn garbage_is_not_a_certificate() {
        assert_eq!(
            Fingerprint::of_certificate(b"not a certificate"),
            Err(FingerprintError::BadCertificate)
        );
        assert!(!cert_is_ed25519(&CertificateDer::from(&b"junk"[..])));
    }

    #[test]
    fn ed25519_spki_shape() {
        let mut spki = ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(&[7; 32]);
        assert!(is_ed25519_spki(&spki));
        spki.push(0);
        assert!(!is_ed25519_spki(&spki));
        assert!(!is_ed25519_spki(&ED25519_SPKI_PREFIX));
    }
}
