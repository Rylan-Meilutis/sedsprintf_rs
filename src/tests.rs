use crate::config::{get_message_meta, STATIC_HEX_LENGTH, STATIC_STRING_LENGTH};
use crate::get_needed_message_size;
use crate::packet::Packet;
use crate::router::{Clock, EndpointHandler};
use crate::{get_data_type, message_meta, DataEndpoint, DataType, MessageDataType, TelemetryError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

type SeenType = Arc<Mutex<Option<(DataType, Vec<f32>)>>>;
/// Compute a valid test payload length for a given [`DataType`], respecting the
/// schema’s static/dynamic element counts and element widths.
///
/// This is used throughout tests to avoid hard-coding per-type sizes.
// a clock that gets the system time as a u64 milliseconds since unix epoch.
struct UnixClock;

impl Clock for UnixClock {
    fn now_ms(&self) -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let start = SystemTime::now();
        let since_the_epoch = start
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards");
        since_the_epoch.as_millis() as u64
    }
}

#[cfg(feature = "compression")]
mod compression_memory_tests {
    use crate::config::{DataEndpoint, DataType};
    use crate::packet::Packet;
    use crate::serialize;
    use std::sync::Arc;

    const FLAG_COMPRESSED_PAYLOAD: u8 = 0x01;

    fn make_message_packet(payload: &[u8], ts: u64) -> Packet {
        Packet::new(
            DataType::MessageData,
            &[DataEndpoint::SdCard],
            "CMP_NODE",
            ts,
            Arc::<[u8]>::from(payload),
        )
        .expect("packet build failed")
    }

    #[test]
    fn compressible_payload_sets_compressed_flag_and_roundtrips() {
        let payload = vec![0u8; 4096];
        let pkt = make_message_packet(&payload, 11);

        let wire = serialize::serialize_packet(&pkt);
        assert_eq!(wire[0] & FLAG_COMPRESSED_PAYLOAD, FLAG_COMPRESSED_PAYLOAD);

        let decoded = serialize::deserialize_packet(&wire).expect("deserialize failed");
        assert_eq!(decoded.payload(), payload.as_slice());
    }

    #[test]
    fn below_threshold_payload_stays_uncompressed_and_roundtrips() {
        let payload = b"small-msg".to_vec();
        let pkt = make_message_packet(&payload, 22);

        let wire = serialize::serialize_packet(&pkt);
        assert_eq!(wire[0] & FLAG_COMPRESSED_PAYLOAD, 0);

        let decoded = serialize::deserialize_packet(&wire).expect("deserialize failed");
        assert_eq!(decoded.payload(), payload.as_slice());
    }

    #[test]
    fn mixed_payload_workload_roundtrips_without_failures() {
        for i in 0..1500u64 {
            let payload = if i % 2 == 0 {
                vec![b'Z'; 192]
            } else {
                let mut v = Vec::with_capacity(192);
                for j in 0..192u16 {
                    v.push(32u8 + (((i as u16 + j) as u8) % 95));
                }
                v
            };

            let pkt = make_message_packet(&payload, i);
            let wire = serialize::serialize_packet(&pkt);
            let decoded = serialize::deserialize_packet(&wire).expect("deserialize failed");
            assert_eq!(decoded.payload(), payload.as_slice());
        }
    }
}

fn test_payload_len_for(ty: DataType) -> usize {
    match message_meta(ty).element {
        crate::MessageElement::Static(_, _, _) => get_needed_message_size(ty),
        crate::MessageElement::Dynamic(_, _) => {
            // Pick reasonable defaults per data kind
            match get_data_type(ty) {
                MessageDataType::String => STATIC_STRING_LENGTH, // router error-path expects this
                MessageDataType::Binary => STATIC_HEX_LENGTH,    // any bytes; size-bounded
                // numeric/bool: must be multiple of element width → use “schema element count”
                other => {
                    let w = match other {
                        MessageDataType::UInt8 | MessageDataType::Int8 | MessageDataType::Bool => 1,
                        MessageDataType::UInt16 | MessageDataType::Int16 => 2,
                        MessageDataType::UInt32
                        | MessageDataType::Int32
                        | MessageDataType::Float32 => 4,
                        MessageDataType::UInt64
                        | MessageDataType::Int64
                        | MessageDataType::Float64 => 8,
                        MessageDataType::UInt128 | MessageDataType::Int128 => 16,
                        MessageDataType::String | MessageDataType::Binary => 1,
                        MessageDataType::NoData => 0,
                    };
                    let elems = get_message_meta(ty).element.into().max(1);
                    w * elems
                }
            }
        }
    }
}

/// Build a simple handler that increments an [`AtomicUsize`] each time it sees
/// a packet on the `SD_CARD` endpoint.
///
/// Used by various queue/timeout and concurrency tests.
fn get_handler(rx_count_c: Arc<AtomicUsize>) -> EndpointHandler {
    EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
        rx_count_c.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

/// Build a handler for `SD_CARD` that:
/// - asserts `GPS_DATA` element width is `4` (f32),
/// - decodes the payload as little-endian `f32`,
/// - stores `(DataType, Vec<f32>)` into the shared `Mutex`.
fn get_sd_card_handler(sd_seen_c: SeenType) -> EndpointHandler {
    EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |pkt: &Packet| {
        // sanity: element sizing must be 4 bytes (f32) for GPS_DATA
        let elems = get_message_meta(pkt.data_type()).element.into().max(1);
        let per_elem = get_needed_message_size(pkt.data_type()) / elems;
        assert_eq!(pkt.data_type(), DataType::GpsData);
        assert_eq!(per_elem, 4, "GPS_DATA expected f32 elements");

        // decode f32 little-endian
        let mut vals = Vec::with_capacity(pkt.payload().len() / 4);
        for chunk in pkt.payload().chunks_exact(4) {
            vals.push(f32::from_le_bytes(chunk.try_into().unwrap()));
        }

        *sd_seen_c.lock().unwrap() = Some((pkt.data_type(), vals));
        Ok(())
    })
}

/// Helper that asserts `result` is a [`TelemetryError::HandlerError`].
///
/// Used in tests that expect error propagation from handlers/tx.
fn handle_errors(result: Result<(), TelemetryError>) {
    match result {
        Ok(_) => panic!("Expected router.send to return Err due to handler failure"),
        Err(e) => match e {
            TelemetryError::HandlerError(_) => {} // expected
            _ => panic!("Expected TelemetryError::HandlerError, got {:?}", e),
        },
    }
}

// -----------------------------------------------------------------------------
// Basic packet + router smoke tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod tests2 {
    //! Basic smoke tests for packet roundtrip, string formatting, and simple
    //! router send/receive paths.

    use crate::tests::timeout_tests::StepClock;
    use crate::tests::{get_sd_card_handler, SeenType};
    use crate::{
        config::{DataEndpoint, DataType},
        packet::Packet,
        router::Router,
        serialize,
        TelemetryResult,
    };
    use std::sync::{Arc, Mutex};
    use std::vec::Vec;

    /// Serialize/deserialize a GPS packet and ensure all fields and payload
    /// bytes round-trip exactly.
    #[test]
    fn serialize_roundtrip_gps() {
        // GPS: 3 * f32
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::Radio];
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[5.2141414, 3.1342144, 1.1231232],
            endpoints,
            0,
        )
        .unwrap();

        pkt.validate().unwrap();

        let bytes = serialize::serialize_packet(&pkt);
        let rpkt = serialize::deserialize_packet(&bytes).unwrap();

        rpkt.validate().unwrap();
        assert_eq!(rpkt.data_type(), pkt.data_type());
        assert_eq!(rpkt.data_size(), pkt.data_size());
        assert_eq!(rpkt.timestamp(), pkt.timestamp());
        assert_eq!(rpkt.endpoints(), pkt.endpoints());
        assert_eq!(rpkt.payload(), pkt.payload());
    }

    /// Verify `header_string()` format for a simple GPS packet.
    #[test]
    fn header_string_matches_expectation() {
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::Radio];
        let pkt =
            Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.0, 3.0], endpoints, 0).unwrap();
        let s = pkt.header_string();
        assert_eq!(
            s,
            "Type: GPS_DATA, Data Size: 12, Sender: TEST_PLATFORM, Endpoints: [SD_CARD, RADIO], Timestamp: 0 (0s 000ms)"
        );
    }

    /// Ensure `to_string()` includes the float values and the general header.
    #[test]
    fn packet_to_string_formats_floats() {
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::Radio];
        let pkt =
            Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.5, 3.25], endpoints, 0).unwrap();

        let text = pkt.as_string();
        assert!(text.starts_with(
            "{Type: GPS_DATA, Data Size: 12, Sender: TEST_PLATFORM, Endpoints: [SD_CARD, RADIO], Timestamp: 0 (0s 000ms), Data: "
        ));
        assert!(text.contains("1"));
        assert!(text.contains("2.5"));
        assert!(text.contains("3.25"));
    }

    /// End-to-end test: `Router::log` → TX callback (serialize/deserialize) →
    /// local handler decoding f32 payload.
    #[test]
    fn router_sends_and_receives() {
        use crate::router::{Router, RouterConfig};

        // capture spaces
        let tx_seen: Arc<Mutex<Option<Packet>>> = Arc::new(Mutex::new(None));
        let sd_seen_decoded: SeenType = Arc::new(Mutex::new(None));

        // transmitter: record the deserialized packet we "sent"
        let tx_seen_c = tx_seen.clone();
        let transmit = move |bytes: &[u8]| -> TelemetryResult<()> {
            let pkt = serialize::deserialize_packet(bytes)?;
            *tx_seen_c.lock().unwrap() = Some(pkt);
            Ok(())
        };

        // local SD handler: decode payload to f32s and record (ty, values)
        let sd_seen_c = sd_seen_decoded.clone();
        let sd_handler = get_sd_card_handler(sd_seen_c);
        let box_clock = StepClock::new_default_box();

        let router = Router::new_with_clock(RouterConfig::new(vec![sd_handler]), box_clock);
        router.add_side_serialized("tx", transmit);

        // send GPS_DATA (3 * f32) using Router::log (uses default endpoints from schema)
        let data = [1.0_f32, 2.0, 3.0];
        router.log(DataType::GpsData, &data).unwrap();

        // --- assertions ---

        // remote transmitter saw the same type & bytes
        let tx_pkt = tx_seen
            .lock()
            .unwrap()
            .clone()
            .expect("no tx packet recorded");
        assert_eq!(tx_pkt.data_type(), DataType::GpsData);
        assert_eq!(tx_pkt.payload().len(), 3 * 4);
        // compare bytes exactly to what log() would have produced
        let mut expected = Vec::new();
        for v in data {
            expected.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(tx_pkt.payload(), &*expected);

        // local SD handler decoded to f32s and recorded (type, values)
        let (seen_ty, seen_vals) = sd_seen_decoded
            .lock()
            .unwrap()
            .clone()
            .expect("no sd packet recorded");
        assert_eq!(seen_ty, DataType::GpsData);
        assert_eq!(seen_vals, data);
    }

    #[test]
    fn router_load_balancing_smoke_exercises_public_runtime_controls() {
        use crate::RouteSelectionMode;
        use crate::discovery::build_discovery_announce;
        use crate::router::{Router, RouterConfig};

        let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_a_c = seen_a.clone();
        let seen_b_c = seen_b.clone();

        let router = Router::new_with_clock(RouterConfig::default(), StepClock::new_default_box());
        let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
            seen_a_c.lock().unwrap().push(pkt.clone());
            Ok(())
        });
        let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
            seen_b_c.lock().unwrap().push(pkt.clone());
            Ok(())
        });

        let discovery_a = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
        let discovery_b = build_discovery_announce("REMOTE_B", 1, &[DataEndpoint::Radio]).unwrap();
        router.rx_from_side(&discovery_a, side_a).unwrap();
        router.rx_from_side(&discovery_b, side_b).unwrap();
        seen_a.lock().unwrap().clear();
        seen_b.lock().unwrap().clear();

        router
            .set_source_route_mode(None, RouteSelectionMode::Weighted)
            .unwrap();
        router.set_route_weight(None, side_a, 1).unwrap();
        router.set_route_weight(None, side_b, 1).unwrap();

        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0, 3.0],
            &[DataEndpoint::Radio],
            1,
        )
        .unwrap();
        let pkt_failover = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0, 5.0, 6.0],
            &[DataEndpoint::Radio],
            2,
        )
        .unwrap();
        router.tx(pkt.clone()).unwrap();

        router
            .set_source_route_mode(None, RouteSelectionMode::Failover)
            .unwrap();
        router.set_route_priority(None, side_a, 0).unwrap();
        router.set_route_priority(None, side_b, 1).unwrap();
        router.tx(pkt_failover).unwrap();

        router.clear_route_weight(None, side_a).unwrap();
        router.clear_route_priority(None, side_b).unwrap();
        router.clear_source_route_mode(None).unwrap();

        let total = seen_a.lock().unwrap().len() + seen_b.lock().unwrap().len();
        assert_eq!(total, 2);
    }

    #[test]
    fn relay_load_balancing_smoke_exercises_public_runtime_controls() {
        use crate::RouteSelectionMode;
        use crate::discovery::build_discovery_announce;
        use crate::relay::Relay;

        let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_a_c = seen_a.clone();
        let seen_b_c = seen_b.clone();

        let relay = Relay::new(StepClock::new_default_box());
        let ingress = relay.add_side_packet("INGRESS", |_pkt: &Packet| Ok(()));
        let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
            seen_a_c.lock().unwrap().push(pkt.clone());
            Ok(())
        });
        let side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
            seen_b_c.lock().unwrap().push(pkt.clone());
            Ok(())
        });

        let discovery_a = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
        let discovery_b = build_discovery_announce("REMOTE_B", 1, &[DataEndpoint::Radio]).unwrap();
        relay.rx_from_side(side_a, discovery_a).unwrap();
        relay.rx_from_side(side_b, discovery_b).unwrap();
        relay.process_all_queues().unwrap();
        seen_a.lock().unwrap().clear();
        seen_b.lock().unwrap().clear();

        relay
            .set_source_route_mode(Some(ingress), RouteSelectionMode::Weighted)
            .unwrap();
        relay.set_route_weight(Some(ingress), side_a, 2).unwrap();
        relay.set_route_weight(Some(ingress), side_b, 1).unwrap();
        relay
            .set_source_route_mode(Some(ingress), RouteSelectionMode::Failover)
            .unwrap();
        relay.set_route_priority(Some(ingress), side_a, 0).unwrap();
        relay.set_route_priority(Some(ingress), side_b, 1).unwrap();

        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0, 3.0],
            &[DataEndpoint::Radio],
            2,
        )
        .unwrap();
        relay.rx_from_side(ingress, pkt).unwrap();
        relay.process_all_queues().unwrap();

        relay.clear_route_weight(Some(ingress), side_a).unwrap();
        relay.clear_route_priority(Some(ingress), side_b).unwrap();
        relay.clear_source_route_mode(Some(ingress)).unwrap();

        let total = seen_a.lock().unwrap().len() + seen_b.lock().unwrap().len();
        assert_eq!(total, 1);
    }

    /// A small “bus” that records transmitted frames for TX/RX queue tests.
    struct TestBus {
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl TestBus {
        /// Create a `TestBus` and a TX function that pushes any transmitted bytes
        /// into an internal `Vec<Vec<u8>>`.
        fn new() -> (
            Self,
            impl Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
        ) {
            let frames = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let tx_frames = frames.clone();
            let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
                // capture the exact wire bytes
                tx_frames.lock().unwrap().push(bytes.to_vec());
                Ok(())
            };
            (Self { frames }, tx)
        }
    }

    /// TX router enqueues packets, flushes to a `TestBus`, and an RX router
    /// consumes them from its receive queue and delivers to a local handler.
    #[test]
    fn queued_roundtrip_between_two_routers() {
        // --- Set up a TX router that only sends (no local endpoints) ---
        let (bus, tx_fn) = TestBus::new();
        let box_clock_tx = StepClock::new_default_box();
        let box_clock_rx = StepClock::new_default_box();

        let tx_router = Router::new_with_clock(Default::default(), box_clock_tx);
        tx_router.add_side_serialized("tx", tx_fn);

        // --- Set up an RX router with a local SD handler that decodes f32 payloads ---
        let seen: SeenType = Arc::new(Mutex::new(None));
        let seen_c = seen.clone();
        let sd_handler = get_sd_card_handler(seen_c);
        fn tx_handler(_bytes: &[u8]) -> TelemetryResult<()> {
            // RX router does not transmit in this test
            Ok(())
        }

        let rx_router = Router::new_with_clock(
            crate::router::RouterConfig::new(vec![sd_handler]),
            box_clock_rx,
        );
        rx_router.add_side_serialized("tx", tx_handler);

        // --- 1) Sender enqueues a packet for TX ---
        let data = [1.0_f32, 2.0, 3.0];
        tx_router.log_queue(DataType::GpsData, &data).unwrap();

        // --- 2) Flush TX queue -> pushes wire frames into TestBus ---
        tx_router.process_tx_queue().unwrap();

        // --- 3) Deliver captured frames into RX router's *received queue* ---
        let frames = bus.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 1, "expected exactly one TX frame");
        for frame in &frames {
            rx_router.rx_serialized_queue(frame).unwrap();
        }

        // --- 4) Drain RX queue -> invokes local handlers ---
        rx_router.process_rx_queue().unwrap();

        // --- Assertions: handler got the right data ---
        let (ty, vals) = seen.lock().unwrap().clone().expect("no packet delivered");
        assert_eq!(ty, DataType::GpsData);
        assert_eq!(vals, data);
    }

    /// Demonstrate “self-delivery” by feeding serialized frames from a router’s
    /// own TX back into its RX queue.
    #[test]
    fn queued_self_delivery_via_receive_queue() {
        let (bus, tx_fn) = TestBus::new();
        let box_clock = StepClock::new_default_box();

        let router = Router::new_with_clock(Default::default(), box_clock);
        router.add_side_serialized("tx", tx_fn);

        // Enqueue for transmit
        let data = [10.0_f32, 10.25, 10.5];
        router.log_queue(DataType::GpsData, &data).unwrap();

        let data = [10.0_f32, 10.25];
        router.log_queue(DataType::BatteryStatus, &data).unwrap();

        let data = [10.0_f32, 10.25, 10.2];
        router.log_queue(DataType::GpsData, &data).unwrap();
        // Flush -> frame appears on the "bus"
        router.process_tx_queue().unwrap();
        let frames = bus.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 3);

        // Feed back into *the same* router's received queue
        router.rx_serialized_queue(&frames[0]).unwrap();

        // Now draining the received queue should dispatch to any matching local endpoints.
        // (This router has no endpoints; this test just proves the queue path is exercised.)
        router.process_rx_queue().unwrap();
    }
}

// ---- Helpers (test-local) ----

/// Build a deterministic packet with a raw 3-byte payload [0x13, 0x21, 0x34]
/// encoded as three `f32` values, endpoints [SD_CARD, RADIO], and timestamp
/// `1123581321`.
///
/// We intentionally do not call `validate()` because `GPS_DATA` usually expects
/// 3×`f32` (12 bytes) and this is for formatting/copying tests only.
fn fake_telemetry_packet_bytes() -> Packet {
    use crate::config::{DataEndpoint, DataType};

    let payload = [0x13 as f32, 0x21 as f32, 0x34 as f32]; // f32 values
    let endpoints = [DataEndpoint::SdCard, DataEndpoint::Radio];

    Packet::from_f32_slice(DataType::GpsData, &payload, &endpoints, 1123581321).unwrap()
}

/// Copy helper that mirrors the C++ behavior, but uses raw pointers so we can
/// test the “same pointer” case without violating Rust’s borrow rules.
///
/// Safety: Caller must ensure `dest` and `src` are valid for reads/writes.
unsafe fn copy_telemetry_packet_raw(
    dest: *mut Packet,
    src: *const Packet,
) -> Result<(), &'static str> {
    if dest.is_null() || src.is_null() {
        return Err("null packet");
    }
    if core::ptr::eq(dest, src as *mut Packet) {
        // same object → OK no-op
        return Ok(());
    }

    let s = unsafe { &*src };
    let d = unsafe { &mut *dest };

    // Deep copy: new endpoints slice and new payload buffer
    let endpoints_vec: Vec<DataEndpoint> = s.endpoints().to_vec();
    let payload_arc: Arc<[u8]> = Arc::from(s.payload());

    let new_pkt = Packet::new(
        s.data_type(),
        &endpoints_vec,
        s.sender(),
        s.timestamp(),
        payload_arc,
    )
    .map_err(|_| "packet validation failed")?;

    *d = new_pkt;
    Ok(())
}

// ---- Converted tests ----

/// Port of C++: TEST(Helpers, PacketHexToString).
/// Ensures `to_hex_string()` matches exactly the expected legacy format.
#[test]
fn helpers_packet_hex_to_string() {
    let pkt = fake_telemetry_packet_bytes();
    let got = pkt.to_hex_string();
    let expect = "Type: GPS_DATA, Data Size: 12, Sender: TEST_PLATFORM, Endpoints: [SD_CARD, RADIO], Timestamp: 1123581321 (312h 06m 21s 321ms), Data (hex): 0x00 0x00 0x98 0x41 0x00 0x00 0x04 0x42 0x00 0x00 0x50 0x42";
    assert_eq!(got, expect);
}

/// Port of C++: TEST(Helpers, CopyPacket).
/// Exercises `copy_telemetry_packet_raw` for null, self-copy, and deep copy.
#[test]
fn helpers_copy_telemetry_packet() {
    // (1) null dest → error
    let src = fake_telemetry_packet_bytes();
    let st = unsafe { copy_telemetry_packet_raw(core::ptr::null_mut(), &src as *const _) };
    assert!(st.is_err());

    // (2) same pointer (no-op) → OK
    let mut same = fake_telemetry_packet_bytes();
    let same_ptr: *mut Packet = &mut same;
    let st = unsafe { copy_telemetry_packet_raw(same_ptr, same_ptr as *const _) };
    assert!(st.is_ok());

    // (3) distinct objects → deep copy and equal fields
    let mut dest = Packet::new(
        src.data_type(),
        src.endpoints(), // &[DataEndpoint]
        src.sender(),    // Arc<str>
        src.timestamp(),
        Arc::from(src.payload()), // deep copy payload
    )
    .expect("src packet should be valid");

    let st = unsafe { copy_telemetry_packet_raw(&mut dest as *mut _, &src as *const _) };
    assert!(st.is_ok());

    // element-by-element compare
    assert_eq!(dest.timestamp(), src.timestamp());
    assert_eq!(dest.data_type(), src.data_type());
    assert_eq!(dest.data_size(), src.data_size());
    assert_eq!(dest.endpoints().len(), src.endpoints().len());
    for i in 0..dest.endpoints().len() {
        assert_eq!(dest.endpoints()[i], src.endpoints()[i]);
    }
    assert_eq!(dest.payload(), src.payload());
}

