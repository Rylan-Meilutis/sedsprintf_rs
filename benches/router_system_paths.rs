use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use sedsprintf_rs::TelemetryResult;
use sedsprintf_rs::config::{DataEndpoint, DataType};
use sedsprintf_rs::packet::Packet;
use sedsprintf_rs::relay::Relay;
use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn zero_clock() -> Box<dyn Clock + Send + Sync> {
    Box::new(|| 0u64)
}

fn endpoints() -> [DataEndpoint; 2] {
    [DataEndpoint::named("SD_CARD"), DataEndpoint::named("RADIO")]
}

fn next_gps_packet(counter: &AtomicU64) -> Packet {
    let ts = counter.fetch_add(1, Ordering::Relaxed);
    let vals = [ts as f32, ts as f32 + 0.25, ts as f32 + 0.5];
    Packet::from_f32_slice(DataType::named("GPS_DATA"), &vals, &endpoints(), ts).unwrap()
}

fn benchmark_router_system_paths(c: &mut Criterion) {
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
                let forwarded = relay_frames_b.lock().unwrap().pop().unwrap();
                assert!(!forwarded.is_empty());
                assert!(relay_frames_a.lock().unwrap().is_empty());
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, benchmark_router_system_paths);
criterion_main!(benches);
