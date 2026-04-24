#!/usr/bin/env python3
import sedsprintf_rs as seds

DT = seds.DataType
RSM = seds.RouteSelectionMode


def main() -> None:
    # Assumes GPS_DATA and RADIO are already seeded into the runtime schema.
    router = seds.Router(handlers=[(seds.endpoint_info_by_name("RADIO")["id"], lambda pkt: None, None)])

    side_a = router.add_side_packet("WAN_A", lambda pkt: print("[router WAN_A]", pkt))
    side_b = router.add_side_packet("WAN_B", lambda pkt: print("[router WAN_B]", pkt))

    router.set_source_route_mode(None, RSM.Weighted)
    router.set_route_weight(None, side_a, 3)
    router.set_route_weight(None, side_b, 1)

    for seq in range(4):
        router.log_f32(int(DT.GPS_DATA), [float(seq), 10.0, 20.0])
    router.process_all_queues()


if __name__ == "__main__":
    main()