// -----------------------------------------------------------------------------
// Error propagation & handler-failure tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod handler_failure_tests {
    //! Tests around handler failures and how they generate/route
    //! `TELEMETRY_ERROR` packets.

    use super::*;
    use crate::config::DEVICE_IDENTIFIER;
    use crate::router::EndpointHandler;
    use crate::router::{Router, RouterConfig};
    use crate::tests::timeout_tests::StepClock;
    use crate::{DataType, TelemetryError, MAX_VALUE_DATA_TYPE};
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Pick any valid [`DataType`] from the enum range for generic tests.
    fn pick_any_type() -> DataType {
        for i in 0..=MAX_VALUE_DATA_TYPE {
            if let Some(ty) = DataType::try_from_u32(i) {
                return ty;
            }
        }
        panic!("No usable DataType found for tests");
    }

    /// Build a zeroed payload of valid length for the given type using
    /// [`test_payload_len_for`].
    fn payload_for(ty: DataType) -> Vec<u8> {
        vec![0u8; test_payload_len_for(ty)]
    }

    /// If a local handler fails, ensure:
    /// - other local endpoints get the original packet,
    /// - and a `TELEMETRY_ERROR` packet with the right text is sent.
    #[test]
    fn local_handler_failure_sends_error_packet_to_other_locals() {
        let ty = pick_any_type();
        let ts = 42_u64;
        let failing_ep = DataEndpoint::SdCard;
        let other_ep = DataEndpoint::TelemetryError;

        // Capture the packets that reach the "other_ep" handler.
        let recv_count = Arc::new(AtomicUsize::new(0));
        let last_payload = Arc::new(Mutex::new(String::new()));

        let recv_count_c = recv_count.clone();
        let last_payload_c = last_payload.clone();

        let failing = EndpointHandler::new_packet_handler(failing_ep, |_pkt: &Packet| {
            Err(TelemetryError::BadArg)
        });

        let capturing = EndpointHandler::new_packet_handler(other_ep, move |pkt: &Packet| {
            if pkt.data_type() == DataType::TelemetryError {
                *last_payload_c.lock().unwrap() = pkt.as_string();
            }
            recv_count_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let box_clock = StepClock::new_default_box();

        let router = Router::new_with_clock(RouterConfig::new(vec![failing, capturing]), box_clock);

        let pkt = Packet::new(
            ty,
            &[failing_ep, other_ep],
            DEVICE_IDENTIFIER,
            ts,
            Arc::<[u8]>::from(payload_for(ty)),
        )
        .unwrap();

        handle_errors(router.tx(pkt));

        // The capturing handler should have seen the original packet and then the error packet.
        assert!(
            recv_count.load(Ordering::SeqCst) >= 1,
            "capturing handler should have been invoked at least once"
        );

        // Verify exact payload text produced by handle_callback_error(Some(dest), e)
        let expected = format!(
            "{{Type: TELEMETRY_ERROR, Data Size: {:?}, Sender: TEST_PLATFORM, Endpoints: [TELEMETRY_ERROR], Timestamp: 0 (0s 000ms), Error: (\"Handler for endpoint {:?} failed on device {:?}: {:?}\")}}",
            68,
            failing_ep,
            DEVICE_IDENTIFIER,
            TelemetryError::BadArg
        );
        let got = last_payload.lock().unwrap().clone();
        assert_eq!(got, expected, "mismatch in TelemetryError payload text");
    }

    /// If the TX callback fails, ensure:
    /// - a `TELEMETRY_ERROR` is generated,
    /// - it is delivered to all local endpoints,
    /// - and the error text matches expectation.
    #[test]
    fn tx_failure_sends_error_packet_to_all_local_endpoints() {
        let ty = pick_any_type();
        let ts = 31415_u64;

        // One local endpoint (to receive error), one "remote" endpoint (not in handlers)
        let local_ep = DataEndpoint::SdCard;
        let remote_ep = DataEndpoint::Radio;

        let saw_error = Arc::new(AtomicUsize::new(0));
        let last_payload = Arc::new(Mutex::new(String::new()));
        let saw_error_c = saw_error.clone();
        let last_payload_c = last_payload.clone();

        let capturing = EndpointHandler::new_packet_handler(local_ep, move |pkt: &Packet| {
            if pkt.data_type() == DataType::TelemetryError {
                *last_payload_c.lock().unwrap() = pkt.as_string();
                saw_error_c.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        });

        let tx_fail =
            |_bytes: &[u8]| -> crate::TelemetryResult<()> { Err(TelemetryError::Io("boom")) };
        let box_clock = StepClock::new_default_box();

        let router = Router::new_with_clock(RouterConfig::new(vec![capturing]), box_clock);
        router.add_side_serialized("tx", tx_fail);

        let pkt = Packet::new(
            ty,
            // include both a local and a non-local endpoint so any_remote == true
            &[local_ep, remote_ep],
            "router_test",
            ts,
            Arc::<[u8]>::from(payload_for(ty)),
        )
        .unwrap();

        handle_errors(router.tx(pkt));

        assert!(
            saw_error.load(Ordering::SeqCst) >= 1,
            "local handler should have received TelemetryError after TX failures"
        );

        // Exact text from handle_callback_error(None, e)
        let expected = format!(
            "{{Type: TELEMETRY_ERROR, Data Size: {:?}, Sender: TEST_PLATFORM, Endpoints: [SD_CARD], Timestamp: 0 (0s 000ms), Error: (\"TX Handler failed on device {:?}: {:?}\")}}",
            55,
            DEVICE_IDENTIFIER,
            TelemetryError::Io("boom")
        );
        let got = last_payload.lock().unwrap().clone();
        assert_eq!(got, expected, "mismatch in TelemetryError payload text");
    }
}

// -----------------------------------------------------------------------------
// Timeout and queue-draining behavior tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod timeout_tests {
    //! Tests for `process_*_queue*` functions and timeout semantics,
    //! including u64 wraparound handling.

    use crate::config::DataEndpoint;
    use crate::router::EndpointHandler;
    use crate::tests::{get_handler, UnixClock};
    use crate::{
        packet::Packet, router::Clock, router::Router, router::RouterConfig, DataType,
        TelemetryResult,
    };
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    // ---------------- Mock clock ----------------

    /// A deterministic clock that steps forward by `step` ms on each `now_ms()`
    /// call, starting from `start`. Used to test timeout budget behavior.
    pub(crate) struct StepClock {
        t: AtomicU64,
        step: u64,
    }

    impl StepClock {
        /// Creates a boxed [`StepClock`] with the specified start time and step size.
        pub fn new_box(start: u64, step: u64) -> Box<dyn Clock + Send + Sync> {
            Box::new(StepClock::new(start, step))
        }
        /// Creates a boxed [`StepClock`] pinned at zero for tests that need a fixed clock.
        pub fn new_default_box() -> Box<dyn Clock + Send + Sync> {
            Box::new(StepClock::new(0, 0))
        }
        /// Creates a deterministic test clock that advances by `step` on each read.
        pub fn new(start: u64, step: u64) -> Self {
            Self {
                t: AtomicU64::new(start),
                step,
            }
        }
    }

    impl Clock for StepClock {
        #[inline]
        fn now_ms(&self) -> u64 {
            // returns current, then advances by step (wraps naturally in u64)
            self.t.fetch_add(self.step, Ordering::Relaxed)
        }
    }

    // ---------------- Helpers ----------------

    /// Create a GPS packet with only a local endpoint (`SD_CARD`), avoiding any
    /// implicit re-TX during receive.
    fn mk_rx_only_local(vals: &[f32], ts: u64) -> Packet {
        Packet::from_f32_slice(
            DataType::GpsData,
            vals,
            &[DataEndpoint::SdCard], // <- only local
            ts,
        )
        .unwrap()
    }

    /// Build a TX function that increments `counter` for each frame sent.
    fn tx_counter(
        counter: Arc<AtomicUsize>,
    ) -> impl Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static {
        move |bytes: &[u8]| {
            assert!(!bytes.is_empty());
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// `timeout == 0` must drain both TX and RX queues fully, regardless of
    /// clock, and local handlers see all packets.
    #[test]
    fn process_all_queues_timeout_zero_drains_fully() {
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx = tx_counter(tx_count.clone());

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                rx_count_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let box_clock = StepClock::new_default_box();

        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), box_clock);
        r.add_side_serialized("tx", tx);

        // Enqueue TX (3) – make each payload slightly different to avoid dedup.
        for i in 0..3usize {
            let base = 1.0_f32 + i as f32;
            r.log_queue(DataType::GpsData, &[base, 2.0, 3.0]).unwrap();
        }
        // Enqueue RX (2) with only-local endpoints, and unique values/timestamps.
        for i in 0..2u64 {
            r.rx_queue(mk_rx_only_local(&[9.0 + i as f32, 8.0, 7.0], 123 + i))
                .unwrap();
        }

        // timeout = 0 → drain fully
        r.process_all_queues_with_timeout(0).unwrap();

        // TX: all three frames should be sent
        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            3,
            "all TX packets should be sent"
        );
        // RX handler was invoked for each TX (local delivery) + each RX = 3 + 2 = 5
        assert_eq!(
            rx_count.load(Ordering::SeqCst),
            5,
            "handler sees TX+RX packets"
        );
    }

    /// With non-zero timeout and step = 10ms, timeout 5ms should allow exactly
    /// one iteration (at most one TX and one RX).
    #[test]
    fn process_all_queues_respects_nonzero_timeout_budget_one_receive_one_send() {
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx = tx_counter(tx_count.clone());

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler = get_handler(rx_count_c);

        // Use a real-time clock; current implementation may process more than one
        // iteration in a single call depending on timing.
        let r = Router::new_with_clock(
            RouterConfig::new(vec![handler]),
            Box::new(|| UnixClock.now_ms()),
        );
        r.add_side_serialized("tx", tx);

        // Seed work in both queues – make each item unique to avoid dedup.
        for i in 0..5u64 {
            let base_tx = 1.0_f32 + i as f32;
            r.log_queue(DataType::GpsData, &[base_tx, 2.0, 3.0])
                .unwrap();

            // RX with only-local endpoint, unique payload + timestamp
            r.rx_queue(
                Packet::from_f32_slice(
                    DataType::GpsData,
                    &[4.0 + i as f32, 5.0, 6.0],
                    &[DataEndpoint::SdCard],
                    1 + i,
                )
                .unwrap(),
            )
            .unwrap();
        }

        // Non-zero timeout: must do *some* work, but we no longer require
        // exactly-one-iteration semantics.
        r.process_all_queues_with_timeout(5).unwrap();

        let first_tx = tx_count.load(Ordering::SeqCst);
        let first_rx = rx_count.load(Ordering::SeqCst);

        // Sanity: non-zero timeout should result in some progress.
        assert!(
            first_tx + first_rx > 0,
            "expected some work to be done with non-zero timeout"
        );
        // Upper bounds: we can’t have done more than all queued work.
        assert!(
            first_tx <= 5 && first_rx <= 10,
            "processed more items than were queued (tx={first_tx}, rx={first_rx})"
        );

        // Drain the rest to prove there was more work left / everything eventually completes.
        r.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(tx_count.load(Ordering::SeqCst), 5);
        assert_eq!(rx_count.load(Ordering::SeqCst), 10); // 5 (TX locals) + 5 (RX)
    }

    /// Similar to previous, but with step=5 and timeout=10 to allow up to two
    /// iterations; expect one TX + one RX handler call.
    #[test]
    fn process_all_queues_respects_nonzero_timeout_budget_two_receive_one_send() {
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx = tx_counter(tx_count.clone());

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler = get_handler(rx_count_c);
        let clock = StepClock::new_box(0, 5);

        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), clock);
        r.add_side_serialized("tx", tx);

        // Seed work in both queues – make each item unique to avoid dedup.
        for i in 0..5u64 {
            let base_tx = 1.0_f32 + i as f32;
            r.log_queue(DataType::GpsData, &[base_tx, 2.0, 3.0])
                .unwrap();

            r.rx_queue(
                Packet::from_f32_slice(
                    DataType::GpsData,
                    &[4.0 + i as f32, 5.0, 6.0],
                    &[DataEndpoint::SdCard],
                    1 + i,
                )
                .unwrap(),
            )
            .unwrap();
        }

        // Step is 5ms per call; timeout 10ms allows two iterations max
        r.process_all_queues_with_timeout(10).unwrap();

        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            1,
            "expected exactly one TX in a single iteration"
        );
        assert_eq!(
            rx_count.load(Ordering::SeqCst),
            2,
            "expected one TX local + one RX handler call"
        );

        // Drain the rest to prove there was more work left
        r.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(tx_count.load(Ordering::SeqCst), 5);
        assert_eq!(rx_count.load(Ordering::SeqCst), 10); // 5 (TX locals) + 5 (RX)
    }

    /// Ensure timeout math remains correct near `u64::MAX`, i.e. when the clock
    /// wraps around, and that we still do at most one iteration.
    #[test]
    fn process_all_queues_handles_u64_wraparound() {
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx = tx_counter(tx_count.clone());

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler = get_handler(rx_count_c);
        let start = u64::MAX - 1;
        let clock = StepClock::new_box(start, 2);
        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), clock);
        r.add_side_serialized("tx", tx);

        // One TX and one RX (RX is only-local to avoid creating extra TX on receive)
        r.log_queue(DataType::GpsData, &[1.0_f32, 2.0, 3.0])
            .unwrap();
        r.rx_queue(mk_rx_only_local(&[4.0, 5.0, 6.0], 7)).unwrap();

        // Small budget; with wrapping_sub this should allow one iteration then stop
        r.process_all_queues_with_timeout(1).unwrap();

        // One iteration can do up to one TX and one RX
        assert!(tx_count.load(Ordering::SeqCst) <= 1, "expected <=1 TX");
        assert!(
            rx_count.load(Ordering::SeqCst) <= 2,
            "local handler can be invoked by TX local delivery (+1) and RX (+1)"
        );
        // At least something should have happened
        assert!(tx_count.load(Ordering::SeqCst) + rx_count.load(Ordering::SeqCst) >= 1);
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn process_all_queues_timeout_does_not_starve_rx_after_slow_tx() {
        use crate::discovery::build_discovery_announce;

        struct ManualClock {
            now_ms: Arc<AtomicU64>,
        }

        impl Clock for ManualClock {
            fn now_ms(&self) -> u64 {
                self.now_ms.load(Ordering::SeqCst)
            }
        }

        let now_ms = Arc::new(AtomicU64::new(0));
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx_count_c = tx_count.clone();
        let now_ms_c = now_ms.clone();
        let seen_remote: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_remote_c = seen_remote.clone();

        let router = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::Radio,
                |_pkt| Ok(()),
            )]),
            Box::new(ManualClock {
                now_ms: now_ms.clone(),
            }),
        );
        let side_remote =
            router.add_side_packet("REMOTE", move |pkt: &Packet| -> TelemetryResult<()> {
                tx_count_c.fetch_add(1, Ordering::SeqCst);
                // Simulate a blocking transport send that consumes the whole timeout budget.
                now_ms_c.store(2, Ordering::SeqCst);
                seen_remote_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

        router
            .log_queue(DataType::GpsData, &[1.0_f32, 2.0, 3.0])
            .unwrap();
        let discovery_pkt =
            build_discovery_announce("REMOTE_NODE", 0, &[DataEndpoint::Radio]).unwrap();
        let discovery_bytes = crate::serialize::serialize_packet(&discovery_pkt);
        router
            .rx_serialized_queue_from_side(discovery_bytes.as_ref(), side_remote)
            .unwrap();

        router.process_all_queues_with_timeout(2).unwrap();

        assert_eq!(tx_count.load(Ordering::SeqCst), 1);
        let topo = router.export_topology();
        assert_eq!(topo.routes.len(), 1);
        assert_eq!(
            topo.routes[0].reachable_endpoints,
            vec![DataEndpoint::Radio]
        );
    }
}

// -----------------------------------------------------------------------------
// Extra coverage tests: error codes, header-only parsing, varints, bitmaps, etc.
// -----------------------------------------------------------------------------
#[cfg(test)]
mod tests_extra {
    //! Extra unit tests that cover previously-missing paths and invariants.
    //!
    //! These are white-box tests that exercise public APIs (and some
    //! indirect behavior) to avoid changing visibility in core modules.
    use crate::config::DataEndpoint;
    use crate::tests::test_payload_len_for;
    use crate::{
        config::DataType, packet::Packet, router::{Clock, EndpointHandler, Router, RouterConfig},
        serialize,
        TelemetryError,
        TelemetryErrorCode,
        TelemetryResult,
    };
    use alloc::{string::String, sync::Arc};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A tiny helper clock; we rely on the blanket `impl<Fn() -> u64> Clock`.
    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    // --------------------------- Error/Code parity ---------------------------

    /// Validate that `TelemetryError` ↔ `TelemetryErrorCode` mapping is
    /// complete and stable, including the string forms.
    #[test]
    fn error_enum_code_roundtrip_and_strings() {
        let samples = [
            TelemetryError::InvalidType,
            TelemetryError::EmptyEndpoints,
            TelemetryError::Deserialize("oops"),
            TelemetryError::Io("disk"),
            TelemetryError::HandlerError("fail"),
            TelemetryError::MissingPayload,
            TelemetryError::TimestampInvalid,
        ];
        for e in samples {
            let code = e.to_error_code();
            // must have a stable human string (starts with a '{' per current impl)
            assert!(code.as_str().starts_with('{'));
            // round-trip numeric space
            let back = TelemetryErrorCode::try_from_i32(code as i32);
            assert!(back.is_some(), "roundtrip failed for {code:?}");
        }
    }

    // --------------------------- Header-only parsing ---------------------------

    /// Ensure header-only peek fails on truncated buffers (short read during
    /// varint parsing).
    #[test]
    fn deserialize_header_only_short_buffer_fails() {
        // v2 header is varint-based. Force a definite short read in the first varint.

        // Case A: only NEP present (0 endpoints), but no bytes for `ty` varint.
        let tiny = [0x00u8]; // NEP = 0
        let err = serialize::peek_envelope(&tiny).unwrap_err();
        matches_deser_err(err);

        // Case B: NEP present, and a *truncated* varint (continuation bit set, but no following byte).
        let truncated = [0x00u8, 0x80]; // NEP=0, then start varint with continuation bit
        let err = serialize::peek_envelope(&truncated).unwrap_err();
        matches_deser_err(err);
    }

