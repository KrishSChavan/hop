//! The commented example config, `config.example.toml`, stays loadable and
//! means what its comments say.

use std::path::Path;

use hop_config::{Config, DeviceConfig, Side};
use hop_types::{DeviceId, Edge, Os};

#[test]
fn the_example_for_a_mac_with_the_pc_on_the_right_loads() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
    let (config, warnings) = Config::load(&path).unwrap();
    assert_eq!(warnings, []);

    assert_eq!(config.device.name, "Krish's MacBook");
    assert_eq!(
        config.device.id,
        DeviceId::new("5f0c9a1e7b3d4c2a8e6f1b9d0a7c3e52")
    );
    assert!(config.device.has_id());

    let [pc] = config.peers.as_slice() else {
        panic!("expected one peer, got {:?}", config.peers);
    };
    assert_eq!(pc.name, "Windows PC");
    assert_eq!(pc.id, DeviceId::new("c41d8e2b9a7f4e06b3d5a1f8e9c2b7d0"));
    assert_eq!(pc.os, Os::Windows);
    assert_eq!(pc.side, Side::Right);
    assert_eq!(pc.side.edge(), Edge::Right);
    assert_eq!(pc.address, None);
    // Paired: the id is the start of the fingerprint, so Hop pins it.
    let fingerprint = "c41d8e2b9a7f4e06b3d5a1f8e9c2b7d05a3f19c7e2b8d4a60f1e9c3b7d2a8e45";
    assert_eq!(pc.fingerprint.as_deref(), Some(fingerprint));
    assert_eq!(pc.pinned_fingerprint(), Some(fingerprint));

    // The comments say every other setting shows its default.
    let mut rest = config.clone();
    rest.device = DeviceConfig::default();
    rest.peers.clear();
    assert_eq!(rest, Config::default());
}
