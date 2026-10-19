//! QUIC transport parameters: every limit set explicitly, because quinn's
//! defaults allow 100 streams of each kind and an unlimited connection
//! window (docs/M2-DESIGN.md, "Limits").

use std::time::Duration;

use quinn::{IdleTimeout, TransportConfig, VarInt};

/// The limits of one kind of connection.
#[derive(Clone, Debug)]
pub(crate) struct TransportParams {
    /// Bidirectional streams the peer may open at once.
    pub(crate) peer_bidi_streams: u32,
    /// Unread bytes buffered per stream.
    pub(crate) stream_window: u32,
    /// Unread bytes buffered for the whole connection.
    pub(crate) connection_window: u32,
    /// Bytes this side may have sent but not yet had acknowledged.
    pub(crate) send_window: u64,
    /// Received datagrams buffered, in bytes; `None` turns datagrams off.
    pub(crate) datagram_receive: Option<usize>,
    /// Outgoing datagrams buffered, in bytes. When full, quinn drops the
    /// oldest, which suits motion: the latest mark wins anyway.
    pub(crate) datagram_send: usize,
    /// How often to send a keep-alive when nothing else is sent.
    pub(crate) keep_alive: Duration,
    /// Silence after which the connection is dead.
    pub(crate) idle_timeout: Duration,
}

impl TransportParams {
    /// Builds quinn's transport config. Fails only for an idle timeout too
    /// long for QUIC to encode (over 2^62 milliseconds).
    pub(crate) fn build(&self) -> Result<TransportConfig, String> {
        let idle = IdleTimeout::try_from(self.idle_timeout)
            .map_err(|_| format!("idle timeout {:?} is too long", self.idle_timeout))?;
        let mut config = TransportConfig::default();
        config
            .max_concurrent_bidi_streams(VarInt::from_u32(self.peer_bidi_streams))
            // None here: a session connection that negotiates `files`
            // allows the peer one at a time once its `Hello` is through
            // (see `link::files`), and pairing never allows any.
            .max_concurrent_uni_streams(VarInt::from_u32(0))
            .stream_receive_window(VarInt::from_u32(self.stream_window))
            .receive_window(VarInt::from_u32(self.connection_window))
            .send_window(self.send_window)
            .datagram_receive_buffer_size(self.datagram_receive)
            .datagram_send_buffer_size(self.datagram_send)
            .keep_alive_interval(Some(self.keep_alive))
            .max_idle_timeout(Some(idle));
        Ok(config)
    }
}