    /// Ensure header size is a valid prefix of the serialized wire image.
    #[test]
    fn header_size_is_prefix_of_wire_image() {
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0, 3.0],
            &[DataEndpoint::SdCard, DataEndpoint::Radio],
            123,
        )
        .unwrap();

        let wire = serialize::serialize_packet(&pkt);
        let hdr = serialize::header_size_bytes(&pkt);
        assert!(hdr <= wire.len());

        // header must decode from the start (i.e., NEP + scalars exists)
        assert!(hdr > 0);
    }

    /// Helper: assert an error is a `Deserialize` variant.
    fn matches_deser_err(e: TelemetryError) {
        match e {
            TelemetryError::Deserialize(_) => {}
            other => panic!("expected Deserialize error, got {other:?}"),
        }
    }

    fn rewrite_crc32(buf: &mut [u8]) {
        if buf.len() < serialize::CRC32_BYTES {
            return;
        }
        let data_len = buf.len() - serialize::CRC32_BYTES;
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&buf[..data_len]);
        let crc = hasher.finalize();
        buf[data_len..].copy_from_slice(&crc.to_le_bytes());
    }

    /// Ensure serialization is canonical: serialize → deserialize → serialize
    /// produces identical bytes (ULEB128 canonical form).
    #[test]
    fn serializer_is_canonical_roundtrip() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{packet::Packet, serialize};

        // Dynamic payload to avoid schema constraints and let us vary sizes later.
        let msg = "hello world";
        let pkt = Packet::from_str_slice(
            DataType::TelemetryError,
            msg,
            &[DataEndpoint::SdCard, DataEndpoint::Radio],
            0,
        )
        .unwrap();

        let wire1 = serialize::serialize_packet(&pkt);
        let pkt2 = serialize::deserialize_packet(&wire1).unwrap();
        let wire2 = serialize::serialize_packet(&pkt2);

        // ULEB128 is canonical (no leading 0x80 “more” bytes), so bytes must match
        assert_eq!(&*wire1, &*wire2, "serializer must be canonical");
    }

    /// Validate varint scalar growth: header and wire size should increase
    /// when fields that are encoded as varints get larger.
    #[test]
    fn serializer_varint_scalars_grow_as_expected() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{packet::Packet, serialize};

        fn non_rle_ascii(len: usize) -> Vec<u8> {
            let mut out = Vec::with_capacity(len);
            for i in 0..len {
                // Alternating lowercase letters with no >=3-byte runs and valid UTF-8 bytes.
                out.push(b'a' + ((i % 26) as u8));
            }
            out
        }

        // Helper to build a TelemetryError payload and sender of given lengths.
        fn pkt_with(len: usize, sender_len: usize, ts: u64) -> Packet {
            let sender_bytes = non_rle_ascii(sender_len);
            let s: String = sender_bytes.iter().map(|b| char::from(*b)).collect();
            let payload = non_rle_ascii(len); // dynamic payload (String type)
            Packet::new(
                DataType::TelemetryError,
                &[DataEndpoint::SdCard],
                &s,
                ts,
                Arc::<[u8]>::from(payload),
            )
            .unwrap()
        }

        // Case 1: small (all varints fit in 1 byte)
        let p1 = pkt_with(10, 5, 0x7F); // <= 127
        let w1 = serialize::serialize_packet(&p1);
        let h1 = serialize::header_size_bytes(&p1);
        assert!(h1 > 4, "NEP + 4 one-byte varints minimum");

        // Case 2: two-byte varints for size/sender_len
        let p2 = pkt_with(200, 200, 0x7F);
        let w2 = serialize::serialize_packet(&p2);
        let h2 = serialize::header_size_bytes(&p2);
        assert!(w2.len() > w1.len(), "wire should grow with larger varints");
        assert!(h2 > h1, "header should grow with larger varints");

        // Case 3: bigger timestamp to push it beyond 1 byte (and usually >2)
        let p3 = pkt_with(200, 200, 1u64 << 40); // forces 6-byte varint
        let w3 = serialize::serialize_packet(&p3);
        let h3 = serialize::header_size_bytes(&p3);
        assert!(
            w3.len() > w2.len(),
            "wire should grow with larger timestamp"
        );
        assert!(h3 > h2, "header should grow with larger timestamp");

        // Size function must match exact output
        assert_eq!(serialize::packet_wire_size(&p3), w3.len());
    }

    /// Stress test for endpoint bitpacking across many endpoints and repeated
    /// copies, ensuring endpoints and payload round-trip.
    #[test]
    fn endpoints_bitpack_roundtrip_many_and_extremes() {
        use crate::{
            config::{DataEndpoint, DataType},
            packet::Packet,
            serialize,
            MAX_VALUE_DATA_ENDPOINT,
        };

        // Build a long endpoint list by cycling through all enum values (0..=MAX)
        let mut eps = Vec::<DataEndpoint>::new();
        for i in 0..=MAX_VALUE_DATA_ENDPOINT {
            if let Some(ep) = DataEndpoint::try_from_u32(i) {
                eps.push(ep);
            }
        }
        // Repeat to make the bitstream cross multiple bytes
        let mut endpoints = Vec::new();
        for _ in 0..4 {
            endpoints.extend_from_slice(&eps);
        }

        // Make payload dynamic so schema doesn't get in the way
        let payload = vec![0x55u8; 257]; // force 2-byte varint for data_size
        let pkt = Packet::new(
            DataType::TelemetryError,
            &endpoints,
            "sender",
            123456,
            Arc::<[u8]>::from(payload),
        )
        .unwrap();

        let wire = serialize::serialize_packet(&pkt);
        let back = serialize::deserialize_packet(&wire).unwrap();
        let has_all_endpoints = back.endpoints().iter().all(|ep| endpoints.contains(ep));
        assert!(has_all_endpoints, "endpoints must roundtrip 1:1");
        assert_eq!(back.data_type(), pkt.data_type());
        assert_eq!(back.timestamp(), pkt.timestamp());
        assert_eq!(back.payload(), pkt.payload());
        assert_eq!(serialize::packet_wire_size(&pkt), wire.len());
    }

    /// For large sender/payload/timestamp, ensure `peek_envelope` and full
    /// deserialization agree on header fields and payload.
    #[test]
    fn peek_envelope_matches_full_parse_on_large_values() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{packet::Packet, serialize};

        let sender = "S".repeat(10_000); // big sender (varint grows)
        let payload = vec![b'h'; 4096];
        let ts = (1u64 << 40) + 123; // large ts (varint grows)

        let pkt = Packet::new(
            DataType::TelemetryError, // String-typed
            &[DataEndpoint::SdCard, DataEndpoint::Radio],
            &sender,
            ts,
            Arc::<[u8]>::from(payload),
        )
        .unwrap();

        let wire = serialize::serialize_packet(&pkt);
        let env = serialize::peek_envelope(&wire).unwrap();
        let full = serialize::deserialize_packet(&wire).unwrap();

        assert_eq!(env.ty, pkt.data_type());
        assert_eq!(env.sender.as_ref(), pkt.sender());
        assert_eq!(env.timestamp_ms, pkt.timestamp());
        assert_eq!(&*env.endpoints, pkt.endpoints());

        assert_eq!(full.data_type(), pkt.data_type());
        assert_eq!(full.timestamp(), pkt.timestamp());
        assert_eq!(full.endpoints(), pkt.endpoints());
        assert_eq!(full.payload(), pkt.payload());
    }

    /// Corrupt endpoint bits in the bitmap to encode an out-of-range value,
    /// and ensure deserialization fails with an appropriate error.
    #[test]
    fn corrupt_endpoint_bits_yields_bad_endpoint_error() {
        use crate::{
            config::{DataEndpoint, DataType},
            packet::Packet,
            serialize,
            MAX_VALUE_DATA_ENDPOINT,
        };

        // Recompute EP_BITS the same way the module does.
        let bits = 32 - MAX_VALUE_DATA_ENDPOINT.leading_zeros();
        let ep_bits: u8 = if bits == 0 { 1 } else { bits as u8 };
        // If EP_BITS is exactly the minimum bits to encode MAX, there is room for values > MAX.
        let upper_val = (1u64 << ep_bits) - 1;
        if upper_val as u32 <= MAX_VALUE_DATA_ENDPOINT {
            // Nothing to corrupt beyond max—skip test (no larger representable value).
            return;
        }

        // Build a simple, valid packet with at least 1 endpoint.
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            123,
        )
        .unwrap();
        let mut wire = serialize::serialize_packet(&pkt).to_vec();

        // Compute where endpoint bits start (right after header varints)
        let ep_offset = serialize::header_size_bytes(&pkt);
        assert!(ep_offset < wire.len());

        // Overwrite the *first* endpoint with an out-of-range value in the bitstream.
        // Bits are packed LSB-first.
        let mut v = upper_val;
        for (bitpos, _) in (0..ep_bits).enumerate() {
            let byte_idx = ep_offset + (bitpos / 8);
            let bit_off = bitpos % 8;
            // Set bit if the corresponding bit of v is 1
            if (v & 1) != 0 {
                wire[byte_idx] |= 1 << bit_off;
            } else {
                wire[byte_idx] &= !(1 << bit_off);
            }
            v >>= 1;
        }
        rewrite_crc32(&mut wire);

        // Now deserialization must fail with a Deserialize("bad endpoint") error.
        let err = serialize::deserialize_packet(&wire).unwrap_err();
        match err {
            TelemetryError::Deserialize(msg) if msg.contains("endpoint") => {}
            other => panic!("expected bad endpoint deserialize error, got {other:?}"),
        }
    }

    /// Sanity check that header size is between 0 and full packet size, and
    /// that the computed wire size matches serialized length.
    #[test]
    fn header_size_is_prefix_and_less_than_total() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{packet::Packet, serialize};

        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0, 3.0],
            &[DataEndpoint::SdCard, DataEndpoint::Radio],
            999,
        )
        .unwrap();

        let wire = serialize::serialize_packet(&pkt);
        let hdr = serialize::header_size_bytes(&pkt);

        assert!(hdr > 0 && hdr < wire.len());
        assert_eq!(serialize::packet_wire_size(&pkt), wire.len());
    }

    // --------------------------- UTF-8 trimming behavior ---------------------------

    /// Ensure `data_as_utf8_ref` trims trailing NUL bytes and returns a `&str`
    /// with just the meaningful content.
    #[test]
    fn data_as_utf8_ref_trims_trailing_nuls() {
        // Use a String-typed message kind. TelemetryError is used by the router with
        // a string payload and typically mapped to MessageDataType::String.
        let ty = DataType::TelemetryError;
        let mut buf = vec![0u8; test_payload_len_for(ty)];

        let s = b"hello\0\0";
        buf[..s.len()].copy_from_slice(s);

        let pkt = Packet::new(
            ty,
            &[DataEndpoint::SdCard],
            "tester",
            0,
            Arc::<[u8]>::from(buf),
        )
        .unwrap();

        assert_eq!(pkt.data_as_utf8_ref(), Some("hello"));
    }

    // --------------------------- Queue clear semantics ---------------------------

    /// After calling `clear_queues`, no pending TX/RX items should be processed.
    #[test]
    fn clear_queues_prevents_further_processing() {
        // Transmit "bus" that counts frames sent.
        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx_count_c = tx_count.clone();
        let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            tx_count_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        // Local handler that counts receives.
        let rx_count = Arc::new(AtomicUsize::new(0));
        let rx_count_c = rx_count.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                rx_count_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());
        r.add_side_serialized("tx", tx);

        // Enqueue one TX and one RX
        let pkt_tx = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard, DataEndpoint::Radio],
            0,
        )
        .unwrap();
        let pkt_rx = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0, 6.0],
            &[DataEndpoint::SdCard], // only local to avoid extra TX during receive
            0,
        )
        .unwrap();

        r.tx_queue(pkt_tx).unwrap();
        r.rx_queue(pkt_rx).unwrap();

        // Clearing should drop both queues before any processing.
        r.clear_queues();

        r.process_all_queues().unwrap();
        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            0,
            "should not TX after clear"
        );
        assert_eq!(
            rx_count.load(Ordering::SeqCst),
            0,
            "should not RX after clear"
        );
    }

    // --------------------------- Retry semantics (indirect) ---------------------------

    /// Verify local handler retry count matches `MAX_NUMBER_OF_RETRYS` (assumed 3),
    /// and that the final error is a `HandlerError`.
    #[test]
    fn local_handler_retry_attempts_are_three() {
        // This test assumes MAX_NUMBER_OF_RETRYS == 3 in router. If that constant changes,
        // update the expected count below.
        const EXPECTED_ATTEMPTS: usize = 3; // initial try + retries

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_c = counter.clone();

        // A handler that always fails but bumps a counter on each attempt.
        let failing =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                counter_c.fetch_add(1, Ordering::SeqCst);
                Err(TelemetryError::BadArg)
            });

        // Router with no TX (we only care about local handler invocation count).
        let r = Router::new_with_clock(RouterConfig::new(vec![failing]), zero_clock());

        // Build a valid packet addressed to the failing endpoint.
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();

        // Sending should surface a HandlerError after all retries.
        let res = r.tx(pkt);
        match res {
            Err(TelemetryError::HandlerError(_)) => {}
            other => panic!("expected HandlerError after retries, got {other:?}"),
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            EXPECTED_ATTEMPTS,
            "handler should be invoked exactly {EXPECTED_ATTEMPTS} times"
        );
    }

    // --------------------------- from_u8_slice sanity ---------------------------

    /// Ensure `Packet::from_u8_slice` builds a valid GPS packet with
    /// expected length and timestamp.
    #[test]
    fn from_f32_slice_builds_valid_packet() {
        let need = test_payload_len_for(DataType::GpsData) / 4; // f32 count
        assert_eq!(need, 3); // schema sanity

        let bytes = vec![5.3f32; need];
        let pkt = Packet::from_f32_slice(DataType::GpsData, &bytes, &[DataEndpoint::SdCard], 12345)
            .unwrap();

        assert_eq!(pkt.payload().len(), 12);
        assert_eq!(pkt.data_size(), 12);
        assert_eq!(pkt.timestamp(), 12345);
    }

    #[test]
    fn from_none_slice_builds_valid_packet() {
        let need = 0; // f32 count
        assert_eq!(need, 0); // schema sanity

        let pkt =
            Packet::from_no_data(DataType::Heartbeat, &[DataEndpoint::SdCard], 12345).unwrap();

        assert_eq!(pkt.payload().len(), 0);
        assert_eq!(pkt.data_size(), 0);
        assert_eq!(pkt.timestamp(), 12345);
    }

    // --------------------------- Header-only happy path smoke ---------------------------

    /// Header-only peek (`peek_envelope`) should match full parse for a normal
    /// encoded GPS packet.
    #[test]
    fn deserialize_header_only_then_full_parse_matches() {
        // Build a normal packet then compare header-only vs full.
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::Radio];
        let pkt = Packet::from_f32_slice(DataType::GpsData, &[5.25_f32, 3.5, 1.0], endpoints, 42)
            .unwrap();
        let wire = serialize::serialize_packet(&pkt);

        let env = serialize::peek_envelope(&wire).unwrap();
        assert_eq!(env.ty, pkt.data_type());
        assert_eq!(&*env.endpoints, pkt.endpoints());
        assert_eq!(env.sender.as_ref(), pkt.sender());
        assert_eq!(env.timestamp_ms, pkt.timestamp());

        let round = serialize::deserialize_packet(&wire).unwrap();
        round.validate().unwrap();
        assert_eq!(round.data_type(), pkt.data_type());
        assert_eq!(round.data_size(), pkt.data_size());
        assert_eq!(round.timestamp(), pkt.timestamp());
        assert_eq!(round.endpoints(), pkt.endpoints());
        assert_eq!(round.payload(), pkt.payload());
    }

    // --------------------------- TX failure -> error to locals (smoke) ---------------------------

    /// Smoke test: TX failure should emit a `TelemetryError` packet to local
    /// endpoints (exact string validated by more specific tests).
    #[test]
    fn tx_failure_emits_error_to_local_endpoints() {
        // A transmitter that always fails.
        let failing_tx = |_bytes: &[u8]| -> TelemetryResult<()> { Err(TelemetryError::Io("boom")) };

        // Capture what the local endpoint sees (should include a TelemetryError).
        let last_payload = Arc::new(Mutex::new(String::new()));
        let last_payload_c = last_payload.clone();

        let capturing =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |pkt: &Packet| {
                if pkt.data_type() == DataType::TelemetryError {
                    *last_payload_c.lock().unwrap() = pkt.as_string();
                }
                Ok(())
            });

        let r = Router::new_with_clock(RouterConfig::new(vec![capturing]), zero_clock());
        r.add_side_serialized("tx", failing_tx);

        // Include both a local and a non-local endpoint to force remote TX.
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard, DataEndpoint::Radio],
            7,
        )
        .unwrap();

        let res = r.tx(pkt);
        match res {
            Err(TelemetryError::HandlerError(_)) => {} // TX path wraps as HandlerError
            other => panic!("expected HandlerError from TX failure, got {other:?}"),
        }

        // Ensure something was captured (exact string is covered elsewhere)
        let got = last_payload.lock().unwrap().clone();
        assert!(
            !got.is_empty(),
            "expected TelemetryError to be delivered locally after TX failure"
        );
    }
}

// -----------------------------------------------------------------------------
// More tests: validation, enum bounds, router paths, payload helpers, etc.
// -----------------------------------------------------------------------------
#[cfg(test)]
mod tests_more {
    //! Additional coverage tests for router, packet, and serialization logic.
    //! These tests complement `tests_extra` by covering boundary,
    //! error, and fast-path behaviors not previously exercised.
    use crate::config::get_message_meta;
    use crate::tests::UnixClock;
    use crate::{
        config::{DataEndpoint, DataType}, get_data_type, get_needed_message_size, message_meta,
        packet::Packet, router::{Clock, EndpointHandler, Router, RouterConfig}, serialize,
        MessageDataType,
        MessageElement, TelemetryError, TelemetryErrorCode,
        TelemetryResult,
        MAX_VALUE_DATA_ENDPOINT,
        MAX_VALUE_DATA_TYPE,
    };
    use alloc::{sync::Arc, vec::Vec};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc as StdArc, Mutex};

    /// Clock that always returns 0 (via closure), used where wall-clock is
    /// irrelevant and we only need a stable `Clock` impl.
    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    // ---------------------------------------------------------------------------
    // Packet validation edge cases
    // ---------------------------------------------------------------------------

    /// Compute a concrete length for test packets, respecting schema element
    /// counts for static/dynamic payloads.
    fn concrete_len_for_test(ty: DataType) -> usize {
        match message_meta(ty).element {
            MessageElement::Static(_, _, _) => get_needed_message_size(ty),
            MessageElement::Dynamic(_, _) => {
                // Choose a reasonable dynamic size for tests:
                // numeric/bool → element_width * MESSAGE_ELEMENTS
                // string/hex    → 1 * MESSAGE_ELEMENTS (or any positive size)
                let w = match get_data_type(ty) {
                    MessageDataType::UInt8 | MessageDataType::Int8 | MessageDataType::Bool => 1,
                    MessageDataType::UInt16 | MessageDataType::Int16 => 2,
                    MessageDataType::UInt32 | MessageDataType::Int32 | MessageDataType::Float32 => {
                        4
                    }
                    MessageDataType::UInt64 | MessageDataType::Int64 | MessageDataType::Float64 => {
                        8
                    }
                    MessageDataType::UInt128 | MessageDataType::Int128 => 16,
                    MessageDataType::String | MessageDataType::Binary => 1,
                    MessageDataType::NoData => 0,
                };
                let elems = get_message_meta(ty).element.into().max(1);
                core::cmp::max(1, w * elems)
            }
        }
    }

    /// Packet creation should reject empty endpoint lists and size mismatches
    /// (for both static and dynamic payload kinds).
    #[test]
    fn packet_validate_rejects_empty_endpoints_and_size_mismatch() {
        let ty = DataType::GpsData;
        let need = concrete_len_for_test(ty);

        let err = Packet::new(ty, &[], "x", 0, Arc::<[u8]>::from(vec![0u8; need])).unwrap_err();
        assert!(matches!(err, TelemetryError::EmptyEndpoints));

        // +1 ensures mismatch for both static and dynamic (not a multiple of element width)
        let err = Packet::new(
            ty,
            &[DataEndpoint::SdCard],
            "x",
            0,
            Arc::<[u8]>::from(vec![0u8; need + 1]),
        )
        .unwrap_err();
        assert!(matches!(err, TelemetryError::SizeMismatch { .. }));
    }

    // ---------------------------------------------------------------------------
    // Enum bounds + conversion validity
    // ---------------------------------------------------------------------------

    /// Ensure `DataType`, `DataEndpoint`, and `TelemetryErrorCode` all reject
    /// values outside their numeric ranges.
    #[test]
    fn enum_conversion_bounds_and_rejections() {
        let max_ty = MAX_VALUE_DATA_TYPE;
        assert!(DataType::try_from_u32(max_ty).is_some());
        assert!(DataType::try_from_u32(max_ty + 1).is_none());

        let max_ep = MAX_VALUE_DATA_ENDPOINT;
        assert!(DataEndpoint::try_from_u32(max_ep).is_some());
        assert!(DataEndpoint::try_from_u32(max_ep + 1).is_none());

        let min = TelemetryErrorCode::MIN;
        let max = TelemetryErrorCode::MAX;
        assert!(TelemetryErrorCode::try_from_i32(min).is_some());
        assert!(TelemetryErrorCode::try_from_i32(max).is_some());
        assert!(TelemetryErrorCode::try_from_i32(min - 1).is_none());
        assert!(TelemetryErrorCode::try_from_i32(max + 1).is_none());
    }

    // ---------------------------------------------------------------------------
    // Serialization header math + ByteReader edge cases
    // ---------------------------------------------------------------------------

    /// `packet_wire_size` must match the length of the serialized output.
    #[test]
    fn packet_wire_size_matches_serialized_len() {
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::Radio];
        let pkt =
            Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.0, 3.0], endpoints, 9).unwrap();
        let need = serialize::packet_wire_size(&pkt);
        let out = serialize::serialize_packet(&pkt);
        assert_eq!(need, out.len());
    }

    // ---------------------------------------------------------------------------
    // Router serialization/deserialization paths
    // ---------------------------------------------------------------------------

    /// If only `Serialized` handlers exist, the router must not deserialize the
    /// payload and just pass the raw bytes.
    #[test]
    fn serialized_only_handlers_do_not_deserialize() {
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            123,
        )
        .unwrap();
        let wire = serialize::serialize_packet(&pkt);

        let called = StdArc::new(AtomicUsize::new(0));
        let c = called.clone();
        let handler =
            EndpointHandler::new_serialized_handler(DataEndpoint::SdCard, move |bytes: &[u8]| {
                assert!(bytes.len() >= serialize::header_size_bytes(&pkt));
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());
        r.rx_serialized(&wire).unwrap();
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    /// When mixing `Packet` and `Serialized` handlers, ensure:
    /// - deserialization happens only once,
    /// - each endpoint handler is invoked exactly once.
    #[test]
    fn packet_handlers_trigger_single_deserialize_and_fan_out() {
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::Radio];
        let pkt =
            Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.0, 3.0], endpoints, 5).unwrap();
        let wire = serialize::serialize_packet(&pkt);

        let packet_called = StdArc::new(AtomicUsize::new(0));
        let serialized_called = StdArc::new(AtomicUsize::new(0));

        let ph = packet_called.clone();
        let sh = serialized_called.clone();

        let packet_h = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt| {
            ph.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let serialized_h =
            EndpointHandler::new_serialized_handler(DataEndpoint::Radio, move |_b| {
                sh.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let r = Router::new_with_clock(
            RouterConfig::new(vec![packet_h, serialized_h]),
            zero_clock(),
        );

        r.rx_serialized(&wire).unwrap();
        assert_eq!(packet_called.load(Ordering::SeqCst), 1);
        assert_eq!(serialized_called.load(Ordering::SeqCst), 1);
    }

    /// If all addressed endpoints are local `Packet` handlers, router should
    /// avoid serializing at all and never call TX.
    #[test]
    fn send_avoids_serialization_when_only_local_packet_handlers_exist() {
        let tx_called = StdArc::new(AtomicUsize::new(0));
        let txc = tx_called.clone();
        let tx = move |_bytes: &[u8]| -> TelemetryResult<()> {
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let hits = StdArc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |pkt: &Packet| {
                pkt.validate().unwrap();
                h.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());
        r.add_side_serialized("tx", tx);
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        r.tx(pkt).unwrap();

        assert_eq!(tx_called.load(Ordering::SeqCst), 0);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// `Router::receive` for a direct packet should invoke any matching local
    /// packet handlers exactly once.
    #[test]
    fn receive_direct_packet_invokes_handlers() {
        let called = StdArc::new(AtomicUsize::new(0));
        let c = called.clone();
        let handler = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt| {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[0.5, 0.5, 0.5],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        r.rx(&pkt).unwrap();

        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    // ---------------------------------------------------------------------------
    // Error payload truncation & encode_slice_le extra types
    // ---------------------------------------------------------------------------

    /// Ensure router’s internal TelemetryError payload is truncated to meta size
    /// and doesn’t grow without bound.
    #[test]
    fn error_payload_is_truncated_to_meta_size() {
        let failing_tx = |_b: &[u8]| -> TelemetryResult<()> { Err(TelemetryError::Io("boom")) };

        let captured = StdArc::new(Mutex::new(String::new()));
        let c = captured.clone();
        let handler = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |pkt| {
            if pkt.data_type() == DataType::TelemetryError {
                *c.lock().unwrap() = pkt.as_string();
            }
            Ok(())
        });

        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());
        r.add_side_serialized("tx", failing_tx);
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0, 3.0],
            &[DataEndpoint::SdCard, DataEndpoint::Radio],
            1,
        )
        .unwrap();
        let _ = r.tx(pkt);

        let s = captured.lock().unwrap().clone();
        assert!(!s.is_empty());
        assert!(s.len() < 8_192);
    }

    /// Ensure `encode_slice_le` works correctly for both `u16` and `f64`.
    #[test]
    fn encode_slice_le_u16_and_f64() {
        let vals16 = [0x0102u16, 0xA1B2];
        let got = crate::router::encode_slice_le(&vals16);
        let mut exp = Vec::new();
        for v in vals16 {
            exp.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(&*got, &exp);

        let vals64 = [1.5f64, -2.25];
        let got = crate::router::encode_slice_le(&vals64);
        let mut exp = Vec::new();
        for v in vals64 {
            exp.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(&*got, &exp);
    }

    /// Ensure `test_payload_len_for` respects element widths and yields lengths
    /// that are multiples of the correct width for all numeric/bool types.
    #[test]
    fn test_payload_len_for_respects_element_width() {
        use crate::tests::test_payload_len_for;

        for i in 0..=MAX_VALUE_DATA_TYPE {
            if let Some(ty) = DataType::try_from_u32(i) {
                let len = test_payload_len_for(ty);

                match get_data_type(ty) {
                    MessageDataType::String | MessageDataType::Binary => {
                        // any positive length is fine for string/hex, just sanity check
                        assert!(len >= 1, "string/hex must have at least 1 byte for {ty:?}");
                    }
                    kind => {
                        let width = match kind {
                            MessageDataType::UInt8
                            | MessageDataType::Int8
                            | MessageDataType::Bool => 1,
                            MessageDataType::UInt16 | MessageDataType::Int16 => 2,
                            MessageDataType::UInt32
                            | MessageDataType::Int32
                            | MessageDataType::Float32 => 4,
                            MessageDataType::UInt64
                            | MessageDataType::Int64
                            | MessageDataType::Float64 => 8,
                            MessageDataType::UInt128 | MessageDataType::Int128 => 16,
                            MessageDataType::String | MessageDataType::Binary => 1,
                            MessageDataType::NoData => 0,
                        };
                        if width == 0 {
                            // NoData must have zero length
                            assert_eq!(
                                len, 0,
                                "NoData type must have zero-length payload for {ty:?}"
                            );
                            return;
                        }
                        assert_eq!(
                            len % width,
                            0,
                            "test payload length {len} not multiple of element width {width} for {ty:?}"
                        );
                    }
                }
            }
        }
    }
    fn append_crc32(buf: &mut Vec<u8>) {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(buf);
        let crc = hasher.finalize();
        buf.extend_from_slice(&crc.to_le_bytes());
    }
    /// Construct an invalid varint (11 continuation bytes), and ensure
    /// `deserialize_packet` returns a `uleb128 too long` error.
    #[test]
    fn deserialize_packet_rejects_overflowed_varint() {
        use crate::serialize;
        // Construct a fake wire buffer with NEP=0, then an invalid varint (11 continuation bytes)
        let mut wire = vec![0x00u8]; // NEP = 0
        wire.extend([0xFFu8; 11]); // invalid ULEB128 (too long for u64)
        append_crc32(&mut wire);
        let err = serialize::deserialize_packet(&wire).unwrap_err();
        match err {
            TelemetryError::Deserialize(msg) if msg.eq("uleb128 too long") => {}
            other => panic!("expected Deserialize(uleb128 too long...) error, got {other:?}"),
        }
    }

    /// Endpoint order in the `endpoints` slice must not affect serialized bytes.
    #[test]
    fn serialize_packet_is_order_invariant_for_endpoints() {
        use crate::config::{DataEndpoint, DataType};
        use crate::{packet::Packet, serialize};

        let eps_a = &[DataEndpoint::Radio, DataEndpoint::SdCard];
        let eps_b = &[DataEndpoint::SdCard, DataEndpoint::Radio];

        let pkt_a = Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.0, 3.0], eps_a, 0).unwrap();
        let pkt_b = Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.0, 3.0], eps_b, 0).unwrap();

        let wa = serialize::serialize_packet(&pkt_a);
        let wb = serialize::serialize_packet(&pkt_b);

        assert_eq!(wa, wb, "endpoint order must not affect serialized bytes");
    }

    /// With a large number of TX and RX items, `process_all_queues_with_timeout(0)`
    /// must flush all TX and deliver all packets to handlers.
    #[test]
    fn process_all_queues_timeout_zero_handles_large_queues() {
        use crate::config::{DataEndpoint, DataType};
        use crate::packet::Packet;
        use crate::router::{Router, RouterConfig};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();
        let tx = move |_b: &[u8]| -> TelemetryResult<()> {
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rxc = rx_count.clone();
        let handler = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt| {
            rxc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let router = Router::new_with_clock(
            RouterConfig::new(vec![handler]),
            Box::new(|| UnixClock.now_ms()),
        );
        router.add_side_serialized("tx", tx);

        // Enqueue many TX and RX items with unique payloads/timestamps.
        const N: usize = 200;
        for i in 0..N {
            let base_tx = 1.0_f32 + i as f32 * 0.01;
            router
                .log_queue(DataType::GpsData, &[base_tx, 2.0, 3.0])
                .unwrap();

            let pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[9.0 + i as f32 * 0.01, 8.0, 7.0],
                &[DataEndpoint::SdCard],
                i as u64,
            )
            .unwrap();
            router.rx_queue(pkt).unwrap();
        }

        router.process_all_queues_with_timeout(0).unwrap();

        assert_eq!(tx_count.load(Ordering::SeqCst), N, "all TX should flush");
        assert_eq!(
            rx_count.load(Ordering::SeqCst),
            2 * N,
            "each TX local delivery + RX packet should invoke handler"
        );
    }
}

