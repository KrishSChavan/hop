//! Turning a configured address ("192.168.1.20", "krish-pc.local:47800")
//! into socket addresses to dial.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// Splits `text` into a host and a port, using `default_port` when the text
/// names none. IPv6 is not supported in M2, so a bracketed or bare IPv6
/// literal is refused rather than misread.
fn split_host_port(text: &str, default_port: u16) -> io::Result<(String, u16)> {
    let text = text.trim();
    let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidInput, why.to_owned());
    if text.is_empty() {
        return Err(invalid("the address is empty"));
    }
    if text.starts_with('[') || text.matches(':').count() > 1 {
        return Err(invalid(
            "IPv6 addresses are not supported yet; use an IPv4 address",
        ));
    }
    match text.split_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| invalid("the port must be a number from 1 to 65535"))?;
            if host.is_empty() {
                return Err(invalid("the address has a port but no host"));
            }
            Ok((host.to_owned(), port))
        }
        None => Ok((text.to_owned(), default_port)),
    }
}

/// Resolves `text` (an IPv4 address or a host name, with an optional
/// `:port`) to the IPv4 socket addresses to dial, waiting at most
/// `timeout` for a name lookup.
///
/// An IPv4 literal needs no lookup. A host name goes to the system
/// resolver, which on both macOS and Windows also answers `.local` names
/// over mDNS.
///
/// # Errors
///
/// `InvalidInput` for an address that cannot be read, `TimedOut` when the
/// lookup takes too long, `NotFound` when it finds no IPv4 address, and
/// the resolver's own error otherwise.
pub async fn resolve_address(
    text: &str,
    default_port: u16,
    timeout: Duration,
) -> io::Result<Vec<SocketAddr>> {
    let (host, port) = split_host_port(text, default_port)?;
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(vec![SocketAddr::from((ip, port))]);
    }
    if host.parse::<IpAddr>().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPv6 addresses are not supported yet; use an IPv4 address",
        ));
    }
    let found = tokio::time::timeout(timeout, tokio::net::lookup_host((host.as_str(), port)))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "looking up the name took too long")
        })??;
    let mut addresses: Vec<SocketAddr> = Vec::new();
    for address in found.filter(SocketAddr::is_ipv4) {
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the name has no IPv4 address",
        ));
    }
    Ok(addresses)
}

/// Whether an address learned from mDNS may be dialled: private or
/// link-local IPv4 only, never loopback. A spoofed mDNS answer then cannot
/// point Hop at the internet or at itself.
pub(crate) fn is_local_lan(ip: Ipv4Addr) -> bool {
    (ip.is_private() || ip.is_link_local()) && !ip.is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_port_split() {
        assert_eq!(
            split_host_port("192.168.1.20", 47800).unwrap(),
            ("192.168.1.20".to_owned(), 47800)
        );
        assert_eq!(
            split_host_port(" krish-pc.local:47801 ", 47800).unwrap(),
            ("krish-pc.local".to_owned(), 47801)
        );
        for bad in [
            "",
            "host:",
            "host:0",
            "host:99999",
            ":47800",
            "::1",
            "[::1]:47800",
            "fe80::1",
        ] {
            assert!(split_host_port(bad, 47800).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn ipv4_literals_resolve_without_a_lookup() {
        let timeout = Duration::from_secs(1);
        assert_eq!(
            resolve_address("127.0.0.1:5000", 47800, timeout)
                .await
                .unwrap(),
            vec![SocketAddr::from(([127, 0, 0, 1], 5000))]
        );
        assert_eq!(
            resolve_address("10.0.0.7", 47800, timeout).await.unwrap(),
            vec![SocketAddr::from(([10, 0, 0, 7], 47800))]
        );
        assert!(resolve_address("::1", 47800, timeout).await.is_err());
    }

    #[test]
    fn only_lan_addresses_are_dialled_from_mdns() {
        for ip in ["192.168.1.20", "10.1.2.3", "172.16.0.1", "169.254.10.10"] {
            assert!(is_local_lan(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "8.8.8.8",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.251",
        ] {
            assert!(!is_local_lan(ip.parse().unwrap()), "{ip}");
        }
    }
}
