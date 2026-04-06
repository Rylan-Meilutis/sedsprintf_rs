#!/usr/bin/env python3
import sedsprintf_rs as seds

DT = seds.DataType
EP = seds.DataEndpoint
RM = seds.RouterMode
RSM = seds.RouteSelectionMode


def main() -> None:
    router = seds.Router(handlers=[(int(EP.RADIO), lambda pkt: None, None)], mode=RM.Sink)

    side_a = router.add_side_packet("WAN_A", lambda pkt: print("[router WAN_A]", pkt))
    side_b = router.add_side_packet("WAN_B", lambda pkt: print("[router WAN_B]", pkt))

    router.set_source_route_mode(None, RSM.Weighted)
    router.set_route_weight(None, side_a, 3)
    router.set_route_weight(None, side_b, 1)

    relay = seds.Relay()
    ingress = relay.add_side_packet("INGRESS", lambda pkt: None)
    path_a = relay.add_side_packet("PATH_A", lambda pkt: print("[relay PATH_A]", pkt))
    path_b = relay.add_side_packet("PATH_B", lambda pkt: print("[relay PATH_B]", pkt))

    relay.set_source_route_mode(ingress, RSM.Failover)
    relay.set_route_priority(ingress, path_a, 0)
    relay.set_route_priority(ingress, path_b, 1)

    for seq in range(4):
        router.log_f32(int(DT.GPS_DATA), [float(seq), 10.0, 20.0])


if __name__ == "__main__":
    main()
