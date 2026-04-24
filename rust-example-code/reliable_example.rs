use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig, RouterSideOptions};
use sedsprintf_rs::{DataEndpoint, DataType, TelemetryResult};

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn main() -> TelemetryResult<()> {
    // This example assumes GPS_DATA is seeded as a reliable type.
    let router = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            DataEndpoint::named("RADIO"),
            |pkt| {
                println!("[RX] {pkt}");
                Ok(())
            },
        )]),
        Box::new(|| now_ms()),
    );
    router.add_side_serialized_with_options(
        "RADIO",
        |_bytes| Ok(()),
        RouterSideOptions {
            reliable_enabled: true,
            ..RouterSideOptions::default()
        },
    );

    router.log_f32(DataType::named("GPS_DATA"), &[1.0_f32, 2.0, 3.0])?;
    router.process_all_queues_with_timeout(0)?;
    Ok(())
}
