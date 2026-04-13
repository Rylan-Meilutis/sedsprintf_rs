#[cfg(test)]
mod reliable_drop_tests {
    use sedsprintf_rs::config::{DataEndpoint, DataType, RELIABLE_RETRANSMIT_MS};
    use sedsprintf_rs::packet::Packet;
    use sedsprintf_rs::router::{
        Clock, EndpointHandler, Router, RouterConfig, RouterMode, RouterSideOptions,
    };
    use sedsprintf_rs::serialize;
    use sedsprintf_rs::TelemetryResult;

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

        let router_a = Router::new_with_clock(
            RouterMode::Sink,
            RouterConfig::default(),
            shared_clock(now.clone()),
        );
        let router_b = Router::new_with_clock(
            RouterMode::Sink,
            RouterConfig::new(vec![handler]),
            shared_clock(now.clone()),
        );

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
        assert!(dropped_control_once, "test did not drop a reliable control frame");
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

        let router = Router::new_with_clock(
            RouterMode::Sink,
            RouterConfig::new(vec![handler]),
            shared_clock(now.clone()),
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
}
