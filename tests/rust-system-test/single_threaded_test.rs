// tests/rust-system-test/single_threaded_test.rs
#[cfg(test)]
mod single_threaded_test {
    use sedsprintf_rs::config::{DataEndpoint, DataType};
    use sedsprintf_rs::packet::Packet;
    use sedsprintf_rs::relay::Relay;
    use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};
    use sedsprintf_rs::TelemetryResult;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, TryRecvError};
    use std::sync::Arc;

    fn env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(default)
    }

    /// Clock that always returns 0
    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    /// Simulated node in the Rust system test (single-threaded).
    struct SimNode {
        router: Router,
        radio_hits: Arc<AtomicUsize>,
        sd_hits: Arc<AtomicUsize>,
    }

    /// Build a handler that counts packets received on the Radio endpoint.
    fn make_radio_handler(counter: Arc<AtomicUsize>) -> EndpointHandler {
        EndpointHandler::new_packet_handler(DataEndpoint::named("RADIO"), move |_pkt: &Packet| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    /// Build a handler that counts packets received on the SdCard endpoint.
    fn make_sd_handler(counter: Arc<AtomicUsize>) -> EndpointHandler {
        EndpointHandler::new_packet_handler(DataEndpoint::named("SD_CARD"), move |_pkt: &Packet| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    /// Helper to generate a simple float series like the C helper `make_series`.
    fn make_series(buf: &mut [f32], base: f32) {
        for (i, v) in buf.iter_mut().enumerate() {
            *v = base + (i as f32) * 0.25;
        }
    }

    /// Build a packet with endpoints [SD_CARD, Radio], mirroring the C system.
    fn make_packet(ty: DataType, vals: &[f32], ts: u64) -> Packet {
        Packet::from_f32_slice(
            ty,
            vals,
            &[DataEndpoint::named("SD_CARD"), DataEndpoint::named("RADIO")],
            ts,
        )
            .unwrap()
    }

    /// Drain both buses once and run the relay once.
    ///
    /// This is intentionally "one pass": we call it repeatedly inside the main
    /// loop and again in the final drain phase to converge all traffic.
    fn drain_buses_and_relay_once(
        nodes: &mut [SimNode],
        bus1_rx: &Receiver<(usize, Vec<u8>)>,
        bus2_rx: &Receiver<(usize, Vec<u8>)>,
        relay: &Relay,
        bus1_side_id: usize,
        bus2_side_id: usize,
    ) {
        // bus1 has nodes 0 and 1
        loop {
            match bus1_rx.try_recv() {
                Ok((from, frame)) => {
                    for (idx, node) in nodes.iter_mut().enumerate() {
                        if idx > 1 {
                            continue; // only nodes 0 and 1 live on bus1
                        }
                        if idx == from {
                            continue; // no loopback to sender on this bus
                        }
                        node.router
                            .rx_serialized_queue(&frame)
                            .expect("bus1: rx_serialized_packet_to_queue failed");
                    }

                    // Feed into relay from bus1 side (even if it came from relay,
                    // dedupe in Relay will ignore duplicates).
                    relay
                        .rx_serialized_from_side(bus1_side_id, &frame)
                        .expect("bus1 -> relay failed");
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    panic!("bus1 channel disconnected unexpectedly");
                }
            }
        }

        // bus2 has node 2
        loop {
            match bus2_rx.try_recv() {
                Ok((from, frame)) => {
                    for (idx, node) in nodes.iter_mut().enumerate() {
                        if idx != 2 {
                            continue; // only node 2 lives on bus2
                        }
                        if idx == from {
                            continue;
                        }
                        node.router
                            .rx_serialized_queue(&frame)
                            .expect("bus2: rx_serialized_packet_to_queue failed");
                    }

                    // Feed into relay from bus2 side
                    relay
                        .rx_serialized_from_side(bus2_side_id, &frame)
                        .expect("bus2 -> relay failed");
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    panic!("bus2 channel disconnected unexpectedly");
                }
            }
        }

        // Let relay fan out whatever it has queued (both RX and TX).
        relay
            .process_all_queues_with_timeout(0)
            .expect("relay process_all_queues_with_timeout failed");
    }

    /// Single-threaded stress test to profile router + relay performance.
    ///
    /// This is intentionally heavy; run it explicitly (e.g. with cargo-flamegraph).
    #[test]
    fn single_threaded_router_stress() {
        // ---------- 1) Two buses + Relay ----------
        type BusMsg = (usize, Vec<u8>);

        let (bus1_tx, bus1_rx) = mpsc::channel::<BusMsg>();
        let (bus2_tx, bus2_rx) = mpsc::channel::<BusMsg>();

        // Relay bridges bus1 <-> bus2.
        let relay = Relay::new(zero_clock());

        // Relay side for bus1: TX from relay -> bus1_rx
        let relay_bus1_tx = bus1_tx.clone();
        let bus1_side_id =
            relay.add_side_serialized("bus1", move |bytes: &[u8]| -> TelemetryResult<()> {
                // from = usize::MAX so we never skip this in bus1 delivery
                relay_bus1_tx.send((usize::MAX, bytes.to_vec())).unwrap();
                Ok(())
            });

        // Relay side for bus2: TX from relay -> bus2_rx
        let relay_bus2_tx = bus2_tx.clone();
        let bus2_side_id =
            relay.add_side_serialized("bus2", move |bytes: &[u8]| -> TelemetryResult<()> {
                relay_bus2_tx.send((usize::MAX, bytes.to_vec())).unwrap();
                Ok(())
            });

        // ---------- 2) Build nodes (same topology as threaded test) ----------
        //
        // Node 0: "Radio Board" (radio_hits via Radio endpoint) on bus1
        // Node 1: "Flight Controller Board" (sd_hits via SdCard endpoint) on bus1
        // Node 2: "Power Board" (no local endpoints) on bus2
        let mut nodes: Vec<SimNode> = Vec::new();

        for (idx, _name) in ["Radio Board", "Flight Controller Board", "Power Board"]
            .iter()
            .enumerate()
        {
            let radio_hits = Arc::new(AtomicUsize::new(0));
            let sd_hits = Arc::new(AtomicUsize::new(0));

            let mut handlers = Vec::<EndpointHandler>::new();
            match idx {
                // Radio Board: only "radio" is local
                0 => {
                    handlers.push(make_radio_handler(radio_hits.clone()));
                }
                // Flight Controller Board: only "SD card" is local
                1 => {
                    handlers.push(make_sd_handler(sd_hits.clone()));
                }
                // Power Board: no local endpoints
                _ => {}
            }

            let clock = zero_clock();

            // Choose which bus this node uses for TX.
            let local_bus_tx = if idx <= 1 {
                // nodes 0 and 1 on bus1
                bus1_tx.clone()
            } else {
                // node 2 on bus2
                bus2_tx.clone()
            };

            // tx: push a copy of the wire bytes onto the node's bus with source id
            let tx = move |bytes: &[u8]| -> TelemetryResult<()> {
                local_bus_tx.send((idx, bytes.to_vec())).unwrap();
                Ok(())
            };

            let router = if handlers.is_empty() {
                Router::new_with_clock(RouterConfig::default(), clock)
            } else {
                Router::new_with_clock(RouterConfig::new(handlers), clock)
            };
            router.add_side_serialized("bus", tx);

            nodes.push(SimNode {
                router,
                radio_hits,
                sd_hits,
            });
        }

        // ---------- 3) Stress loop: single-threaded send + route + relay ----------
        //
        // Each iteration:
        //   A: GPS
        //   B: GYRO + BARO
        //   C: BATTERY + MESSAGE
        // → 5 packets / iteration, all with [SD_CARD, Radio] endpoints.
        //
        let repeats = env_usize("SEDSPRINTF_SINGLE_THREADED_REPEATS", 1);
        let iters = env_usize("SEDSPRINTF_SINGLE_THREADED_ITERS", 10_000);
        const PACKETS_PER_ITER: usize = 5;

        let mut gps_buf = [0.0_f32; 8];
        let mut gyro_buf = [0.0_f32; 8];
        let mut baro_buf = [0.0_f32; 8];
        let mut batt_buf = [0.0_f32; 8];
        let msg = "hello world!";

        for repeat_idx in 0..repeats {
            for i in 0..iters {
                let seq_base = (repeat_idx * iters + i) as u64;
                {
                    let node = &mut nodes[0];
                    make_series(&mut gps_buf[..3], 10.0);
                    let pkt = make_packet(DataType::named("GPS_DATA"), &gps_buf[..3], seq_base);
                    node.router.tx(pkt).unwrap();
                }

                {
                    let node = &mut nodes[1];
                    make_series(&mut gyro_buf[..3], 0.5);
                    let pkt1 = make_packet(
                        DataType::named("GPS_DATA"),
                        &gyro_buf[..3],
                        seq_base + 10_000,
                    );
                    node.router.tx(pkt1).unwrap();

                    make_series(&mut baro_buf[..3], 101.3);
                    let pkt2 = make_packet(
                        DataType::named("GPS_DATA"),
                        &baro_buf[..3],
                        seq_base + 20_000,
                    );
                    node.router.tx(pkt2).unwrap();
                }

                {
                    let node = &mut nodes[2];
                    make_series(&mut batt_buf[..2], 3.7);
                    let pkt1 = make_packet(
                        DataType::named("BATTERY_STATUS"),
                        &batt_buf[..2],
                        seq_base + 30_000,
                    );
                    node.router.tx(pkt1).unwrap();

                    let pkt2 = Packet::from_str_slice(
                        DataType::TelemetryError,
                        msg,
                        &[DataEndpoint::named("SD_CARD"), DataEndpoint::named("RADIO")],
                        seq_base + 40_000,
                    )
                        .unwrap();
                    node.router.tx(pkt2).unwrap();
                }

                drain_buses_and_relay_once(
                    &mut nodes,
                    &bus1_rx,
                    &bus2_rx,
                    &relay,
                    bus1_side_id,
                    bus2_side_id,
                );

                for node in nodes.iter_mut() {
                    node.router
                        .process_all_queues_with_timeout(0)
                        .expect("process_all_queues_with_timeout failed");
                }
            }
        }

        // ---------- 4) Final drain: repeat bus + relay + router passes ----------
        for _ in 0..10 {
            drain_buses_and_relay_once(
                &mut nodes,
                &bus1_rx,
                &bus2_rx,
                &relay,
                bus1_side_id,
                bus2_side_id,
            );

            for node in nodes.iter_mut() {
                node.router
                    .process_all_queues_with_timeout(0)
                    .expect("process_all_queues_with_timeout failed in final drain");
            }
        }

        // ---------- 5) Assertions (scaled by ITERS) ----------
        let expected_total = repeats * iters * PACKETS_PER_ITER;

        let radio_board = &nodes[0];
        let flight_board = &nodes[1];
        let power_board = &nodes[2];

        let a_radio = radio_board.radio_hits.load(Ordering::SeqCst);
        let b_sd = flight_board.sd_hits.load(Ordering::SeqCst);
        let c_radio = power_board.radio_hits.load(Ordering::SeqCst);
        let c_sd = power_board.sd_hits.load(Ordering::SeqCst);

        println!(
            "single-threaded (with relay): repeats={}, iters={}, A.radio_hits={}, B.sd_hits={}, C.radio_hits={}, C.sd_hits={}",
            repeats, iters, a_radio, b_sd, c_radio, c_sd
        );

        assert_eq!(a_radio, expected_total, "Radio Board hit count");
        assert_eq!(b_sd, expected_total, "Flight Controller SD hit count");
        assert_eq!(c_radio, 0, "Power Board must not have a radio handler");
        assert_eq!(c_sd, 0, "Power Board must not have an SD handler");
    }
}
