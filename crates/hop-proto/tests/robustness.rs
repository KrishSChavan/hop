//! Property tests: hostile or random bytes never make the decoders panic,
//! and valid streams decode the same however they are split.

mod common;

use common::{every_message, pop_all, raw_frame};
use hop_proto::{
    Capability, Decoded, FrameDecoder, KIND_CORE, KIND_HELLO, MAX_DATAGRAM_LEN, Message,
    decode_datagram, encode_datagram, encode_frame, negotiate,
};
use hop_types::{CoreMessage, MotionMark};
use proptest::prelude::*;

/// Feeds `bytes` to a decoder in pieces whose sizes cycle through `chunks`,
/// popping after every piece, and checks the decoder never holds more than
/// it was given.
fn feed(bytes: &[u8], chunks: &[usize]) {
    let mut decoder = FrameDecoder::new();
    let mut rest = bytes;
    let mut pushed = 0;
    for &size in chunks.iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let (piece, tail) = rest.split_at(size.min(rest.len()));
        rest = tail;
        pushed += piece.len();
        // Errors are expected for random bytes; panics are not.
        let _ = decoder.push(piece);
        let _ = pop_all(&mut decoder);
        assert!(decoder.buffered_len() <= pushed);
    }
}

fn valid_stream() -> Vec<u8> {
    every_message()
        .iter()
        .flat_map(|message| encode_frame(message).expect("encodes"))
        .collect()
}

proptest! {
    #[test]
    fn random_bytes_never_panic_the_frame_decoder(
        bytes in prop::collection::vec(any::<u8>(), 0..4096),
        chunks in prop::collection::vec(1usize..64, 1..8),
    ) {
        feed(&bytes, &chunks);
    }

    #[test]
    fn random_bodies_of_known_kinds_never_panic(
        kind in prop_oneof![Just(KIND_HELLO), Just(KIND_CORE)],
        body in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let mut decoder = FrameDecoder::new();
        decoder.push(&raw_frame(kind, &body)).expect("framing is fine");
        prop_assert!(decoder.pop().is_some());
        let mut datagram = kind.to_be_bytes().to_vec();
        datagram.extend_from_slice(&body);
        let _ = decode_datagram(&datagram);
    }

    #[test]
    fn corrupted_valid_streams_never_panic(
        flips in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..16),
        chunks in prop::collection::vec(1usize..256, 1..8),
    ) {
        let mut stream = valid_stream();
        for (index, byte) in flips {
            let at = index.index(stream.len());
            stream[at] ^= byte;
        }
        feed(&stream, &chunks);
    }

    #[test]
    fn random_bytes_never_panic_decode_datagram(
        bytes in prop::collection::vec(any::<u8>(), 0..1500),
    ) {
        let _ = decode_datagram(&bytes);
    }

    #[test]
    fn corrupted_datagrams_never_panic(
        message_index in any::<prop::sample::Index>(),
        flips in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
    ) {
        let messages = every_message();
        let message = &messages[message_index.index(messages.len())];
        let mut datagram = encode_datagram(message).expect("fits");
        for (index, byte) in flips {
            let at = index.index(datagram.len());
            datagram[at] ^= byte;
        }
        let _ = decode_datagram(&datagram);
    }

    #[test]
    fn a_valid_stream_decodes_the_same_however_it_is_split(
        chunks in prop::collection::vec(1usize..300, 1..10),
    ) {
        let stream = valid_stream();
        let mut decoder = FrameDecoder::new();
        let mut out = Vec::new();
        let mut rest = stream.as_slice();
        for &size in chunks.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (piece, tail) = rest.split_at(size.min(rest.len()));
            rest = tail;
            decoder.push(piece).expect("valid stream");
            out.extend(pop_all(&mut decoder));
        }
        let expected: Vec<_> = every_message()
            .into_iter()
            .map(|message| Ok(Decoded::Message(message)))
            .collect();
        prop_assert_eq!(out, expected);
        prop_assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn any_motion_round_trips_and_stays_small(
        seq in any::<u64>(),
        n in any::<u64>(),
        total_dx in any::<f64>(),
        total_dy in any::<f64>(),
    ) {
        let motion = Message::Core(CoreMessage::Motion {
            seq,
            mark: MotionMark { n, total_dx, total_dy },
        });
        let datagram = encode_datagram(&motion).expect("fits");
        prop_assert!(datagram.len() * 10 < MAX_DATAGRAM_LEN);
        let Ok(Decoded::Message(Message::Core(CoreMessage::Motion { seq: s, mark }))) =
            decode_datagram(&datagram)
        else {
            panic!("not a motion message");
        };
        // Compare bits so NaN totals count as equal.
        prop_assert_eq!(s, seq);
        prop_assert_eq!(mark.n, n);
        prop_assert_eq!(mark.total_dx.to_bits(), total_dx.to_bits());
        prop_assert_eq!(mark.total_dy.to_bits(), total_dy.to_bits());
    }

    #[test]
    fn negotiation_is_symmetric_sorted_and_a_subset_of_both_sides(
        local_caps in prop::collection::vec(("[a-d]", 0u16..4), 0..6),
        remote_caps in prop::collection::vec(("[a-d]", 0u16..4), 0..6),
        local_minor in 0u16..5,
        remote_minor in 0u16..5,
    ) {
        let hello = |minor: u16, caps: &[(String, u16)]| hop_proto::Hello {
            proto_minor: minor,
            capabilities: caps
                .iter()
                .map(|(name, version)| Capability::new(name.clone(), *version))
                .collect(),
            ..common::sample_hello()
        };
        let local = hello(local_minor, &local_caps);
        let remote = hello(remote_minor, &remote_caps);
        let ab = negotiate(&local, &remote).expect("same major");
        let ba = negotiate(&remote, &local).expect("same major");
        prop_assert_eq!(&ab, &ba);
        prop_assert_eq!(ab.proto_minor, local_minor.min(remote_minor));
        prop_assert!(ab.capabilities.windows(2).all(|pair| pair[0].name < pair[1].name));
        for capability in &ab.capabilities {
            for side in [&local_caps, &remote_caps] {
                let best = side
                    .iter()
                    .filter(|(name, _)| *name == capability.name)
                    .map(|(_, version)| *version)
                    .max();
                prop_assert!(best.is_some_and(|best| best >= capability.version));
            }
        }
    }
}
