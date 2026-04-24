use sedsprintf_rs::router::{EndpointHandler, Router, RouterConfig};
use sedsprintf_rs::{DataEndpoint, DataType, TelemetryResult};

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn main() -> TelemetryResult<()> {
    let node_a = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            DataEndpoint::named("RADIO"),
            |pkt| {
                println!("[NODE A RX] {pkt}");
                Ok(())
            },
        )]),
        Box::new(|| now_ms()),
    );
    let node_b = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            DataEndpoint::named("SD_CARD"),
            |pkt| {
                println!("[NODE B RX] {pkt}");
                Ok(())
            },
        )]),
        Box::new(|| now_ms()),
    );

    let link = move |bytes: &[u8]| -> TelemetryResult<()> {
        node_b.rx_serialized(bytes)?;
        Ok(())
    };
    node_a.add_side_serialized("LINK", link);

    node_a.log_f32(DataType::named("GPS_DATA"), &[1.0_f32, 2.0, 3.0])?;
    node_a.process_all_queues_with_timeout(0)?;
    node_b.process_all_queues_with_timeout(0)?;
    Ok(())
}
