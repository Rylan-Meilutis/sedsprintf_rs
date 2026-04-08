use sedsprintf_rs::config::{DataEndpoint, DataType};
use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig, RouterMode};
use sedsprintf_rs::telemetry_packet::Packet;
use sedsprintf_rs::TelemetryResult;

struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        0
    }
}

fn main() -> TelemetryResult<()> {
    let router = Router::new_with_clock(
        RouterMode::Sink,
        RouterConfig::new([EndpointHandler::new_packet_handler(
            DataEndpoint::Radio,
            |_pkt| Ok(()),
        )]),
        Box::new(FixedClock),
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

    // Keep ordinary local traffic off the command links.
    router.set_route(None, command_a, false)?;
    router.set_route(None, command_b, false)?;

    // Treat MessageData as a stand-in for "command" traffic and fan it out to
    // both command links. No weighted/failover route mode is enabled here.
    router.set_typed_route(None, DataType::MessageData, command_a, true)?;
    router.set_typed_route(None, DataType::MessageData, command_b, true)?;

    let telemetry_pkt = Packet::from_f32_slice(
        DataType::GpsData,
        &[1.0, 2.0, 3.0],
        &[DataEndpoint::Radio],
        1,
    )?;
    router.tx(telemetry_pkt)?;

    let command_pkt = Packet::from_str_slice(
        DataType::MessageData,
        &["ARM PAYLOAD"],
        &[DataEndpoint::Radio],
        2,
    )?;
    router.tx(command_pkt)?;

    let _ = telemetry;
    Ok(())
}
