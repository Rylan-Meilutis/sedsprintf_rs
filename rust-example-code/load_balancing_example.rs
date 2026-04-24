use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};
use sedsprintf_rs::{DataEndpoint, DataType, RouteSelectionMode, TelemetryResult};
use std::sync::atomic::{AtomicU64, Ordering};

struct StepClock(AtomicU64);

impl Clock for StepClock {
    fn now_ms(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }
}

fn main() -> TelemetryResult<()> {
    // This example assumes GPS_DATA and RADIO are already seeded into the runtime schema.
    let router = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            DataEndpoint::named("RADIO"),
            |_pkt| Ok(()),
        )]),
        Box::new(StepClock(AtomicU64::new(0))),
    );

    let side_a = router.add_side_packet("WAN_A", |pkt| {
        println!("[router WAN_A] {pkt}");
        Ok(())
    });
    let side_b = router.add_side_packet("WAN_B", |pkt| {
        println!("[router WAN_B] {pkt}");
        Ok(())
    });

    router.set_source_route_mode(None, RouteSelectionMode::Weighted)?;
    router.set_route_weight(None, side_a, 3)?;
    router.set_route_weight(None, side_b, 1)?;

    for seq in 0..4 {
        router.log_f32(DataType::named("GPS_DATA"), &[seq as f32, 10.0, 20.0])?;
    }
    router.process_all_queues_with_timeout(0)?;
    Ok(())
}
