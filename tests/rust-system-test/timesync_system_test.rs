#[cfg(feature = "timesync")]
mod timesync_system_test {
    use sedsprintf_rs::config::{DEVICE_IDENTIFIER, DataEndpoint, DataType};
    use sedsprintf_rs::packet::Packet;
    use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};
    use sedsprintf_rs::serialize;
    use sedsprintf_rs::timesync::{
        PartialNetworkTime, TimeSyncConfig, TimeSyncRole, TimeSyncTracker,
        build_timesync_announce_with_sender, build_timesync_request, build_timesync_response,
        compute_offset_delay,
    };

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    struct StepClock {
        now_ns: AtomicU64,
        step_ns: u64,
    }

    impl Clock for StepClock {
        fn now_ms(&self) -> u64 {
            self.now_ns.fetch_add(self.step_ns, Ordering::SeqCst) / 1_000_000
        }

        fn now_ns(&self) -> u64 {
            self.now_ns.fetch_add(self.step_ns, Ordering::SeqCst)
        }
    }

    fn zero_clock() -> Box<dyn Clock + Send + Sync> {
        Box::new(|| 0u64)
    }

    fn shared_clock(now: Arc<AtomicU64>) -> Box<dyn Clock + Send + Sync> {
        Box::new(move || now.load(Ordering::SeqCst))
    }

    #[test]
    fn timesync_offset_delay_and_timestamp_update() {
        let req = build_timesync_request(1, 1_000).unwrap();
        let resp = build_timesync_response(1, 1_000, 1_010, 1_020).unwrap();
        let t4_ms = 1_030;
        let sample = compute_offset_delay(1_000, 1_010, 1_020, t4_ms);

        assert_eq!(sample.offset_ms, 0);
        assert_eq!(sample.delay_ms, 20);

        let captured = Arc::new(Mutex::new(None));
        let captured_c = captured.clone();
        let router = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint::SdCard,
                |_pkt| Ok(()),
            )]),
            zero_clock(),
        );
        router.add_side_packet("CAP", move |pkt| {
            *captured_c.lock().unwrap() = Some(pkt.timestamp());
            Ok(())
        });
        let offset_ts = (t4_ms as i64 + sample.offset_ms) as u64;
        router
            .log_ts(
                DataType::TimeSyncRequest,
                offset_ts,
                &req.data_as_u64().unwrap(),
            )
            .unwrap();

        let got = captured.lock().unwrap().expect("no timestamp captured");
        assert_eq!(got, offset_ts);

        let _ = resp;
    }

    #[test]
    fn timesync_failover_selects_next_source() {
        let mut tracker = TimeSyncTracker::new(TimeSyncConfig {
            role: TimeSyncRole::Auto,
            priority: 50,
            source_timeout_ms: 1_000,
            ..Default::default()
        });

        let pkt_a = build_timesync_announce_with_sender("SRC_A", 10, 5_000).unwrap();
        let pkt_b = build_timesync_announce_with_sender("SRC_B", 20, 5_000).unwrap();

        tracker.handle_announce(&pkt_a, 5_000).unwrap();
        tracker.handle_announce(&pkt_b, 5_000).unwrap();
        assert_eq!(tracker.current_source().unwrap().sender, "SRC_A");
        assert!(!tracker.should_announce(5_000, true));

        tracker.refresh(6_500);
        assert!(tracker.current_source().is_none());
        assert!(tracker.should_announce(6_500, true));

        let pkt_b_late = build_timesync_announce_with_sender("SRC_B", 20, 6_500).unwrap();
        tracker.handle_announce(&pkt_b_late, 6_500).unwrap();
        assert_eq!(tracker.current_source().unwrap().sender, "SRC_B");
        assert!(!tracker.should_announce(6_500, true));
    }

    #[test]
    fn timesync_equal_priority_failover_uses_standby_without_reannounce() {
        let mut tracker = TimeSyncTracker::new(TimeSyncConfig {
            role: TimeSyncRole::Consumer,
            priority: 50,
            source_timeout_ms: 1_000,
            ..Default::default()
        });

        let pkt_a = build_timesync_announce_with_sender("SRC_A", 10, 5_000).unwrap();
        let pkt_b = build_timesync_announce_with_sender("SRC_B", 10, 5_500).unwrap();

        tracker.handle_announce(&pkt_a, 5_000).unwrap();
        tracker.handle_announce(&pkt_b, 5_500).unwrap();
        assert_eq!(tracker.current_source().unwrap().sender, "SRC_A");

        assert!(matches!(
            tracker.refresh(6_050),
            sedsprintf_rs::timesync::TimeSyncUpdate::SourceChanged
        ));
        assert_eq!(tracker.current_source().unwrap().sender, "SRC_B");
        assert!(!tracker.should_announce(6_050, true));
    }

    #[test]
    fn source_role_participates_in_priority_election_and_follows_better_remote() {
        let mut tracker = TimeSyncTracker::new(TimeSyncConfig {
            role: TimeSyncRole::Source,
            priority: 20,
            source_timeout_ms: 1_000,
            ..Default::default()
        });

        assert!(tracker.should_announce(0, true));

        let pkt_better = build_timesync_announce_with_sender("SRC_A", 10, 5_000).unwrap();
        tracker.handle_announce(&pkt_better, 5_000).unwrap();

        assert_eq!(tracker.current_source().unwrap().sender, "SRC_A");
        assert!(!tracker.should_announce(5_000, true));
        assert_eq!(
            tracker.leader(5_000, true),
            Some(sedsprintf_rs::timesync::TimeSyncLeader::Remote(
                tracker.current_source().unwrap().clone()
            ))
        );
    }

    #[test]
    fn same_priority_leader_gets_boosted_priority() {
        let mut tracker = TimeSyncTracker::new(TimeSyncConfig {
            role: TimeSyncRole::Source,
            priority: 10,
            source_timeout_ms: 1_000,
            ..Default::default()
        });

        let remote = if DEVICE_IDENTIFIER < "ZZZ" {
            "ZZZ"
        } else {
            "zzzz"
        };
        let pkt_same = build_timesync_announce_with_sender(remote, 10, 5_000).unwrap();
        tracker.handle_announce(&pkt_same, 5_000).unwrap();

        assert_eq!(tracker.local_announce_priority(5_000, true), Some(9));
    }

    #[test]
    fn consumer_can_promote_when_no_remote_producer_and_has_time() {
        let tracker = TimeSyncTracker::new(TimeSyncConfig {
            role: TimeSyncRole::Consumer,
            priority: 40,
            source_timeout_ms: 1_000,
            consumer_promotion_enabled: true,
            ..Default::default()
        });

        assert!(tracker.should_announce(1_000, true));
    }

    #[test]
    fn consumer_promotion_can_be_disabled() {
        let tracker = TimeSyncTracker::new(TimeSyncConfig {
            role: TimeSyncRole::Consumer,
            priority: 40,
            source_timeout_ms: 1_000,
            consumer_promotion_enabled: false,
            ..Default::default()
        });

        assert!(!tracker.should_announce(1_000, true));
    }

    #[test]
    fn router_internal_timesync_endpoint_updates_network_time() {
        let now = Arc::new(AtomicU64::new(1_000));
        let router = Router::new_with_clock(
            RouterConfig::default().with_timesync(TimeSyncConfig::default()),
            shared_clock(now.clone()),
        );

        let announce = build_timesync_announce_with_sender("GM", 1, 1_700_000_000_000).unwrap();
        router.rx(&announce).unwrap();

        let first = router.network_time_ms().expect("network time unavailable");
        now.store(1_025, Ordering::SeqCst);
        let later = router.network_time_ms().expect("network time unavailable");
        assert!(
            later >= first + 25,
            "network time should advance with monotonic clock"
        );
    }

    #[test]
    fn router_failover_slews_without_jumping_backwards() {
        let now = Arc::new(AtomicU64::new(1_000));
        let router = Router::new_with_clock(
            RouterConfig::default().with_timesync(TimeSyncConfig {
                role: TimeSyncRole::Consumer,
                priority: 50,
                source_timeout_ms: 100,
                max_slew_ppm: 50_000,
                ..Default::default()
            }),
            shared_clock(now.clone()),
        );

        let leader_a = build_timesync_announce_with_sender("SRC_A", 10, 1_700_000_000_000).unwrap();
        let leader_b = build_timesync_announce_with_sender("SRC_B", 20, 1_699_999_990_000).unwrap();

        router.rx(&leader_a).unwrap();
        router.rx(&leader_b).unwrap();
        let before_failover = router.network_time_ms().expect("network time unavailable");

        now.store(1_200, Ordering::SeqCst);
        let after_timeout = router.network_time_ms().expect("network time unavailable");

        assert!(
            after_timeout >= before_failover,
            "failover must not jump backwards"
        );
    }

    #[test]
    fn router_clears_stale_pending_request_when_source_fails_over() {
        let now = Arc::new(AtomicU64::new(0));
        let request_seqs = Arc::new(Mutex::new(Vec::new()));
        let request_seqs_c = request_seqs.clone();
        let router = Router::new_with_clock(
            RouterConfig::default().with_timesync(TimeSyncConfig {
                role: TimeSyncRole::Consumer,
                priority: 50,
                source_timeout_ms: 1_000,
                request_interval_ms: 1_000,
                max_slew_ppm: 999_999,
                ..Default::default()
            }),
            shared_clock(now.clone()),
        );
        router.add_side_packet("CAP", move |pkt| {
            if pkt.data_type() == DataType::TimeSyncRequest {
                request_seqs_c
                    .lock()
                    .unwrap()
                    .push(pkt.data_as_u64().unwrap()[0]);
            }
            Ok(())
        });

        let leader_a = build_timesync_announce_with_sender("SRC_A", 1, 1_700_000_000_000).unwrap();
        router.rx(&leader_a).unwrap();
        router.process_tx_queue().unwrap();

        now.store(1_500, Ordering::SeqCst);
        let leader_b = build_timesync_announce_with_sender("SRC_B", 2, 1_700_000_001_500).unwrap();
        router.rx(&leader_b).unwrap();
        router.process_tx_queue().unwrap();

        let request_seqs = request_seqs.lock().unwrap().clone();
        assert_eq!(request_seqs, vec![1, 2]);

        let before_response = router.network_time_ms().expect("network time unavailable");
        let resp_b_wire =
            build_timesync_response(request_seqs[1], 1_500, 1_700_000_001_550, 1_700_000_001_550)
                .unwrap();
        let resp_b = Packet::new(
            DataType::TimeSyncResponse,
            &[DataEndpoint::TimeSync],
            "SRC_B",
            resp_b_wire.timestamp(),
            resp_b_wire.payload().into(),
        )
        .unwrap();
        router.rx(&resp_b).unwrap();

        now.store(1_600, Ordering::SeqCst);
        let after_response = router.network_time_ms().expect("network time unavailable");
        assert!(
            after_response >= before_response + 140,
            "failover response from replacement source should be accepted and influence the slewed clock: before={before_response}, after={after_response}"
        );
    }

    #[test]
    fn router_merges_partial_network_time_sources() {
        let now = Arc::new(AtomicU64::new(2_000));
        let router = Router::new_with_clock(
            RouterConfig::default().with_timesync(TimeSyncConfig::default()),
            shared_clock(now.clone()),
        );

        router.update_network_time_source(
            "rtc_date",
            50,
            PartialNetworkTime {
                year: Some(2026),
                month: Some(3),
                day: Some(21),
                ..Default::default()
            },
            None,
        );
        router.update_network_time_source(
            "gps_tod",
            1,
            PartialNetworkTime {
                hour: Some(12),
                minute: Some(34),
                second: Some(56),
                ..Default::default()
            },
            None,
        );

        let merged = router.network_time().expect("network time unavailable");
        assert_eq!(merged.time.year, Some(2026));
        assert_eq!(merged.time.month, Some(3));
        assert_eq!(merged.time.day, Some(21));
        assert_eq!(merged.time.hour, Some(12));
        assert_eq!(merged.time.minute, Some(34));
        assert_eq!(merged.time.second, Some(56));
        assert!(
            merged.unix_time_ms.is_some(),
            "merged date/time should produce epoch ms"
        );

        now.store(3_500, Ordering::SeqCst);
        let advanced = router.network_time().expect("network time unavailable");
        assert_eq!(advanced.time.second, Some(57));
        assert_eq!(advanced.time.nanosecond, Some(500_000_000));
    }

    #[test]
    fn local_master_setters_merge_partial_fields_and_anchor_at_commit_time() {
        let router = Router::new_with_clock(
            RouterConfig::default().with_timesync(TimeSyncConfig {
                role: TimeSyncRole::Source,
                priority: 1,
                ..Default::default()
            }),
            Box::new(StepClock {
                now_ns: AtomicU64::new(0),
                step_ns: 25_000_000,
            }),
        );

        router.set_local_network_date(2026, 3, 21);
        router.set_local_network_time_hms_millis(12, 34, 56, 0);

        let reading = router.network_time().expect("network time unavailable");
        assert_eq!(reading.time.year, Some(2026));
        assert_eq!(reading.time.month, Some(3));
        assert_eq!(reading.time.day, Some(21));
        assert_eq!(reading.time.hour, Some(12));
        assert_eq!(reading.time.minute, Some(34));
        assert_eq!(reading.time.second, Some(56));
        assert!(
            reading.time.nanosecond.unwrap_or(0) >= 100_000_000,
            "setter should compensate for elapsed monotonic time during the call"
        );
    }

    #[cfg(feature = "compression")]
    #[test]
    fn compression_mixed_workload_threaded_system_stability() {
        let worker_count = 4usize;
        let iters_per_worker = 600usize;

        let mut joins = Vec::new();
        for tid in 0..worker_count {
            joins.push(thread::spawn(move || {
                for i in 0..iters_per_worker {
                    let ts = (tid as u64) * 10_000 + (i as u64);
                    let payload = if i % 2 == 0 {
                        vec![b'Q'; 224]
                    } else {
                        let mut v = Vec::with_capacity(224);
                        for j in 0..224u16 {
                            v.push(32u8 + (((i as u16 + j + tid as u16) as u8) % 95));
                        }
                        v
                    };

                    let pkt = Packet::new(
                        DataType::MessageData,
                        &[DataEndpoint::SdCard],
                        "SYS_COMP",
                        ts,
                        Arc::<[u8]>::from(payload.as_slice()),
                    )
                    .expect("packet build failed");

                    let wire = serialize::serialize_packet(&pkt);
                    let decoded = serialize::deserialize_packet(&wire).expect("deserialize failed");
                    assert_eq!(decoded.payload(), payload.as_slice());
                }
            }));
        }

        for j in joins {
            j.join().expect("compression worker panicked");
        }
    }
}
