#[cfg(test)]
mod reliable_drop_tests {
    use sedsprintf_rs::TelemetryResult;
    use sedsprintf_rs::config::{DataEndpoint, DataType, RELIABLE_RETRANSMIT_MS};
    use sedsprintf_rs::discovery::build_discovery_announce;
    use sedsprintf_rs::packet::Packet;
    use sedsprintf_rs::relay::{Relay, RelaySideOptions};
    use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig, RouterSideOptions};
    use sedsprintf_rs::serialize;

    use std::collections::{BTreeSet, VecDeque};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    type SharedBusFrameQueue = Arc<Mutex<VecDeque<(usize, Vec<u8>)>>>;

    fn shared_clock(now: Arc<AtomicU64>) -> Box<dyn Clock + Send + Sync> {
        Box::new(move || now.load(Ordering::SeqCst))
    }

    fn drain_queue(q: &Arc<Mutex<VecDeque<Vec<u8>>>>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut guard = q.lock().expect("queue lock poisoned");
        while let Some(frame) = guard.pop_front() {
            out.push(frame);
        }
        out
    }

    struct RocketTopology {
        now: Arc<AtomicU64>,
        gs: Router,
        gw: Relay,
        rf: Relay,
        actuator_hits: Arc<Mutex<Vec<u32>>>,
        gs_gw_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        gw_gs_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        gs_rf_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        rf_gs_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        gw_ab_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        ab_gw_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        gw_vb_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        vb_gw_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        gw_daq_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        daq_gw_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        rf_pb_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        pb_rf_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        rf_fc_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        fc_rf_tx: Arc<Mutex<VecDeque<Vec<u8>>>>,
        gs_gw_side: usize,
        gs_rf_side: usize,
        gw_gs_side: usize,
        gw_ab_side: usize,
        gw_vb_side: usize,
        gw_daq_side: usize,
        rf_gs_side: usize,
        rf_pb_side: usize,
        rf_fc_side: usize,
        actuator: Router,
        actuator_side: usize,
        valve: Router,
        valve_side: usize,
        daq: Router,
        daq_side: usize,
        power: Router,
        power_side: usize,
        flight: Router,
        flight_side: usize,
    }

    impl RocketTopology {
        fn new() -> Self {
            let now = Arc::new(AtomicU64::new(0));
            let gs = Router::new_with_clock(
                RouterConfig::default().with_sender("GS"),
                shared_clock(now.clone()),
            );
            let gw = Relay::new(shared_clock(now.clone()));
            let rf = Relay::new(shared_clock(now.clone()));

            let actuator_hits: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
            let actuator_handler_hits = actuator_hits.clone();
            let actuator = Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::Radio,
                    move |pkt| {
                        let vals = pkt.data_as_f32()?;
                        if let Some(first) = vals.first() {
                            actuator_handler_hits.lock().unwrap().push(*first as u32);
                        }
                        Ok(())
                    },
                )])
                .with_sender("AB"),
                shared_clock(now.clone()),
            );
            let valve = Router::new_with_clock(
                RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                    DataEndpoint::SdCard,
                    |_pkt| Ok(()),
                )])
                .with_sender("VB"),
                shared_clock(now.clone()),
            );
            let daq = Router::new_with_clock(
                RouterConfig::default().with_sender("DAQ"),
                shared_clock(now.clone()),
            );
            let power = Router::new_with_clock(
                RouterConfig::default().with_sender("PB"),
                shared_clock(now.clone()),
            );
            let flight = Router::new_with_clock(
                RouterConfig::default().with_sender("FC"),
                shared_clock(now.clone()),
            );

            let gs_gw_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let gw_gs_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let gs_rf_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let rf_gs_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let gw_ab_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let ab_gw_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let gw_vb_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let vb_gw_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let gw_daq_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let daq_gw_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let rf_pb_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let pb_rf_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let rf_fc_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
            let fc_rf_tx: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));

            let side_opts = RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            };
            let relay_side_opts = RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            };

            let gs_gw_side = gs.add_side_serialized_with_options(
                "gw",
                {
                    let q = gs_gw_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                side_opts,
            );
            let gs_rf_side = gs.add_side_serialized_with_options(
                "rf",
                {
                    let q = gs_rf_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                side_opts,
            );

            let gw_gs_side = gw.add_side_serialized_with_options(
                "gs",
                {
                    let q = gw_gs_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                relay_side_opts,
            );
            let gw_ab_side = gw.add_side_serialized_with_options(
                "ab",
                {
                    let q = gw_ab_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                relay_side_opts,
            );
            let gw_vb_side = gw.add_side_serialized_with_options(
                "vb",
                {
                    let q = gw_vb_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                relay_side_opts,
            );
            let gw_daq_side = gw.add_side_serialized_with_options(
                "daq",
                {
                    let q = gw_daq_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                relay_side_opts,
            );

            let rf_gs_side = rf.add_side_serialized_with_options(
                "gs",
                {
                    let q = rf_gs_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                relay_side_opts,
            );
            let rf_pb_side = rf.add_side_serialized_with_options(
                "pb",
                {
                    let q = rf_pb_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                relay_side_opts,
            );
            let rf_fc_side = rf.add_side_serialized_with_options(
                "fc",
                {
                    let q = rf_fc_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                relay_side_opts,
            );

            let actuator_side = actuator.add_side_serialized_with_options(
                "gw",
                {
                    let q = ab_gw_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                side_opts,
            );
            let valve_side = valve.add_side_serialized_with_options(
                "gw",
                {
                    let q = vb_gw_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                side_opts,
            );
            let daq_side = daq.add_side_serialized_with_options(
                "gw",
                {
                    let q = daq_gw_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                side_opts,
            );
            let power_side = power.add_side_serialized_with_options(
                "rf",
                {
                    let q = pb_rf_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                side_opts,
            );
            let flight_side = flight.add_side_serialized_with_options(
                "rf",
                {
                    let q = fc_rf_tx.clone();
                    move |bytes: &[u8]| -> TelemetryResult<()> {
                        q.lock().unwrap().push_back(bytes.to_vec());
                        Ok(())
                    }
                },
                side_opts,
            );

            Self {
                now,
                gs,
                gw,
                rf,
                actuator_hits,
                gs_gw_tx,
                gw_gs_tx,
                gs_rf_tx,
                rf_gs_tx,
                gw_ab_tx,
                ab_gw_tx,
                gw_vb_tx,
                vb_gw_tx,
                gw_daq_tx,
                daq_gw_tx,
                rf_pb_tx,
                pb_rf_tx,
                rf_fc_tx,
                fc_rf_tx,
                gs_gw_side,
                gs_rf_side,
                gw_gs_side,
                gw_ab_side,
                gw_vb_side,
                gw_daq_side,
                rf_gs_side,
                rf_pb_side,
                rf_fc_side,
                actuator,
                actuator_side,
                valve,
                valve_side,
                daq,
                daq_side,
                power,
                power_side,
                flight,
                flight_side,
            }
        }

        fn pump_once(&self) {
            self.gs.process_all_queues_with_timeout(0).unwrap();
            self.gw.process_all_queues_with_timeout(0).unwrap();
            self.rf.process_all_queues_with_timeout(0).unwrap();
            self.actuator.process_all_queues_with_timeout(0).unwrap();
            self.valve.process_all_queues_with_timeout(0).unwrap();
            self.daq.process_all_queues_with_timeout(0).unwrap();
            self.power.process_all_queues_with_timeout(0).unwrap();
            self.flight.process_all_queues_with_timeout(0).unwrap();

            for frame in drain_queue(&self.gs_gw_tx) {
                self.gw
                    .rx_serialized_from_side(self.gw_gs_side, &frame)
                    .unwrap();
            }
            for frame in drain_queue(&self.gw_gs_tx) {
                self.gs
                    .rx_serialized_queue_from_side(&frame, self.gs_gw_side)
                    .unwrap();
            }
            for frame in drain_queue(&self.gs_rf_tx) {
                self.rf
                    .rx_serialized_from_side(self.rf_gs_side, &frame)
                    .unwrap();
            }
            for frame in drain_queue(&self.rf_gs_tx) {
                self.gs
                    .rx_serialized_queue_from_side(&frame, self.gs_rf_side)
                    .unwrap();
            }
            for frame in drain_queue(&self.gw_ab_tx) {
                self.actuator
                    .rx_serialized_queue_from_side(&frame, self.actuator_side)
                    .unwrap();
            }
            for frame in drain_queue(&self.ab_gw_tx) {
                self.gw
                    .rx_serialized_from_side(self.gw_ab_side, &frame)
                    .unwrap();
            }
            for frame in drain_queue(&self.gw_vb_tx) {
                self.valve
                    .rx_serialized_queue_from_side(&frame, self.valve_side)
                    .unwrap();
            }
            for frame in drain_queue(&self.vb_gw_tx) {
                self.gw
                    .rx_serialized_from_side(self.gw_vb_side, &frame)
                    .unwrap();
            }
            for frame in drain_queue(&self.gw_daq_tx) {
                self.daq
                    .rx_serialized_queue_from_side(&frame, self.daq_side)
                    .unwrap();
            }
            for frame in drain_queue(&self.daq_gw_tx) {
                self.gw
                    .rx_serialized_from_side(self.gw_daq_side, &frame)
                    .unwrap();
            }
            for frame in drain_queue(&self.rf_pb_tx) {
                self.power
                    .rx_serialized_queue_from_side(&frame, self.power_side)
                    .unwrap();
            }
            for frame in drain_queue(&self.pb_rf_tx) {
                self.rf
                    .rx_serialized_from_side(self.rf_pb_side, &frame)
                    .unwrap();
            }
            for frame in drain_queue(&self.rf_fc_tx) {
                self.flight
                    .rx_serialized_queue_from_side(&frame, self.flight_side)
                    .unwrap();
            }
            for frame in drain_queue(&self.fc_rf_tx) {
                self.rf
                    .rx_serialized_from_side(self.rf_fc_side, &frame)
                    .unwrap();
            }
        }

        fn advance(&self, delta_ms: u64) {
            self.now.fetch_add(delta_ms, Ordering::SeqCst);
        }

        fn settle(&self, rounds: usize) {
            for _ in 0..rounds {
                self.pump_once();
            }
        }
    }

    struct SharedBus {
        frames: SharedBusFrameQueue,
    }

    impl SharedBus {
        fn new() -> Self {
            Self {
                frames: Arc::new(Mutex::new(VecDeque::new())),
            }
        }

        fn tx_handler(
            &self,
            node_id: usize,
        ) -> impl Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static {
            let q = self.frames.clone();
            move |bytes: &[u8]| -> TelemetryResult<()> {
                q.lock().unwrap().push_back((node_id, bytes.to_vec()));
                Ok(())
            }
        }

        fn drain(&self) -> Vec<(usize, Vec<u8>)> {
            let mut out = Vec::new();
            let mut guard = self.frames.lock().unwrap();
            while let Some(frame) = guard.pop_front() {
                out.push(frame);
            }
            out
        }
    }

    #[test]
    fn reliable_link_recovers_from_dropped_frames() {
        let now = Arc::new(AtomicU64::new(0));

        let received: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let recv_sink = received.clone();
        let handler = EndpointHandler::new_packet_handler(DataEndpoint::Radio, move |pkt| {
            let vals = pkt.data_as_f32()?;
            if let Some(first) = vals.first() {
                recv_sink
                    .lock()
                    .expect("received lock poisoned")
                    .push(*first as u32);
            }
            Ok(())
        });

        let router_a = Router::new_with_clock(RouterConfig::default(), shared_clock(now.clone()));
        let router_b =
            Router::new_with_clock(RouterConfig::new(vec![handler]), shared_clock(now.clone()));

        let a_to_b: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let b_to_a: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));

        let a_to_b_tx = a_to_b.clone();
        let a_side = router_a.add_side_serialized_with_options(
            "link",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                a_to_b_tx
                    .lock()
                    .expect("a_to_b lock poisoned")
                    .push_back(bytes.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        let b_to_a_tx = b_to_a.clone();
        let b_side = router_b.add_side_serialized_with_options(
            "link",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                b_to_a_tx
                    .lock()
                    .expect("b_to_a lock poisoned")
                    .push_back(bytes.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        const TOTAL: u32 = 6;
        for i in 0..TOTAL {
            let pkt = Packet::from_f32_slice(
                DataType::GpsData,
                &[i as f32, 0.0, 0.0],
                &[DataEndpoint::Radio],
                i as u64,
            )
            .expect("failed to build packet");
            router_a.tx(pkt).expect("tx failed");
        }

        let mut dropped_data_once = false;
        let mut dropped_control_once = false;

        for _ in 0..200 {
            router_a
                .process_all_queues_with_timeout(0)
                .expect("router_a process failed");
            router_b
                .process_all_queues_with_timeout(0)
                .expect("router_b process failed");

            for frame in drain_queue(&a_to_b) {
                let info = serialize::peek_frame_info(&frame).expect("peek frame failed");
                if info.envelope.ty == DataType::GpsData
                    && !info.ack_only()
                    && let Some(hdr) = info.reliable
                    && hdr.seq == 1
                    && !dropped_data_once
                {
                    dropped_data_once = true;
                    continue; // drop first data frame for seq=1
                }
                router_b
                    .rx_serialized_queue_from_side(&frame, b_side)
                    .expect("router_b rx failed");
            }

            for frame in drain_queue(&b_to_a) {
                let info = serialize::peek_frame_info(&frame).expect("peek ack failed");
                if matches!(
                    info.envelope.ty,
                    DataType::ReliableAck | DataType::ReliablePacketRequest
                ) && !dropped_control_once
                {
                    let pkt = serialize::deserialize_packet(&frame).expect("decode control failed");
                    let vals = pkt.data_as_u32().expect("control payload decode failed");
                    if vals.first().copied() == Some(DataType::GpsData as u32) {
                        dropped_control_once = true;
                        continue; // drop first control packet for the reliable stream
                    }
                }
                router_a
                    .rx_serialized_queue_from_side(&frame, a_side)
                    .expect("router_a rx failed");
            }

            router_a
                .process_all_queues_with_timeout(0)
                .expect("router_a process failed");
            router_b
                .process_all_queues_with_timeout(0)
                .expect("router_b process failed");

            if received.lock().expect("received lock poisoned").len() == TOTAL as usize {
                break;
            }

            now.fetch_add(RELIABLE_RETRANSMIT_MS, Ordering::SeqCst);
        }

        let got = received.lock().expect("received lock poisoned").clone();
        let expected: Vec<u32> = (0..TOTAL).collect();

        assert!(dropped_data_once, "test did not drop a data frame");
        assert!(
            dropped_control_once,
            "test did not drop a reliable control frame"
        );
        assert_eq!(got, expected, "reliable delivery should recover from drops");
    }

    #[test]
    fn reliable_ordered_delivers_in_order() {
        let now = Arc::new(AtomicU64::new(0));

        let received: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let recv_sink = received.clone();
        let handler = EndpointHandler::new_packet_handler(DataEndpoint::Radio, move |pkt| {
            let vals = pkt.data_as_f32()?;
            if let Some(first) = vals.first() {
                recv_sink
                    .lock()
                    .expect("received lock poisoned")
                    .push(*first as u32);
            }
            Ok(())
        });

        let router =
            Router::new_with_clock(RouterConfig::new(vec![handler]), shared_clock(now.clone()));

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
            &[1.0_f32, 0.0, 0.0],
            &[DataEndpoint::Radio],
            1,
        )
        .expect("failed to build packet");
        let pkt2 = Packet::from_f32_slice(
            DataType::GpsData,
            &[2.0_f32, 0.0, 0.0],
            &[DataEndpoint::Radio],
            2,
        )
        .expect("failed to build packet");

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

        router
            .rx_serialized_from_side(seq2.as_ref(), side)
            .expect("rx seq2 failed");
        router
            .rx_serialized_from_side(seq1.as_ref(), side)
            .expect("rx seq1 failed");
        router
            .rx_serialized_from_side(seq2.as_ref(), side)
            .expect("rx seq2 retransmit failed");

        let got = received.lock().expect("received lock poisoned").clone();
        assert_eq!(got, vec![1, 2], "ordered reliable delivery must reorder");
    }

    #[test]
    fn end_to_end_reliable_ack_routes_back_without_flooding() {
        let now = Arc::new(AtomicU64::new(0));

        let delivered: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_c = delivered.clone();
        let handler = EndpointHandler::new_packet_handler(DataEndpoint::Radio, move |pkt| {
            let vals = pkt.data_as_f32()?;
            if let Some(first) = vals.first() {
                delivered_c
                    .lock()
                    .expect("delivered lock poisoned")
                    .push(*first as u32);
            }
            Ok(())
        });

        let source = Router::new_with_clock(RouterConfig::default(), shared_clock(now.clone()));
        let relay = Relay::new(shared_clock(now.clone()));
        let dest =
            Router::new_with_clock(RouterConfig::new(vec![handler]), shared_clock(now.clone()));

        let s_to_r: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_s: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_d: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let d_to_r: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_spur: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));

        let s_to_r_tx = s_to_r.clone();
        let source_side = source.add_side_serialized_with_options(
            "relay",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                s_to_r_tx
                    .lock()
                    .expect("s_to_r lock poisoned")
                    .push_back(bytes.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        let r_to_s_tx = r_to_s.clone();
        let relay_source_side = relay.add_side_serialized_with_options(
            "source",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                r_to_s_tx
                    .lock()
                    .expect("r_to_s lock poisoned")
                    .push_back(bytes.to_vec());
                Ok(())
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        let r_to_d_tx = r_to_d.clone();
        let relay_dest_side = relay.add_side_serialized_with_options(
            "dest",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                r_to_d_tx
                    .lock()
                    .expect("r_to_d lock poisoned")
                    .push_back(bytes.to_vec());
                Ok(())
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );

        let d_to_r_tx = d_to_r.clone();
        let dest_side = dest.add_side_serialized_with_options(
            "relay",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                d_to_r_tx
                    .lock()
                    .expect("d_to_r lock poisoned")
                    .push_back(bytes.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        let r_to_spur_tx = r_to_spur.clone();
        relay.add_side_serialized_with_options(
            "spur",
            move |bytes: &[u8]| -> TelemetryResult<()> {
                r_to_spur_tx
                    .lock()
                    .expect("r_to_spur lock poisoned")
                    .push_back(bytes.to_vec());
                Ok(())
            },
            RelaySideOptions::default(),
        );

        let discovery = build_discovery_announce("DEST", 0, &[DataEndpoint::Radio]).unwrap();
        relay.rx_from_side(relay_dest_side, discovery).unwrap();
        relay.process_all_queues().unwrap();
        let _ = drain_queue(&r_to_s);
        let _ = drain_queue(&r_to_spur);

        let pkt = Packet::from_f32_slice(
            DataType::GpsData,
            &[42.0, 0.0, 0.0],
            &[DataEndpoint::Radio],
            42,
        )
        .expect("failed to build packet");
        source.tx(pkt).expect("source tx failed");

        let mut dropped_end_to_end_ack = false;
        let mut forwarded_data_frames = 0usize;
        let mut spur_ack_frames = 0usize;

        for _ in 0..200 {
            source.process_all_queues_with_timeout(0).unwrap();
            relay.process_all_queues().unwrap();
            dest.process_all_queues_with_timeout(0).unwrap();

            for frame in drain_queue(&s_to_r) {
                let info = serialize::peek_frame_info(&frame).expect("source->relay peek failed");
                if info.envelope.ty == DataType::GpsData && !info.ack_only() {
                    forwarded_data_frames += 1;
                }
                relay
                    .rx_serialized_from_side(relay_source_side, &frame)
                    .unwrap();
            }

            for frame in drain_queue(&r_to_d) {
                dest.rx_serialized_queue_from_side(&frame, dest_side)
                    .unwrap();
            }

            for frame in drain_queue(&d_to_r) {
                let info = serialize::peek_frame_info(&frame).expect("dest->relay peek failed");
                if info.envelope.ty == DataType::ReliableAck {
                    let pkt = serialize::deserialize_packet(&frame).expect("decode ack failed");
                    if pkt.sender().starts_with("E2EACK:")
                        && pkt.payload().len() == 8
                        && !dropped_end_to_end_ack
                    {
                        dropped_end_to_end_ack = true;
                        continue;
                    }
                }
                relay
                    .rx_serialized_from_side(relay_dest_side, &frame)
                    .unwrap();
            }

            for frame in drain_queue(&r_to_s) {
                source
                    .rx_serialized_queue_from_side(&frame, source_side)
                    .unwrap();
            }

            for frame in drain_queue(&r_to_spur) {
                let info = serialize::peek_frame_info(&frame).expect("spur peek failed");
                if info.envelope.ty == DataType::ReliableAck {
                    spur_ack_frames += 1;
                }
            }

            source.process_all_queues_with_timeout(0).unwrap();
            relay.process_all_queues().unwrap();
            dest.process_all_queues_with_timeout(0).unwrap();

            if dropped_end_to_end_ack
                && forwarded_data_frames >= 2
                && delivered.lock().expect("delivered lock poisoned").len() == 1
            {
                break;
            }

            now.fetch_add(RELIABLE_RETRANSMIT_MS, Ordering::SeqCst);
        }

        assert!(
            dropped_end_to_end_ack,
            "test never dropped an end-to-end ACK"
        );
        assert!(
            forwarded_data_frames >= 2,
            "source should retransmit when the end-to-end ACK is lost"
        );
        assert_eq!(
            delivered
                .lock()
                .expect("delivered lock poisoned")
                .as_slice(),
            &[42],
            "destination should only consume the packet once"
        );
        assert_eq!(
            spur_ack_frames, 0,
            "reliable ACKs should not flood to unrelated sides"
        );
    }

    #[test]
    fn end_to_end_reliable_waits_for_all_discovered_holders() {
        let now = Arc::new(AtomicU64::new(0));
        let delivered_a: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_b: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));

        let handler_a = {
            let delivered = delivered_a.clone();
            EndpointHandler::new_packet_handler(DataEndpoint::Radio, move |pkt| {
                let vals = pkt.data_as_f32()?;
                if let Some(first) = vals.first() {
                    delivered.lock().unwrap().push(*first as u32);
                }
                Ok(())
            })
        };
        let handler_b = {
            let delivered = delivered_b.clone();
            EndpointHandler::new_packet_handler(DataEndpoint::Radio, move |pkt| {
                let vals = pkt.data_as_f32()?;
                if let Some(first) = vals.first() {
                    delivered.lock().unwrap().push(*first as u32);
                }
                Ok(())
            })
        };

        let source = Router::new_with_clock(
            RouterConfig::default().with_sender("SRC"),
            shared_clock(now.clone()),
        );
        let relay = Relay::new(shared_clock(now.clone()));
        let dest_a = Router::new_with_clock(
            RouterConfig::new(vec![handler_a]).with_sender("DEST_A"),
            shared_clock(now.clone()),
        );
        let dest_b = Router::new_with_clock(
            RouterConfig::new(vec![handler_b]).with_sender("DEST_B"),
            shared_clock(now.clone()),
        );

        let s_to_r: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_s: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_a: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let a_to_r: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_b: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let b_to_r: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));

        let source_side = source.add_side_serialized_with_options(
            "relay",
            {
                let q = s_to_r.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );
        let relay_source_side = relay.add_side_serialized_with_options(
            "source",
            {
                let q = r_to_s.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );
        let relay_a_side = relay.add_side_serialized_with_options(
            "dest_a",
            {
                let q = r_to_a.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );
        let dest_a_side = dest_a.add_side_serialized_with_options(
            "relay",
            {
                let q = a_to_r.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );
        let relay_b_side = relay.add_side_serialized_with_options(
            "dest_b",
            {
                let q = r_to_b.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );
        let dest_b_side = dest_b.add_side_serialized_with_options(
            "relay",
            {
                let q = b_to_r.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        relay
            .rx_from_side(
                relay_a_side,
                build_discovery_announce("DEST_A", 0, &[DataEndpoint::Radio]).unwrap(),
            )
            .unwrap();
        relay
            .rx_from_side(
                relay_b_side,
                build_discovery_announce("DEST_B", 0, &[DataEndpoint::Radio]).unwrap(),
            )
            .unwrap();
        relay.process_all_queues().unwrap();
        for frame in drain_queue(&r_to_s) {
            source
                .rx_serialized_queue_from_side(&frame, source_side)
                .unwrap();
        }
        source.process_all_queues_with_timeout(0).unwrap();

        source
            .tx(Packet::from_f32_slice(
                DataType::GpsData,
                &[7.0, 0.0, 0.0],
                &[DataEndpoint::Radio],
                7,
            )
            .unwrap())
            .unwrap();
        source.process_all_queues_with_timeout(0).unwrap();

        let mut dropped_b_ack = false;
        let mut forwarded_b_frames = 0usize;
        let mut a_ack_seen_by_relay = false;
        let mut forwarded_a_after_ack = 0usize;

        for _ in 0..200 {
            relay.process_all_queues().unwrap();
            dest_a.process_all_queues_with_timeout(0).unwrap();
            dest_b.process_all_queues_with_timeout(0).unwrap();

            for frame in drain_queue(&s_to_r) {
                relay
                    .rx_serialized_from_side(relay_source_side, &frame)
                    .unwrap();
            }

            for frame in drain_queue(&r_to_a) {
                let info = serialize::peek_frame_info(&frame).unwrap();
                if info.envelope.ty == DataType::GpsData && !info.ack_only() && a_ack_seen_by_relay
                {
                    forwarded_a_after_ack += 1;
                }
                dest_a
                    .rx_serialized_queue_from_side(&frame, dest_a_side)
                    .unwrap();
            }

            for frame in drain_queue(&r_to_b) {
                let info = serialize::peek_frame_info(&frame).unwrap();
                if info.envelope.ty == DataType::GpsData && !info.ack_only() {
                    forwarded_b_frames += 1;
                }
                dest_b
                    .rx_serialized_queue_from_side(&frame, dest_b_side)
                    .unwrap();
            }

            for frame in drain_queue(&a_to_r) {
                let info = serialize::peek_frame_info(&frame).unwrap();
                if info.envelope.ty == DataType::ReliableAck {
                    let pkt = serialize::deserialize_packet(&frame).unwrap();
                    if pkt.sender() == "E2EACK:DEST_A" {
                        a_ack_seen_by_relay = true;
                    }
                }
                relay.rx_serialized_from_side(relay_a_side, &frame).unwrap();
            }

            for frame in drain_queue(&b_to_r) {
                let info = serialize::peek_frame_info(&frame).unwrap();
                if info.envelope.ty == DataType::ReliableAck {
                    let pkt = serialize::deserialize_packet(&frame).unwrap();
                    if pkt.sender() == "E2EACK:DEST_B" && !dropped_b_ack {
                        dropped_b_ack = true;
                        continue;
                    }
                }
                relay.rx_serialized_from_side(relay_b_side, &frame).unwrap();
            }

            for frame in drain_queue(&r_to_s) {
                source
                    .rx_serialized_queue_from_side(&frame, source_side)
                    .unwrap();
            }

            source.process_all_queues_with_timeout(0).unwrap();
            relay.process_all_queues().unwrap();
            dest_a.process_all_queues_with_timeout(0).unwrap();
            dest_b.process_all_queues_with_timeout(0).unwrap();

            if dropped_b_ack
                && delivered_a.lock().unwrap().len() == 1
                && delivered_b.lock().unwrap().len() == 1
                && a_ack_seen_by_relay
                && forwarded_a_after_ack == 0
                && forwarded_b_frames >= 2
            {
                break;
            }

            now.fetch_add(RELIABLE_RETRANSMIT_MS, Ordering::SeqCst);
        }

        assert!(dropped_b_ack, "test never dropped DEST_B's end-to-end ACK");
        assert_eq!(delivered_a.lock().unwrap().as_slice(), &[7]);
        assert_eq!(delivered_b.lock().unwrap().as_slice(), &[7]);
        assert!(
            a_ack_seen_by_relay,
            "relay never observed DEST_A's end-to-end ACK"
        );
        assert_eq!(
            forwarded_a_after_ack, 0,
            "acknowledged destination should not receive more end-to-end retransmits once its ACK is known"
        );
        assert!(
            forwarded_b_frames >= 2,
            "unacknowledged destination should keep receiving end-to-end retransmits"
        );
    }

    #[test]
    fn reliable_rocket_topology_exports_missing_boards_and_reaches_actuator() {
        let topo = RocketTopology::new();

        topo.gs.announce_discovery().unwrap();
        topo.gw.announce_discovery().unwrap();
        topo.rf.announce_discovery().unwrap();
        topo.actuator.announce_discovery().unwrap();
        topo.valve.announce_discovery().unwrap();
        topo.daq.announce_discovery().unwrap();
        topo.power.announce_discovery().unwrap();
        topo.flight.announce_discovery().unwrap();

        for _ in 0..24 {
            topo.settle(1);
            topo.advance(25);
            topo.gs.poll_discovery().unwrap();
            topo.gw.poll_discovery().unwrap();
            topo.rf.poll_discovery().unwrap();
            topo.actuator.poll_discovery().unwrap();
            topo.valve.poll_discovery().unwrap();
            topo.daq.poll_discovery().unwrap();
            topo.power.poll_discovery().unwrap();
            topo.flight.poll_discovery().unwrap();
        }
        topo.settle(8);

        let gs_topology = topo.gs.export_topology();
        assert!(
            gs_topology
                .routers
                .iter()
                .any(|board| board.sender_id == "DAQ"),
            "DAQ should appear in GS topology export"
        );
        assert!(
            gs_topology
                .routers
                .iter()
                .any(|board| board.sender_id == "FC"),
            "FC should appear in GS topology export"
        );
        assert!(
            gs_topology
                .routers
                .iter()
                .any(|board| board.sender_id == "AB"),
            "AB should appear in GS topology export"
        );

        topo.gs
            .tx(Packet::from_f32_slice(
                DataType::GpsData,
                &[42.0, 1.0, 0.0],
                &[DataEndpoint::Radio],
                42,
            )
            .unwrap())
            .unwrap();

        for _ in 0..80 {
            topo.settle(1);
            if topo.actuator_hits.lock().unwrap().as_slice() == [42] {
                break;
            }
            topo.advance(RELIABLE_RETRANSMIT_MS / 2);
        }

        assert_eq!(topo.actuator_hits.lock().unwrap().as_slice(), &[42]);
    }

    #[test]
    fn reliable_multidrop_rocket_bus_reaches_actuator_and_exports_full_topology() {
        let now = Arc::new(AtomicU64::new(0));
        let trunk_bus = SharedBus::new();
        let gw_bus = SharedBus::new();
        let rf_bus = SharedBus::new();

        let actuator_hits: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let actuator_seen = actuator_hits.clone();

        let gs = Router::new_with_clock(
            RouterConfig::default().with_sender("GS"),
            shared_clock(now.clone()),
        );
        let gw = Relay::new(shared_clock(now.clone()));
        let rf = Relay::new(shared_clock(now.clone()));
        let actuator = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::Radio,
                move |pkt| {
                    let vals = pkt.data_as_f32()?;
                    if let Some(first) = vals.first() {
                        actuator_seen.lock().unwrap().push(*first as u32);
                    }
                    Ok(())
                },
            )])
            .with_sender("AB"),
            shared_clock(now.clone()),
        );
        let valve = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::SdCard,
                |_pkt| Ok(()),
            )])
            .with_sender("VB"),
            shared_clock(now.clone()),
        );
        let daq = Router::new_with_clock(
            RouterConfig::default().with_sender("DAQ"),
            shared_clock(now.clone()),
        );
        let power = Router::new_with_clock(
            RouterConfig::default().with_sender("PB"),
            shared_clock(now.clone()),
        );
        let flight = Router::new_with_clock(
            RouterConfig::default().with_sender("FC"),
            shared_clock(now.clone()),
        );

        let side_opts = RouterSideOptions {
            reliable_enabled: true,
            link_local_enabled: false,
            ..RouterSideOptions::default()
        };
        let relay_side_opts = RelaySideOptions {
            reliable_enabled: true,
            link_local_enabled: false,
            ..RelaySideOptions::default()
        };

        let gs_trunk =
            gs.add_side_serialized_with_options("trunk", trunk_bus.tx_handler(0), side_opts);
        let gw_trunk =
            gw.add_side_serialized_with_options("trunk", trunk_bus.tx_handler(1), relay_side_opts);
        let rf_trunk =
            rf.add_side_serialized_with_options("trunk", trunk_bus.tx_handler(2), relay_side_opts);

        let gw_child =
            gw.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(0), relay_side_opts);
        let actuator_side =
            actuator.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(1), side_opts);
        let valve_side =
            valve.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(2), side_opts);
        let daq_side =
            daq.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(3), side_opts);

        let rf_child =
            rf.add_side_serialized_with_options("rf_bus", rf_bus.tx_handler(0), relay_side_opts);
        let power_side =
            power.add_side_serialized_with_options("rf_bus", rf_bus.tx_handler(1), side_opts);
        let flight_side =
            flight.add_side_serialized_with_options("rf_bus", rf_bus.tx_handler(2), side_opts);

        gs.announce_discovery().unwrap();
        gw.announce_discovery().unwrap();
        rf.announce_discovery().unwrap();
        actuator.announce_discovery().unwrap();
        valve.announce_discovery().unwrap();
        daq.announce_discovery().unwrap();
        power.announce_discovery().unwrap();
        flight.announce_discovery().unwrap();

        for _ in 0..32 {
            gs.process_all_queues_with_timeout(0).unwrap();
            gw.process_all_queues_with_timeout(0).unwrap();
            rf.process_all_queues_with_timeout(0).unwrap();
            actuator.process_all_queues_with_timeout(0).unwrap();
            valve.process_all_queues_with_timeout(0).unwrap();
            daq.process_all_queues_with_timeout(0).unwrap();
            power.process_all_queues_with_timeout(0).unwrap();
            flight.process_all_queues_with_timeout(0).unwrap();

            for (src, frame) in trunk_bus.drain() {
                if src != 0 {
                    gs.rx_serialized_queue_from_side(&frame, gs_trunk).unwrap();
                }
                if src != 1 {
                    gw.rx_serialized_from_side(gw_trunk, &frame).unwrap();
                }
                if src != 2 {
                    rf.rx_serialized_from_side(rf_trunk, &frame).unwrap();
                }
            }

            for (src, frame) in gw_bus.drain() {
                if src != 0 {
                    gw.rx_serialized_from_side(gw_child, &frame).unwrap();
                }
                if src != 1 {
                    actuator
                        .rx_serialized_queue_from_side(&frame, actuator_side)
                        .unwrap();
                }
                if src != 2 {
                    valve
                        .rx_serialized_queue_from_side(&frame, valve_side)
                        .unwrap();
                }
                if src != 3 {
                    daq.rx_serialized_queue_from_side(&frame, daq_side).unwrap();
                }
            }

            for (src, frame) in rf_bus.drain() {
                if src != 0 {
                    rf.rx_serialized_from_side(rf_child, &frame).unwrap();
                }
                if src != 1 {
                    power
                        .rx_serialized_queue_from_side(&frame, power_side)
                        .unwrap();
                }
                if src != 2 {
                    flight
                        .rx_serialized_queue_from_side(&frame, flight_side)
                        .unwrap();
                }
            }

            now.fetch_add(25, Ordering::SeqCst);
            gs.poll_discovery().unwrap();
            gw.poll_discovery().unwrap();
            rf.poll_discovery().unwrap();
            actuator.poll_discovery().unwrap();
            valve.poll_discovery().unwrap();
            daq.poll_discovery().unwrap();
            power.poll_discovery().unwrap();
            flight.poll_discovery().unwrap();
        }

        let gs_topology = gs.export_topology();
        assert!(
            gs_topology
                .routers
                .iter()
                .any(|board| board.sender_id == "DAQ"),
            "DAQ should appear in GS topology export"
        );
        assert!(
            gs_topology
                .routers
                .iter()
                .any(|board| board.sender_id == "FC"),
            "FC should appear in GS topology export"
        );

        gs.tx(Packet::from_f32_slice(
            DataType::GpsData,
            &[99.0, 0.0, 0.0],
            &[DataEndpoint::Radio],
            99,
        )
        .unwrap())
            .unwrap();

        for _ in 0..96 {
            gs.process_all_queues_with_timeout(0).unwrap();
            gw.process_all_queues_with_timeout(0).unwrap();
            rf.process_all_queues_with_timeout(0).unwrap();
            actuator.process_all_queues_with_timeout(0).unwrap();
            valve.process_all_queues_with_timeout(0).unwrap();
            daq.process_all_queues_with_timeout(0).unwrap();
            power.process_all_queues_with_timeout(0).unwrap();
            flight.process_all_queues_with_timeout(0).unwrap();

            for (src, frame) in trunk_bus.drain() {
                if src != 0 {
                    gs.rx_serialized_queue_from_side(&frame, gs_trunk).unwrap();
                }
                if src != 1 {
                    gw.rx_serialized_from_side(gw_trunk, &frame).unwrap();
                }
                if src != 2 {
                    rf.rx_serialized_from_side(rf_trunk, &frame).unwrap();
                }
            }
            for (src, frame) in gw_bus.drain() {
                if src != 0 {
                    gw.rx_serialized_from_side(gw_child, &frame).unwrap();
                }
                if src != 1 {
                    actuator
                        .rx_serialized_queue_from_side(&frame, actuator_side)
                        .unwrap();
                }
                if src != 2 {
                    valve
                        .rx_serialized_queue_from_side(&frame, valve_side)
                        .unwrap();
                }
                if src != 3 {
                    daq.rx_serialized_queue_from_side(&frame, daq_side).unwrap();
                }
            }
            for (src, frame) in rf_bus.drain() {
                if src != 0 {
                    rf.rx_serialized_from_side(rf_child, &frame).unwrap();
                }
                if src != 1 {
                    power
                        .rx_serialized_queue_from_side(&frame, power_side)
                        .unwrap();
                }
                if src != 2 {
                    flight
                        .rx_serialized_queue_from_side(&frame, flight_side)
                        .unwrap();
                }
            }

            if actuator_hits.lock().unwrap().as_slice() == [99] {
                break;
            }

            now.fetch_add(RELIABLE_RETRANSMIT_MS / 2, Ordering::SeqCst);
        }

        assert_eq!(actuator_hits.lock().unwrap().as_slice(), &[99]);
    }

    #[test]
    fn reliable_multidrop_bus_retries_until_missed_listener_receives_frame() {
        let now = Arc::new(AtomicU64::new(0));
        let gw_bus = SharedBus::new();
        let src_to_gw: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let gw_to_src: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));

        let actuator_hits: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let actuator_seen = actuator_hits.clone();

        let source = Router::new_with_clock(
            RouterConfig::default().with_sender("GS"),
            shared_clock(now.clone()),
        );
        let gateway = Relay::new(shared_clock(now.clone()));
        let actuator = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::Radio,
                move |pkt| {
                    let vals = pkt.data_as_f32()?;
                    if let Some(first) = vals.first() {
                        actuator_seen.lock().unwrap().push(*first as u32);
                    }
                    Ok(())
                },
            )])
            .with_sender("AB"),
            shared_clock(now.clone()),
        );
        let valve = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::SdCard,
                |_pkt| Ok(()),
            )])
            .with_sender("VB"),
            shared_clock(now.clone()),
        );
        let daq = Router::new_with_clock(
            RouterConfig::default().with_sender("DAQ"),
            shared_clock(now.clone()),
        );

        let relay_side_opts = RelaySideOptions {
            reliable_enabled: true,
            link_local_enabled: false,
            ..RelaySideOptions::default()
        };
        let side_opts = RouterSideOptions {
            reliable_enabled: true,
            link_local_enabled: false,
            ..RouterSideOptions::default()
        };

        let source_uplink = source.add_side_serialized_with_options(
            "gw",
            {
                let q = src_to_gw.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            side_opts,
        );
        let uplink = gateway.add_side_serialized_with_options(
            "uplink",
            {
                let q = gw_to_src.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            relay_side_opts,
        );
        let gw_child = gateway.add_side_serialized_with_options(
            "gw_bus",
            gw_bus.tx_handler(0),
            relay_side_opts,
        );
        let actuator_side =
            actuator.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(1), side_opts);
        let valve_side =
            valve.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(2), side_opts);
        let daq_side =
            daq.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(3), side_opts);

        gateway
            .rx_from_side(
                gw_child,
                build_discovery_announce("AB", 0, &[DataEndpoint::Radio]).unwrap(),
            )
            .unwrap();
        gateway
            .rx_from_side(
                gw_child,
                build_discovery_announce("VB", 0, &[DataEndpoint::SdCard]).unwrap(),
            )
            .unwrap();
        gateway
            .rx_from_side(gw_child, build_discovery_announce("DAQ", 0, &[]).unwrap())
            .unwrap();
        for _ in 0..6 {
            gateway.process_all_queues().unwrap();
            gateway.announce_discovery().unwrap();
            gateway.process_all_queues().unwrap();
            for frame in drain_queue(&gw_to_src) {
                source
                    .rx_serialized_queue_from_side(&frame, source_uplink)
                    .unwrap();
            }
            source.process_all_queues_with_timeout(0).unwrap();
            now.fetch_add(25, Ordering::SeqCst);
        }
        assert!(
            source
                .export_topology()
                .routers
                .iter()
                .any(|board| board.sender_id == "AB"),
            "source should learn that AB is reachable behind GW before the command is sent"
        );

        source
            .tx(Packet::from_f32_slice(
                DataType::GpsData,
                &[7.0, 0.0, 0.0],
                &[DataEndpoint::Radio],
                7,
            )
            .unwrap())
            .unwrap();

        let mut dropped_actuator_first_delivery = false;
        let mut source_to_gateway_data_frames = 0usize;
        let mut gateway_to_bus_data_frames = 0usize;
        let mut source_to_gateway_seqs = BTreeSet::new();
        for _ in 0..96 {
            source.process_all_queues_with_timeout(0).unwrap();
            gateway.process_all_queues().unwrap();
            actuator.process_all_queues_with_timeout(0).unwrap();
            valve.process_all_queues_with_timeout(0).unwrap();
            daq.process_all_queues_with_timeout(0).unwrap();

            for frame in drain_queue(&src_to_gw) {
                let info = serialize::peek_frame_info(&frame).unwrap();
                if info.envelope.ty == DataType::GpsData && !info.ack_only() {
                    source_to_gateway_data_frames += 1;
                    if let Some(hdr) = info.reliable {
                        source_to_gateway_seqs.insert(hdr.seq);
                    }
                }
                gateway.rx_serialized_from_side(uplink, &frame).unwrap();
            }
            for frame in drain_queue(&gw_to_src) {
                source
                    .rx_serialized_queue_from_side(&frame, source_uplink)
                    .unwrap();
            }

            for (src, frame) in gw_bus.drain() {
                let info = serialize::peek_frame_info(&frame).unwrap();
                if src == 0 && info.envelope.ty == DataType::GpsData && !info.ack_only() {
                    gateway_to_bus_data_frames += 1;
                }
                if src != 0 {
                    gateway.rx_serialized_from_side(gw_child, &frame).unwrap();
                }

                let drop_for_actuator = src == 0
                    && info.envelope.ty == DataType::GpsData
                    && !info.ack_only()
                    && !dropped_actuator_first_delivery;

                if src != 1 && !drop_for_actuator {
                    actuator
                        .rx_serialized_queue_from_side(&frame, actuator_side)
                        .unwrap();
                }
                if src != 2 {
                    valve
                        .rx_serialized_queue_from_side(&frame, valve_side)
                        .unwrap();
                }
                if src != 3 {
                    daq.rx_serialized_queue_from_side(&frame, daq_side).unwrap();
                }
                if drop_for_actuator {
                    dropped_actuator_first_delivery = true;
                }
            }

            actuator.process_all_queues_with_timeout(0).unwrap();
            valve.process_all_queues_with_timeout(0).unwrap();
            daq.process_all_queues_with_timeout(0).unwrap();

            for (src, frame) in gw_bus.drain() {
                if src != 0 {
                    gateway.rx_serialized_from_side(gw_child, &frame).unwrap();
                }
                if src != 1 {
                    actuator
                        .rx_serialized_queue_from_side(&frame, actuator_side)
                        .unwrap();
                }
                if src != 2 {
                    valve
                        .rx_serialized_queue_from_side(&frame, valve_side)
                        .unwrap();
                }
                if src != 3 {
                    daq.rx_serialized_queue_from_side(&frame, daq_side).unwrap();
                }
            }

            gateway.process_all_queues().unwrap();
            source.process_all_queues_with_timeout(0).unwrap();

            for frame in drain_queue(&gw_to_src) {
                source
                    .rx_serialized_queue_from_side(&frame, source_uplink)
                    .unwrap();
            }
            source.process_all_queues_with_timeout(0).unwrap();

            if actuator_hits.lock().unwrap().as_slice() == [7] {
                break;
            }

            now.fetch_add(RELIABLE_RETRANSMIT_MS, Ordering::SeqCst);
        }

        assert!(
            dropped_actuator_first_delivery,
            "test never dropped the first actuator-bound frame"
        );
        assert_eq!(
            actuator_hits.lock().unwrap().as_slice(),
            &[7],
            "shared-bus reliable forwarding should retry until the missed listener receives the frame; src->gw data frames={source_to_gateway_data_frames}, src->gw seqs={source_to_gateway_seqs:?}, gw->bus data frames={gateway_to_bus_data_frames}"
        );
    }

    #[test]
    fn reliable_multidrop_forwarding_disables_hop_reliable_on_shared_side() {
        let now = Arc::new(AtomicU64::new(0));
        let gw_bus = SharedBus::new();
        let src_to_gw: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let gw_to_src: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));

        let actuator_hits: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let actuator_seen = actuator_hits.clone();

        let source = Router::new_with_clock(
            RouterConfig::default().with_sender("GS"),
            shared_clock(now.clone()),
        );
        let gateway = Relay::new(shared_clock(now.clone()));
        let actuator = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::Radio,
                move |pkt| {
                    let vals = pkt.data_as_f32()?;
                    if let Some(first) = vals.first() {
                        actuator_seen.lock().unwrap().push(*first as u32);
                    }
                    Ok(())
                },
            )])
            .with_sender("AB"),
            shared_clock(now.clone()),
        );
        let valve = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::SdCard,
                |_pkt| Ok(()),
            )])
            .with_sender("VB"),
            shared_clock(now.clone()),
        );
        let daq = Router::new_with_clock(
            RouterConfig::default().with_sender("DAQ"),
            shared_clock(now.clone()),
        );

        let relay_side_opts = RelaySideOptions {
            reliable_enabled: true,
            link_local_enabled: false,
            ..RelaySideOptions::default()
        };
        let side_opts = RouterSideOptions {
            reliable_enabled: true,
            link_local_enabled: false,
            ..RouterSideOptions::default()
        };

        let source_uplink = source.add_side_serialized_with_options(
            "gw",
            {
                let q = src_to_gw.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            side_opts,
        );
        let uplink = gateway.add_side_serialized_with_options(
            "uplink",
            {
                let q = gw_to_src.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            relay_side_opts,
        );
        let gw_child = gateway.add_side_serialized_with_options(
            "gw_bus",
            gw_bus.tx_handler(0),
            relay_side_opts,
        );
        let actuator_side =
            actuator.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(1), side_opts);
        let valve_side =
            valve.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(2), side_opts);
        let daq_side =
            daq.add_side_serialized_with_options("gw_bus", gw_bus.tx_handler(3), side_opts);

        gateway
            .rx_from_side(
                gw_child,
                build_discovery_announce("AB", 0, &[DataEndpoint::Radio]).unwrap(),
            )
            .unwrap();
        gateway
            .rx_from_side(
                gw_child,
                build_discovery_announce("VB", 0, &[DataEndpoint::SdCard]).unwrap(),
            )
            .unwrap();
        gateway
            .rx_from_side(gw_child, build_discovery_announce("DAQ", 0, &[]).unwrap())
            .unwrap();
        for _ in 0..6 {
            gateway.process_all_queues().unwrap();
            gateway.announce_discovery().unwrap();
            gateway.process_all_queues().unwrap();
            for frame in drain_queue(&gw_to_src) {
                source
                    .rx_serialized_queue_from_side(&frame, source_uplink)
                    .unwrap();
            }
            source.process_all_queues_with_timeout(0).unwrap();
            now.fetch_add(25, Ordering::SeqCst);
        }
        assert!(
            source
                .export_topology()
                .routers
                .iter()
                .any(|board| board.sender_id == "AB"),
            "source should learn that AB is reachable behind GW before the command is sent"
        );

        source
            .tx(Packet::from_f32_slice(
                DataType::GpsData,
                &[11.0, 0.0, 0.0],
                &[DataEndpoint::Radio],
                11,
            )
            .unwrap())
            .unwrap();

        let mut saw_reliable_source_frame = false;
        let mut saw_shared_side_data_frame = false;
        let mut shared_side_frame_flags = None;
        let mut saw_shared_side_control_frame = false;
        for _ in 0..24 {
            source.process_all_queues_with_timeout(0).unwrap();
            gateway.process_all_queues().unwrap();
            actuator.process_all_queues_with_timeout(0).unwrap();
            valve.process_all_queues_with_timeout(0).unwrap();
            daq.process_all_queues_with_timeout(0).unwrap();

            for frame in drain_queue(&src_to_gw) {
                let info = serialize::peek_frame_info(&frame).unwrap();
                if info.envelope.ty == DataType::GpsData && !info.ack_only() {
                    saw_reliable_source_frame = info.reliable.is_some();
                }
                gateway.rx_serialized_from_side(uplink, &frame).unwrap();
            }

            for (src, frame) in gw_bus.drain() {
                let info = serialize::peek_frame_info(&frame).unwrap();
                if src == 0
                    && matches!(
                        info.envelope.ty,
                        DataType::ReliableAck | DataType::ReliablePacketRequest
                    )
                {
                    saw_shared_side_control_frame = true;
                }
                if src == 0 && info.envelope.ty == DataType::GpsData && !info.ack_only() {
                    saw_shared_side_data_frame = true;
                    shared_side_frame_flags = info.reliable.map(|hdr| hdr.flags);
                }
                if src != 0 {
                    gateway.rx_serialized_from_side(gw_child, &frame).unwrap();
                }
                if src != 1 {
                    actuator
                        .rx_serialized_queue_from_side(&frame, actuator_side)
                        .unwrap();
                }
                if src != 2 {
                    valve
                        .rx_serialized_queue_from_side(&frame, valve_side)
                        .unwrap();
                }
                if src != 3 {
                    daq.rx_serialized_queue_from_side(&frame, daq_side).unwrap();
                }
            }

            actuator.process_all_queues_with_timeout(0).unwrap();
            valve.process_all_queues_with_timeout(0).unwrap();
            daq.process_all_queues_with_timeout(0).unwrap();

            if actuator_hits.lock().unwrap().as_slice() == [11] {
                break;
            }

            now.fetch_add(RELIABLE_RETRANSMIT_MS / 2, Ordering::SeqCst);
        }

        assert!(
            saw_reliable_source_frame,
            "source->gateway hop should still use reliable framing"
        );
        assert!(
            saw_shared_side_data_frame,
            "gateway never forwarded the application packet onto the shared child side"
        );
        assert!(
            !saw_shared_side_control_frame,
            "gateway should not emit hop-level reliable control frames onto a shared serialized side with multiple announcers"
        );
        assert_eq!(
            shared_side_frame_flags,
            Some(serialize::RELIABLE_FLAG_UNSEQUENCED),
            "gateway should rewrite the forwarded shared-side application frame as unsequenced instead of using hop-level ACK sequencing"
        );
        assert_eq!(actuator_hits.lock().unwrap().as_slice(), &[11]);
    }

    #[test]
    fn mixed_first_hop_reliable_modes_still_reach_final_node() {
        let now = Arc::new(AtomicU64::new(0));
        let delivered: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_sink = delivered.clone();

        let source = Router::new_with_clock(
            RouterConfig::default().with_sender("GS"),
            shared_clock(now.clone()),
        );
        let relay = Relay::new(shared_clock(now.clone()));
        let dest = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::Radio,
                move |pkt| {
                    let vals = pkt.data_as_f32()?;
                    if let Some(first) = vals.first() {
                        delivered_sink.lock().unwrap().push(*first as u32);
                    }
                    Ok(())
                },
            )])
            .with_sender("AB"),
            shared_clock(now.clone()),
        );

        let s_to_r: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_s: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_d: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let d_to_r: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));

        let source_side = source.add_side_serialized_with_options(
            "relay",
            {
                let q = s_to_r.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );
        let relay_source_side = relay.add_side_serialized_with_options(
            "source_old_fw",
            {
                let q = r_to_s.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RelaySideOptions {
                reliable_enabled: false,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );
        let relay_dest_side = relay.add_side_serialized_with_options(
            "dest",
            {
                let q = r_to_d.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );
        let dest_side = dest.add_side_serialized_with_options(
            "relay",
            {
                let q = d_to_r.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        relay
            .rx_from_side(
                relay_dest_side,
                build_discovery_announce("AB", 0, &[DataEndpoint::Radio]).unwrap(),
            )
            .unwrap();
        relay.announce_discovery().unwrap();
        relay.process_all_queues().unwrap();
        for frame in drain_queue(&r_to_s) {
            source
                .rx_serialized_queue_from_side(&frame, source_side)
                .unwrap();
        }
        source.process_all_queues_with_timeout(0).unwrap();

        source
            .tx(Packet::from_f32_slice(
                DataType::GpsData,
                &[21.0, 0.0, 0.0],
                &[DataEndpoint::Radio],
                21,
            )
            .unwrap())
            .unwrap();

        for _ in 0..32 {
            source.process_all_queues_with_timeout(0).unwrap();
            relay.process_all_queues().unwrap();
            dest.process_all_queues_with_timeout(0).unwrap();

            for frame in drain_queue(&s_to_r) {
                relay
                    .rx_serialized_from_side(relay_source_side, &frame)
                    .unwrap();
            }
            for frame in drain_queue(&r_to_d) {
                dest.rx_serialized_queue_from_side(&frame, dest_side)
                    .unwrap();
            }
            for frame in drain_queue(&d_to_r) {
                relay
                    .rx_serialized_from_side(relay_dest_side, &frame)
                    .unwrap();
            }
            for frame in drain_queue(&r_to_s) {
                source
                    .rx_serialized_queue_from_side(&frame, source_side)
                    .unwrap();
            }

            if delivered.lock().unwrap().as_slice() == [21] {
                break;
            }
            now.fetch_add(RELIABLE_RETRANSMIT_MS, Ordering::SeqCst);
        }

        assert_eq!(
            delivered.lock().unwrap().as_slice(),
            &[21],
            "mixed reliable/non-reliable first hop should still deliver the command"
        );
    }

    #[test]
    fn delayed_intermediate_rx_processing_only_delays_reliable_delivery() {
        let now = Arc::new(AtomicU64::new(0));
        let delivered: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_sink = delivered.clone();

        let source = Router::new_with_clock(
            RouterConfig::default().with_sender("GS"),
            shared_clock(now.clone()),
        );
        let relay = Relay::new(shared_clock(now.clone()));
        let dest = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::Radio,
                move |pkt| {
                    let vals = pkt.data_as_f32()?;
                    if let Some(first) = vals.first() {
                        delivered_sink.lock().unwrap().push(*first as u32);
                    }
                    Ok(())
                },
            )])
            .with_sender("AB"),
            shared_clock(now.clone()),
        );

        let s_to_r: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_s: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let r_to_d: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let d_to_r: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));

        let source_side = source.add_side_serialized_with_options(
            "relay",
            {
                let q = s_to_r.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );
        let relay_source_side = relay.add_side_serialized_with_options(
            "source",
            {
                let q = r_to_s.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );
        let relay_dest_side = relay.add_side_serialized_with_options(
            "dest",
            {
                let q = r_to_d.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RelaySideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RelaySideOptions::default()
            },
        );
        let dest_side = dest.add_side_serialized_with_options(
            "relay",
            {
                let q = d_to_r.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        relay
            .rx_from_side(
                relay_dest_side,
                build_discovery_announce("AB", 0, &[DataEndpoint::Radio]).unwrap(),
            )
            .unwrap();
        relay.announce_discovery().unwrap();
        relay.process_all_queues().unwrap();
        for frame in drain_queue(&r_to_s) {
            source
                .rx_serialized_queue_from_side(&frame, source_side)
                .unwrap();
        }
        source.process_all_queues_with_timeout(0).unwrap();

        source
            .tx(Packet::from_f32_slice(
                DataType::GpsData,
                &[33.0, 0.0, 0.0],
                &[DataEndpoint::Radio],
                33,
            )
            .unwrap())
            .unwrap();

        let mut delayed_first_hop_frames = Vec::new();
        for tick in 0..40 {
            source.process_all_queues_with_timeout(0).unwrap();
            relay.process_all_queues().unwrap();
            dest.process_all_queues_with_timeout(0).unwrap();

            delayed_first_hop_frames.extend(drain_queue(&s_to_r));

            if tick >= 4 {
                for frame in delayed_first_hop_frames.drain(..) {
                    relay
                        .rx_serialized_from_side(relay_source_side, &frame)
                        .unwrap();
                }
            }

            for frame in drain_queue(&r_to_d) {
                dest.rx_serialized_queue_from_side(&frame, dest_side)
                    .unwrap();
            }
            for frame in drain_queue(&d_to_r) {
                relay
                    .rx_serialized_from_side(relay_dest_side, &frame)
                    .unwrap();
            }
            for frame in drain_queue(&r_to_s) {
                source
                    .rx_serialized_queue_from_side(&frame, source_side)
                    .unwrap();
            }

            if delivered.lock().unwrap().as_slice() == [33] {
                break;
            }
            now.fetch_add(RELIABLE_RETRANSMIT_MS, Ordering::SeqCst);
        }

        assert_eq!(
            delivered.lock().unwrap().as_slice(),
            &[33],
            "slow intermediate RX processing should delay but not prevent delivery"
        );
    }

    #[test]
    fn reflected_duplicate_ordered_frames_do_not_confuse_final_receiver() {
        let now = Arc::new(AtomicU64::new(0));
        let received: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let recv_sink = received.clone();

        let receiver = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::Radio,
                move |pkt| {
                    let vals = pkt.data_as_f32()?;
                    if let Some(first) = vals.first() {
                        recv_sink.lock().unwrap().push(*first as u32);
                    }
                    Ok(())
                },
            )])
            .with_sender("AB"),
            shared_clock(now.clone()),
        );
        let echoed_frames: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let side = receiver.add_side_serialized_with_options(
            "echoed_bus",
            {
                let q = echoed_frames.clone();
                move |bytes: &[u8]| -> TelemetryResult<()> {
                    q.lock().unwrap().push_back(bytes.to_vec());
                    Ok(())
                }
            },
            RouterSideOptions {
                reliable_enabled: true,
                link_local_enabled: false,
                ..RouterSideOptions::default()
            },
        );

        let pkt1 = Packet::from_f32_slice(
            DataType::GpsData,
            &[1.0, 0.0, 0.0],
            &[DataEndpoint::Radio],
            1,
        )
        .unwrap();
        let pkt2 = Packet::from_f32_slice(
            DataType::GpsData,
            &[2.0, 0.0, 0.0],
            &[DataEndpoint::Radio],
            2,
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

        receiver.rx_serialized_queue_from_side(&seq1, side).unwrap();
        receiver.process_all_queues_with_timeout(0).unwrap();
        receiver.rx_serialized_queue_from_side(&seq1, side).unwrap();
        receiver.process_all_queues_with_timeout(0).unwrap();
        receiver.rx_serialized_queue_from_side(&seq2, side).unwrap();
        receiver.process_all_queues_with_timeout(0).unwrap();
        receiver.rx_serialized_queue_from_side(&seq1, side).unwrap();
        receiver.process_all_queues_with_timeout(0).unwrap();
        receiver.rx_serialized_queue_from_side(&seq2, side).unwrap();
        receiver.process_all_queues_with_timeout(0).unwrap();

        assert_eq!(
            received.lock().unwrap().as_slice(),
            &[1, 2],
            "reflected duplicates should not break ordered reliable delivery"
        );
        assert!(
            !drain_queue(&echoed_frames).is_empty(),
            "receiver should still emit ACK/control traffic for the ordered stream"
        );
    }
}
