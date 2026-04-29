use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use sedsprintf_rs::{MessageClass, MessageDataType, MessageElement, ReliableMode};
use sedsprintf_rs::TelemetryResult;
use sedsprintf_rs::config::{
    data_type_definition_by_name, endpoint_definition_by_name, register_data_type_with_description,
    register_endpoint_with_description, DataEndpoint, DataType,
};
use sedsprintf_rs::packet::Packet;
use sedsprintf_rs::relay::Relay;
use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};
use sedsprintf_rs::serialize::peek_frame_info;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::Once;

fn zero_clock() -> Box<dyn Clock + Send + Sync> {
    Box::new(|| 0u64)
}

fn ensure_common_bench_schema() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if endpoint_definition_by_name("RADIO").is_none() {
            register_endpoint_with_description(
                "RADIO",
                "Radio or external link (telemetry uplink/downlink).",
                false,
            )
            .unwrap();
        }
        if endpoint_definition_by_name("SD_CARD").is_none() {
            register_endpoint_with_description(
                "SD_CARD",
                "On-board storage (e.g. SD card / flash).",
                false,
            )
            .unwrap();
        }
        if data_type_definition_by_name("GPS_DATA").is_none() {
            register_data_type_with_description(
                "GPS_DATA",
                "GPS data (typically 3x f32: latitude, longitude, altitude).",
                MessageElement::Static(3, MessageDataType::Float32, MessageClass::Data),
                &[DataEndpoint::named("RADIO"), DataEndpoint::named("SD_CARD")],
                ReliableMode::Ordered,
                80,
            )
            .unwrap();
        }
    });
}

fn endpoints() -> [DataEndpoint; 2] {
    [DataEndpoint::named("SD_CARD"), DataEndpoint::named("RADIO")]
}

fn next_gps_packet(counter: &AtomicU64) -> Packet {
    let ts = counter.fetch_add(1, Ordering::Relaxed);
    let vals = [ts as f32, ts as f32 + 0.25, ts as f32 + 0.5];
    Packet::from_f32_slice(DataType::named("GPS_DATA"), &vals, &endpoints(), ts).unwrap()
}

fn count_frames_of_type(frames: &[Vec<u8>], ty: DataType) -> usize {
    frames
        .iter()
        .filter(|frame| {
            peek_frame_info(frame.as_slice())
                .map(|info| info.envelope.ty == ty && !info.ack_only())
                .unwrap_or(false)
        })
        .count()
}

fn benchmark_router_system_paths(c: &mut Criterion) {
    ensure_common_bench_schema();
    let mut group = c.benchmark_group("router_system_paths");

    let delivered = Arc::new(AtomicUsize::new(0));
    let tx_frames = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));

    let sink = {
        let delivered = delivered.clone();
        let handlers = vec![
            EndpointHandler::new_packet_handler(
                DataEndpoint::named("RADIO"),
                move |_pkt: &Packet| {
                    delivered.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                },
            ),
            EndpointHandler::new_packet_handler(DataEndpoint::named("SD_CARD"), |_pkt: &Packet| {
                Ok(())
            }),
        ];
        Router::new_with_clock(RouterConfig::new(handlers), zero_clock())
    };

    let source = {
        let tx_frames = tx_frames.clone();
        let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
        router.add_side_serialized("bench_bus", move |bytes: &[u8]| -> TelemetryResult<()> {
            tx_frames.lock().unwrap().push(bytes.to_vec());
            Ok(())
        });
        router
    };

    let packet_counter = AtomicU64::new(1);
    group.bench_function("router_to_router_roundtrip", |b| {
        b.iter_batched(
            || next_gps_packet(&packet_counter),
            |pkt| {
                source.tx(pkt).unwrap();
                let frame = tx_frames.lock().unwrap().pop().unwrap();
                sink.rx_serialized(&frame).unwrap();
            },
            BatchSize::SmallInput,
        );
    });

    let relay_frames_a = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let relay_frames_b = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let relay = Relay::new(zero_clock());
    let side_a = {
        let relay_frames_a = relay_frames_a.clone();
        relay.add_side_serialized("bus_a", move |bytes: &[u8]| -> TelemetryResult<()> {
            relay_frames_a.lock().unwrap().push(bytes.to_vec());
            Ok(())
        })
    };
    let _side_b = {
        let relay_frames_b = relay_frames_b.clone();
        relay.add_side_serialized("bus_b", move |bytes: &[u8]| -> TelemetryResult<()> {
            relay_frames_b.lock().unwrap().push(bytes.to_vec());
            Ok(())
        })
    };

    let relay_counter = AtomicU64::new(10_000);
    group.bench_function("relay_forward_between_sides", |b| {
        b.iter_batched(
            || {
                let pkt = next_gps_packet(&relay_counter);
                relay_frames_a.lock().unwrap().clear();
                relay_frames_b.lock().unwrap().clear();
                sedsprintf_rs::serialize::serialize_packet(&pkt)
            },
            |frame| {
                relay.rx_serialized_from_side(side_a, &frame).unwrap();
                relay.process_all_queues().unwrap();
                let frames_b = relay_frames_b.lock().unwrap().clone();
                let frames_a = relay_frames_a.lock().unwrap().clone();
                assert!(count_frames_of_type(&frames_b, DataType::named("GPS_DATA")) >= 1);
                assert_eq!(count_frames_of_type(&frames_a, DataType::named("GPS_DATA")), 0);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, benchmark_router_system_paths);
criterion_main!(benches);
