use sedsprintf_rs::router::{EndpointHandler, Router, RouterConfig};
use sedsprintf_rs::timesync::{TimeSyncConfig, TimeSyncRole};
use sedsprintf_rs::{DataEndpoint, DataType, Packet, TelemetryResult};

fn main() -> TelemetryResult<()> {
    // Build with `--features timesync`. The router owns TIME_SYNC internally.
    let router = Router::new(
        RouterConfig::new([
            EndpointHandler::new_packet_handler(DataEndpoint::named("RADIO"), |pkt| {
                println!("[RADIO] {pkt}");
                Ok(())
            }),
            EndpointHandler::new_packet_handler(DataEndpoint::named("SD_CARD"), |pkt| {
                println!("[SD] {pkt}");
                Ok(())
            }),
        ])
        .with_timesync(TimeSyncConfig {
            role: TimeSyncRole::Source,
            priority: 10,
            ..TimeSyncConfig::default()
        }),
    );
    router.add_side_serialized("TX", |_bytes| Ok(()));
    router.set_local_network_datetime_millis(2025, 1, 1, 12, 0, 0, 0);

    router.log_f32(DataType::named("GPS_DATA"), &[37.7749, -122.4194, 30.0])?;
    router.tx(Packet::from_str_slice(
        DataType::named("MESSAGE_DATA"),
        &["hello from rust timesync example"],
        &[DataEndpoint::named("RADIO"), DataEndpoint::named("SD_CARD")],
        0,
    )?)?;
    router.tx(Packet::from_no_data(
        DataType::named("HEARTBEAT"),
        &[DataEndpoint::named("RADIO"), DataEndpoint::named("SD_CARD")],
        0,
    )?)?;

    router.periodic(0)?;
    println!("network_time_ms={:?}", router.network_time_ms());
    Ok(())
}
