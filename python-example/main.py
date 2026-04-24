#!/usr/bin/env python3
import multiprocessing as mp
import random
import sys
import time
from queue import Empty

import sedsprintf_rs as seds

DT = seds.DataType


def _now_ms() -> int:
    return int(time.time() * 1000)


def _tx(_bytes_buf: bytes):
    pass


def _on_packet(pkt: seds.Packet):
    print("[RX Packet]", pkt)


def _on_serialized(data: bytes):
    print(f"[RX Serialized] {len(data)} bytes")


def router_server(cmd_q: mp.Queue, pump_period_ms: int = 2, max_total_seconds: float = 10.0):
    radio = seds.endpoint_info_by_name("RADIO")["id"]
    sd_card = seds.endpoint_info_by_name("SD_CARD")["id"]
    router = seds.Router(
        now_ms=_now_ms,
        handlers=[
            (sd_card, _on_packet, None),
            (radio, None, _on_serialized),
        ],
        timesync_enabled=True,
    )
    router.add_side_serialized("TX", _tx, reliable_enabled=True)
    router.set_local_network_datetime_millis(2025, 1, 1, 12, 0, 0, 0)

    start_time = time.time()
    last_pump = 0.0
    while True:
        now = time.time()
        if now - start_time > max_total_seconds:
            break
        if (now - last_pump) * 1000.0 >= pump_period_ms:
            router.periodic(0)
            last_pump = now

        try:
            op, payload = cmd_q.get(timeout=0.05)
        except Empty:
            continue

        if op == "shutdown":
            break
        if op == "log_f32":
            router.log_f32(payload["ty"], payload["values"])
        elif op == "log_bytes":
            router.log_bytes(payload["ty"], payload["data"])

    router.periodic(0)
    print(f"[SERVER] network_time_ms={router.network_time_ms()}")


def producer_proc(name: str, cmd_q: mp.Queue, n_iters: int, seed: int):
    random.seed(seed)
    for i in range(n_iters):
        if random.randint(0, 1) == 0:
            cmd_q.put(("log_f32", {"ty": int(DT.GPS_DATA), "values": [float(i), 10.0, 20.0]}))
        else:
            cmd_q.put(("log_bytes", {"ty": int(DT.MESSAGE_DATA), "data": f"{name} iteration {i}".encode()}))
        time.sleep(random.random() * 0.01)


def main() -> int:
    cmd_q: mp.Queue = mp.Queue()
    server = mp.Process(target=router_server, args=(cmd_q,))
    server.start()

    producers = [
        mp.Process(target=producer_proc, args=(f"P{i}", cmd_q, 5, i))
        for i in range(2)
    ]
    for proc in producers:
        proc.start()
    for proc in producers:
        proc.join()

    cmd_q.put(("shutdown", None))
    server.join()
    return server.exitcode or 0


if __name__ == "__main__":
    raise SystemExit(main())
