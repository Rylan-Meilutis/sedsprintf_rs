use sedsprintf_rs::relay::Relay;
use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig};
use sedsprintf_rs::{DataEndpoint, DataType, Packet, TelemetryResult};

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn main() -> TelemetryResult<()> {
    // This example assumes GPS_DATA and RADIO are already seeded.
    let node = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            DataEndpoint::named("RADIO"),
            |pkt| {
                println!("[NODE] {pkt}");
                Ok(())
            },
        )]),
        Box::new(|| now_ms()),
    );
    node.add_side_serialized("RADIO", |_bytes| Ok(()));

    let relay = Relay::new(Box::new(|| now_ms()));
    let _side_a = relay.add_side_serialized("CAN", |_bytes| Ok(()));
    let _side_b = relay.add_side_serialized("RADIO", |_bytes| Ok(()));

    let pkt = Packet::from_f32_slice(
        DataType::named("GPS_DATA"),
        &[1.0, 2.0, 3.0],
        &[DataEndpoint::named("RADIO")],
        now_ms(),
    )?;
    node.tx(pkt)?;

    relay.process_all_queues_with_timeout(0)?;
    node.process_all_queues_with_timeout(0)?;
    Ok(())
}
