//! Finding paired devices on the local network with mDNS (DNS-SD), as
//! `_hop._udp.local.` (docs/M2-DESIGN.md, "Discovery").
//!
//! Everything learned here is unauthenticated. That is safe only because
//! every connection is checked against a pinned key: a spoofed answer can
//! at worst waste a dial.

use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hop_types::DeviceId;
use mdns_sd::{IfKind, IfPredicate, ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::address::is_local_lan;
use crate::sanitize::{MAX_DISPLAY_CHARS, display_text};

/// The DNS-SD service type Hop advertises and browses.
pub const SERVICE_TYPE: &str = "_hop._udp.local.";

/// The TXT record version this build writes and understands.
const TXT_VERSION: &str = "1";

/// Instances kept from browsing; more are ignored (mdns-sd's own cache has
/// no limit, but Hop's view of the network does).
const MAX_INSTANCES: usize = 32;

/// Addresses kept per instance.
const MAX_ADDRESSES: usize = 8;

/// Longest DNS label, in bytes: the limit for an instance name.
const MAX_LABEL_BYTES: usize = 63;

/// What a browse found: one advertised Hop instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundDevice {
    /// The advertised instance name, cleaned for display. Unauthenticated:
    /// anyone can advertise any name.
    pub name: String,
    /// The id from the TXT record. Unauthenticated too; the pin decides.
    pub id: DeviceId,
    /// Whether this is a `hop pair` instance (`pair=1`), advertising the
    /// pairing port.
    pub pairing: bool,
    /// The instance's private or link-local IPv4 addresses with its port,
    /// at most 8.
    pub addresses: Vec<SocketAddrV4>,
}

/// Called with everything found so far, after each change.
pub(crate) type OnChange = Arc<dyn Fn(&[FoundDevice]) + Send + Sync>;

/// Why discovery could not start or advertise.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The mDNS responder could not start or refused a request.
    #[error("mDNS: {0}")]
    Mdns(String),
}

impl From<mdns_sd::Error> for DiscoveryError {
    fn from(error: mdns_sd::Error) -> Self {
        DiscoveryError::Mdns(display_text(&error.to_string(), 120))
    }
}

/// An mDNS responder that advertises this device and browses for others.
///
/// It runs on its own thread (mdns-sd's daemon); a tokio task drains its
/// browse events at once, because the daemon thread blocks while its
/// 10-event channel is full. Loopback, IPv6 and VPN interfaces (`utun*`,
/// `ipsec*`, `ppp*` and point-to-point links) are left out, so Hop is not
/// announced into a work VPN.
pub struct Discovery {
    daemon: ServiceDaemon,
    own_id: DeviceId,
    registered: Mutex<Vec<String>>,
    found: Arc<Mutex<HashMap<String, FoundDevice>>>,
    browser: Option<tokio::task::JoinHandle<()>>,
}

impl fmt::Debug for Discovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Discovery")
            .field("own_id", &self.own_id)
            .finish_non_exhaustive()
    }
}