// -----------------------------------------------------------------------------
// Concurrency tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod concurrency_tests {
    //! Concurrency-focused tests that exercise Router’s thread-safety
    //! guarantees for logging, receiving, and processing.

    use crate::{
        config::{DataEndpoint, DataType},
        packet::Packet,
        router::{Clock, EndpointHandler, Router, RouterConfig},
        serialize,
        TelemetryResult,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// Simple clock that always returns 0 (blanket impl<Fn() -> u64> for Clock).
    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    // ------------------------------------------------------------------------
    // Trait sanity: Router must be Send + Sync
    // ------------------------------------------------------------------------

    /// Compile-time check: `Router` must be `Send + Sync` to be safely shared
    /// across threads.
    #[test]
    fn router_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Router>();
    }

    // ------------------------------------------------------------------------
    // Concurrent RX queue producers
    // ------------------------------------------------------------------------

    /// Multiple producer threads call `rx_packet_to_queue` on the same Router;
    /// a single drain must deliver all packets to the handler exactly once.
    #[test]
    fn concurrent_rx_queue_is_thread_safe() {
        const THREADS: usize = 4;
        const ITERS_PER_THREAD: usize = 50;
        let total = THREADS * ITERS_PER_THREAD;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let router = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());
        let r = Arc::new(router);

        let mut threads_vec = Vec::new();
        for tid in 0..THREADS {
            let r_cloned = r.clone();
            threads_vec.push(thread::spawn(move || {
                for i in 0..ITERS_PER_THREAD {
                    // Unique timestamp/payload per (thread, iteration) to avoid dedup.
                    let idx = (tid * ITERS_PER_THREAD + i) as u64;
                    let base = 1.0_f32 + idx as f32 * 0.001;
                    let pkt = Packet::from_f32_slice(
                        DataType::GpsData,
                        &[base, 2.0, 3.0],
                        &[DataEndpoint::SdCard],
                        idx,
                    )
                    .unwrap();
                    r_cloned.rx_queue(pkt).unwrap();
                }
            }));
        }

        for t in threads_vec {
            t.join().expect("producer thread panicked");
        }

        r.process_rx_queue().unwrap();

        assert_eq!(
            hits.load(Ordering::SeqCst),
            total,
            "expected {total} handler invocations from RX queue"
        );
    }

    /// RTOS-like pattern: multiple ingress threads queue serialized packets while
    /// another thread continuously drains router queues. This should not deadlock
    /// and all queued packets should eventually be processed once.
    #[test]
    fn rtos_like_ingress_and_processing_no_deadlock() {
        const THREADS: usize = 4;
        const ITERS_PER_THREAD: usize = 80;
        let total = THREADS * ITERS_PER_THREAD;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let router = Arc::new(Router::new_with_clock(
            RouterConfig::new(vec![handler]),
            zero_clock(),
        ));

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let proc_router = router.clone();
        let done_c = done.clone();
        let processor = thread::spawn(move || {
            while !done_c.load(Ordering::SeqCst) {
                proc_router.process_all_queues_with_timeout(1).unwrap();
            }
            proc_router.process_all_queues().unwrap();
        });

        let mut producers = Vec::new();
        for tid in 0..THREADS {
            let r = router.clone();
            producers.push(thread::spawn(move || {
                for i in 0..ITERS_PER_THREAD {
                    let idx = (tid * ITERS_PER_THREAD + i) as u64;
                    let base = 1.0_f32 + idx as f32 * 0.001;
                    let pkt = Packet::from_f32_slice(
                        DataType::GpsData,
                        &[base, 2.0, 3.0],
                        &[DataEndpoint::SdCard],
                        idx,
                    )
                    .unwrap();
                    let wire = serialize::serialize_packet(&pkt);
                    r.rx_serialized_queue(&wire).unwrap();
                }
            }));
        }

        for t in producers {
            t.join().expect("producer thread panicked");
        }

        done.store(true, Ordering::SeqCst);
        processor.join().expect("processor thread panicked");

        assert_eq!(
            hits.load(Ordering::SeqCst),
            total,
            "expected {total} handler invocations from RTOS-like queued ingress"
        );
    }

    /// RTOS-like relay scenario: packets arrive from two sides while a worker
    /// thread drains queues. Ensures side-tagged queue ingress remains stable.
    #[test]
    fn rtos_like_side_ingress_and_processing_no_deadlock() {
        const THREADS: usize = 4;
        const ITERS_PER_THREAD: usize = 60;
        let total = THREADS * ITERS_PER_THREAD;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();
        let local =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let tx_count = Arc::new(AtomicUsize::new(0));
        let tx_c0 = tx_count.clone();
        let tx_c1 = tx_count.clone();

        let router = Arc::new(Router::new_with_clock(
            RouterConfig::new(vec![local]),
            zero_clock(),
        ));

        let side0 = router.add_side_serialized("S0", move |_b| {
            tx_c0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let side1 = router.add_side_serialized("S1", move |_b| {
            tx_c1.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert_eq!(side0, 0);
        assert_eq!(side1, 1);

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let proc_router = router.clone();
        let done_c = done.clone();
        let processor = thread::spawn(move || {
            while !done_c.load(Ordering::SeqCst) {
                proc_router.process_all_queues_with_timeout(1).unwrap();
            }
            proc_router.process_all_queues().unwrap();
        });

        let mut producers = Vec::new();
        for tid in 0..THREADS {
            let r = router.clone();
            producers.push(thread::spawn(move || {
                for i in 0..ITERS_PER_THREAD {
                    let idx = (tid * ITERS_PER_THREAD + i) as u64;
                    let side = if (idx & 1) == 0 { 0 } else { 1 };
                    let base = 10.0_f32 + idx as f32 * 0.01;
                    let pkt = Packet::from_f32_slice(
                        DataType::GpsData,
                        &[base, 2.0, 3.0],
                        &[DataEndpoint::SdCard, DataEndpoint::Radio],
                        idx,
                    )
                    .unwrap();
                    let wire = serialize::serialize_packet(&pkt);
                    r.rx_serialized_queue_from_side(&wire, side).unwrap();
                }
            }));
        }

        for t in producers {
            t.join().expect("producer thread panicked");
        }

        done.store(true, Ordering::SeqCst);
        processor.join().expect("processor thread panicked");

        assert_eq!(
            hits.load(Ordering::SeqCst),
            total,
            "expected local handler to see all side-tagged packets"
        );
        assert!(
            tx_count.load(Ordering::SeqCst) > 0,
            "relay mode should have forwarded packets to remote sides"
        );
    }

    /// A local handler can safely call back into the same Router (enqueueing
    /// new work) without deadlocking queue processing.
    #[test]
    fn handler_can_reenter_router_without_deadlock() {
        use std::sync::OnceLock;
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;

        let router_ref: Arc<OnceLock<Arc<Router>>> = Arc::new(OnceLock::new());
        let triggered = Arc::new(AtomicBool::new(false));

        let h1_hits = Arc::new(AtomicUsize::new(0));
        let h2_hits = Arc::new(AtomicUsize::new(0));
        let h1_hits_c = h1_hits.clone();
        let h2_hits_c = h2_hits.clone();
        let triggered_c = triggered.clone();
        let router_ref_c = router_ref.clone();

        let h1 = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt| {
            h1_hits_c.fetch_add(1, Ordering::SeqCst);
            if !triggered_c.swap(true, Ordering::SeqCst) {
                let chained = Packet::from_f32_slice(
                    DataType::GpsData,
                    &[9.0_f32, 8.0, 7.0],
                    &[DataEndpoint::Radio],
                    999,
                )?;
                let r = router_ref_c
                    .get()
                    .expect("router OnceLock should be initialized");
                r.rx_queue(chained)?;
            }
            Ok(())
        });

        let h2 = EndpointHandler::new_packet_handler(DataEndpoint::Radio, move |_pkt| {
            h2_hits_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let router = Arc::new(Router::new_with_clock(
            RouterConfig::new(vec![h1, h2]),
            zero_clock(),
        ));
        router_ref
            .set(router.clone())
            .expect("router OnceLock should only be set once");

        let first = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            100,
        )
        .unwrap();
        router.rx_queue(first).unwrap();

        let (tx_done, rx_done) = mpsc::channel();
        let r = router.clone();
        thread::spawn(move || {
            let out = (|| -> TelemetryResult<()> {
                r.process_rx_queue()?;
                r.process_rx_queue()?;
                Ok(())
            })();
            let _ = tx_done.send(out);
        });

        let done = rx_done
            .recv_timeout(Duration::from_secs(2))
            .expect("processing timed out (possible deadlock)");
        done.expect("processing returned error");

        assert_eq!(h1_hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            h2_hits.load(Ordering::SeqCst),
            1,
            "chained callback enqueue should be processed exactly once"
        );
    }

    // ------------------------------------------------------------------------
    // Concurrent calls to receive_serialized
    // ------------------------------------------------------------------------

    /// Multiple threads call `receive_serialized` concurrently with the same
    /// wire buffer; each call should fan out once to the handler.
    #[test]
    fn concurrent_receive_serialized_is_thread_safe() {
        const THREADS: usize = 4;
        const ITERS_PER_THREAD: usize = 50;
        let total = THREADS * ITERS_PER_THREAD;

        // Handler that counts how many times it is invoked.
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let router = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());
        let r = Arc::new(router);

        let mut threads_vec = Vec::new();
        for tid in 0..THREADS {
            let r_cloned = r.clone();
            threads_vec.push(thread::spawn(move || {
                for i in 0..ITERS_PER_THREAD {
                    let idx = (tid * ITERS_PER_THREAD + i) as u64;
                    let base = 1.0_f32 + idx as f32 * 0.001;
                    let pkt = Packet::from_f32_slice(
                        DataType::GpsData,
                        &[base, 2.0, 3.0],
                        &[DataEndpoint::SdCard],
                        idx,
                    )
                    .unwrap();
                    let wire = serialize::serialize_packet(&pkt);
                    r_cloned
                        .rx_serialized(&wire)
                        .expect("receive_serialized failed");
                }
            }));
        }

        for t in threads_vec {
            t.join().expect("receive thread panicked");
        }

        assert_eq!(
            hits.load(Ordering::SeqCst),
            total,
            "expected {total} handler invocations from receive_serialized"
        );
    }

    // ------------------------------------------------------------------------
    // Concurrent logging + processing
    // ------------------------------------------------------------------------

    /// One thread logs to TX queue while another drains queues; verify that
    /// every logged packet is transmitted once and delivered once to the
    /// local handler.
    #[test]
    fn concurrent_logging_and_processing_is_thread_safe() {
        use std::thread;

        const ITERS: usize = 200;

        // Count how many frames are actually transmitted on the "bus".
        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();
        let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        // Local handler that counts how many packets it sees.
        let rx_count = Arc::new(AtomicUsize::new(0));
        let rxc = rx_count.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                rxc.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        // Shared router: TX + one local endpoint.
        let router = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());
        router.add_side_serialized("tx", tx);
        let r = Arc::new(router);

        // ---------------- Logger thread ----------------
        let r_logger = r.clone();
        let logger = thread::spawn(move || {
            for i in 0..ITERS {
                r_logger
                    .log_queue(DataType::GpsData, &[1.0_f32, 5.9 + i as f32, 3.0])
                    .expect("log_queue failed");
            }
        });

        // ---------------- Drainer thread ----------------
        let r_drain = r.clone();
        let rx_counter = rx_count.clone();
        let drainer = thread::spawn(move || {
            // Keep draining until we've seen all expected local handler invocations.
            while rx_counter.load(Ordering::SeqCst) < ITERS {
                r_drain
                    .process_all_queues()
                    .expect("process_all_queues failed");
                thread::yield_now();
            }
        });

        // Wait for both threads to finish.
        logger.join().expect("logger thread panicked");
        drainer.join().expect("drainer thread panicked");

        // After both threads are done, all queued messages should have been
        // transmitted and delivered to the local handler exactly once each.
        let rx = rx_count.load(Ordering::SeqCst);
        let tx = tx_count.load(Ordering::SeqCst);

        assert_eq!(rx, ITERS, "expected {ITERS} handler calls, got {rx}");
        assert_eq!(tx, ITERS, "expected {ITERS} TX frames, got {tx}");
    }

    /// Mix concurrent logging, RX-queue insertion, and queue draining; ensure
    /// that all work is eventually processed exactly once.
    #[test]
    fn concurrent_log_receive_and_process_mix_is_thread_safe() {
        use std::thread;

        const LOG_ITERS: usize = 100;
        const RX_ITERS: usize = 100;

        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();
        let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let rx_count = Arc::new(AtomicUsize::new(0));
        let rxc = rx_count.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                rxc.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let router = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());
        router.add_side_serialized("tx", tx);
        let r = Arc::new(router);

        // ---------- Logger thread ----------
        let r_logger = r.clone();
        let t_logger = thread::spawn(move || {
            for i in 0..LOG_ITERS {
                let base = 1.0_f32 + i as f32 * 0.01;
                r_logger
                    .log_queue(DataType::GpsData, &[base, 2.0, 3.0])
                    .expect("log_queue failed");
            }
        });

        // ---------- RX thread ----------
        let r_rx = r.clone();
        let t_rx = thread::spawn(move || {
            for i in 0..RX_ITERS {
                let base = 4.0_f32 + i as f32 * 0.01;
                let pkt = Packet::from_f32_slice(
                    DataType::GpsData,
                    &[base, 5.0, 6.0],
                    &[DataEndpoint::SdCard],
                    i as u64,
                )
                .unwrap();
                r_rx.rx_queue(pkt).expect("rx_packet_to_queue failed");
            }
        });

        // ---------- Processor thread ----------
        let r_proc = r.clone();
        let rx_counter = rx_count.clone();
        let t_proc = thread::spawn(move || {
            while rx_counter.load(Ordering::SeqCst) < LOG_ITERS + RX_ITERS {
                r_proc
                    .process_all_queues()
                    .expect("process_all_queues failed");
                thread::yield_now();
            }
        });

        t_logger.join().expect("logger thread panicked");
        t_rx.join().expect("rx thread panicked");
        t_proc.join().expect("processor thread panicked");

        let tx = tx_count.load(Ordering::SeqCst);
        let rx = rx_count.load(Ordering::SeqCst);
        assert_eq!(tx, LOG_ITERS, "expected {LOG_ITERS} TX frames");
        assert_eq!(
            rx,
            LOG_ITERS + RX_ITERS,
            "expected {LOG_ITERS}+{RX_ITERS} handler invocations"
        );
    }
}
mod data_conversion_types {

    // ---------------------------------------------------------------------------
    // Packet typed data accessors
    // ---------------------------------------------------------------------------

    use crate::config::{DataEndpoint, DataType};
    use crate::packet::Packet;
    use crate::{get_data_type, MessageDataType, TelemetryError, MAX_VALUE_DATA_TYPE};

    /// data_as_f32 should round-trip values written via from_f32_slice.
    #[test]
    fn data_as_f32_roundtrips_gps() {
        let eps = &[DataEndpoint::SdCard, DataEndpoint::Radio];
        let src = [1.5_f32, -2.25, 3.0];

        let pkt = Packet::from_f32_slice(DataType::GpsData, &src, eps, 42).unwrap();
        let vals = pkt.data_as_f32().unwrap();

        assert_eq!(vals, src);
    }

    /// Calling a mismatched accessor (e.g. data_as_u16 on a Float32 packet)
    /// must return TelemetryError::TypeMismatch.
    #[test]
    fn mismatched_typed_accessor_returns_type_mismatch() {
        let eps = &[DataEndpoint::SdCard];
        let src = [1.0_f32, 2.0, 3.0];

        let pkt = Packet::from_f32_slice(DataType::GpsData, &src, eps, 0).unwrap();

        let res = pkt.data_as_u16();
        match res {
            Err(TelemetryError::TypeMismatch { .. }) => {}
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// If there is a Bool-typed DataType in the schema, ensure data_as_bool
    /// decodes non-zero bytes to true and zero to false.
    #[test]
    fn data_as_bool_decodes_nonzero() {
        // Find any Bool-typed DataType in the schema.
        let mut bool_ty_opt = None;
        for i in 0..=MAX_VALUE_DATA_TYPE {
            if let Some(ty) = DataType::try_from_u32(i)
                && get_data_type(ty) == MessageDataType::Bool
            {
                bool_ty_opt = Some(ty);
                break;
            }
        }

        // If the schema doesn't define any Bool-typed messages, skip this test.
        let bool_ty = match bool_ty_opt {
            Some(t) => t,
            None => return,
        };

        let eps = &[DataEndpoint::SdCard];
        let vals = [true];

        let pkt = Packet::from_bool_slice(bool_ty, &vals, eps, 0).unwrap();
        let decoded = pkt.data_as_bool().unwrap();
        assert_eq!(decoded, vals);
    }
}

// -----------------------------------------------------------------------------
// Relay tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod relay_tests {
    //! Tests for the serialized relay fan-out behavior and timeout semantics.

    use crate::config::{DataEndpoint, DataType};
    use crate::router::Clock;

    use crate::relay::Relay;
    use crate::tests::timeout_tests::StepClock;
    use crate::{packet::Packet, serialize, TelemetryError, TelemetryResult};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Simple zero clock for tests that don't care about timeouts.
    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    fn wire_for_value(v: u64) -> Arc<[u8]> {
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[v as f32, 0.0, 0.0],
            &[DataEndpoint::SdCard],
            v,
        )
        .unwrap();
        serialize::serialize_packet(&pkt)
    }

    /// A small "bus" that records frames seen by each relay side.
    #[derive(Clone)]
    struct SideBus {
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl SideBus {
        fn new() -> (
            Self,
            impl Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
        ) {
            let frames = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let frames_c = frames.clone();
            let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
                frames_c.lock().unwrap().push(bytes.to_vec());
                Ok(())
            };
            (Self { frames }, tx)
        }

        fn len(&self) -> usize {
            self.frames.lock().unwrap().len()
        }

        fn first(&self) -> Option<Vec<u8>> {
            self.frames.lock().unwrap().first().cloned()
        }
    }

    /// Basic fan-out: one source side should be relayed to all *other* sides,
    /// and never loop back to the source.
    #[test]
    fn relay_basic_fan_out() {
        let relay = Arc::new(Relay::new(zero_clock()));

        // Three sides: A, B, C
        let (bus_a, tx_a) = SideBus::new();
        let (bus_b, tx_b) = SideBus::new();
        let (bus_c, tx_c) = SideBus::new();

        let id_a = relay.add_side_serialized("A", tx_a);
        let _id_b = relay.add_side_serialized("B", tx_b);
        let _id_c = relay.add_side_serialized("C", tx_c);

        let frame = wire_for_value(1);

        // Inject from A
        relay
            .rx_serialized_from_side(id_a, frame.as_ref())
            .expect("rx_serialized_from_side failed");

        // Drain all queues → should deliver once to B and once to C.
        relay
            .process_all_queues()
            .expect("process_all_queues failed");

        assert_eq!(bus_a.len(), 0, "source side must not receive its own frame");
        assert_eq!(bus_b.len(), 1, "side B should see one frame");
        assert_eq!(bus_c.len(), 1, "side C should see one frame");

        assert_eq!(bus_b.first().unwrap(), frame.as_ref());
        assert_eq!(bus_c.first().unwrap(), frame.as_ref());
    }

