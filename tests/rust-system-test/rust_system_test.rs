#[cfg(test)]
mod threaded_system_tests {
    use sedsprintf_rs::RouteSelectionMode;
    use sedsprintf_rs::TelemetryResult;
    use sedsprintf_rs::config::{DataEndpoint, DataType};
    use sedsprintf_rs::discovery::{DISCOVERY_ROUTE_TTL_MS, build_discovery_announce};
    use sedsprintf_rs::packet::Packet;
    use sedsprintf_rs::relay::Relay;
    use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};

    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    struct SharedClock {
        now_ms: Arc<AtomicU64>,
    }

    impl Clock for SharedClock {
        fn now_ms(&self) -> u64 {
            self.now_ms.load(Ordering::SeqCst)
        }
    }

    /// Simulated node in the Rust system test.
    struct SimNode {
        router: Arc<Router>,
        radio_hits: Arc<AtomicUsize>,
        sd_hits: Arc<AtomicUsize>,
    }

    /// Build a handler that counts packets received on the Radio endpoint
    /// (this plays the role of the "radio" handler in the C test).
    fn make_radio_handler(counter: Arc<AtomicUsize>) -> EndpointHandler {
        EndpointHandler::new_packet_handler(DataEndpoint::Radio, move |_pkt: &Packet| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    /// Build a handler that counts packets received on the SdCard endpoint
    /// (this plays the role of the "SD" handler in the C test).
    fn make_sd_handler(counter: Arc<AtomicUsize>) -> EndpointHandler {
        EndpointHandler::new_packet_handler(DataEndpoint::SdCard, move |_pkt: &Packet| {
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

    /// Build a packet with endpoints [SD_CARD, Radio], mirroring the C
    /// system’s idea that every message goes to both "radio" and "SD".
    fn make_packet(ty: DataType, vals: &[f32], ts: u64) -> Packet {
        Packet::from_f32_slice(ty, vals, &[DataEndpoint::SdCard, DataEndpoint::Radio], ts).unwrap()
    }

    #[test]
    fn router_runtime_route_modes_work_in_system_flow() {
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

        let discovery_a = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
        router
            .rx_serialized_queue_from_side(
                &sedsprintf_rs::serialize::serialize_packet(&discovery_a),
                side_a,
            )
            .unwrap();
        router.process_all_queues().unwrap();
        now_ms.store(DISCOVERY_ROUTE_TTL_MS / 2, Ordering::SeqCst);
        let discovery_b = build_discovery_announce(
            "REMOTE_B",
            DISCOVERY_ROUTE_TTL_MS / 2,
            &[DataEndpoint::Radio],
        )
        .unwrap();
        router
            .rx_serialized_queue_from_side(
                &sedsprintf_rs::serialize::serialize_packet(&discovery_b),
                side_b,
            )
            .unwrap();
        router.process_all_queues().unwrap();
        seen_a.lock().unwrap().clear();
        seen_b.lock().unwrap().clear();

        router
            .set_source_route_mode(None, RouteSelectionMode::Weighted)
            .unwrap();
        router.set_route_weight(None, side_a, 2).unwrap();
        router.set_route_weight(None, side_b, 1).unwrap();

        for seq in 0..6 {
            let pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[seq as f32, seq as f32 + 1.0, seq as f32 + 2.0],
                &[DataEndpoint::Radio],
                seq as u64,
            )
            .unwrap();
            router.tx_queue(pkt).unwrap();
        }
        router.process_tx_queue().unwrap();
        assert_eq!(seen_a.lock().unwrap().len(), 4);
        assert_eq!(seen_b.lock().unwrap().len(), 2);

        router
            .set_source_route_mode(None, RouteSelectionMode::Failover)
            .unwrap();
        router.set_route_priority(None, side_a, 0).unwrap();
        router.set_route_priority(None, side_b, 1).unwrap();
        now_ms.store(DISCOVERY_ROUTE_TTL_MS + 1, Ordering::SeqCst);

        let failover_pkt = Packet::from_f32_slice(
            DataType::BatteryStatus,
            &[7.0, 8.0],
            &[DataEndpoint::Radio],
            99,
        )
        .unwrap();
        router.tx_queue(failover_pkt).unwrap();
        router.process_tx_queue().unwrap();

        assert_eq!(seen_a.lock().unwrap().len(), 4);
        assert_eq!(seen_b.lock().unwrap().len(), 3);
    }

    #[test]
    fn relay_runtime_route_modes_work_in_system_flow() {
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
        let ingress = relay.add_side_packet("INGRESS", |_pkt: &Packet| Ok(()));

        let discovery_a = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint::Radio]).unwrap();
        relay
            .rx_serialized_from_side(
                side_a,
                &sedsprintf_rs::serialize::serialize_packet(&discovery_a),
            )
            .unwrap();
        relay.process_all_queues().unwrap();
        now_ms.store(DISCOVERY_ROUTE_TTL_MS / 2, Ordering::SeqCst);
        let discovery_b = build_discovery_announce(
            "REMOTE_B",
            DISCOVERY_ROUTE_TTL_MS / 2,
            &[DataEndpoint::Radio],
        )
        .unwrap();
        relay
            .rx_serialized_from_side(
                side_b,
                &sedsprintf_rs::serialize::serialize_packet(&discovery_b),
            )
            .unwrap();
        relay.process_all_queues().unwrap();
        seen_a.lock().unwrap().clear();
        seen_b.lock().unwrap().clear();

        relay
            .set_source_route_mode(Some(ingress), RouteSelectionMode::Weighted)
            .unwrap();
        relay.set_route_weight(Some(ingress), side_a, 2).unwrap();
        relay.set_route_weight(Some(ingress), side_b, 1).unwrap();

        for seq in 0..6 {
            let pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[seq as f32, seq as f32 + 1.0, seq as f32 + 2.0],
                &[DataEndpoint::Radio],
                seq as u64,
            )
            .unwrap();
            relay.rx_from_side(ingress, pkt).unwrap();
        }
        relay.process_all_queues().unwrap();
        assert_eq!(seen_a.lock().unwrap().len(), 4);
        assert_eq!(seen_b.lock().unwrap().len(), 2);

        relay
            .set_source_route_mode(Some(ingress), RouteSelectionMode::Failover)
            .unwrap();
        relay.set_route_priority(Some(ingress), side_a, 0).unwrap();
        relay.set_route_priority(Some(ingress), side_b, 1).unwrap();
        relay.remove_side(side_a).unwrap();

        let failover_pkt = Packet::from_f32_slice(
            DataType::BatteryStatus,
            &[7.0, 8.0],
            &[DataEndpoint::Radio],
            99,
        )
        .unwrap();
        relay.rx_from_side(ingress, failover_pkt).unwrap();
        relay.process_all_queues().unwrap();

        assert_eq!(seen_a.lock().unwrap().len(), 4);
        assert_eq!(seen_b.lock().unwrap().len(), 3);
    }

    /// Threaded system test that mirrors `main.c` but now uses the Rust
    /// Relay in addition to the buses.
    #[test]
    fn threaded_system_sim_rust() {
        // ------------- 1) Relay + two buses -------------
        // Buses are MPSC channels of (from_node_idx, wire_bytes).
        type BusMsg = (usize, Vec<u8>);

        let (bus1_tx, bus1_rx) = mpsc::channel::<BusMsg>();
        let (bus2_tx, bus2_rx) = mpsc::channel::<BusMsg>();

        // Relay with two sides: bus1, bus2.
        let relay = Arc::new(Relay::new(zero_clock()));

        // Frames sent out of relay on "bus1" side get injected into bus1_rx
        let relay_bus1_tx = bus1_tx.clone();
        let bus1_side_id =
            relay.add_side_serialized("bus1", move |bytes: &[u8]| -> TelemetryResult<()> {
                // from = usize::MAX so we don't accidentally "skip" any node
                relay_bus1_tx.send((usize::MAX, bytes.to_vec())).unwrap();
                Ok(())
            });

        // Frames sent out of relay on "bus2" side get injected into bus2_rx
        let relay_bus2_tx = bus2_tx.clone();
        let bus2_side_id =
            relay.add_side_serialized("bus2", move |bytes: &[u8]| -> TelemetryResult<()> {
                relay_bus2_tx.send((usize::MAX, bytes.to_vec())).unwrap();
                Ok(())
            });

        // ------------- 2) Build nodes -------------
        let stop_flag = Arc::new(AtomicBool::new(false));

        // Node 0: "Radio Board" (radio_hits via Radio endpoint) — on bus1
        // Node 1: "Flight Controller Board" (sd_hits via SdCard endpoint) — on bus1
        // Node 2: "Power Board" (no local endpoints) — on bus2
        let mut nodes: Vec<SimNode> = Vec::new();

        for (idx, _) in ["Radio Board", "Flight Controller Board", "Power Board"]
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

            // Choose bus based on node index
            let (local_bus_tx, _relay_side_id) = if idx <= 1 {
                // Nodes 0 and 1 on bus1
                (bus1_tx.clone(), bus1_side_id)
            } else {
                // Node 2 on bus2
                (bus2_tx.clone(), bus2_side_id)
            };

            // TX closure: router -> bus (which then forwards to other nodes and relay)
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
                router: Arc::new(router),
                radio_hits,
                sd_hits,
            });
        }

        let nodes = Arc::new(nodes);

        // ------------- 3) Bus threads -------------
        // bus1: nodes 0, 1
        let bus1_nodes = vec![0usize, 1usize];
        let bus1_nodes_arc = nodes.clone();
        let bus1_stop = stop_flag.clone();
        let bus1_relay = relay.clone();
        let bus1_handle = thread::spawn(move || {
            while !bus1_stop.load(Ordering::SeqCst) {
                match bus1_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok((from, frame)) => {
                        // Broadcast to nodes on bus1 (except sender if it’s a real node index)
                        for idx in &bus1_nodes {
                            if *idx != from {
                                bus1_nodes_arc[*idx]
                                    .router
                                    .rx_serialized_queue(&frame)
                                    .expect("bus1: rx_serialized_packet_to_queue failed");
                            }
                        }

                        // Feed into relay from bus1 side
                        bus1_relay
                            .rx_serialized_from_side(bus1_side_id, &frame)
                            .expect("bus1 -> relay failed");
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // nothing pending, loop again
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            // Final drain (non-blocking)
            while let Ok((from, frame)) = bus1_rx.try_recv() {
                for idx in &bus1_nodes {
                    if *idx != from {
                        bus1_nodes_arc[*idx]
                            .router
                            .rx_serialized_queue(&frame)
                            .expect("bus1: rx_serialized_packet_to_queue failed (drain)");
                    }
                }
                bus1_relay
                    .rx_serialized_from_side(bus1_side_id, &frame)
                    .expect("bus1 -> relay failed (drain)");
            }
        });

        // bus2: node 2
        let bus2_nodes = vec![2usize];
        let bus2_nodes_arc = nodes.clone();
        let bus2_stop = stop_flag.clone();
        let bus2_relay = relay.clone();
        let bus2_handle = thread::spawn(move || {
            while !bus2_stop.load(Ordering::SeqCst) {
                match bus2_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok((from, frame)) => {
                        for idx in &bus2_nodes {
                            if *idx != from {
                                bus2_nodes_arc[*idx]
                                    .router
                                    .rx_serialized_queue(&frame)
                                    .expect("bus2: rx_serialized_packet_to_queue failed");
                            }
                        }

                        // Feed into relay from bus2 side
                        bus2_relay
                            .rx_serialized_from_side(bus2_side_id, &frame)
                            .expect("bus2 -> relay failed");
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            // Final drain
            while let Ok((from, frame)) = bus2_rx.try_recv() {
                for idx in &bus2_nodes {
                    if *idx != from {
                        bus2_nodes_arc[*idx]
                            .router
                            .rx_serialized_queue(&frame)
                            .expect("bus2: rx_serialized_packet_to_queue failed (drain)");
                    }
                }
                bus2_relay
                    .rx_serialized_from_side(bus2_side_id, &frame)
                    .expect("bus2 -> relay failed (drain)");
            }
        });

        // ------------- 4) Relay processor thread -------------
        let relay_stop = stop_flag.clone();
        let relay_clone = relay.clone();
        let relay_handle = thread::spawn(move || {
            while !relay_stop.load(Ordering::SeqCst) {
                relay_clone
                    .process_all_queues_with_timeout(5)
                    .expect("relay process_all_queues_with_timeout failed");
                thread::sleep(Duration::from_millis(1));
            }

            // Final drain
            for _ in 0..50 {
                relay_clone
                    .process_all_queues_with_timeout(0)
                    .expect("relay final drain failed");
                thread::sleep(Duration::from_millis(1));
            }
        });

        // ------------- 5) Processor threads (one per node) -------------
        let mut proc_handles = Vec::new();
        for node in nodes.iter() {
            let r = node.router.clone();
            let stop = stop_flag.clone();
            let handle = thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    r.process_all_queues_with_timeout(5).unwrap();
                    thread::sleep(Duration::from_millis(1));
                }

                // Final drain
                for _ in 0..50 {
                    r.process_all_queues_with_timeout(0).unwrap();
                    thread::sleep(Duration::from_millis(1));
                }
            });
            proc_handles.push(handle);
        }

        // ------------- 6) Sender threads (A, B, C) -------------
        let radio_router = nodes[0].router.clone();
        let flight_router = nodes[1].router.clone();
        let power_router = nodes[2].router.clone();

        // Sender A – GPS-like data
        let sender_a = thread::spawn(move || {
            let mut buf = [0.0_f32; 8];
            for i in 0..5 {
                make_series(&mut buf[..3], 10.0);
                let pkt = make_packet(DataType::GpsData, &buf[..3], i);
                radio_router.tx(pkt).unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        });

        // Sender B – IMU + BARO-like data
        let sender_b = thread::spawn(move || {
            let mut buf = [0.0_f32; 8];
            for i in 0..5 {
                // IMU-like data
                make_series(&mut buf[..3], 0.5);
                let pkt1 = make_packet(DataType::GpsData, &buf[..3], i);
                flight_router.tx(pkt1).unwrap();
                thread::sleep(Duration::from_millis(5));

                // BARO-like data
                make_series(&mut buf[..3], 101.3);
                let pkt2 = make_packet(DataType::GpsData, &buf[..3], i + 100);
                flight_router.tx(pkt2).unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        });

        // Sender C – battery + message
        let sender_c = thread::spawn(move || {
            let mut buf = [0.0_f32; 8];
            for i in 0..5 {
                make_series(&mut buf[..2], 3.7);
                let pkt1 = make_packet(DataType::BatteryStatus, &buf[..2], i + 200);
                power_router.tx(pkt1).unwrap();
                thread::sleep(Duration::from_millis(5));

                let msg = "hello world!";
                let pkt2 = Packet::from_str_slice(
                    DataType::TelemetryError,
                    msg,
                    &[DataEndpoint::SdCard, DataEndpoint::Radio],
                    i + 300,
                )
                .unwrap();
                power_router.tx(pkt2).unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        });

        // ------------- 7) Join senders -------------
        sender_a.join().expect("sender A panicked");
        sender_b.join().expect("sender B panicked");
        sender_c.join().expect("sender C panicked");

        // ------------- 8) Wait for expected hits or timeout -------------
        let expected_total = 25; // same as before
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            let a_radio = nodes[0].radio_hits.load(Ordering::SeqCst);
            let b_sd = nodes[1].sd_hits.load(Ordering::SeqCst);

            if a_radio == expected_total && b_sd == expected_total {
                break;
            }

            if Instant::now() > deadline {
                eprintln!(
                    "Timeout waiting for processors: A.radio_hits={}, B.sd_hits={}",
                    a_radio, b_sd
                );
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        // ------------- 9) Stop processors + buses + relay and join -------------
        stop_flag.store(true, Ordering::SeqCst);

        for handle in proc_handles {
            handle.join().expect("processor thread panicked");
        }
        bus1_handle.join().expect("bus1 thread panicked");
        bus2_handle.join().expect("bus2 thread panicked");
        relay_handle.join().expect("relay thread panicked");

        // ------------- 10) Assertions (mirror C system test) -------------
        let radio_board = &nodes[0];
        let flight_board = &nodes[1];
        let power_board = &nodes[2];

        println!(
            "A.radio_hits={}, B.sd_hits={}, C.radio_hits={}, C.sd_hits={}",
            radio_board.radio_hits.load(Ordering::SeqCst),
            flight_board.sd_hits.load(Ordering::SeqCst),
            power_board.radio_hits.load(Ordering::SeqCst),
            power_board.sd_hits.load(Ordering::SeqCst),
        );

        assert_eq!(
            radio_board.radio_hits.load(Ordering::SeqCst),
            expected_total,
            "Radio Board should see {} packets",
            expected_total
        );
        assert_eq!(
            flight_board.sd_hits.load(Ordering::SeqCst),
            expected_total,
            "Flight Controller Board should see {} SD packets",
            expected_total
        );
        assert_eq!(
            power_board.radio_hits.load(Ordering::SeqCst),
            0,
            "Power Board must not have a radio handler"
        );
        assert_eq!(
            power_board.sd_hits.load(Ordering::SeqCst),
            0,
            "Power Board must not have an SD handler"
        );
    }
}