impl Discovery {
    /// Starts the responder and a browse for [`SERVICE_TYPE`], calling
    /// `on_change` with every instance found (other than this device's own)
    /// after each change. Must be called inside a tokio runtime.
    ///
    /// This sends multicast on the local network; on macOS the first use
    /// asks for Local Network access.
    pub(crate) fn start(
        own_id: DeviceId,
        on_change: OnChange,
    ) -> Result<Discovery, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        // IPv4 only in M2, never loopback, never a VPN tunnel.
        daemon.disable_interface(IfKind::IPv6)?;
        daemon.disable_interface(IfKind::LoopbackV4)?;
        daemon.disable_interface(IfKind::Predicate(IfPredicate::new(|interface| {
            is_tunnel(&interface.name) || interface.is_p2p
        })))?;
        let events = daemon.browse(SERVICE_TYPE)?;
        let found = Arc::new(Mutex::new(HashMap::new()));
        let browser = tokio::spawn(drain_browse_events(
            events,
            own_id.clone(),
            found.clone(),
            on_change,
        ));
        Ok(Discovery {
            daemon,
            own_id,
            registered: Mutex::new(Vec::new()),
            found,
            browser: Some(browser),
        })
    }

    /// Advertises this device: instance `name` (cleaned, at most 63 bytes;
    /// with " (pairing)" appended when `pairing`), host name
    /// `hop-<first 8 of the id>.local.`, TXT `v=1` and `id` (and `pair=1`
    /// when pairing), on `port`.
    ///
    /// The host name is Hop's own, never the computer's: a second
    /// responder claiming the computer's name could make macOS see a
    /// conflict and rename the Mac.
    pub(crate) fn advertise(
        &self,
        name: &str,
        port: u16,
        pairing: bool,
    ) -> Result<(), DiscoveryError> {
        let instance = instance_name(name, pairing);
        let host = host_name(&self.own_id);
        let mut properties = vec![
            ("v".to_owned(), TXT_VERSION.to_owned()),
            ("id".to_owned(), self.own_id.as_str().to_owned()),
        ];
        if pairing {
            properties.push(("pair".to_owned(), "1".to_owned()));
        }
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", port, &properties[..])?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        self.daemon.register(info)?;
        self.registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(fullname);
        Ok(())
    }

    /// Everything found so far, other than this device.
    pub(crate) fn found(&self) -> Vec<FoundDevice> {
        let found = self.found.lock().unwrap_or_else(PoisonError::into_inner);
        let mut devices: Vec<FoundDevice> = found.values().cloned().collect();
        devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        devices
    }

    /// Withdraws the advertisements (mDNS goodbye packets) and stops the
    /// responder, waiting at most `wait` in all.
    pub(crate) async fn shutdown(mut self, wait: Duration) {
        if let Some(browser) = self.browser.take() {
            browser.abort();
        }
        let registered = std::mem::take(
            &mut *self
                .registered
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let goodbyes = async {
            for fullname in registered {
                if let Ok(status) = self.daemon.unregister(&fullname) {
                    let _ = status.recv_async().await;
                }
            }
            if let Ok(status) = self.daemon.shutdown() {
                let _ = status.recv_async().await;
            }
        };
        let _ = tokio::time::timeout(wait, goodbyes).await;
    }

    /// Stops the responder without waiting (the link was dropped without a
    /// shutdown).
    pub(crate) fn stop_now(mut self) {
        if let Some(browser) = self.browser.take() {
            browser.abort();
        }
        let _ = self.daemon.shutdown();
    }
}

impl Drop for Discovery {
    /// mdns-sd's responder thread stops only when told to, so a `Discovery`
    /// dropped without [`Discovery::shutdown`] (a search cancelled while it
    /// listens, or a failed advertisement) stops it here, without goodbyes.
    /// Otherwise it would stay on UDP 5353 for the rest of the process. After
    /// a shutdown, telling it again only returns an error, which is ignored.
    fn drop(&mut self) {
        if let Some(browser) = self.browser.take() {
            browser.abort();
        }
        let _ = self.daemon.shutdown();
    }
}

/// Looks for `hop pair` instances for `wait`, and returns those found other
/// than this device's own. The joiner uses it when no `--address` is given;
/// if more than one laptop advertises pairing, it must ask the user to pick
/// with `--address` rather than choose.
///
/// This sends multicast on the local network; on macOS a command-line tool
/// run from Terminal is exempt from Local Network privacy (Apple TN3179).
///
/// # Errors
///
/// [`DiscoveryError::Mdns`] if the responder cannot start.
pub async fn find_pairing_hosts(
    own_id: DeviceId,
    wait: Duration,
) -> Result<Vec<FoundDevice>, DiscoveryError> {
    let discovery = Discovery::start(own_id, Arc::new(|_: &[FoundDevice]| {}))?;
    tokio::time::sleep(wait).await;
    let hosts: Vec<FoundDevice> = discovery
        .found()
        .into_iter()
        .filter(|device| device.pairing && !device.addresses.is_empty())
        .collect();
    discovery.shutdown(Duration::from_secs(1)).await;
    Ok(hosts)
}