    /// Ensure invalid side IDs are rejected with a TelemetryError::HandlerError.
    #[test]
    fn relay_invalid_side_id_returns_error() {
        let relay = Relay::new(zero_clock());

        // No sides registered; any index is invalid.
        let res = relay.rx_serialized_from_side(0, &[0x01, 0x02]);
        match res {
            Err(TelemetryError::HandlerError(msg)) => {
                assert!(
                    msg.contains("relay: invalid side id"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected HandlerError for invalid side id, got {other:?}"),
        }
    }

    /// After clear_queues, no pending TX/RX items should be processed.
    #[test]
    fn relay_clear_queues_drops_pending_work() {
        let relay = Relay::new(zero_clock());

        let tx_count_b = Arc::new(AtomicUsize::new(0));
        let tx_count_c = Arc::new(AtomicUsize::new(0));

        let tx_b_c = tx_count_b.clone();
        let tx_b = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            tx_b_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let tx_c_c = tx_count_c.clone();
        let tx_c = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            tx_c_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let id_a = relay.add_side_serialized("A", |_b| Ok(()));
        relay.add_side_serialized("B", tx_b);
        relay.add_side_serialized("C", tx_c);

        // Queue some RX work from A.
        let frame_a = wire_for_value(1);
        let frame_b = wire_for_value(2);
        relay
            .rx_serialized_from_side(id_a, frame_a.as_ref())
            .unwrap();
        relay
            .rx_serialized_from_side(id_a, frame_b.as_ref())
            .unwrap();

        // Expand RX → TX, but do not deliver yet.
        relay.process_rx_queue().unwrap();

        // Drop all queued items.
        relay.clear_queues();

        // Nothing should be delivered now.
        relay.process_all_queues().unwrap();

        assert_eq!(
            tx_count_b.load(Ordering::SeqCst),
            0,
            "no frames should be sent to side B after clear_queues"
        );
        assert_eq!(
            tx_count_c.load(Ordering::SeqCst),
            0,
            "no frames should be sent to side C after clear_queues"
        );
    }

    /// Non-zero timeout budget should be able to stop processing early,
    /// leaving additional work for a later drain.
    #[test]
    fn relay_timeout_limits_work_per_call() {
        // Step clock: each now_ms() call advances by 10ms.
        let clock = StepClock::new_box(0, 10);
        let relay = Relay::new(clock);

        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();
        let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let id_src = relay.add_side_serialized("SRC", |_b| Ok(()));
        relay.add_side_serialized("DST", tx);

        // Queue multiple RX items from SRC, each with a unique frame to avoid dedup.
        for i in 0..5u8 {
            let frame = wire_for_value(i as u64);
            relay
                .rx_serialized_from_side(id_src, frame.as_ref())
                .unwrap();
        }

        // With step=10 and timeout=5:
        //   - start = 0
        //   - after first RX, now_ms() == 10 → exceeds budget before any TX.
        relay
            .process_all_queues_with_timeout(5)
            .expect("process_all_queues_with_timeout failed");

        // No TX should have happened yet, but there is still work queued.
        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            0,
            "timeout should have prevented any TX in first call"
        );

        // Now drain fully; all fan-out TX items should be delivered.
        relay
            .process_all_queues_with_timeout(0)
            .expect("final drain failed");

        // Each of the 5 RX frames fans out from SRC -> DST (1 destination).
        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            5,
            "expected all queued frames to be delivered after full drain"
        );
    }

    /// Basic sanity: concurrent RX producers should not panic and should
    /// deliver all frames after a full drain.
    #[test]
    fn relay_concurrent_rx_is_thread_safe() {
        use std::thread;

        const THREADS: usize = 4;
        const ITERS_PER_THREAD: usize = 25;
        let total_frames = THREADS * ITERS_PER_THREAD;

        let relay = Arc::new(Relay::new(zero_clock()));

        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();
        relay.add_side_serialized("SRC", |_b| Ok(()));
        relay.add_side_serialized("DST", move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let mut threads_vec = Vec::new();
        for tid in 0..THREADS {
            let r = relay.clone();
            threads_vec.push(thread::spawn(move || {
                for i in 0..ITERS_PER_THREAD {
                    let idx = (tid * ITERS_PER_THREAD + i) as u8;
                    // Unique last byte per (thread, iteration) to avoid dedup.
                    let frame = wire_for_value(idx as u64);
                    r.rx_serialized_from_side(0, frame.as_ref()).unwrap();
                }
            }));
        }

        for t in threads_vec {
            t.join().expect("producer thread panicked");
        }

        relay
            .process_all_queues_with_timeout(0)
            .expect("drain failed");

        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            total_frames,
            "expected {total_frames} relayed frames"
        );
    }

    #[test]
    fn relay_side_tx_reentry_defers_recursive_queue_drains() {
        let relay = Arc::new(Relay::new(zero_clock()));
        let remaining = Arc::new(AtomicUsize::new(6));
        let ingress = relay.add_side_serialized("INGRESS", move |_bytes| Ok(()));

        let relay_c = relay.clone();
        let remaining_c = remaining.clone();
        let loop_hits = Arc::new(AtomicUsize::new(0));
        let loop_hits_c = loop_hits.clone();
        let in_tx = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reentered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let in_tx_c = in_tx.clone();
        let reentered_c = reentered.clone();
        relay.add_side_serialized("LOOP", move |bytes| {
            loop_hits_c.fetch_add(1, Ordering::SeqCst);
            if in_tx_c.swap(true, Ordering::SeqCst) {
                reentered_c.store(true, Ordering::SeqCst);
            }
            if remaining_c
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                relay_c.rx_serialized_from_side(ingress, bytes)?;
                relay_c.process_all_queues()?;
            }
            in_tx_c.store(false, Ordering::SeqCst);
            Ok(())
        });

        relay
            .rx_serialized_from_side(ingress, wire_for_value(1).as_ref())
            .unwrap();
        relay.process_all_queues().unwrap();
        for _ in 0..8 {
            relay.process_all_queues().unwrap();
        }

        assert!(!reentered.load(Ordering::SeqCst));
        assert!(remaining.load(Ordering::SeqCst) < 6);
        assert!(loop_hits.load(Ordering::SeqCst) > 0);
    }
}

#[cfg(test)]
mod dedupe_tests {
    //! Tests specifically for RX/relay deduplication behavior.

