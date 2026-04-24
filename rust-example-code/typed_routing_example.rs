use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};
use sedsprintf_rs::{DataEndpoint, DataType, Packet, TelemetryResult};
use std::sync::atomic::{AtomicU64, Ordering};

struct StepClock(AtomicU64);

impl Clock for StepClock {
    fn now_ms(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }
}

fn main() -> TelemetryResult<()> {
    // This example assumes MESSAGE_DATA, GPS_DATA, and RADIO are already seeded.
    let router = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            DataEndpoint::named("RADIO"),
            |_pkt| Ok(()),
        )]),
        Box::new(StepClock(AtomicU64::new(0))),
    );

    let telemetry = router.add_side_packet("TELEMETRY", |pkt| {
        println!("[TELEMETRY] {pkt}");
        Ok(())
    });
    let command_a = router.add_side_packet("COMMAND_A", |pkt| {
        println!("[COMMAND_A] {pkt}");
        Ok(())
    });
    let command_b = router.add_side_packet("COMMAND_B", |pkt| {
        println!("[COMMAND_B] {pkt}");
        Ok(())
    });

    router.set_route(None, command_a, false)?;
    router.set_route(None, command_b, false)?;
    router.set_typed_route(None, DataType::named("MESSAGE_DATA"), command_a, true)?;
    router.set_typed_route(None, DataType::named("MESSAGE_DATA"), command_b, true)?;

    let telemetry_pkt = Packet::from_f32_slice(
        DataType::named("GPS_DATA"),
        &[1.0, 2.0, 3.0],
        &[DataEndpoint::named("RADIO")],
        1,
    )?;
    router.tx(telemetry_pkt)?;

    let command_pkt = Packet::from_str_slice(
        DataType::named("MESSAGE_DATA"),
        &["ARM PAYLOAD"],
        &[DataEndpoint::named("RADIO")],
        2,
    )?;
    router.tx(command_pkt)?;

    router.process_all_queues_with_timeout(0)?;
    let _ = telemetry;
    Ok(())
}