/// Keeps `found` up to date from the daemon's browse events. Runs until the
/// daemon stops.
async fn drain_browse_events(
    events: mdns_sd::Receiver<ServiceEvent>,
    own_id: DeviceId,
    found: Arc<Mutex<HashMap<String, FoundDevice>>>,
    on_change: OnChange,
) {
    while let Ok(event) = events.recv_async().await {
        let changed = match event {
            ServiceEvent::ServiceResolved(service) => {
                let addresses: Vec<Ipv4Addr> = service.get_addresses_v4().into_iter().collect();
                let txt: Vec<(String, String)> = service
                    .txt_properties
                    .iter()
                    .map(|property| (property.key().to_owned(), property.val_str().to_owned()))
                    .collect();
                let device =
                    parse_instance(&service.fullname, service.port, &addresses, &txt, &own_id);
                let mut found = found.lock().unwrap_or_else(PoisonError::into_inner);
                match device {
                    Some(device)
                        if found.contains_key(&service.fullname) || found.len() < MAX_INSTANCES =>
                    {
                        found.insert(service.fullname.clone(), device);
                        true
                    }
                    Some(_) => false,
                    None => found.remove(&service.fullname).is_some(),
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => found
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&fullname)
                .is_some(),
            _ => false,
        };
        if changed {
            let snapshot: Vec<FoundDevice> = found
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .values()
                .cloned()
                .collect();
            on_change(&snapshot);
        }
    }
}

/// Reads a resolved instance, or `None` if it is not a usable Hop instance:
/// a TXT version other than 1, an id that is not 32 lowercase hex
/// characters, this device's own id, or no dialable address.
fn parse_instance(
    fullname: &str,
    port: u16,
    addresses: &[Ipv4Addr],
    txt: &[(String, String)],
    own_id: &DeviceId,
) -> Option<FoundDevice> {
    let value = |key: &str| {
        txt.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };
    if value("v") != Some(TXT_VERSION) || port == 0 {
        return None;
    }
    let id = value("id")?;
    if id.len() != 32 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let id = DeviceId::new(id);
    if id == *own_id {
        return None;
    }
    let mut dialable: Vec<SocketAddrV4> = Vec::new();
    for ip in addresses.iter().copied().filter(|ip| is_local_lan(*ip)) {
        let address = SocketAddrV4::new(ip, port);
        if !dialable.contains(&address) && dialable.len() < MAX_ADDRESSES {
            dialable.push(address);
        }
    }
    dialable.sort();
    if dialable.is_empty() {
        return None;
    }
    let instance = fullname
        .strip_suffix(SERVICE_TYPE)
        .map(|name| name.trim_end_matches('.'))
        .unwrap_or(fullname);
    Some(FoundDevice {
        name: display_text(&unescape(instance), MAX_DISPLAY_CHARS),
        id,
        pairing: value("pair") == Some("1"),
        addresses: dialable,
    })
}

/// mdns-sd escapes dots and backslashes in instance names; undo it for
/// display.
fn unescape(instance: &str) -> String {
    let mut out = String::with_capacity(instance.len());
    let mut chars = instance.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The instance name to advertise: the cleaned device name, at most 63
/// bytes in all, plus " (pairing)" for a pairing instance.
fn instance_name(name: &str, pairing: bool) -> String {
    let suffix = if pairing { " (pairing)" } else { "" };
    let mut base = display_text(name, MAX_LABEL_BYTES);
    if base.is_empty() {
        base = "Hop".to_owned();
    }
    let room = MAX_LABEL_BYTES - suffix.len();
    while base.len() > room {
        base.pop();
    }
    let base = base.trim_end().to_owned();
    format!("{base}{suffix}")
}

/// Hop's own mDNS host name: `hop-<first 8 characters of the id>.local.`.
fn host_name(id: &DeviceId) -> String {
    let short: String = id.as_str().chars().take(8).collect();
    format!("hop-{short}.local.")
}

/// Whether an interface name is a VPN tunnel on macOS (`utun*`, `ipsec*`,
/// `ppp*`) or a common tunnel elsewhere.
fn is_tunnel(name: &str) -> bool {
    ["utun", "ipsec", "ppp", "tun", "tap", "wg"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const PEER: &str = "0123456789abcdef0123456789abcdef";
    const OWN: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn instances_are_parsed_and_filtered() {
        let fullname = format!("Krish's PC.{SERVICE_TYPE}");
        let addresses: Vec<Ipv4Addr> = ["192.168.1.20", "8.8.8.8", "127.0.0.1", "169.254.1.2"]
            .iter()
            .map(|ip| ip.parse().unwrap())
            .collect();
        let device = parse_instance(
            &fullname,
            47800,
            &addresses,
            &txt(&[("v", "1"), ("id", PEER)]),
            &DeviceId::new(OWN),
        )
        .unwrap();
        assert_eq!(device.name, "Krish's PC");
        assert_eq!(device.id, DeviceId::new(PEER));
        assert!(!device.pairing);
        assert_eq!(
            device.addresses,
            vec![
                "169.254.1.2:47800".parse().unwrap(),
                "192.168.1.20:47800".parse().unwrap()
            ],
            "only private and link-local addresses are kept"
        );

        let pairing = parse_instance(
            &format!("Krish's PC (pairing).{SERVICE_TYPE}"),
            47801,
            &addresses,
            &txt(&[("v", "1"), ("id", PEER), ("pair", "1")]),
            &DeviceId::new(OWN),
        )
        .unwrap();
        assert!(pairing.pairing);
    }

    #[test]
    fn unusable_instances_are_ignored() {
        let lan = ["192.168.1.20".parse().unwrap()];
        let own = DeviceId::new(OWN);
        let name = format!("x.{SERVICE_TYPE}");
        let cases = [
            (txt(&[("v", "2"), ("id", PEER)]), &lan[..], 47800),
            (txt(&[("id", PEER)]), &lan[..], 47800),
            (txt(&[("v", "1"), ("id", "ABC")]), &lan[..], 47800),
            (
                txt(&[("v", "1"), ("id", &PEER.to_uppercase())]),
                &lan[..],
                47800,
            ),
            (txt(&[("v", "1"), ("id", OWN)]), &lan[..], 47800),
            (txt(&[("v", "1"), ("id", PEER)]), &[][..], 47800),
            (txt(&[("v", "1"), ("id", PEER)]), &lan[..], 0),
        ];
        for (properties, addresses, port) in cases {
            assert_eq!(
                parse_instance(&name, port, addresses, &properties, &own),
                None,
                "{properties:?} {addresses:?} {port}"
            );
        }
    }

    #[test]
    fn addresses_are_capped() {
        let many: Vec<Ipv4Addr> = (1..=20).map(|n| Ipv4Addr::new(10, 0, 0, n)).collect();
        let device = parse_instance(
            &format!("x.{SERVICE_TYPE}"),
            1,
            &many,
            &txt(&[("v", "1"), ("id", PEER)]),
            &DeviceId::new(OWN),
        )
        .unwrap();
        assert_eq!(device.addresses.len(), MAX_ADDRESSES);
    }

    #[test]
    fn instance_names_fit_a_dns_label() {
        assert_eq!(instance_name("Krish's MacBook", false), "Krish's MacBook");
        assert_eq!(
            instance_name("Krish's MacBook", true),
            "Krish's MacBook (pairing)"
        );
        let long = "é".repeat(100);
        for pairing in [false, true] {
            let name = instance_name(&long, pairing);
            assert!(name.len() <= MAX_LABEL_BYTES, "{} bytes", name.len());
            assert_eq!(name.ends_with(" (pairing)"), pairing);
        }
        assert_eq!(instance_name("\n", false), "Hop");
        assert_eq!(unescape(r"Krish\.s PC"), "Krish.s PC");
    }

    #[test]
    fn host_names_are_hops_own() {
        assert_eq!(host_name(&DeviceId::new(PEER)), "hop-01234567.local.");
    }

    #[test]
    fn vpn_interfaces_are_recognised() {
        for name in ["utun3", "ipsec0", "ppp0", "wg0"] {
            assert!(is_tunnel(name), "{name}");
        }
        for name in ["en0", "en1", "Wi-Fi", "Ethernet"] {
            assert!(!is_tunnel(name), "{name}");
        }
    }

    // Real mDNS sends multicast on the local network, which is unreliable
    // in CI and would trigger macOS's Local Network prompt, so these run
    // only on request: `cargo test -p hop-net -- --ignored`.
    #[tokio::test]
    #[ignore = "sends mDNS multicast on the real network"]
    async fn a_device_finds_another_by_its_advertisement() {
        let seen: Arc<Mutex<Vec<FoundDevice>>> = Arc::default();
        let sink = seen.clone();
        let browser = Discovery::start(
            DeviceId::new(OWN),
            Arc::new(move |found: &[FoundDevice]| {
                *sink.lock().unwrap() = found.to_vec();
            }),
        )
        .unwrap();
        let advertiser =
            Discovery::start(DeviceId::new(PEER), Arc::new(|_: &[FoundDevice]| {})).unwrap();
        advertiser
            .advertise("hop-net test device", 47999, false)
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if seen
                .lock()
                .unwrap()
                .iter()
                .any(|device| device.id == DeviceId::new(PEER) && !device.pairing)
            {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "not found");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        advertiser.shutdown(Duration::from_secs(1)).await;
        browser.shutdown(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    #[ignore = "sends mDNS multicast on the real network"]
    async fn the_joiner_finds_a_pairing_instance() {
        let advertiser =
            Discovery::start(DeviceId::new(PEER), Arc::new(|_: &[FoundDevice]| {})).unwrap();
        advertiser
            .advertise("hop-net test device", 47998, true)
            .unwrap();
        let hosts = find_pairing_hosts(DeviceId::new(OWN), Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            hosts.iter().any(|host| host.id == DeviceId::new(PEER)),
            "{hosts:?}"
        );
        advertiser.shutdown(Duration::from_secs(1)).await;
    }
}
