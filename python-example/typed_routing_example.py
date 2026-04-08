#!/usr/bin/env python3
import sedsprintf_rs as seds

DT = seds.DataType
EP = seds.DataEndpoint
RM = seds.RouterMode


def main() -> None:
    router = seds.Router(handlers=[(int(EP.RADIO), lambda pkt: None, None)], mode=RM.Sink)

    telemetry = router.add_side_packet("TELEMETRY", lambda pkt: print("[TELEMETRY]", pkt))
    command_a = router.add_side_packet("COMMAND_A", lambda pkt: print("[COMMAND_A]", pkt))
    command_b = router.add_side_packet("COMMAND_B", lambda pkt: print("[COMMAND_B]", pkt))

    # Keep ordinary traffic off the command links.
    router.set_route(None, command_a, False)
    router.set_route(None, command_b, False)

    # Treat MESSAGE_DATA as "command" traffic and send it through both command
    # links. This uses typed-route fanout only, not load balancing.
    router.set_typed_route(None, int(DT.MESSAGE_DATA), command_a, True)
    router.set_typed_route(None, int(DT.MESSAGE_DATA), command_b, True)

    router.log_f32(int(DT.GPS_DATA), [1.0, 2.0, 3.0])
    router.log_bytes(int(DT.MESSAGE_DATA), b"ARM PAYLOAD")

    _ = telemetry


if __name__ == "__main__":
    main()
