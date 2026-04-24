#!/usr/bin/env python3
import sedsprintf_rs as seds

DT = seds.DataType


def main() -> None:
    radio = seds.endpoint_info_by_name("RADIO")["id"]
    router = seds.Router(handlers=[(radio, lambda pkt: None, None)])

    telemetry = router.add_side_packet("TELEMETRY", lambda pkt: print("[TELEMETRY]", pkt))
    command_a = router.add_side_packet("COMMAND_A", lambda pkt: print("[COMMAND_A]", pkt))
    command_b = router.add_side_packet("COMMAND_B", lambda pkt: print("[COMMAND_B]", pkt))

    router.set_route(None, command_a, False)
    router.set_route(None, command_b, False)
    router.set_typed_route(None, int(DT.MESSAGE_DATA), command_a, True)
    router.set_typed_route(None, int(DT.MESSAGE_DATA), command_b, True)

    router.log_f32(int(DT.GPS_DATA), [1.0, 2.0, 3.0])
    router.log_bytes(int(DT.MESSAGE_DATA), b"ARM PAYLOAD")
    router.process_all_queues()
    _ = telemetry


if __name__ == "__main__":
    main()
