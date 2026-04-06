use sedsprintf_rs::relay::Relay;
use sedsprintf_rs::router::{Clock, EndpointHandler, Router, RouterConfig, RouterMode};
use sedsprintf_rs::{DataEndpoint, DataType, RouteSelectionMode, TelemetryResult};

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

    let relay = Relay::new(Box::new(FixedClock));
    let ingress = relay.add_side_packet("INGRESS", |_pkt| Ok(()));
    let path_a = relay.add_side_packet("PATH_A", |pkt| {
        println!("[relay PATH_A] {pkt}");
        Ok(())
    });
    let path_b = relay.add_side_packet("PATH_B", |pkt| {
        println!("[relay PATH_B] {pkt}");
        Ok(())
    });

    relay.set_source_route_mode(Some(ingress), RouteSelectionMode::Failover)?;
    relay.set_route_priority(Some(ingress), path_a, 0)?;
    relay.set_route_priority(Some(ingress), path_b, 1)?;

    for seq in 0..4 {
        router.log_f32(DataType::GpsData, &[seq as f32, 10.0, 20.0])?;
    }

    Ok(())
}
