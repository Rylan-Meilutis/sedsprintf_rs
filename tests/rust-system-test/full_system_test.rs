#[cfg(test)]
mod mega_library_system_tests {
    use sedsprintf_rs::config::{DataEndpoint, DataType};
    use sedsprintf_rs::packet::Packet;
    use sedsprintf_rs::relay::Relay;
    use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};
    use sedsprintf_rs::TelemetryResult;

    use sedsprintf_rs::serialize::serialize_packet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    type BusMsg = (&'static str, Vec<u8>);

    fn mk_counter_handler(endpoint: DataEndpoint, counter: Arc<AtomicUsize>) -> EndpointHandler {
        EndpointHandler::new_packet_handler(endpoint, move |_pkt: &Packet| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn make_series(buf: &mut [f32], base: f32) {
        for (i, v) in buf.iter_mut().enumerate() {
            *v = base + (i as f32) * 0.25;
        }
    }

    fn make_packet(ty: DataType, vals: &[f32], ts: u64) -> Packet {
        // Every packet targets BOTH endpoints (full coverage)
        Packet::from_f32_slice(
            ty,
            vals,
            &[DataEndpoint::named("SD_CARD"), DataEndpoint::named("RADIO")],
            ts,
        )
            .unwrap()
    }

    #[test]
    fn multibus_relay_and_router_relay_mode_both_endpoints_exercised() {
        // -------------------------------
        // 1) Create the 3 buses
        // -------------------------------
        let (bus_a_tx, bus_a_rx) = mpsc::channel::<BusMsg>();
        let (bus_b_tx, bus_b_rx) = mpsc::channel::<BusMsg>();
        let (bus_c_tx, bus_c_rx) = mpsc::channel::<BusMsg>();

        // -------------------------------
        // 2) Relay bridging all 3 buses
        // -------------------------------
        let relay = Arc::new(Relay::new(zero_clock()));

        let r_a_tx = bus_a_tx.clone();
        let relay_side_a =
            relay.add_side_serialized("bus_a", move |bytes: &[u8]| -> TelemetryResult<()> {
                r_a_tx.send(("relay", bytes.to_vec())).unwrap();
                Ok(())
            });

        let r_b_tx = bus_b_tx.clone();
        let relay_side_b =
            relay.add_side_serialized("bus_b", move |bytes: &[u8]| -> TelemetryResult<()> {
                r_b_tx.send(("relay", bytes.to_vec())).unwrap();
                Ok(())
            });

        let r_c_tx = bus_c_tx.clone();
        let relay_side_c =
            relay.add_side_serialized("bus_c", move |bytes: &[u8]| -> TelemetryResult<()> {
                r_c_tx.send(("relay", bytes.to_vec())).unwrap();
                Ok(())
            });

        // -------------------------------
        // 3) Stop + stats
        // -------------------------------
        let stop = Arc::new(AtomicBool::new(false));

        let a_radio_hits = Arc::new(AtomicUsize::new(0));
        let a_sd_hits = Arc::new(AtomicUsize::new(0));
        let b_radio_hits = Arc::new(AtomicUsize::new(0));
        let b_sd_hits = Arc::new(AtomicUsize::new(0));
        let c_radio_hits = Arc::new(AtomicUsize::new(0));
        let c_sd_hits = Arc::new(AtomicUsize::new(0));

        // Track TX counts only; link-specific tracking is no longer available.

        // -------------------------------
        // 4) Three sink routers, each has BOTH endpoint handlers
        // -------------------------------
        let node_a_router = {
            let handlers = vec![
                mk_counter_handler(DataEndpoint::named("RADIO"), a_radio_hits.clone()),
                mk_counter_handler(DataEndpoint::named("SD_CARD"), a_sd_hits.clone()),
            ];

            let router = Router::new_with_clock(RouterConfig::new(handlers), zero_clock());
            router.add_side_serialized("bus_a", {
                let bus = bus_a_tx.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    bus.send(("node_a", bytes.to_vec())).unwrap();
                    Ok(())
                }
            });
            Arc::new(router)
        };

        let node_b_router = {
            let handlers = vec![
                mk_counter_handler(DataEndpoint::named("RADIO"), b_radio_hits.clone()),
                mk_counter_handler(DataEndpoint::named("SD_CARD"), b_sd_hits.clone()),
            ];

            let router = Router::new_with_clock(RouterConfig::new(handlers), zero_clock());
            router.add_side_serialized("bus_b", {
                let bus = bus_b_tx.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    bus.send(("node_b", bytes.to_vec())).unwrap();
                    Ok(())
                }
            });
            Arc::new(router)
        };

        let node_c_router = {
            let handlers = vec![
                mk_counter_handler(DataEndpoint::named("RADIO"), c_radio_hits.clone()),
                mk_counter_handler(DataEndpoint::named("SD_CARD"), c_sd_hits.clone()),
            ];

            let router = Router::new_with_clock(RouterConfig::new(handlers), zero_clock());
            router.add_side_serialized("bus_c", {
                let bus = bus_c_tx.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    bus.send(("node_c", bytes.to_vec())).unwrap();
                    Ok(())
                }
            });
            Arc::new(router)
        };

        // -------------------------------
        // 5) Hub router in RELAY mode (no local handlers)
        // -------------------------------
        let (hub_router, hub_side_a, hub_side_b, hub_side_c) = {
            let router = Router::new_with_clock(RouterConfig::default(), zero_clock());
            let hub_side_a = router.add_side_serialized("bus_a", {
                let bus = bus_a_tx.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    bus.send(("hub_router", bytes.to_vec())).unwrap();
                    Ok(())
                }
            });
            let hub_side_b = router.add_side_serialized("bus_b", {
                let bus = bus_b_tx.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    bus.send(("hub_router", bytes.to_vec())).unwrap();
                    Ok(())
                }
            });
            let hub_side_c = router.add_side_serialized("bus_c", {
                let bus = bus_c_tx.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    bus.send(("hub_router", bytes.to_vec())).unwrap();
                    Ok(())
                }
            });
            (Arc::new(router), hub_side_a, hub_side_b, hub_side_c)
        };

        // -------------------------------
        // 6) Bus threads deliver to:
        //   - local sink router
        //   - relay (fanout)
        //   - hub router (link-aware receive)
        // -------------------------------
        let spawn_bus = |name: &'static str,
                         rx: mpsc::Receiver<BusMsg>,
                         local_node: Arc<Router>,
                         relay: Arc<Relay>,
                         relay_side: usize,
                         hub: Arc<Router>,
                         hub_side: usize,
                         stop: Arc<AtomicBool>| {
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match rx.recv_timeout(Duration::from_millis(10)) {
                        Ok((_from, frame)) => {
                            local_node.rx_serialized_queue(&frame).unwrap();
                            hub.rx_serialized_queue_from_side(&frame, hub_side).unwrap();
                            relay.rx_serialized_from_side(relay_side, &frame).unwrap();
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                while let Ok((_from, frame)) = rx.try_recv() {
                    local_node.rx_serialized_queue(&frame).unwrap();
                    hub.rx_serialized_queue_from_side(&frame, hub_side).unwrap();
                    relay.rx_serialized_from_side(relay_side, &frame).unwrap();
                }
                eprintln!("bus thread {name} exiting");
            })
        };

        let bus_a_handle = spawn_bus(
            "bus_a",
            bus_a_rx,
            node_a_router.clone(),
            relay.clone(),
            relay_side_a,
            hub_router.clone(),
            hub_side_a,
            stop.clone(),
        );
        let bus_b_handle = spawn_bus(
            "bus_b",
            bus_b_rx,
            node_b_router.clone(),
            relay.clone(),
            relay_side_b,
            hub_router.clone(),
            hub_side_b,
            stop.clone(),
        );
        let bus_c_handle = spawn_bus(
            "bus_c",
            bus_c_rx,
            node_c_router.clone(),
            relay.clone(),
            relay_side_c,
            hub_router.clone(),
            hub_side_c,
            stop.clone(),
        );

        // -------------------------------
        // 7) Processor threads (relay + routers)
        // -------------------------------
        let relay_proc = {
            let stop = stop.clone();
            let relay = relay.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    relay.process_all_queues_with_timeout(5).unwrap();
                    thread::sleep(Duration::from_millis(1));
                }
                for _ in 0..50 {
                    relay.process_all_queues_with_timeout(0).unwrap();
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let spawn_router_proc = |r: Arc<Router>, stop: Arc<AtomicBool>| {
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    r.process_all_queues_with_timeout(5).unwrap();
                    thread::sleep(Duration::from_millis(1));
                }
                for _ in 0..50 {
                    r.process_all_queues_with_timeout(0).unwrap();
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let proc_a = spawn_router_proc(node_a_router.clone(), stop.clone());
        let proc_b = spawn_router_proc(node_b_router.clone(), stop.clone());
        let proc_c = spawn_router_proc(node_c_router.clone(), stop.clone());
        let proc_hub = spawn_router_proc(hub_router.clone(), stop.clone());

        // -------------------------------
        // 8) Generators
        //   - nodes generate traffic (packet + serialized, queue + immediate)
        //   - hub ALSO generates traffic (forces hub TX paths to execute)
        // -------------------------------
        let gen_a = {
            let r = node_a_router.clone();
            thread::spawn(move || {
                let mut buf = [0.0_f32; 8];
                for i in 0..8 {
                    make_series(&mut buf[..3], 10.0);
                    let pkt = make_packet(DataType::named("GPS_DATA"), &buf[..3], i);
                    r.tx(pkt).unwrap();
                    thread::sleep(Duration::from_millis(3));
                }
            })
        };

        let gen_b = {
            let r = node_b_router.clone();
            thread::spawn(move || {
                let mut buf = [0.0_f32; 8];
                for i in 0..8 {
                    make_series(&mut buf[..2], 3.7);
                    let pkt = make_packet(DataType::named("BATTERY_STATUS"), &buf[..2], 100 + i);

                    r.tx_queue(pkt.clone()).unwrap();

                    let wire = serialize_packet(&pkt);
                    r.tx_serialized(wire).unwrap();

                    thread::sleep(Duration::from_millis(3));
                }
            })
        };

        let gen_c = {
            let r = node_c_router.clone();
            thread::spawn(move || {
                for i in 0..8 {
                    let msg = format!("hello-{i}");
                    let pkt = Packet::from_str_slice(
                        DataType::TelemetryError,
                        &msg,
                        &[DataEndpoint::named("SD_CARD"), DataEndpoint::named("RADIO")],
                        200 + i as u64,
                    )
                        .unwrap();

                    let wire = serialize_packet(&pkt);
                    r.tx_serialized_queue(wire).unwrap();

                    thread::sleep(Duration::from_millis(3));
                }
            })
        };

        let gen_hub = {
            let hub = hub_router.clone();
            thread::spawn(move || {
                let mut buf = [0.0_f32; 8];

                for i in 0..6 {
                    make_series(&mut buf[..3], 42.0 + i as f32);

                    let pkt_a = make_packet(DataType::named("GPS_DATA"), &buf[..3], 1000 + i);
                    hub.tx(pkt_a.clone()).unwrap();
                    hub.tx_queue(pkt_a).unwrap();

                    let pkt_b = make_packet(DataType::named("BATTERY_STATUS"), &buf[..2], 2000 + i);
                    let wire_b = serialize_packet(&pkt_b);
                    hub.tx_serialized(wire_b.clone()).unwrap();
                    hub.tx_serialized_queue(wire_b).unwrap();

                    let pkt_c = Packet::from_str_slice(
                        DataType::TelemetryError,
                        "hub-msg",
                        &[DataEndpoint::named("SD_CARD"), DataEndpoint::named("RADIO")],
                        3000 + i,
                    )
                        .unwrap();
                    let wire_c = serialize_packet(&pkt_c);
                    hub.tx_serialized_queue(wire_c).unwrap();

                    thread::sleep(Duration::from_millis(2));
                }
            })
        };

        gen_a.join().unwrap();
        gen_b.join().unwrap();
        gen_c.join().unwrap();
        gen_hub.join().unwrap();

        // -------------------------------
        // 9) Wait for BOTH endpoints to be hit on ALL nodes
        // -------------------------------
        let deadline = Instant::now() + Duration::from_secs(6);
        let min_per_node_per_endpoint = 8;

        loop {
            let a_r = a_radio_hits.load(Ordering::SeqCst);
            let a_s = a_sd_hits.load(Ordering::SeqCst);
            let b_r = b_radio_hits.load(Ordering::SeqCst);
            let b_s = b_sd_hits.load(Ordering::SeqCst);
            let c_r = c_radio_hits.load(Ordering::SeqCst);
            let c_s = c_sd_hits.load(Ordering::SeqCst);

            if a_r >= min_per_node_per_endpoint
                && a_s >= min_per_node_per_endpoint
                && b_r >= min_per_node_per_endpoint
                && b_s >= min_per_node_per_endpoint
                && c_r >= min_per_node_per_endpoint
                && c_s >= min_per_node_per_endpoint
            {
                break;
            }

            if Instant::now() > deadline {
                eprintln!(
                    "timeout hits: A(R={},S={}) B(R={},S={}) C(R={},S={})",
                    a_r, a_s, b_r, b_s, c_r, c_s
                );
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        // -------------------------------
        // 10) Stop + join
        // -------------------------------
        stop.store(true, Ordering::SeqCst);

        proc_a.join().unwrap();
        proc_b.join().unwrap();
        proc_c.join().unwrap();
        proc_hub.join().unwrap();
        relay_proc.join().unwrap();

        bus_a_handle.join().unwrap();
        bus_b_handle.join().unwrap();
        bus_c_handle.join().unwrap();

        // -------------------------------
        // 11) Assertions / invariants
        // -------------------------------
        let a_r = a_radio_hits.load(Ordering::SeqCst);
        let a_s = a_sd_hits.load(Ordering::SeqCst);
        let b_r = b_radio_hits.load(Ordering::SeqCst);
        let b_s = b_sd_hits.load(Ordering::SeqCst);
        let c_r = c_radio_hits.load(Ordering::SeqCst);
        let c_s = c_sd_hits.load(Ordering::SeqCst);

        assert!(
            a_r >= min_per_node_per_endpoint,
            "node A radio too low: {a_r}"
        );
        assert!(a_s >= min_per_node_per_endpoint, "node A sd too low: {a_s}");
        assert!(
            b_r >= min_per_node_per_endpoint,
            "node B radio too low: {b_r}"
        );
        assert!(b_s >= min_per_node_per_endpoint, "node B sd too low: {b_s}");
        assert!(
            c_r >= min_per_node_per_endpoint,
            "node C radio too low: {c_r}"
        );
        assert!(c_s >= min_per_node_per_endpoint, "node C sd too low: {c_s}");

        // Link-specific handler provenance is no longer exposed (sides are internal).
    }
}
