//! The link used the way the agent will use it: a plain (non-tokio) core
//! thread that waits on a doorbell rung by `NetConfig::on_event`, drains
//! `NetEvents` with `try_recv`, and sends with `NetHandle::send`, while the
//! network runs on a two-worker tokio runtime. Loopback only.

use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use hop_net::{Identity, NetConfig, NetEvent, NetEvents, NetHandle, PinnedPeer};
use hop_types::{CoreMessage, Os};

/// What the agent's core thread waits on: rung by the network (and, in
/// the agent, by the capture sink).
#[derive(Default)]
struct Doorbell {
    rung: Mutex<bool>,
    wake: Condvar,
}

impl Doorbell {
    fn ring(&self) {
        *self.rung.lock().unwrap() = true;
        self.wake.notify_one();
    }

    /// Waits until rung or `timeout` (the core's 10 ms tick).
    fn wait(&self, timeout: Duration) {
        let rung = self.rung.lock().unwrap();
        let (mut rung, _) = self
            .wake
            .wait_timeout_while(rung, timeout, |rung| !*rung)
            .unwrap();
        *rung = false;
    }
}

fn pin(identity: &Identity) -> PinnedPeer {
    PinnedPeer {
        id: identity.device_id(),
        fingerprint: identity.fingerprint(),
        name: "peer".to_owned(),
        address: None,
    }
}

fn start(
    runtime: &tokio::runtime::Runtime,
    identity: &Arc<Identity>,
    peer: &Identity,
    os: Os,
    doorbell: Option<Arc<Doorbell>>,
) -> (NetHandle, NetEvents) {
    let mut config = NetConfig::new(0, "device", env!("CARGO_PKG_VERSION"));
    config.bind = SocketAddr::from(([127, 0, 0, 1], 0));
    config.discovery = false;
    config.os = os;
    config.peers = vec![pin(peer)];
    if let Some(doorbell) = doorbell {
        config.on_event = Some(Arc::new(move || doorbell.ring()));
    }
    runtime
        .block_on(hop_net::start(identity.clone(), config))
        .unwrap()
}

#[test]
fn a_core_thread_drives_the_link_without_blocking() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mac = Arc::new(Identity::generate().unwrap());
    let pc = Arc::new(Identity::generate().unwrap());
    let doorbell = Arc::new(Doorbell::default());
    let (mac_net, mac_events) = start(&runtime, &mac, &pc, Os::MacOs, Some(doorbell.clone()));
    let (pc_net, pc_events) = start(&runtime, &pc, &mac, Os::Windows, None);
    mac_net.set_peer_address(&pc.device_id(), Some(pc_net.local_addr().to_string()));

    // The Mac's "core thread": no tokio here.
    let pc_id = pc.device_id();
    let core = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen_up = None;
        let mut replies = 0;
        while Instant::now() < deadline && replies < 3 {
            doorbell.wait(Duration::from_millis(10));
            while let Some(event) = mac_events.try_recv() {
                match event {
                    NetEvent::PeerUp(up) => {
                        assert_eq!(up.peer, pc_id);
                        // The OS each side announces: what the Cmd/Ctrl swap
                        // keys on.
                        assert_eq!(up.os, Os::Windows);
                        seen_up = Some(up.peer.clone());
                        mac_net
                            .send(&up.peer, CoreMessage::Heartbeat { seq: 1 })
                            .unwrap();
                    }
                    NetEvent::Message { from, msg } => {
                        assert_eq!(from, pc_id);
                        assert!(matches!(msg, CoreMessage::Heartbeat { .. }));
                        replies += 1;
                    }
                    NetEvent::PeerDown { .. } => panic!("unexpected disconnect"),
                }
            }
        }
        (seen_up, replies, mac_net)
    });

    // The PC answers each heartbeat with three of its own.
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match pc_events.try_recv() {
                Some(NetEvent::PeerUp(up)) => assert_eq!(up.os, Os::MacOs),
                Some(NetEvent::Message { from, .. }) => {
                    for seq in 2..5 {
                        pc_net.send(&from, CoreMessage::Heartbeat { seq }).unwrap();
                    }
                    break;
                }
                Some(other) => panic!("unexpected {other:?}"),
                None => {
                    assert!(tokio::time::Instant::now() < deadline, "no heartbeat");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    });

    let (seen_up, replies, mac_net) = core.join().unwrap();
    assert_eq!(seen_up, Some(pc.device_id()));
    assert_eq!(replies, 3);
    runtime.block_on(async {
        mac_net.shutdown().await;
        pc_net.shutdown().await;
    });
}
