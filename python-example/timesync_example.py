#!/usr/bin/env python3
import sys
import time

import sedsprintf_rs as seds

DT = seds.DataType


def _now_ms() -> int:
    return int(time.time() * 1000)


def _tx(_bytes_buf: bytes):
    pass


def _on_packet(pkt: seds.Packet):
    print(pkt)


def main() -> None:
    radio = seds.endpoint_info_by_name("RADIO")["id"]
    sd_card = seds.endpoint_info_by_name("SD_CARD")["id"]
    router = seds.Router(
        now_ms=_now_ms,
        handlers=[
            (sd_card, _on_packet, None),
            (radio, _on_packet, None),
        ],
        timesync_enabled=True,
    )
    router.add_side_serialized("TX", _tx)
    router.set_local_network_datetime_millis(2025, 1, 1, 12, 0, 0, 0)

    router.log_f32(int(DT.GPS_DATA), [37.7749, -122.4194, 30.0])
    router.log_f32(int(DT.IMU_DATA), [0.1, 0.2, 0.3, 1.1, 1.2, 1.3])
    router.log_bytes(int(DT.MESSAGE_DATA), b"hello from python timesync example")
    router.log_bytes(int(DT.HEARTBEAT), b"")
    router.periodic(0)

    print(f"network_time_ms={router.network_time_ms()}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nInterrupted by user.", file=sys.stderr)
        raise SystemExit(130)
    except Exception as e:
        print(f"Error: Unexpected failure in timesync example: {e}", file=sys.stderr)
        raise SystemExit(1) from e
