use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use sedsprintf_rs::config::{DataEndpoint, DataType};
use sedsprintf_rs::packet::Packet;
use sedsprintf_rs::relay::Relay;
use sedsprintf_rs::router::{Clock, Router, RouterConfig};
use sedsprintf_rs::RouteSelectionMode;
use sedsprintf_rs::TelemetryResult;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn zero_clock() -> Box<dyn Clock + Send + Sync> {
    Box::new(|| 0u64)
}

fn next_packet(counter: &AtomicU64) -> Packet {
    let ts = counter.fetch_add(1, Ordering::Relaxed);
    Packet::from_f32_slice(
        DataType::named("GPS_DATA"),
        &[ts as f32, ts as f32 + 1.0, ts as f32 + 2.0],
        &[DataEndpoint::named("RADIO")],
        ts,
    )
        .unwrap()
}

fn benchmark_route_selection_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("route_selection_paths");

    let weighted_hits_a = Arc::new(AtomicUsize::new(0));
    let weighted_hits_b = Arc::new(AtomicUsize::new(0));
    let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
    let side_a = {
        let weighted_hits_a = weighted_hits_a.clone();
        router.add_side_packet("A", move |_pkt| -> TelemetryResult<()> {
            weighted_hits_a.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    };
    let side_b = {
        let weighted_hits_b = weighted_hits_b.clone();
        router.add_side_packet("B", move |_pkt| -> TelemetryResult<()> {
            weighted_hits_b.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    };
    let discovery_a = sedsprintf_rs::discovery::build_discovery_announce(
        "REMOTE_A",
        0,
        &[DataEndpoint::named("RADIO")],
    )
        .unwrap();
    let discovery_b = sedsprintf_rs::discovery::build_discovery_announce(
        "REMOTE_B",
        1,
        &[DataEndpoint::named("RADIO")],
    )
        .unwrap();
    router.rx_from_side(&discovery_a, side_a).unwrap();
    router.rx_from_side(&discovery_b, side_b).unwrap();
    router
        .set_source_route_mode(None, RouteSelectionMode::Weighted)
        .unwrap();
    router.set_route_weight(None, side_a, 3).unwrap();
    router.set_route_weight(None, side_b, 1).unwrap();

    let counter = AtomicU64::new(1);
    group.bench_function("router_weighted_local_tx", |b| {
        b.iter_batched(
            || next_packet(&counter),
            |pkt| {
                router.tx(pkt).unwrap();
            },
            BatchSize::SmallInput,
        );
    });

    let relay_out_a = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let relay_out_b = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let relay = Relay::new(zero_clock());
    let ingress = relay.add_side_serialized("INGRESS", |_bytes| Ok(()));
    let path_a = {
        let relay_out_a = relay_out_a.clone();
        relay.add_side_serialized("A", move |bytes| -> TelemetryResult<()> {
            relay_out_a.lock().unwrap().push(bytes.to_vec());
            Ok(())
        })
    };
    let path_b = {
        let relay_out_b = relay_out_b.clone();
        relay.add_side_serialized("B", move |bytes| -> TelemetryResult<()> {
            relay_out_b.lock().unwrap().push(bytes.to_vec());
            Ok(())
        })
    };
    let relay_discovery_a = sedsprintf_rs::discovery::build_discovery_announce(
        "REMOTE_A",
        0,
        &[DataEndpoint::named("RADIO")],
    )
        .unwrap();
    let relay_discovery_b = sedsprintf_rs::discovery::build_discovery_announce(
        "REMOTE_B",
        1,
        &[DataEndpoint::named("RADIO")],
    )
        .unwrap();
    relay.rx_from_side(path_a, relay_discovery_a).unwrap();
    relay.rx_from_side(path_b, relay_discovery_b).unwrap();
    relay.process_all_queues().unwrap();
    relay
        .set_source_route_mode(Some(ingress), RouteSelectionMode::Failover)
        .unwrap();
    relay.set_route_priority(Some(ingress), path_a, 0).unwrap();
    relay.set_route_priority(Some(ingress), path_b, 1).unwrap();

    let relay_counter = AtomicU64::new(10_000);
    group.bench_function("relay_failover_forward", |b| {
        b.iter_batched(
            || sedsprintf_rs::serialize::serialize_packet(&next_packet(&relay_counter)),
            |frame| {
                relay_out_a.lock().unwrap().clear();
                relay_out_b.lock().unwrap().clear();
                relay.rx_serialized_from_side(ingress, &frame).unwrap();
                relay.process_all_queues().unwrap();
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, benchmark_route_selection_paths);
criterion_main!(benches);
