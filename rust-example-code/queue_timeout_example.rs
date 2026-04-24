use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};
use sedsprintf_rs::{DataEndpoint, DataType, TelemetryResult};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
struct StepClock {
    now: AtomicU64,
}

impl Clock for StepClock {
    fn now_ms(&self) -> u64 {
        self.now.fetch_add(5, Ordering::SeqCst)
    }
}

fn main() -> TelemetryResult<()> {
    let router = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            DataEndpoint::named("SD_CARD"),
            |pkt| {
                println!("[RX] {pkt}");
                Ok(())
            },
        )]),
        Box::new(StepClock::default()),
    );
    router.add_side_serialized("TX", |_bytes| Ok(()));

    for i in 0..5 {
        router.log_f32(DataType::named("GPS_DATA"), &[i as f32, 0.0, 0.0])?;
    }

    router.process_all_queues_with_timeout(5)?;
    router.process_all_queues_with_timeout(0)?;
    Ok(())
}
