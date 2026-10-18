//! Key fingerprints and the device ids they give (docs/M2-DESIGN.md,
//! "Identity").
//!
//! A device's fingerprint is the SHA-256 of its long-term public key, written
//! as 64 lowercase hex characters, and its device id is the first 32 of them.
//! `hop-net` computes fingerprints; this module only knows their format, so
//! that the config file and the network agree on it. Ids only name devices:
//! security checks always compare whole fingerprints.

use hop_types::DeviceId;

/// Characters in a fingerprint: 32 bytes of SHA-256, two hex digits each.
const FINGERPRINT_LEN: usize = 64;

/// Characters of a fingerprint that make up a device id.
const DEVICE_ID_LEN: usize = 32;

/// Whether `text` is a key fingerprint as Hop writes it: exactly 64
/// lowercase hex characters (`0`-`9`, `a`-`f`).
///
/// Uppercase is refused rather than accepted: fingerprints are compared as
/// text, so a second spelling of the same one would not match.
pub fn is_fingerprint(text: &str) -> bool {
    text.len() == FINGERPRINT_LEN
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The device id that goes with a key fingerprint: its first 32 characters.
/// `None` when `fingerprint` is not one (see [`is_fingerprint`]).
///
/// The agent uses it for this device's own id at every start (see
/// [`Config::set_device_id`](crate::Config::set_device_id)), and pairing
/// uses it for the id of the peer it saves.
pub fn device_id_from_fingerprint(fingerprint: &str) -> Option<DeviceId> {
    if !is_fingerprint(fingerprint) {
        return None;
    }
    // Every character is ASCII, so this cannot split one.
    fingerprint.get(..DEVICE_ID_LEN).map(DeviceId::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINGERPRINT: &str = "c41d8e2b9a7f4e06b3d5a1f8e9c2b7d05a3f19c7e2b8d4a60f1e9c3b7d2a8e45";

    #[test]
    fn a_fingerprint_is_64_lowercase_hex_characters() {
        assert!(is_fingerprint(FINGERPRINT));
        assert!(is_fingerprint(&"0".repeat(64)));
        assert!(is_fingerprint(&"f".repeat(64)));

        let upper = FINGERPRINT.to_uppercase();
        let with_space = format!(" {}", &FINGERPRINT[1..]);
        let with_g = format!("g{}", &FINGERPRINT[1..]);
        // 64 bytes, but only 63 characters: "é" takes two bytes.
        let accented = format!("\u{e9}{}", &FINGERPRINT[2..]);
        for bad in [
            "",
            &FINGERPRINT[..63],
            &format!("{FINGERPRINT}0"),
            &upper,
            &with_space,
            &with_g,
            &accented,
            &FINGERPRINT[..32],
        ] {
            assert!(!is_fingerprint(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_device_id_is_the_first_half_of_the_fingerprint() {
        assert_eq!(
            device_id_from_fingerprint(FINGERPRINT),
            Some(DeviceId::new("c41d8e2b9a7f4e06b3d5a1f8e9c2b7d0"))
        );
        assert_eq!(device_id_from_fingerprint(&FINGERPRINT[..63]), None);
        assert_eq!(
            device_id_from_fingerprint(&FINGERPRINT.to_uppercase()),
            None
        );
        assert_eq!(device_id_from_fingerprint(""), None);
    }

    #[test]
    fn odd_input_never_panics() {
        // Multi-byte characters around where the id would be cut.
        let tricky = format!("{}\u{1F4BB}{}", &FINGERPRINT[..31], &FINGERPRINT[..29]);
        assert_eq!(tricky.len(), 64);
        assert_eq!(device_id_from_fingerprint(&tricky), None);
    }
}