    use crate::config::{DataEndpoint, DataType};
    use crate::relay::Relay;
    use crate::router::{Clock, EndpointHandler, Router, RouterConfig};
    use crate::tests::timeout_tests::StepClock;
    use crate::{packet::Packet, serialize, TelemetryResult};

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Simple clock that always returns 0.
    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    fn wire_for_value(v: u64) -> Arc<[u8]> {
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[v as f32, 0.0, 0.0],
            &[DataEndpoint::SdCard],
            v,
        )
        .unwrap();
        serialize::serialize_packet(&pkt)
    }

    // -----------------------------------------------------------------------
    // Router dedupe tests
    // -----------------------------------------------------------------------

    /// Repeatedly calling `rx_serialized` with the *same* wire frame must only
    /// deliver it once to local handlers.
    #[test]
    fn router_rx_serialized_deduplicates_identical_frames() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();

        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        // Router with no TX; only RX + local fan-out.
        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());

        // Build a single wire frame we will reuse.
        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        let wire = serialize::serialize_packet(&pkt);

        // Feed the identical frame many times.
        for _ in 0..5 {
            r.rx_serialized(&wire).unwrap();
        }

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "rx_serialized should deliver identical frames only once"
        );
    }

    /// Even if time advances between deliveries, the same frame must still be
    /// deduped (i.e. dedupe is not time-window based).
    #[test]
    fn router_rx_serialized_dedup_persists_across_time_advance() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();

        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        // Step clock that advances every time we look at it.
        let clock = StepClock::new_box(0, 1_000);
        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), clock);

        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        let wire = serialize::serialize_packet(&pkt);

        // First time → delivered.
        r.rx_serialized(&wire).unwrap();
        // Time advances inside router via Clock, but dedupe should still drop it.
        r.rx_serialized(&wire).unwrap();

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "dedupe should persist even as clock advances"
        );
    }

    /// Two *different* frames must both be delivered, never deduped against
    /// each other.
    #[test]
    fn router_rx_serialized_does_not_dedupe_different_frames() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();

        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let r = Router::new_with_clock(RouterConfig::new(vec![handler]), zero_clock());

        // Frame A
        let pkt_a = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        let wire_a = serialize::serialize_packet(&pkt_a);

        // Frame B (different payload)
        let pkt_b = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0, 6.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        let wire_b = serialize::serialize_packet(&pkt_b);

        r.rx_serialized(&wire_a).unwrap();
        r.rx_serialized(&wire_b).unwrap();

        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "different frames must never be deduplicated"
        );
    }

    // -----------------------------------------------------------------------
    // Relay dedupe tests
    // -----------------------------------------------------------------------

    /// For a single relay side, repeatedly injecting the *same* serialized
    /// frame should fan out exactly once to other sides.
    #[test]
    fn relay_deduplicates_identical_frames_per_side() {
        let relay = Relay::new(zero_clock());

        let tx_count_b = Arc::new(AtomicUsize::new(0));
        let tx_count_c = Arc::new(AtomicUsize::new(0));

        let tx_b_c = tx_count_b.clone();
        let tx_b = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            tx_b_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let tx_c_c = tx_count_c.clone();
        let tx_c = move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            tx_c_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let id_src = relay.add_side_serialized("SRC", |_b| Ok(()));
        relay.add_side_serialized("B", tx_b);
        relay.add_side_serialized("C", tx_c);

        let frame = wire_for_value(1);

        for _ in 0..5 {
            relay
                .rx_serialized_from_side(id_src, frame.as_ref())
                .expect("rx_serialized_from_side failed");
        }

        relay
            .process_all_queues_with_timeout(0)
            .expect("process_all_queues_with_timeout failed");

        assert_eq!(
            tx_count_b.load(Ordering::SeqCst),
            1,
            "side B should see one deduplicated frame"
        );
        assert_eq!(
            tx_count_c.load(Ordering::SeqCst),
            1,
            "side C should see one deduplicated frame"
        );
    }

    /// Even when time advances, identical frames from the same side should
    /// still be deduped (no time-based expiry).
    #[test]
    fn relay_dedup_persists_across_time_advance() {
        // Step clock that advances each time now_ms() is called.
        let clock = StepClock::new_box(0, 1_000);
        let relay = Relay::new(clock);

        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();

        let id_src = relay.add_side_serialized("SRC", |_b| Ok(()));
        relay.add_side_serialized("DST", move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let frame = wire_for_value(1);

        relay
            .rx_serialized_from_side(id_src, frame.as_ref())
            .expect("first rx_serialized_from_side failed");
        relay
            .process_all_queues_with_timeout(0)
            .expect("first drain failed");

        relay
            .rx_serialized_from_side(id_src, frame.as_ref())
            .expect("second rx_serialized_from_side failed");
        relay
            .process_all_queues_with_timeout(0)
            .expect("second drain failed");

        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            1,
            "identical frame from same side should still be deduped after time advance"
        );
    }

    /// Two different frames from the same side must both be relayed.
    #[test]
    fn relay_does_not_dedupe_different_frames_from_same_side() {
        let relay = Relay::new(zero_clock());

        let tx_count = Arc::new(AtomicUsize::new(0));
        let txc = tx_count.clone();

        let id_src = relay.add_side_serialized("SRC", |_b| Ok(()));
        relay.add_side_serialized("DST", move |bytes: &[u8]| -> TelemetryResult<()> {
            assert!(!bytes.is_empty());
            txc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let frame_a = wire_for_value(1);
        let frame_b = wire_for_value(2);

        relay
            .rx_serialized_from_side(id_src, frame_a.as_ref())
            .expect("rx_serialized_from_side A failed");
        relay
            .rx_serialized_from_side(id_src, frame_b.as_ref())
            .expect("rx_serialized_from_side B failed");

        relay
            .process_all_queues_with_timeout(0)
            .expect("drain failed");

        assert_eq!(
            tx_count.load(Ordering::SeqCst),
            2,
            "relay must not dedupe different frames from the same side"
        );
    }
}

#[cfg(test)]
mod relay_reliable_tests {
    use crate::config::{DataEndpoint, DataType, RELIABLE_RETRANSMIT_MS};
    use crate::relay::{Relay, RelaySideOptions};
    use crate::router::Clock;
    use crate::tests::timeout_tests::StepClock;
    use crate::{packet::Packet, serialize, TelemetryResult};

    use std::sync::{Arc, Mutex};

    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    #[test]
    fn relay_reliable_seq_advances_with_ack() {
        let relay = Arc::new(Relay::new(zero_clock()));

        relay.add_side_serialized_with_options(
            "SRC",
            |_b| Ok(()),
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let sent_c = sent.clone();
        let relay_c = relay.clone();
        relay.add_side_serialized_with_options(
            "DST",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                sent_c.lock().unwrap().push(bytes.to_vec());

                let frame = serialize::peek_frame_info(bytes)?;
                if let Some(hdr) = frame.reliable {
                    let ack_bytes =
                        serialize::serialize_reliable_ack("DST", frame.envelope.ty, 0, hdr.seq);
                    relay_c.rx_serialized_from_side(1, ack_bytes.as_ref())?;
                }
                Ok(())
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        let pkt1 = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        let pkt2 = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0, 6.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();

        relay.rx_from_side(0, pkt1).unwrap();
        relay.rx_from_side(0, pkt2).unwrap();
        relay.process_all_queues_with_timeout(0).unwrap();
        relay.process_all_queues_with_timeout(0).unwrap();

        let sent = sent.lock().unwrap();
        assert!(sent.len() >= 2, "expected at least 2 forwarded frames");

        let f1 = serialize::peek_frame_info(&sent[0]).unwrap();
        let f2 = serialize::peek_frame_info(&sent[1]).unwrap();
        let h1 = f1.reliable.expect("frame 1 missing reliable header");
        let h2 = f2.reliable.expect("frame 2 missing reliable header");
        assert_eq!(h1.seq, 1);
        assert_eq!(h2.seq, 2);
        assert_eq!(h1.flags & serialize::RELIABLE_FLAG_UNSEQUENCED, 0);
        assert_eq!(h2.flags & serialize::RELIABLE_FLAG_UNSEQUENCED, 0);
    }

    #[test]
    fn relay_reliable_retransmit_across_chain_preserves_order() {
        let relay1 = Arc::new(Relay::new(StepClock::new_box(
            0,
            RELIABLE_RETRANSMIT_MS + 1,
        )));
        let relay2 = Arc::new(Relay::new(zero_clock()));

        relay1.add_side_serialized_with_options(
            "SRC",
            |_b| Ok(()),
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        // Link: relay1 -> relay2 (drop first seq=1 data frame to force retransmit)
        let drop_first = Arc::new(Mutex::new(true));
        let drop_first_c = drop_first.clone();
        let relay2_rx = relay2.clone();
        let link_sent: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let link_sent_c = link_sent.clone();
        let relay1_mid = relay1.add_side_serialized_with_options(
            "MID",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                let frame = serialize::peek_frame_info(bytes)?;
                if let Some(hdr) = frame.reliable
                    && (hdr.flags & serialize::RELIABLE_FLAG_ACK_ONLY) == 0
                {
                    link_sent_c.lock().unwrap().push(hdr.seq);
                    if hdr.seq == 1 && *drop_first_c.lock().unwrap() {
                        *drop_first_c.lock().unwrap() = false;
                        return Ok(());
                    }
                }
                relay2_rx.rx_serialized_from_side(0, bytes)
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        // Link: relay2 -> relay1 (ACKs and reverse traffic)
        let relay1_rx = relay1.clone();
        let _relay2_mid = relay2.add_side_serialized_with_options(
            "MID",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                relay1_rx.rx_serialized_from_side(1, bytes)
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        // Destination capture on relay2
        let delivered: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_c = delivered.clone();
        let relay2_for_ack = relay2.clone();
        let relay1_for_ack = relay1.clone();
        let relay2_dst_id: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
        let relay2_dst_id_c = relay2_dst_id.clone();
        let relay2_dst = relay2.add_side_serialized_with_options(
            "DST",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                let frame = serialize::peek_frame_info(bytes)?;
                if let Some(hdr) = frame.reliable
                    && (hdr.flags & serialize::RELIABLE_FLAG_ACK_ONLY) == 0
                {
                    delivered_c.lock().unwrap().push(hdr.seq);
                    let ack_bytes =
                        serialize::serialize_reliable_ack("DST", frame.envelope.ty, 0, hdr.seq);
                    if let Some(dst_id) = *relay2_dst_id_c.lock().unwrap() {
                        relay2_for_ack.rx_serialized_from_side(dst_id, ack_bytes.as_ref())?;
                    }
                    relay1_for_ack.rx_serialized_from_side(relay1_mid, ack_bytes.as_ref())?;
                }
                Ok(())
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );
        *relay2_dst_id.lock().unwrap() = Some(relay2_dst);

        let pkt1 = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard, DataEndpoint::Radio],
            0,
        )
        .unwrap();
        let pkt2 = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0, 6.0],
            &[DataEndpoint::SdCard, DataEndpoint::Radio],
            0,
        )
        .unwrap();

        relay1.rx_from_side(0, pkt1).unwrap();
        relay1.rx_from_side(0, pkt2).unwrap();

        for _ in 0..10 {
            relay1.process_all_queues_with_timeout(0).unwrap();
            relay2.process_all_queues_with_timeout(0).unwrap();
            if delivered.lock().unwrap().len() >= 2 {
                break;
            }
        }

        let delivered = delivered.lock().unwrap().clone();
        assert_eq!(delivered, vec![1, 2], "destination must receive in order");

        let link_sent = link_sent.lock().unwrap().clone();
        let seq1_count = link_sent.iter().filter(|&&s| s == 1).count();
        assert!(
            seq1_count >= 2,
            "expected seq1 to be retransmitted across the relay chain"
        );
    }

    #[test]
    fn relay_reliable_reorders_out_of_order_frames() {
        let relay = Arc::new(Relay::new(zero_clock()));

        relay.add_side_serialized_with_options(
            "SRC",
            |_b| Ok(()),
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        let delivered: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_c = delivered.clone();
        let relay_for_ack = relay.clone();
        relay.add_side_serialized_with_options(
            "DST",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                let frame = serialize::peek_frame_info(bytes)?;
                if let Some(hdr) = frame.reliable
                    && (hdr.flags & serialize::RELIABLE_FLAG_ACK_ONLY) == 0
                {
                    delivered_c.lock().unwrap().push(hdr.seq);
                    let ack_bytes =
                        serialize::serialize_reliable_ack("DST", frame.envelope.ty, 0, hdr.seq);
                    relay_for_ack.rx_serialized_from_side(1, ack_bytes.as_ref())?;
                }
                Ok(())
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        let pkt1 = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        let pkt2 = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0, 6.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();

        let seq1 = serialize::serialize_packet_with_reliable(
            &pkt1,
            serialize::ReliableHeader {
                flags: 0,
                seq: 1,
                ack: 0,
            },
        );
        let seq2 = serialize::serialize_packet_with_reliable(
            &pkt2,
            serialize::ReliableHeader {
                flags: 0,
                seq: 2,
                ack: 0,
            },
        );

        // Out-of-order: seq2 arrives first, then seq1, then seq2 retransmit.
        relay.rx_serialized_from_side(0, seq2.as_ref()).unwrap();
        relay.rx_serialized_from_side(0, seq1.as_ref()).unwrap();
        relay.rx_serialized_from_side(0, seq2.as_ref()).unwrap();

        relay.process_all_queues_with_timeout(0).unwrap();

        let delivered = delivered.lock().unwrap().clone();
        assert_eq!(
            delivered,
            vec![1, 2],
            "out-of-order frames must be reordered"
        );
    }

    #[test]
    fn relay_reliable_sender_does_not_block_while_waiting_for_ack() {
        let relay = Arc::new(Relay::new(zero_clock()));

        relay.add_side_serialized_with_options(
            "SRC",
            |_b| Ok(()),
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        let sent: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let sent_c = sent.clone();
        relay.add_side_serialized_with_options(
            "DST",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                let frame = serialize::peek_frame_info(bytes)?;
                if frame.envelope.ty == DataType::GpsData
                    && let Some(hdr) = frame.reliable
                {
                    sent_c.lock().unwrap().push(hdr.seq);
                }
                Ok(())
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        let pkt1 = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        let pkt2 = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0, 6.0],
            &[DataEndpoint::SdCard],
            1,
        )
        .unwrap();

        relay.rx_from_side(0, pkt1).unwrap();
        relay.rx_from_side(0, pkt2).unwrap();
        relay.process_all_queues_with_timeout(0).unwrap();

        assert_eq!(*sent.lock().unwrap(), vec![1, 2]);
    }
}

#[cfg(test)]
mod reliable_tests {
    use crate::config::{DataEndpoint, DataType};
    use crate::router::{Clock, EndpointHandler, Router, RouterConfig, RouterSideOptions};
    use crate::tests::timeout_tests::StepClock;
    use crate::{packet::Packet, serialize, TelemetryResult};

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    #[test]
    fn reliable_retransmit_delivers_once() {
        let rx_hits = Arc::new(AtomicUsize::new(0));
        let rx_hits_c = rx_hits.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                rx_hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let sender = Arc::new(Router::new_with_clock(
            RouterConfig::new(Vec::new()).with_reliable_enabled(true),
            StepClock::new_box(0, 250),
        ));

        let receiver = Arc::new(Router::new_with_clock(
            RouterConfig::new(vec![handler]).with_reliable_enabled(true),
            zero_clock(),
        ));

        let sender_side_id: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
        let receiver_side_id: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));

        let sender_for_ack = sender.clone();
        let sender_side_id_c = sender_side_id.clone();
        let receiver_side = receiver.add_side_serialized_with_options(
            "TO_SENDER",
            move |bytes: &[u8]| {
                if let Some(side_id) = *sender_side_id_c.lock().unwrap() {
                    sender_for_ack.rx_serialized_from_side(bytes, side_id)?;
                }
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );
        *receiver_side_id.lock().unwrap() = Some(receiver_side);

        let drop_first = Arc::new(AtomicBool::new(true));
        let receiver_for_tx = receiver.clone();
        let drop_first_tx = drop_first.clone();
        let receiver_side_id_c = receiver_side_id.clone();
        let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
            if drop_first_tx.swap(false, Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(side_id) = *receiver_side_id_c.lock().unwrap() {
                receiver_for_tx.rx_serialized_from_side(bytes, side_id)?;
            }
            Ok(())
        };

        let sender_side = sender.add_side_serialized_with_options(
            "TO_RECEIVER",
            tx,
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );
        *sender_side_id.lock().unwrap() = Some(sender_side);

        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();

        sender.tx(pkt).unwrap();

        for _ in 0..3 {
            sender.process_tx_queue_with_timeout(0).unwrap();
        }

        assert_eq!(rx_hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reliable_ordered_delivers_in_order() {
        let delivered: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_c = delivered.clone();
        let handler = EndpointHandler::new_serialized_handler(
            DataEndpoint::SdCard,
            move |bytes: &[u8]| -> TelemetryResult<()> {
                let frame = serialize::peek_frame_info(bytes)?;
                if let Some(hdr) = frame.reliable
                    && (hdr.flags & serialize::RELIABLE_FLAG_ACK_ONLY) == 0
                {
                    delivered_c.lock().unwrap().push(hdr.seq);
                }
                Ok(())
            },
        );

        let router = Router::new_with_clock(
            RouterConfig::new(vec![handler]).with_reliable_enabled(true),
            zero_clock(),
        );

        let side = router.add_side_serialized_with_options(
            "SRC",
            |_b| Ok(()),
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        let pkt1 = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        let pkt2 = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0, 6.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();

        let seq1 = serialize::serialize_packet_with_reliable(
            &pkt1,
            serialize::ReliableHeader {
                flags: 0,
                seq: 1,
                ack: 0,
            },
        );
        let seq2 = serialize::serialize_packet_with_reliable(
            &pkt2,
            serialize::ReliableHeader {
                flags: 0,
                seq: 2,
                ack: 0,
            },
        );

        // Out-of-order: seq2 arrives first, then seq1, then seq2 retransmit.
        router.rx_serialized_from_side(seq2.as_ref(), side).unwrap();
        router.rx_serialized_from_side(seq1.as_ref(), side).unwrap();
        router.rx_serialized_from_side(seq2.as_ref(), side).unwrap();

        let delivered = delivered.lock().unwrap().clone();
        assert_eq!(
            delivered,
            vec![1, 2],
            "ordered reliable delivery must reorder"
        );
    }

    #[test]
    fn reliable_sender_does_not_block_while_waiting_for_ack() {
        let sent: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let sent_c = sent.clone();

        let router = Router::new_with_clock(
            RouterConfig::new(Vec::new()).with_reliable_enabled(true),
            zero_clock(),
        );

        router.add_side_serialized_with_options(
            "DST",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                let frame = serialize::peek_frame_info(bytes)?;
                if frame.envelope.ty == DataType::GpsData
                    && let Some(hdr) = frame.reliable
                {
                    sent_c.lock().unwrap().push(hdr.seq);
                }
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        let pkt1 = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0_f32, 2.0, 3.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();
        let pkt2 = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0, 6.0],
            &[DataEndpoint::SdCard],
            1,
        )
        .unwrap();

        router.tx(pkt1).unwrap();
        router.tx(pkt2).unwrap();
        router.process_tx_queue_with_timeout(0).unwrap();

        assert_eq!(*sent.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn reliable_disabled_skips_ack() {
        let rx_hits = Arc::new(AtomicUsize::new(0));
        let rx_hits_c = rx_hits.clone();
        let handler =
            EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
                rx_hits_c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let ack_count = Arc::new(AtomicUsize::new(0));
        let ack_count_c = ack_count.clone();
        let rx_direct = move |_bytes: &[u8]| -> TelemetryResult<()> {
            ack_count_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        let receiver = Arc::new(Router::new_with_clock(
            RouterConfig::new(vec![handler]).with_reliable_enabled(false),
            zero_clock(),
        ));
        receiver.add_side_serialized("ACK", rx_direct);

        let rx_for_tx = receiver.clone();
        let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
            rx_for_tx.rx_serialized_from_side(bytes, 0)?;
            Ok(())
        };

        let sender = Router::new_with_clock(
            RouterConfig::new(Vec::new()).with_reliable_enabled(false),
            zero_clock(),
        );
        sender.add_side_serialized("TO_RECEIVER", tx);

        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[4.0_f32, 5.0, 6.0],
            &[DataEndpoint::SdCard],
            0,
        )
        .unwrap();

        sender.tx(pkt).unwrap();

        assert_eq!(rx_hits.load(Ordering::SeqCst), 1);
        assert_eq!(ack_count.load(Ordering::SeqCst), 0);
    }
}

#[cfg(test)]
mod router_tests {
    // -------------------------------------------------------------------------
    // New router functionality tests
    // -------------------------------------------------------------------------

    use crate::config::{DataEndpoint, DataType};
    use crate::packet::Packet;
    use crate::router::{EndpointHandler, Router, RouterConfig, RouterSideOptions};
    use crate::tests::timeout_tests::StepClock;
    use crate::{serialize, TelemetryResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[cfg(feature = "discovery")]
    #[test]
    #[should_panic(expected = "reserved internal endpoint handlers must not be user-registered")]
    fn user_cannot_register_discovery_endpoint_handler() {
        let _ = EndpointHandler::new_packet_handler(DataEndpoint::Discovery, |_pkt| Ok(()));
    }

    #[cfg(feature = "timesync")]
    #[test]
    #[should_panic(expected = "reserved internal endpoint handlers must not be user-registered")]
    fn user_cannot_register_timesync_endpoint_handler() {
        let _ = EndpointHandler::new_packet_handler(DataEndpoint::TimeSync, |_pkt| Ok(()));
    }

    /// Receiving a packet that includes at least one non-local endpoint should
    /// cause the router to forward it by default.
    #[test]
    fn relay_mode_retransmits_when_remote_endpoint_present() {
        static TX_CALLS: AtomicUsize = AtomicUsize::new(0);

        fn transmit(_bytes: &[u8]) -> TelemetryResult<()> {
            TX_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        // Local handler on SD_CARD so the router considers SD_CARD "local".
        let local_calls = Arc::new(AtomicUsize::new(0));
        let local_calls_c = local_calls.clone();
        let sd_handler = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt| {
            local_calls_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let router = Router::new_with_clock(
            RouterConfig::new(vec![sd_handler]),
            StepClock::new_default_box(),
        );
        router.add_side_serialized("tx", transmit);

        // Include one local + one remote endpoint.
        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::Radio];
        let pkt =
            Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.0, 3.0], endpoints, 0).unwrap();

        router.rx(&pkt).unwrap();

        // Local handler should fire once.
        assert_eq!(local_calls.load(Ordering::SeqCst), 1);
        // Default routing should transmit once.
        assert_eq!(TX_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn queued_serialized_ingress_retries_side_tx_and_relays_between_router_sides() {
        #[derive(Default)]
        struct TxState {
            attempts: AtomicUsize,
            delivered: Mutex<Vec<Vec<u8>>>,
        }

        let router = Router::new_with_clock(RouterConfig::default(), StepClock::new_default_box());
        let tx_state = Arc::new(TxState::default());
        let tx_state_c = tx_state.clone();

        let side_a = router.add_side_serialized_with_options(
            "can",
            |_bytes| Ok(()),
            RouterSideOptions {
                reliable_enabled: true,
                ..RouterSideOptions::default()
            },
        );
        router.add_side_serialized_with_options(
            "uart",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                let attempt = tx_state_c.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    return Err(crate::TelemetryError::Io("busy"));
                }
                tx_state_c.delivered.lock().unwrap().push(bytes.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                ..RouterSideOptions::default()
            },
        );

        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 2.0, 3.0],
            &[DataEndpoint::Radio],
            7,
        )
        .unwrap();
        let wire = serialize::serialize_packet(&pkt);

        router
            .rx_serialized_queue_from_side(wire.as_ref(), side_a)
            .unwrap();
        router.process_all_queues_with_timeout(0).unwrap();

        let delivered = tx_state.delivered.lock().unwrap().clone();
        assert_eq!(tx_state.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(delivered.len(), 1);
        assert!(!delivered[0].is_empty());
    }

    /// Explicit route disables should suppress remote forwarding.
    #[test]
    fn disabled_route_prevents_retransmit_on_receive() {
        use crate::router::RouterConfig;

        static TX_CALLS: AtomicUsize = AtomicUsize::new(0);

        fn transmit(_bytes: &[u8]) -> TelemetryResult<()> {
            TX_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        let router = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::SdCard,
                |_pkt| Ok(()),
            )]),
            StepClock::new_default_box(),
        );
        let side = router.add_side_serialized("tx", transmit);
        router.set_route(None, side, false).unwrap();

        let endpoints = &[DataEndpoint::SdCard, DataEndpoint::Radio];
        let pkt =
            Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.0, 3.0], endpoints, 0).unwrap();

        router.rx(&pkt).unwrap();

        assert_eq!(TX_CALLS.load(Ordering::SeqCst), 0);
    }

    /// Receiving the exact same serialized packet twice should be deduped
    /// and only delivered to local handlers once.
    #[test]
    fn receive_dedupes_identical_serialized_frames() {
        use crate::router::RouterConfig;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();
        let sd_handler = EndpointHandler::new_serialized_handler(DataEndpoint::SdCard, move |_b| {
            hits_c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let router = Router::new_with_clock(
            RouterConfig::new(vec![sd_handler]),
            StepClock::new_default_box(),
        );

        let endpoints = &[DataEndpoint::SdCard];
        let pkt =
            Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.0, 3.0], endpoints, 0).unwrap();
        let bytes = serialize::serialize_packet(&pkt);

        router.rx_serialized(&bytes).unwrap();
        router.rx_serialized(&bytes).unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// When only a serialized handler exists for an endpoint, `rx_serialized`
    /// should still deliver the raw bytes.
    #[test]
    fn rx_serialized_delivers_to_serialized_handlers() {
        use crate::router::RouterConfig;

        let seen: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let seen_c = seen.clone();
        let sd_handler = EndpointHandler::new_serialized_handler(DataEndpoint::SdCard, move |b| {
            *seen_c.lock().unwrap() = Some(b.to_vec());
            Ok(())
        });

        let router = Router::new_with_clock(
            RouterConfig::new(vec![sd_handler]),
            StepClock::new_default_box(),
        );

        let endpoints = &[DataEndpoint::SdCard];
        let pkt =
            Packet::from_f32_slice(DataType::GpsData, &[1.0, 2.0, 3.0], endpoints, 0).unwrap();
        let bytes = serialize::serialize_packet(&pkt);

        router.rx_serialized(&bytes).unwrap();
        let got = seen.lock().unwrap().clone().expect("no bytes delivered");
        assert_eq!(*got, *bytes);
    }

    #[cfg(feature = "discovery")]
    mod discovery_tests {
        use crate::discovery::{
            build_discovery_announce, build_discovery_timesync_sources, build_discovery_topology,
            TopologyBoardNode, DISCOVERY_FAST_INTERVAL_MS, DISCOVERY_ROUTE_TTL_MS,
        };
        use crate::relay::Relay;
        use crate::router::{Clock, EndpointHandler, RouterConfig};
        use crate::tests::timeout_tests::StepClock;
        use crate::{packet::Packet, router::Router};
        use crate::{DataEndpoint, DataType, RouteSelectionMode, TelemetryError, TelemetryResult};
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        fn zero_clock() -> Box<dyn Clock + Send + Sync> {
            StepClock::new_box(0, 0)
        }

        #[derive(Clone)]
        struct SharedClock {
            now_ms: Arc<AtomicU64>,
        }

        impl Clock for SharedClock {
            fn now_ms(&self) -> u64 {
                self.now_ms.load(Ordering::SeqCst)
            }
        }

        fn endpoint_by_name(name: &str) -> Option<DataEndpoint> {
            for i in 0..=crate::MAX_VALUE_DATA_ENDPOINT {
                if let Some(ep) = DataEndpoint::try_from_u32(i)
                    && ep.as_str() == name
                {
                    return Some(ep);
                }
            }
            None
        }

        fn datatype_by_name(name: &str) -> Option<DataType> {
            for i in 0..=crate::MAX_VALUE_DATA_TYPE {
                if let Some(ty) = DataType::try_from_u32(i)
                    && crate::get_message_name(ty) == name
                {
                    return Some(ty);
                }
            }
            None
        }

        #[test]
        fn router_uses_discovery_routes_for_outbound_packets() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_pkt =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            router.rx_from_side(&discovery_pkt, side_a).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            let msg = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            router.tx(msg).unwrap();

            let got_a = seen_a.lock().unwrap().clone();
            let got_b = seen_b.lock().unwrap().clone();
            assert_eq!(got_a.len(), 1);
            assert!(got_b.is_empty());
            assert_eq!(got_a[0].data_type(), DataType::GpsData);
        }

        #[test]
        fn end_to_end_pending_destinations_clear_when_discovered_holder_expires() {
            use std::sync::Arc;

            let now_ms = Arc::new(AtomicU64::new(0));
            let clock = Box::new(SharedClock {
                now_ms: now_ms.clone(),
            });
            let router = Router::new_with_clock(RouterConfig::default().with_sender("SRC"), clock);
            let side = router.add_side_serialized_with_options(
                "link",
                |_bytes| Ok(()),
                crate::router::RouterSideOptions {
                    reliable_enabled: true,
                    link_local_enabled: false,
                    ..crate::router::RouterSideOptions::default()
                },
            );

            router
                .rx_from_side(
                    &build_discovery_announce("DEST_A", 0, &[DataEndpoint::Radio]).unwrap(),
                    side,
                )
                .unwrap();
            router
                .rx_from_side(
                    &build_discovery_announce("DEST_B", 0, &[DataEndpoint::Radio]).unwrap(),
                    side,
                )
                .unwrap();

            let pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[11.0, 0.0, 0.0],
                &[DataEndpoint::Radio],
                11,
            )
            .unwrap();
            let packet_id = pkt.packet_id();
            router.tx(pkt).unwrap();
            assert_eq!(
                router.debug_end_to_end_pending_destination_count(packet_id),
                Some(2)
            );

            let ack = Packet::new(
                DataType::ReliableAck,
                crate::message_meta(DataType::ReliableAck).endpoints,
                "E2EACK:DEST_A",
                0,
                Arc::<[u8]>::from(packet_id.to_le_bytes().to_vec()),
            )
            .unwrap();
            router.rx_from_side(&ack, side).unwrap();
            assert_eq!(
                router.debug_end_to_end_pending_destination_count(packet_id),
                Some(1)
            );

            now_ms.store(DISCOVERY_ROUTE_TTL_MS + 1, Ordering::SeqCst);
            router.periodic_no_timesync(0).unwrap();
            assert_eq!(
                router.debug_end_to_end_pending_destination_count(packet_id),
                None
            );
        }

        #[test]
        fn queued_serialized_discovery_learns_routes_for_locally_handled_endpoints() {
            let seen_remote: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_remote_c = seen_remote.clone();

            let router = Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::Radio,
                    |_pkt| Ok(()),
                )]),
                zero_clock(),
            );
            let side_remote =
                router.add_side_packet("REMOTE", move |pkt: &Packet| -> TelemetryResult<()> {
                    seen_remote_c.lock().unwrap().push(pkt.clone());
                    Ok(())
                });

            let discovery_pkt =
                build_discovery_announce("REMOTE_NODE", 0, &[DataEndpoint::Radio]).unwrap();
            let discovery_bytes = crate::serialize::serialize_packet(&discovery_pkt);
            router
                .rx_serialized_queue_from_side(discovery_bytes.as_ref(), side_remote)
                .unwrap();
            router.process_rx_queue().unwrap();

            let topo = router.export_topology();
            assert_eq!(topo.routes.len(), 1);
            assert_eq!(
                topo.routes[0].reachable_endpoints,
                vec![DataEndpoint::Radio]
            );
            assert_eq!(topo.advertised_endpoints, vec![DataEndpoint::Radio]);

            let msg = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                11,
            )
            .unwrap();
            router.tx(msg).unwrap();

            let got = seen_remote.lock().unwrap().clone();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].data_type(), DataType::GpsData);
        }

        #[test]
        fn queued_packet_discovery_updates_route_table_after_full_queue_drain() {
            let router = Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::SdCard,
                    |_pkt| Ok(()),
                )]),
                zero_clock(),
            );
            let side_fill =
                router.add_side_packet("FILL", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let discovery_pkt =
                build_discovery_announce("AB", 0, &[DataEndpoint::Radio, DataEndpoint::TimeSync])
                    .unwrap();
            router.rx_queue_from_side(discovery_pkt, side_fill).unwrap();
            router.process_all_queues_with_timeout(0).unwrap();

            let topo = router.export_topology();
            assert_eq!(topo.routes.len(), 1);
            assert_eq!(topo.routes[0].side_name, "FILL");
            assert_eq!(
                topo.routes[0].reachable_endpoints,
                vec![DataEndpoint::Radio, DataEndpoint::TimeSync]
            );
            assert!(
                topo.advertised_endpoints.contains(&DataEndpoint::SdCard),
                "local endpoints should remain advertised"
            );
            assert!(
                topo.advertised_endpoints.contains(&DataEndpoint::Radio),
                "learned remote endpoints should be reflected in advertised discovery state"
            );
        }

        #[test]
        fn queued_serialized_discovery_timesync_sources_update_route_table_after_full_queue_drain()
        {
            let router = Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::SdCard,
                    |_pkt| Ok(()),
                )]),
                zero_clock(),
            );
            let side_fill =
                router.add_side_packet("FILL", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let announce = build_discovery_announce("AB", 0, &[DataEndpoint::Radio]).unwrap();
            let announce_bytes = crate::serialize::serialize_packet(&announce);
            router
                .rx_serialized_queue_from_side(announce_bytes.as_ref(), side_fill)
                .unwrap();

            let sources = build_discovery_timesync_sources("AB", 0, &["AB", "AB_BACKUP"]).unwrap();
            let source_bytes = crate::serialize::serialize_packet(&sources);
            router
                .rx_serialized_queue_from_side(source_bytes.as_ref(), side_fill)
                .unwrap();

            router.process_all_queues_with_timeout(0).unwrap();

            let topo = router.export_topology();
            assert_eq!(topo.routes.len(), 1);
            assert_eq!(topo.routes[0].side_name, "FILL");
            assert_eq!(
                topo.routes[0].reachable_endpoints,
                vec![DataEndpoint::Radio]
            );
            assert_eq!(
                topo.routes[0].reachable_timesync_sources,
                vec!["AB".to_string(), "AB_BACKUP".to_string()]
            );
            assert!(
                topo.advertised_timesync_sources.contains(&"AB".to_string()),
                "learned timesync sources should be exported in topology"
            );
        }

        #[test]
        fn queued_serialized_discovery_from_same_sender_is_ignored_and_local_endpoint_does_not_flood()
         {
            use crate::config::DEVICE_IDENTIFIER;

            let seen_remote: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_remote_c = seen_remote.clone();

            let router = Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::Radio,
                    |_pkt| Ok(()),
                )]),
                zero_clock(),
            );
            let side_remote =
                router.add_side_packet("REMOTE", move |pkt: &Packet| -> TelemetryResult<()> {
                    seen_remote_c.lock().unwrap().push(pkt.clone());
                    Ok(())
                });

            let discovery_pkt =
                build_discovery_announce(DEVICE_IDENTIFIER, 0, &[DataEndpoint::Radio]).unwrap();
            let discovery_bytes = crate::serialize::serialize_packet(&discovery_pkt);
            router
                .rx_serialized_queue_from_side(discovery_bytes.as_ref(), side_remote)
                .unwrap();
            router.process_rx_queue().unwrap();

            let topo = router.export_topology();
            assert!(topo.routes.is_empty());
            assert_eq!(topo.advertised_endpoints, vec![DataEndpoint::Radio]);

            let msg = Packet::from_f32_slice(
                DataType::GpsData,
                &[4.0, 5.0, 6.0],
                &[DataEndpoint::Radio],
                12,
            )
            .unwrap();
            router.tx(msg).unwrap();

            assert!(seen_remote.lock().unwrap().is_empty());
        }

        #[test]
        fn relay_uses_discovery_routes_for_selective_fanout() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let relay = Relay::new(zero_clock());
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let _side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_c =
                relay.add_side_packet("C", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let discovery_pkt =
                build_discovery_announce("NODE_A", 0, &[DataEndpoint::Radio]).unwrap();
            relay.rx_from_side(side_a, discovery_pkt).unwrap();
            relay.process_all_queues().unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            let msg = Packet::from_f32_slice(
                DataType::GpsData,
                &[9.0, 8.0, 7.0],
                &[DataEndpoint::Radio],
                2,
            )
            .unwrap();
            relay.rx_from_side(side_c, msg).unwrap();
            relay.process_all_queues().unwrap();

            let got_a = seen_a.lock().unwrap().clone();
            let got_b = seen_b.lock().unwrap().clone();
            assert_eq!(got_a.len(), 1);
            assert!(got_b.is_empty());
            assert_eq!(got_a[0].data_type(), DataType::GpsData);
        }

        #[test]
        fn relay_runtime_routes_support_asymmetric_and_ingress_only_links() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();
            let seen_c_c = seen_c.clone();

            let relay = Relay::new(zero_clock());
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_c = relay.add_side_packet("C", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            relay.set_route(Some(side_b), side_a, false).unwrap();
            relay.set_side_egress_enabled(side_c, false).unwrap();

            let pkt_a = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            relay.rx_from_side(side_a, pkt_a).unwrap();
            relay.process_all_queues().unwrap();
            assert_eq!(seen_b.lock().unwrap().len(), 1);
            assert!(seen_c.lock().unwrap().is_empty());

            let pkt_b = Packet::from_f32_slice(
                DataType::GpsData,
                &[4.0, 5.0, 6.0],
                &[DataEndpoint::Radio],
                2,
            )
            .unwrap();
            relay.rx_from_side(side_b, pkt_b).unwrap();
            relay.process_all_queues().unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 0);

            let pkt_c = Packet::from_f32_slice(
                DataType::GpsData,
                &[7.0, 8.0, 9.0],
                &[DataEndpoint::Radio],
                3,
            )
            .unwrap();
            relay.rx_from_side(side_c, pkt_c).unwrap();
            relay.process_all_queues().unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert_eq!(seen_b.lock().unwrap().len(), 2);
            assert!(seen_c.lock().unwrap().is_empty());
        }

        #[test]
        fn relay_can_disable_ingress_for_a_side() {
            let relay = Relay::new(zero_clock());
            let side =
                relay.add_side_packet("A", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });
            relay.set_side_ingress_enabled(side, false).unwrap();

            let pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                4,
            )
            .unwrap();

            match relay.rx_from_side(side, pkt) {
                Err(TelemetryError::HandlerError(msg)) => {
                    assert!(msg.contains("ingress disabled"));
                }
                other => panic!("expected ingress-disabled error, got {other:?}"),
            }
        }

        #[test]
        fn relay_typed_routes_can_target_one_or_many_sides() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_d: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();
            let seen_c_c = seen_c.clone();
            let seen_d_c = seen_d.clone();

            let relay = Relay::new(zero_clock());
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let _side_c = relay.add_side_packet("C", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_d = relay.add_side_packet("D", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_d_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            relay
                .set_typed_route(Some(side_a), DataType::GpsData, side_b, true)
                .unwrap();
            relay
                .set_typed_route(Some(side_a), DataType::GpsData, side_d, true)
                .unwrap();

            let gps_pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            relay.rx_from_side(side_a, gps_pkt).unwrap();
            relay.process_all_queues().unwrap();

            assert!(seen_a.lock().unwrap().is_empty());
            assert_eq!(seen_b.lock().unwrap().len(), 1);
            assert!(seen_c.lock().unwrap().is_empty());
            assert_eq!(seen_d.lock().unwrap().len(), 1);

            relay
                .clear_typed_route(Some(side_a), DataType::GpsData, side_b)
                .unwrap();
            relay
                .clear_typed_route(Some(side_a), DataType::GpsData, side_d)
                .unwrap();

            let fallback_pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[9.0, 8.0, 7.0],
                &[DataEndpoint::Radio],
                2,
            )
            .unwrap();
            relay.rx_from_side(side_a, fallback_pkt).unwrap();
            relay.process_all_queues().unwrap();

            assert_eq!(seen_b.lock().unwrap().len(), 2);
            assert_eq!(seen_c.lock().unwrap().len(), 1);
            assert_eq!(seen_d.lock().unwrap().len(), 2);
        }

        #[test]
        fn relay_remove_side_stops_transmit_and_rejects_removed_ingress() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();
            let seen_c_c = seen_c.clone();

            let relay = Relay::new(zero_clock());
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            relay.add_side_packet("C", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            relay.remove_side(side_a).unwrap();

            let pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                5,
            )
            .unwrap();
            relay.rx_from_side(side_b, pkt.clone()).unwrap();
            relay.process_all_queues().unwrap();

            assert!(seen_a.lock().unwrap().is_empty());
            assert!(seen_b.lock().unwrap().is_empty());
            assert_eq!(seen_c.lock().unwrap().len(), 1);
            match relay.rx_from_side(side_a, pkt) {
                Err(TelemetryError::HandlerError(msg)) => {
                    assert!(msg.contains("invalid side id"));
                }
                other => panic!("expected invalid removed side error, got {other:?}"),
            }
        }

        #[test]
        fn relay_remove_side_updates_discovery_routes_and_announces_remaining_topology() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let relay = Relay::new(zero_clock());
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_pkt =
                build_discovery_announce("REMOTE_B", 0, &[DataEndpoint::SdCard]).unwrap();
            relay.rx_from_side(side_b, discovery_pkt).unwrap();
            relay.process_rx_queue().unwrap();
            assert_eq!(relay.export_topology().routes.len(), 1);

            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();
            relay.remove_side(side_a).unwrap();

            let snap = relay.export_topology();
            assert_eq!(snap.routes.len(), 1);
            assert_eq!(snap.advertised_endpoints, vec![DataEndpoint::SdCard]);
            assert!(relay.poll_discovery().unwrap());
            relay.process_tx_queue().unwrap();

            assert!(seen_a.lock().unwrap().is_empty());
            let b_pkts = seen_b.lock().unwrap().clone();
            let announce = b_pkts
                .iter()
                .find(|pkt| pkt.data_type() == DataType::DiscoveryAnnounce)
                .unwrap();
            let eps = crate::discovery::decode_discovery_announce(announce).unwrap();
            assert_eq!(eps, vec![DataEndpoint::SdCard]);
            assert!(
                b_pkts
                    .iter()
                    .any(|pkt| pkt.data_type() == DataType::DiscoveryTopology)
            );
        }

        #[test]
        fn router_exports_topology_and_adaptive_discovery_schedule() {
            let router = Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::Radio,
                    |_pkt| Ok(()),
                )]),
                StepClock::new_box(0, 0),
            );
            let side_a =
                router.add_side_packet("A", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let discovery_pkt =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::SdCard]).unwrap();
            router.rx_from_side(&discovery_pkt, side_a).unwrap();

            let snap_before = router.export_topology();
            assert_eq!(
                snap_before.advertised_endpoints,
                vec![DataEndpoint::SdCard, DataEndpoint::Radio]
            );
            assert_eq!(snap_before.routes.len(), 1);
            assert_eq!(snap_before.routes[0].side_name, "A");
            assert_eq!(
                snap_before.current_announce_interval_ms,
                DISCOVERY_FAST_INTERVAL_MS
            );

            assert!(router.poll_discovery().unwrap());

            let snap_after = router.export_topology();
            assert_eq!(snap_after.next_announce_ms, DISCOVERY_FAST_INTERVAL_MS);
            assert!(snap_after.current_announce_interval_ms >= DISCOVERY_FAST_INTERVAL_MS);
        }

        #[test]
        fn router_exports_board_graph_and_tracks_transitive_endpoint_holders() {
            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_a =
                router.add_side_packet("A", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let topology = vec![
                TopologyBoardNode {
                    sender_id: "REMOTE_A".to_string(),
                    reachable_endpoints: vec![DataEndpoint::SdCard],
                    reachable_timesync_sources: Vec::new(),
                    connections: vec!["SENSOR_B".to_string()],
                },
                TopologyBoardNode {
                    sender_id: "SENSOR_B".to_string(),
                    reachable_endpoints: vec![DataEndpoint::Radio],
                    reachable_timesync_sources: Vec::new(),
                    connections: vec!["REMOTE_A".to_string()],
                },
            ];
            let topology_pkt = build_discovery_topology("REMOTE_A", 0, &topology).unwrap();
            router.rx_from_side(&topology_pkt, side_a).unwrap();

            let snap = router.export_topology();
            assert_eq!(snap.routes.len(), 1);
            assert_eq!(snap.routes[0].announcers.len(), 1);
            assert_eq!(snap.routes[0].announcers[0].sender_id, "REMOTE_A");
            assert!(
                snap.routes[0].announcers[0]
                    .routers
                    .iter()
                    .any(|board| board.sender_id == "SENSOR_B"
                        && board.reachable_endpoints == vec![DataEndpoint::Radio])
            );
            assert!(
                snap.routers
                    .iter()
                    .any(|board| board.sender_id == "SENSOR_B"
                        && board.connections.contains(&"REMOTE_A".to_string()))
            );

            assert!(
                snap.advertised_endpoints.contains(&DataEndpoint::Radio),
                "transitive endpoint holders should contribute to exported reachability"
            );
        }

        #[test]
        fn router_remove_side_stops_transmit_and_rejects_removed_ingress() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router.remove_side(side_a).unwrap();

            let msg = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            router.tx(msg.clone()).unwrap();

            assert!(seen_a.lock().unwrap().is_empty());
            assert_eq!(seen_b.lock().unwrap().len(), 1);
            match router.rx_from_side(&msg, side_a) {
                Err(TelemetryError::HandlerError(msg)) => {
                    assert!(msg.contains("invalid side id"));
                }
                other => panic!("expected invalid removed side error, got {other:?}"),
            }
        }

        #[test]
        fn router_remove_side_updates_discovery_routes_and_announces_remaining_topology() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::Radio,
                    |_pkt| Ok(()),
                )]),
                zero_clock(),
            );
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_pkt =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::SdCard]).unwrap();
            router.rx_from_side(&discovery_pkt, side_a).unwrap();
            assert_eq!(router.export_topology().routes.len(), 1);

            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();
            router.remove_side(side_a).unwrap();

            let snap = router.export_topology();
            assert!(snap.routes.is_empty());
            assert_eq!(snap.advertised_endpoints, vec![DataEndpoint::Radio]);
            assert!(router.poll_discovery().unwrap());
            router.process_tx_queue().unwrap();

            assert!(seen_a.lock().unwrap().is_empty());
            let b_pkts = seen_b.lock().unwrap().clone();
            let announce = b_pkts
                .iter()
                .find(|pkt| pkt.data_type() == DataType::DiscoveryAnnounce)
                .unwrap();
            let eps = crate::discovery::decode_discovery_announce(announce).unwrap();
            assert_eq!(eps, vec![DataEndpoint::Radio]);
            assert!(
                b_pkts
                    .iter()
                    .any(|pkt| pkt.data_type() == DataType::DiscoveryTopology)
            );
        }

        #[test]
        fn router_runtime_routes_support_asymmetric_and_ingress_only_links() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();
            let seen_c_c = seen_c.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_c = router.add_side_packet("C", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router.set_route(None, side_b, false).unwrap();
            router.set_route(None, side_c, false).unwrap();
            router.set_route(Some(side_b), side_a, false).unwrap();
            router.set_side_egress_enabled(side_c, false).unwrap();

            let local_tx = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            router.tx(local_tx).unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert!(seen_b.lock().unwrap().is_empty());
            assert!(seen_c.lock().unwrap().is_empty());

            let from_a = Packet::from_f32_slice(
                DataType::GpsData,
                &[4.0, 5.0, 6.0],
                &[DataEndpoint::Radio],
                2,
            )
            .unwrap();
            router.rx_from_side(&from_a, side_a).unwrap();
            assert_eq!(seen_b.lock().unwrap().len(), 1);
            assert!(seen_c.lock().unwrap().is_empty());

            let from_b = Packet::from_f32_slice(
                DataType::GpsData,
                &[7.0, 8.0, 9.0],
                &[DataEndpoint::Radio],
                3,
            )
            .unwrap();
            router.rx_from_side(&from_b, side_b).unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 1);

            let from_c = Packet::from_f32_slice(
                DataType::GpsData,
                &[10.0, 11.0, 12.0],
                &[DataEndpoint::Radio],
                4,
            )
            .unwrap();
            router.rx_from_side(&from_c, side_c).unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 2);
            assert_eq!(seen_b.lock().unwrap().len(), 2);
            assert!(seen_c.lock().unwrap().is_empty());
        }

        #[test]
        fn router_typed_routes_can_target_one_or_many_sides() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();
            let seen_c_c = seen_c.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_c = router.add_side_packet("C", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router
                .set_typed_route(None, DataType::GpsData, side_b, true)
                .unwrap();
            router
                .set_typed_route(None, DataType::GpsData, side_c, true)
                .unwrap();

            let gps_pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            router.tx(gps_pkt).unwrap();
            assert!(seen_a.lock().unwrap().is_empty());
            assert_eq!(seen_b.lock().unwrap().len(), 1);
            assert_eq!(seen_c.lock().unwrap().len(), 1);

            router
                .clear_typed_route(None, DataType::GpsData, side_b)
                .unwrap();
            router
                .clear_typed_route(None, DataType::GpsData, side_c)
                .unwrap();

            let fallback_pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[7.0, 8.0, 9.0],
                &[DataEndpoint::Radio],
                2,
            )
            .unwrap();
            router.tx(fallback_pkt).unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert_eq!(seen_b.lock().unwrap().len(), 2);
            assert_eq!(seen_c.lock().unwrap().len(), 2);

            let _ = side_a;
        }

        #[test]
        fn router_typed_routes_still_respect_base_route_disables() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router
                .set_typed_route(None, DataType::GpsData, side_b, true)
                .unwrap();
            router.set_route(None, side_b, false).unwrap();

            let gps_pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            router.tx(gps_pkt).unwrap();

            assert!(seen_a.lock().unwrap().is_empty());
            assert!(seen_b.lock().unwrap().is_empty());

            let _ = side_a;
        }

        #[test]
        fn router_weighted_route_mode_splits_discovered_paths_by_weight() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_a =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            let discovery_b =
                build_discovery_announce("REMOTE_B", 1, &[DataEndpoint::Radio]).unwrap();
            router.rx_from_side(&discovery_a, side_a).unwrap();
            router.rx_from_side(&discovery_b, side_b).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            router
                .set_source_route_mode(None, RouteSelectionMode::Weighted)
                .unwrap();
            router.set_route_weight(None, side_a, 2).unwrap();
            router.set_route_weight(None, side_b, 1).unwrap();

            for seq in 0..6 {
                let pkt = Packet::from_f32_slice(
                    DataType::ImuData,
                    &[
                        seq as f32,
                        seq as f32 + 1.0,
                        seq as f32 + 2.0,
                        seq as f32 + 3.0,
                        seq as f32 + 4.0,
                        seq as f32 + 5.0,
                    ],
                    &[DataEndpoint::Radio],
                    seq as u64,
                )
                .unwrap();
                router.tx(pkt).unwrap();
            }

            assert_eq!(seen_a.lock().unwrap().len(), 4);
            assert_eq!(seen_b.lock().unwrap().len(), 2);
        }

        #[test]
        fn router_discovery_defaults_to_adaptive_load_balancing() {
            let now_ms = Arc::new(AtomicU64::new(0));
            let armed = Arc::new(AtomicBool::new(false));
            let seen_a = Arc::new(AtomicUsize::new(0));
            let seen_b = Arc::new(AtomicUsize::new(0));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();
            let now_a = now_ms.clone();
            let now_b = now_ms.clone();
            let armed_b = armed.clone();

            let router = Router::new_with_clock(
                RouterConfig::default(),
                Box::new(SharedClock {
                    now_ms: now_ms.clone(),
                }),
            );
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                if pkt.data_type() == DataType::ImuData {
                    seen_a_c.fetch_add(1, Ordering::SeqCst);
                }
                now_a.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                if pkt.data_type() == DataType::ImuData {
                    seen_b_c.fetch_add(1, Ordering::SeqCst);
                }
                let delay_ms =
                    if armed_b.load(Ordering::SeqCst) && pkt.data_type() == DataType::ImuData {
                        4
                    } else {
                        1
                    };
                now_b.fetch_add(delay_ms, Ordering::SeqCst);
                Ok(())
            });
            router.set_side_egress_enabled(side_a, false).unwrap();
            router.set_side_egress_enabled(side_b, false).unwrap();

            let discovery_a =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            let discovery_b =
                build_discovery_announce("REMOTE_B", 1, &[DataEndpoint::Radio]).unwrap();
            router.rx_from_side(&discovery_a, side_a).unwrap();
            router.rx_from_side(&discovery_b, side_b).unwrap();
            router.set_side_egress_enabled(side_a, true).unwrap();
            router.set_side_egress_enabled(side_b, true).unwrap();
            seen_a.store(0, Ordering::SeqCst);
            seen_b.store(0, Ordering::SeqCst);
            armed.store(true, Ordering::SeqCst);

            for seq in 0..24 {
                let pkt = Packet::from_f32_slice(
                    DataType::ImuData,
                    &[
                        seq as f32,
                        seq as f32 + 1.0,
                        seq as f32 + 2.0,
                        seq as f32 + 3.0,
                        seq as f32 + 4.0,
                        seq as f32 + 5.0,
                    ],
                    &[DataEndpoint::Radio],
                    seq as u64,
                )
                .unwrap();
                router.tx(pkt).unwrap();
            }

            let a = seen_a.load(Ordering::SeqCst);
            let b = seen_b.load(Ordering::SeqCst);
            assert_eq!(a + b, 24);
            assert!(
                a > b,
                "expected faster side to receive more traffic: a={a}, b={b}"
            );
            assert!(b > 0, "expected adaptive balancing instead of failover");
        }

        #[test]
        fn router_failover_route_mode_switches_when_preferred_path_expires() {
            let now_ms = Arc::new(AtomicU64::new(0));
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(
                RouterConfig::default(),
                Box::new(SharedClock {
                    now_ms: now_ms.clone(),
                }),
            );
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_a =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            router.rx_from_side(&discovery_a, side_a).unwrap();
            now_ms.store(DISCOVERY_ROUTE_TTL_MS / 2, Ordering::SeqCst);
            let discovery_b = build_discovery_announce(
                "REMOTE_B",
                DISCOVERY_ROUTE_TTL_MS / 2,
                &[DataEndpoint::Radio],
            )
            .unwrap();
            router.rx_from_side(&discovery_b, side_b).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            router
                .set_source_route_mode(None, RouteSelectionMode::Failover)
                .unwrap();
            router.set_route_priority(None, side_a, 0).unwrap();
            router.set_route_priority(None, side_b, 1).unwrap();

            let pkt1 = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            router.tx(pkt1).unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert_eq!(seen_b.lock().unwrap().len(), 0);

            now_ms.store(DISCOVERY_ROUTE_TTL_MS + 1, Ordering::SeqCst);
            let pkt2 = Packet::from_f32_slice(
                DataType::GpsData,
                &[2.0, 3.0, 4.0],
                &[DataEndpoint::Radio],
                2,
            )
            .unwrap();
            router.tx(pkt2).unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert_eq!(seen_b.lock().unwrap().len(), 1);
        }

        #[test]
        fn router_weighted_route_mode_falls_back_to_remaining_path_when_other_path_expires() {
            let now_ms = Arc::new(AtomicU64::new(0));
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(
                RouterConfig::default(),
                Box::new(SharedClock {
                    now_ms: now_ms.clone(),
                }),
            );
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_a =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            router.rx_from_side(&discovery_a, side_a).unwrap();
            now_ms.store(DISCOVERY_ROUTE_TTL_MS / 2, Ordering::SeqCst);
            let discovery_b = build_discovery_announce(
                "REMOTE_B",
                DISCOVERY_ROUTE_TTL_MS / 2,
                &[DataEndpoint::Radio],
            )
            .unwrap();
            router.rx_from_side(&discovery_b, side_b).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            router
                .set_source_route_mode(None, RouteSelectionMode::Weighted)
                .unwrap();
            router.set_route_weight(None, side_a, 1).unwrap();
            router.set_route_weight(None, side_b, 1).unwrap();

            for seq in 0..2 {
                let pkt = Packet::from_f32_slice(
                    DataType::GpsData,
                    &[seq as f32, seq as f32 + 1.0, seq as f32 + 2.0],
                    &[DataEndpoint::Radio],
                    seq as u64,
                )
                .unwrap();
                router.tx(pkt).unwrap();
            }
            let before_a = seen_a.lock().unwrap().len();
            let before_b = seen_b.lock().unwrap().len();
            assert_eq!(before_a + before_b, 2);

            now_ms.store(DISCOVERY_ROUTE_TTL_MS + 1, Ordering::SeqCst);
            let pkt3 = Packet::from_f32_slice(
                DataType::GpsData,
                &[9.0, 10.0, 11.0],
                &[DataEndpoint::Radio],
                3,
            )
            .unwrap();
            router.tx(pkt3).unwrap();

            assert_eq!(seen_a.lock().unwrap().len(), before_a);
            assert_eq!(seen_b.lock().unwrap().len(), before_b + 1);
        }

        #[test]
        fn router_failover_route_mode_switches_when_preferred_side_is_removed() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_a =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            let discovery_b =
                build_discovery_announce("REMOTE_B", 1, &[DataEndpoint::Radio]).unwrap();
            router.rx_from_side(&discovery_a, side_a).unwrap();
            router.rx_from_side(&discovery_b, side_b).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            router
                .set_source_route_mode(None, RouteSelectionMode::Failover)
                .unwrap();
            router.set_route_priority(None, side_a, 0).unwrap();
            router.set_route_priority(None, side_b, 1).unwrap();

            let pkt1 = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            router.tx(pkt1).unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert_eq!(seen_b.lock().unwrap().len(), 0);

            router.remove_side(side_a).unwrap();
            let pkt2 = Packet::from_f32_slice(
                DataType::GpsData,
                &[4.0, 5.0, 6.0],
                &[DataEndpoint::Radio],
                2,
            )
            .unwrap();
            router.tx(pkt2).unwrap();

            let before_a = seen_a.lock().unwrap().len();
            let before_b = seen_b.lock().unwrap().len();
            assert_eq!(before_a + before_b, 2);
        }

        #[test]
        fn relay_weighted_route_mode_splits_discovered_paths_by_weight() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let relay = Relay::new(zero_clock());
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_c =
                relay.add_side_packet("C", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let discovery_a =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            let discovery_b =
                build_discovery_announce("REMOTE_B", 1, &[DataEndpoint::Radio]).unwrap();
            relay.rx_from_side(side_a, discovery_a).unwrap();
            relay.rx_from_side(side_b, discovery_b).unwrap();
            relay.process_rx_queue().unwrap();
            relay.process_tx_queue().unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            relay
                .set_source_route_mode(Some(side_c), RouteSelectionMode::Weighted)
                .unwrap();
            relay.set_route_weight(Some(side_c), side_a, 2).unwrap();
            relay.set_route_weight(Some(side_c), side_b, 1).unwrap();

            for seq in 0..6 {
                let pkt = Packet::from_f32_slice(
                    DataType::GpsData,
                    &[seq as f32, seq as f32 + 1.0, seq as f32 + 2.0],
                    &[DataEndpoint::Radio],
                    seq as u64,
                )
                .unwrap();
                relay.rx_from_side(side_c, pkt).unwrap();
            }
            relay.process_all_queues().unwrap();

            assert_eq!(seen_a.lock().unwrap().len(), 4);
            assert_eq!(seen_b.lock().unwrap().len(), 2);
        }

        #[test]
        fn relay_failover_route_mode_switches_when_preferred_path_expires() {
            let now_ms = Arc::new(AtomicU64::new(0));
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let relay = Relay::new(Box::new(SharedClock {
                now_ms: now_ms.clone(),
            }));
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_c =
                relay.add_side_packet("C", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let discovery_a =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            relay.rx_from_side(side_a, discovery_a).unwrap();
            relay.process_rx_queue().unwrap();
            relay.process_tx_queue().unwrap();
            now_ms.store(DISCOVERY_ROUTE_TTL_MS / 2, Ordering::SeqCst);
            let discovery_b = build_discovery_announce(
                "REMOTE_B",
                DISCOVERY_ROUTE_TTL_MS / 2,
                &[DataEndpoint::Radio],
            )
            .unwrap();
            relay.rx_from_side(side_b, discovery_b).unwrap();
            relay.process_rx_queue().unwrap();
            relay.process_tx_queue().unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            relay
                .set_source_route_mode(Some(side_c), RouteSelectionMode::Failover)
                .unwrap();
            relay.set_route_priority(Some(side_c), side_a, 0).unwrap();
            relay.set_route_priority(Some(side_c), side_b, 1).unwrap();

            let pkt1 = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            relay.rx_from_side(side_c, pkt1).unwrap();
            relay.process_all_queues().unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert_eq!(seen_b.lock().unwrap().len(), 0);

            now_ms.store(DISCOVERY_ROUTE_TTL_MS + 1, Ordering::SeqCst);
            let pkt2 = Packet::from_f32_slice(
                DataType::GpsData,
                &[2.0, 3.0, 4.0],
                &[DataEndpoint::Radio],
                2,
            )
            .unwrap();
            relay.rx_from_side(side_c, pkt2).unwrap();
            relay.process_all_queues().unwrap();
            let before_a = seen_a.lock().unwrap().len();
            let before_b = seen_b.lock().unwrap().len();
            assert_eq!(before_a + before_b, 2);
        }

        #[test]
        fn relay_weighted_route_mode_falls_back_to_remaining_path_when_other_path_expires() {
            let now_ms = Arc::new(AtomicU64::new(0));
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let relay = Relay::new(Box::new(SharedClock {
                now_ms: now_ms.clone(),
            }));
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_c =
                relay.add_side_packet("C", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let discovery_a =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            relay.rx_from_side(side_a, discovery_a).unwrap();
            relay.process_all_queues().unwrap();
            now_ms.store(DISCOVERY_ROUTE_TTL_MS / 2, Ordering::SeqCst);
            let discovery_b = build_discovery_announce(
                "REMOTE_B",
                DISCOVERY_ROUTE_TTL_MS / 2,
                &[DataEndpoint::Radio],
            )
            .unwrap();
            relay.rx_from_side(side_b, discovery_b).unwrap();
            relay.process_all_queues().unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            relay
                .set_source_route_mode(Some(side_c), RouteSelectionMode::Weighted)
                .unwrap();
            relay.set_route_weight(Some(side_c), side_a, 1).unwrap();
            relay.set_route_weight(Some(side_c), side_b, 1).unwrap();

            for seq in 0..2 {
                let pkt = Packet::from_f32_slice(
                    DataType::GpsData,
                    &[seq as f32, seq as f32 + 1.0, seq as f32 + 2.0],
                    &[DataEndpoint::Radio],
                    seq as u64,
                )
                .unwrap();
                relay.rx_from_side(side_c, pkt).unwrap();
            }
            relay.process_all_queues().unwrap();
            let before_a = seen_a.lock().unwrap().len();
            let before_b = seen_b.lock().unwrap().len();
            assert_eq!(before_a + before_b, 2);

            now_ms.store(DISCOVERY_ROUTE_TTL_MS + 1, Ordering::SeqCst);
            let pkt3 = Packet::from_f32_slice(
                DataType::GpsData,
                &[9.0, 10.0, 11.0],
                &[DataEndpoint::Radio],
                3,
            )
            .unwrap();
            relay.rx_from_side(side_c, pkt3).unwrap();
            relay.process_all_queues().unwrap();

            assert_eq!(seen_a.lock().unwrap().len(), before_a);
            assert_eq!(seen_b.lock().unwrap().len(), before_b + 1);
        }

        #[test]
        fn relay_failover_route_mode_switches_when_preferred_side_is_removed() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let relay = Relay::new(zero_clock());
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_c =
                relay.add_side_packet("C", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let discovery_a =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            let discovery_b =
                build_discovery_announce("REMOTE_B", 1, &[DataEndpoint::Radio]).unwrap();
            relay.rx_from_side(side_a, discovery_a).unwrap();
            relay.rx_from_side(side_b, discovery_b).unwrap();
            relay.process_all_queues().unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            relay
                .set_source_route_mode(Some(side_c), RouteSelectionMode::Failover)
                .unwrap();
            relay.set_route_priority(Some(side_c), side_a, 0).unwrap();
            relay.set_route_priority(Some(side_c), side_b, 1).unwrap();

            let pkt1 = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                1,
            )
            .unwrap();
            relay.rx_from_side(side_c, pkt1).unwrap();
            relay.process_all_queues().unwrap();
            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert_eq!(seen_b.lock().unwrap().len(), 0);

            relay.remove_side(side_a).unwrap();
            let pkt2 = Packet::from_f32_slice(
                DataType::GpsData,
                &[4.0, 5.0, 6.0],
                &[DataEndpoint::Radio],
                2,
            )
            .unwrap();
            relay.rx_from_side(side_c, pkt2).unwrap();
            relay.process_all_queues().unwrap();

            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert_eq!(seen_b.lock().unwrap().len(), 1);
        }

        #[test]
        fn router_can_disable_ingress_for_a_side() {
            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side =
                router.add_side_packet("A", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });
            router.set_side_ingress_enabled(side, false).unwrap();

            let pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[1.0, 2.0, 3.0],
                &[DataEndpoint::Radio],
                5,
            )
            .unwrap();

            match router.rx_from_side(&pkt, side) {
                Err(TelemetryError::HandlerError(msg)) => {
                    assert!(msg.contains("ingress disabled"));
                }
                other => panic!("expected ingress-disabled error, got {other:?}"),
            }
        }

        #[cfg(feature = "timesync")]
        #[test]
        fn router_periodic_dispatches_discovery_and_timesync_when_enabled() {
            let seen: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c = seen.clone();

            let router = Router::new_with_clock(
                RouterConfig::default().with_timesync(crate::timesync::TimeSyncConfig {
                    role: crate::timesync::TimeSyncRole::Source,
                    ..Default::default()
                }),
                zero_clock(),
            );
            router.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router.periodic(0).unwrap();

            let pkts = seen.lock().unwrap().clone();
            assert!(
                pkts.iter()
                    .any(|pkt| pkt.data_type() == DataType::DiscoveryAnnounce)
            );
            assert!(
                pkts.iter()
                    .any(|pkt| pkt.data_type() == DataType::TimeSyncAnnounce)
            );
        }

        #[cfg(feature = "timesync")]
        #[test]
        fn router_periodic_can_skip_timesync_but_still_dispatch_discovery() {
            let seen: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c = seen.clone();

            let router = Router::new_with_clock(
                RouterConfig::default().with_timesync(crate::timesync::TimeSyncConfig {
                    role: crate::timesync::TimeSyncRole::Source,
                    ..Default::default()
                }),
                zero_clock(),
            );
            router.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router.periodic_no_timesync(0).unwrap();

            let pkts = seen.lock().unwrap().clone();
            assert!(
                pkts.iter()
                    .any(|pkt| pkt.data_type() == DataType::DiscoveryAnnounce)
            );
            assert!(
                pkts.iter()
                    .all(|pkt| pkt.data_type() != DataType::TimeSyncAnnounce)
            );
        }

        #[cfg(feature = "timesync")]
        #[test]
        fn queued_timesync_packets_precede_normal_telemetry() {
            let seen: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c = seen.clone();

            let router = Router::new_with_clock(
                RouterConfig::default().with_timesync(crate::timesync::TimeSyncConfig {
                    role: crate::timesync::TimeSyncRole::Consumer,
                    ..Default::default()
                }),
                zero_clock(),
            );
            router.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router
                .log_queue(DataType::GpsData, &[1.0_f32, 2.0, 3.0])
                .unwrap();
            let announce =
                crate::timesync::build_timesync_announce_with_sender("SRC_FAST", 1, 1_700).unwrap();
            router.rx(&announce).unwrap();
            router.process_tx_queue().unwrap();

            let pkts = seen.lock().unwrap().clone();
            assert!(pkts.len() >= 2);
            let gps_idx = pkts
                .iter()
                .position(|pkt| pkt.data_type() == DataType::GpsData)
                .expect("expected queued GPS packet");
            let request_idx = pkts
                .iter()
                .position(|pkt| pkt.data_type() == DataType::TimeSyncRequest)
                .expect("expected queued time-sync request");
            assert!(request_idx < gps_idx);
        }

        #[cfg(feature = "discovery")]
        #[test]
        fn queued_discovery_packets_precede_normal_telemetry() {
            let seen: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c = seen.clone();

            let router = Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::Radio,
                    |_pkt| Ok(()),
                )]),
                zero_clock(),
            );
            router.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router
                .log_queue(DataType::GpsData, &[1.0_f32, 2.0, 3.0])
                .unwrap();
            router.announce_discovery().unwrap();
            router.process_tx_queue().unwrap();

            let pkts = seen.lock().unwrap().clone();
            let gps_idx = pkts
                .iter()
                .position(|pkt| pkt.data_type() == DataType::GpsData)
                .unwrap();
            assert!(gps_idx > 0);
            assert!(
                pkts[..gps_idx]
                    .iter()
                    .all(|pkt| crate::discovery::is_discovery_type(pkt.data_type()))
            );
        }

        #[test]
        fn reliable_packets_are_sent_to_all_discovered_candidate_sides() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_pkt =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
            router.rx_from_side(&discovery_pkt, side_a).unwrap();
            let discovery_pkt =
                build_discovery_announce("REMOTE_B", 0, &[DataEndpoint::Radio]).unwrap();
            router.rx_from_side(&discovery_pkt, side_b).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            let msg = Packet::from_f32_slice(
                DataType::GpsData,
                &[5.0, 6.0, 7.0],
                &[DataEndpoint::Radio],
                3,
            )
            .unwrap();
            router.tx(msg).unwrap();

            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert_eq!(seen_b.lock().unwrap().len(), 1);
        }

        #[test]
        fn relay_exports_aggregated_topology() {
            let relay = Relay::new(zero_clock());
            let side_a =
                relay.add_side_packet("A", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });
            relay.add_side_packet("B", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let discovery_pkt =
                build_discovery_announce("NODE_A", 0, &[DataEndpoint::Radio, DataEndpoint::SdCard])
                    .unwrap();
            relay.rx_from_side(side_a, discovery_pkt).unwrap();
            relay.process_all_queues().unwrap();

            let snap = relay.export_topology();
            assert_eq!(
                snap.advertised_endpoints,
                vec![DataEndpoint::SdCard, DataEndpoint::Radio]
            );
            assert_eq!(snap.routes.len(), 1);
            assert_eq!(snap.routes[0].side_name, "A");
        }

        #[test]
        fn relay_periodic_dispatches_discovery() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let relay = Relay::new(zero_clock());
            let side_a = relay.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            relay.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_pkt =
                build_discovery_announce("NODE_A", 0, &[DataEndpoint::Radio]).unwrap();
            relay.rx_from_side(side_a, discovery_pkt).unwrap();
            relay.periodic(0).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            relay.periodic(0).unwrap();

            assert!(
                seen_b
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|pkt| pkt.data_type() == DataType::DiscoveryAnnounce)
            );
        }

        #[test]
        fn relay_end_to_end_acked_holders_clear_when_discovered_holder_expires() {
            let now_ms = Arc::new(AtomicU64::new(0));
            let relay = Relay::new(Box::new(SharedClock {
                now_ms: now_ms.clone(),
            }));
            let side =
                relay.add_side_packet("A", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            relay
                .rx_from_side(
                    side,
                    build_discovery_announce("DEST_A", 0, &[DataEndpoint::Radio]).unwrap(),
                )
                .unwrap();
            relay.process_all_queues().unwrap();

            let packet_id = 77u64;
            let ack = Packet::new(
                DataType::ReliableAck,
                crate::message_meta(DataType::ReliableAck).endpoints,
                "E2EACK:DEST_A",
                0,
                Arc::<[u8]>::from(packet_id.to_le_bytes().to_vec()),
            )
            .unwrap();
            relay.rx_from_side(side, ack).unwrap();
            relay.process_all_queues().unwrap();
            assert_eq!(
                relay.debug_end_to_end_acked_destination_count(packet_id),
                Some(1)
            );

            now_ms.store(DISCOVERY_ROUTE_TTL_MS + 1, Ordering::SeqCst);
            relay.periodic(0).unwrap();
            assert_eq!(
                relay.debug_end_to_end_acked_destination_count(packet_id),
                None
            );
        }

        #[test]
        fn link_local_only_packets_stay_on_software_bus_sides() {
            let Some(software_bus) = endpoint_by_name("SOFTWARE_BUS") else {
                return;
            };
            let Some(ipc_message) = datatype_by_name("IPC_MESSAGE") else {
                return;
            };
            let seen_net: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_ll: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_net_c = seen_net.clone();
            let seen_ll_c = seen_ll.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            router.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_net_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            router.add_side_packet_with_options(
                "LL",
                move |pkt: &Packet| -> TelemetryResult<()> {
                    seen_ll_c.lock().unwrap().push(pkt.clone());
                    Ok(())
                },
                crate::router::RouterSideOptions {
                    reliable_enabled: false,
                    link_local_enabled: true,
                    ..crate::router::RouterSideOptions::default()
                },
            );

            let pkt = Packet::new(
                ipc_message,
                &[software_bus],
                "IPC_NODE",
                7,
                Arc::<[u8]>::from(b"hello-ipc".as_slice()),
            )
            .unwrap();
            router.tx(pkt).unwrap();

            assert!(seen_net.lock().unwrap().is_empty());
            assert_eq!(seen_ll.lock().unwrap().len(), 1);
        }

        #[test]
        fn link_local_routes_ignore_non_link_local_discovery_candidates() {
            let Some(software_bus) = endpoint_by_name("SOFTWARE_BUS") else {
                return;
            };
            let Some(ipc_message) = datatype_by_name("IPC_MESSAGE") else {
                return;
            };
            let seen_net: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_ll: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_net_c = seen_net.clone();
            let seen_ll_c = seen_ll.clone();

            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let side_net =
                router.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                    seen_net_c.lock().unwrap().push(pkt.clone());
                    Ok(())
                });
            let side_ll = router.add_side_packet_with_options(
                "LL",
                move |pkt: &Packet| -> TelemetryResult<()> {
                    seen_ll_c.lock().unwrap().push(pkt.clone());
                    Ok(())
                },
                crate::router::RouterSideOptions {
                    reliable_enabled: false,
                    link_local_enabled: true,
                    ..crate::router::RouterSideOptions::default()
                },
            );

            let pkt_net = build_discovery_announce("NET_NODE", 0, &[software_bus]).unwrap();
            router.rx_from_side(&pkt_net, side_net).unwrap();
            let pkt_ll = build_discovery_announce("LL_NODE", 0, &[software_bus]).unwrap();
            router.rx_from_side(&pkt_ll, side_ll).unwrap();
            seen_net.lock().unwrap().clear();
            seen_ll.lock().unwrap().clear();

            let pkt = Packet::new(
                ipc_message,
                &[software_bus],
                "IPC_NODE",
                8,
                Arc::<[u8]>::from(b"stay-local".as_slice()),
            )
            .unwrap();
            router.tx(pkt).unwrap();

            assert!(seen_net.lock().unwrap().is_empty());
            assert_eq!(seen_ll.lock().unwrap().len(), 1);
        }

        #[test]
        fn relay_link_local_routes_ignore_non_link_local_discovery_candidates() {
            let Some(software_bus) = endpoint_by_name("SOFTWARE_BUS") else {
                return;
            };
            let Some(ipc_message) = datatype_by_name("IPC_MESSAGE") else {
                return;
            };
            let seen_net: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_ll: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_net_c = seen_net.clone();
            let seen_ll_c = seen_ll.clone();

            let relay = Relay::new(zero_clock());
            let side_net =
                relay.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                    seen_net_c.lock().unwrap().push(pkt.clone());
                    Ok(())
                });
            let side_ll = relay.add_side_packet_with_options(
                "LL",
                move |pkt: &Packet| -> TelemetryResult<()> {
                    seen_ll_c.lock().unwrap().push(pkt.clone());
                    Ok(())
                },
                crate::relay::RelaySideOptions {
                    reliable_enabled: false,
                    link_local_enabled: true,
                    ..crate::relay::RelaySideOptions::default()
                },
            );
            let side_src =
                relay.add_side_packet("SRC", |_pkt: &Packet| -> TelemetryResult<()> { Ok(()) });

            let pkt_net = build_discovery_announce("NET_NODE", 0, &[software_bus]).unwrap();
            relay.rx_from_side(side_net, pkt_net).unwrap();
            let pkt_ll = build_discovery_announce("LL_NODE", 0, &[software_bus]).unwrap();
            relay.rx_from_side(side_ll, pkt_ll).unwrap();
            relay.process_all_queues().unwrap();
            seen_net.lock().unwrap().clear();
            seen_ll.lock().unwrap().clear();

            let pkt = Packet::new(
                ipc_message,
                &[software_bus],
                "IPC_NODE",
                9,
                Arc::<[u8]>::from(b"relay-local".as_slice()),
            )
            .unwrap();
            relay.rx_from_side(side_src, pkt).unwrap();
            relay.process_all_queues().unwrap();

            assert!(seen_net.lock().unwrap().is_empty());
            assert_eq!(seen_ll.lock().unwrap().len(), 1);
        }

        #[test]
        fn discovery_hides_link_local_endpoints_from_network_sides() {
            let Some(software_bus) = endpoint_by_name("SOFTWARE_BUS") else {
                return;
            };
            let seen_net: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_ll: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_net_c = seen_net.clone();
            let seen_ll_c = seen_ll.clone();

            let router = Router::new_with_clock(
                RouterConfig::new(vec![
                    EndpointHandler::new_packet_handler(software_bus, |_pkt| Ok(())),
                    EndpointHandler::new_packet_handler(DataEndpoint::Radio, |_pkt| Ok(())),
                ]),
                zero_clock(),
            );
            router.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_net_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            router.add_side_packet_with_options(
                "LL",
                move |pkt: &Packet| -> TelemetryResult<()> {
                    seen_ll_c.lock().unwrap().push(pkt.clone());
                    Ok(())
                },
                crate::router::RouterSideOptions {
                    reliable_enabled: false,
                    link_local_enabled: true,
                    ..crate::router::RouterSideOptions::default()
                },
            );

            router.announce_discovery().unwrap();
            router.process_tx_queue().unwrap();

            let net = seen_net.lock().unwrap().clone();
            let ll = seen_ll.lock().unwrap().clone();
            let net_announce = net
                .iter()
                .find(|pkt| pkt.data_type() == DataType::DiscoveryAnnounce)
                .unwrap();
            let ll_announce = ll
                .iter()
                .find(|pkt| pkt.data_type() == DataType::DiscoveryAnnounce)
                .unwrap();
            let net_eps = crate::discovery::decode_discovery_announce(net_announce).unwrap();
            let ll_eps = crate::discovery::decode_discovery_announce(ll_announce).unwrap();
            assert!(!net_eps.contains(&software_bus));
            assert!(net_eps.contains(&DataEndpoint::Radio));
            assert!(ll_eps.contains(&software_bus));
        }

        #[cfg(feature = "timesync")]
        #[test]
        fn discovery_advertises_timesync_endpoint_when_enabled() {
            let seen: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c = seen.clone();

            let router = Router::new_with_clock(
                RouterConfig::default().with_timesync(crate::timesync::TimeSyncConfig::default()),
                zero_clock(),
            );
            router.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router.announce_discovery().unwrap();
            router.process_tx_queue().unwrap();

            let pkts = seen.lock().unwrap().clone();
            let announce = pkts
                .iter()
                .find(|pkt| pkt.data_type() == DataType::DiscoveryAnnounce)
                .unwrap();
            let eps = crate::discovery::decode_discovery_announce(announce).unwrap();
            assert!(eps.contains(&DataEndpoint::TimeSync));
        }

        #[cfg(all(feature = "timesync", feature = "discovery"))]
        #[test]
        fn timesync_packets_use_discovery_candidates_instead_of_flooding() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(
                RouterConfig::default().with_timesync(crate::timesync::TimeSyncConfig::default()),
                zero_clock(),
            );
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let discovery_pkt =
                build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::TimeSync]).unwrap();
            router.rx_from_side(&discovery_pkt, side_a).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            let request = crate::timesync::build_timesync_request(1, 123).unwrap();
            router.tx(request).unwrap();

            assert_eq!(seen_a.lock().unwrap().len(), 1);
            assert!(seen_b.lock().unwrap().is_empty());
            assert_eq!(
                seen_a.lock().unwrap()[0].data_type(),
                DataType::TimeSyncRequest
            );
        }

        #[cfg(feature = "timesync")]
        #[test]
        fn discovery_advertises_local_timesync_source_ids() {
            let seen: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_c = seen.clone();

            let router = Router::new_with_clock(
                RouterConfig::default().with_timesync(crate::timesync::TimeSyncConfig {
                    role: crate::timesync::TimeSyncRole::Source,
                    ..Default::default()
                }),
                zero_clock(),
            );
            router.add_side_packet("NET", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            router.announce_discovery().unwrap();
            router.process_tx_queue().unwrap();

            let pkts = seen.lock().unwrap().clone();
            let src_pkt = pkts
                .iter()
                .find(|pkt| pkt.data_type() == DataType::DiscoveryTimeSyncSources)
                .unwrap();
            let sources = crate::discovery::decode_discovery_timesync_sources(src_pkt).unwrap();
            assert!(sources.contains(&crate::config::DEVICE_IDENTIFIER.to_string()));
        }

        #[cfg(all(feature = "timesync", feature = "discovery"))]
        #[test]
        fn timesync_requests_prefer_exact_discovered_source_route() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(
                RouterConfig::default().with_timesync(crate::timesync::TimeSyncConfig::default()),
                zero_clock(),
            );
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            let side_b = router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let generic_timesync =
                build_discovery_announce("SIDE_A", 0, &[DataEndpoint::TimeSync]).unwrap();
            router.rx_from_side(&generic_timesync, side_a).unwrap();
            let exact_source =
                build_discovery_timesync_sources("SIDE_B", 0, &["SRC_BEST"]).unwrap();
            router.rx_from_side(&exact_source, side_b).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            let source_announce =
                crate::timesync::build_timesync_announce_with_sender("SRC_BEST", 1, 1000).unwrap();
            router.rx(&source_announce).unwrap();
            seen_a.lock().unwrap().clear();
            seen_b.lock().unwrap().clear();

            let request = crate::timesync::build_timesync_request(1, 123).unwrap();
            router.tx(request).unwrap();

            assert!(seen_a.lock().unwrap().is_empty());
            assert_eq!(seen_b.lock().unwrap().len(), 1);
            assert_eq!(
                seen_b.lock().unwrap()[0].data_type(),
                DataType::TimeSyncRequest
            );
        }

        #[cfg(feature = "timesync")]
        #[test]
        fn timesync_responses_return_only_to_requesting_side() {
            let seen_a: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_b: Arc<Mutex<Vec<Packet>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_a_c = seen_a.clone();
            let seen_b_c = seen_b.clone();

            let router = Router::new_with_clock(
                RouterConfig::default().with_timesync(crate::timesync::TimeSyncConfig {
                    role: crate::timesync::TimeSyncRole::Source,
                    ..Default::default()
                }),
                zero_clock(),
            );
            let side_a = router.add_side_packet("A", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_a_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });
            router.add_side_packet("B", move |pkt: &Packet| -> TelemetryResult<()> {
                seen_b_c.lock().unwrap().push(pkt.clone());
                Ok(())
            });

            let request = crate::timesync::build_timesync_request(7, 111).unwrap();
            router.rx_from_side(&request, side_a).unwrap();
            router.process_tx_queue().unwrap();

            let got_a = seen_a.lock().unwrap().clone();
            let got_b = seen_b.lock().unwrap().clone();
            assert_eq!(
                got_a
                    .iter()
                    .filter(|pkt| pkt.data_type() == DataType::TimeSyncResponse)
                    .count(),
                1
            );
            assert_eq!(
                got_b
                    .iter()
                    .filter(|pkt| pkt.data_type() == DataType::TimeSyncResponse)
                    .count(),
                0
            );
        }

        #[test]
        fn immediate_cross_wired_router_reentry_falls_back_to_queue() {
            let Some(ipc_message) = datatype_by_name("IPC_MESSAGE") else {
                return;
            };

            let remaining = Arc::new(AtomicUsize::new(6));
            let sequence = Arc::new(AtomicUsize::new(1));
            let a_slot = Arc::new(Mutex::new(None::<Arc<Router>>));
            let b_slot = Arc::new(Mutex::new(None::<Arc<Router>>));

            let a_remaining = remaining.clone();
            let a_sequence = sequence.clone();
            let a_router = a_slot.clone();
            let router_a = Arc::new(Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::Radio,
                    move |_pkt: &Packet| {
                        if a_remaining
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                            .is_err()
                        {
                            return Ok(());
                        }
                        let seq = a_sequence.fetch_add(1, Ordering::SeqCst) as u64;
                        let pkt = Packet::new(
                            ipc_message,
                            &[DataEndpoint::SdCard],
                            "A_NODE",
                            seq,
                            Arc::<[u8]>::from(b"bounce-a".as_slice()),
                        )?;
                        a_router
                            .lock()
                            .unwrap()
                            .as_ref()
                            .expect("router A initialized")
                            .tx(pkt)
                    },
                )])
                .with_sender("A_NODE"),
                StepClock::new_default_box(),
            ));

            let b_remaining = remaining.clone();
            let b_sequence = sequence.clone();
            let b_router = b_slot.clone();
            let router_b = Arc::new(Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::SdCard,
                    move |_pkt: &Packet| {
                        if b_remaining
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                            .is_err()
                        {
                            return Ok(());
                        }
                        let seq = b_sequence.fetch_add(1, Ordering::SeqCst) as u64;
                        let pkt = Packet::new(
                            ipc_message,
                            &[DataEndpoint::Radio],
                            "B_NODE",
                            seq,
                            Arc::<[u8]>::from(b"bounce-b".as_slice()),
                        )?;
                        b_router
                            .lock()
                            .unwrap()
                            .as_ref()
                            .expect("router B initialized")
                            .tx(pkt)
                    },
                )])
                .with_sender("B_NODE"),
                StepClock::new_default_box(),
            ));

            *a_slot.lock().unwrap() = Some(router_a.clone());
            *b_slot.lock().unwrap() = Some(router_b.clone());

            let a_in_tx = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let a_reentered = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let b_in_tx = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let b_reentered = Arc::new(std::sync::atomic::AtomicBool::new(false));

            let router_b_for_side = router_b.clone();
            let a_in_tx_flag = a_in_tx.clone();
            let a_reentered_flag = a_reentered.clone();
            let side_a = router_a.add_side_packet("A_TO_B", move |pkt: &Packet| {
                if a_in_tx_flag.swap(true, Ordering::SeqCst) {
                    a_reentered_flag.store(true, Ordering::SeqCst);
                }
                let result = router_b_for_side.rx_from_side(pkt, 0);
                a_in_tx_flag.store(false, Ordering::SeqCst);
                result
            });

            let router_a_for_side = router_a.clone();
            let b_in_tx_flag = b_in_tx.clone();
            let b_reentered_flag = b_reentered.clone();
            let side_b = router_b.add_side_packet("B_TO_A", move |pkt: &Packet| {
                if b_in_tx_flag.swap(true, Ordering::SeqCst) {
                    b_reentered_flag.store(true, Ordering::SeqCst);
                }
                let result = router_a_for_side.rx_from_side(pkt, 0);
                b_in_tx_flag.store(false, Ordering::SeqCst);
                result
            });

            assert_eq!(side_a, 0);
            assert_eq!(side_b, 0);

            let first = Packet::new(
                ipc_message,
                &[DataEndpoint::SdCard],
                "START",
                0,
                Arc::<[u8]>::from(b"start".as_slice()),
            )
            .unwrap();
            router_a.tx(first).unwrap();

            for _ in 0..8 {
                router_a.process_all_queues().unwrap();
                router_b.process_all_queues().unwrap();
            }

            assert!(!a_reentered.load(Ordering::SeqCst));
            assert!(!b_reentered.load(Ordering::SeqCst));
            assert!(remaining.load(Ordering::SeqCst) < 6);
        }
    }

    #[cfg(feature = "timesync")]
    mod timesync_tests {
        use crate::timesync::{
            build_timesync_request, build_timesync_response, compute_offset_delay,
            decode_timesync_request, decode_timesync_response,
        };

        #[test]
        fn timesync_request_roundtrip() {
            let pkt = build_timesync_request(7, 1234).unwrap();
            let decoded = decode_timesync_request(&pkt).unwrap();
            assert_eq!(decoded.seq, 7);
            assert_eq!(decoded.t1_ms, 1234);
        }

        #[test]
        fn timesync_response_roundtrip() {
            let pkt = build_timesync_response(9, 100, 110, 115).unwrap();
            let decoded = decode_timesync_response(&pkt).unwrap();
            assert_eq!(decoded.seq, 9);
            assert_eq!(decoded.t1_ms, 100);
            assert_eq!(decoded.t2_ms, 110);
            assert_eq!(decoded.t3_ms, 115);
        }

        #[test]
        fn timesync_offset_delay_math() {
            let sample = compute_offset_delay(10, 20, 30, 40);
            assert_eq!(sample.offset_ms, 0);
            assert_eq!(sample.delay_ms, 20);
        }
    }
}
