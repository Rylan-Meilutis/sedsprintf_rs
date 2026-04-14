#[cfg(test)]
mod reliable_drop_tests {
    use sedsprintf_rs::TelemetryResult;
    use sedsprintf_rs::config::{DataEndpoint, DataType, RELIABLE_RETRANSMIT_MS};
    use sedsprintf_rs::discovery::build_discovery_announce;
    use sedsprintf_rs::packet::Packet;
    use sedsprintf_rs::relay::{Relay, RelaySideOptions};
    use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig, RouterSideOptions};
    use sedsprintf_rs::serialize;

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

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
}
