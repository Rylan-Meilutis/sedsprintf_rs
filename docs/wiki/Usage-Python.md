# Python Usage

Python bindings are built with pyo3 and maturin. The Python module name is `sedsprintf_rs`.

## Build and install

Option 1: use
build.py ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.py)) (
recommended in this repo)

```
./build.py python
```

Option 2: maturin

```
maturin develop
```

If you want a wheel:

```
./build.py maturin-build
```

## Minimal example

```python
import sedsprintf_rs as seds

DT = seds.DataType
EP = seds.DataEndpoint
RM = seds.RouterMode


def tx(bytes_buf):
    # send bytes to transport
    pass


def on_packet(pkt):
    print(pkt)


handlers = [
    (int(EP.SD_CARD), on_packet, None),
]

router = seds.Router(handlers=handlers, mode=RM.Sink)
router.add_side_serialized("RADIO", tx)
router.log_f32(ty=DT.GPS_DATA, values=[1.0, 2.0, 3.0])
router.process_all_queues()
```

If you need a custom monotonic source for tests or simulation, pass `now_ms=...`. Otherwise the
router uses its internal monotonic clock on `std` builds.
If the extension was built with the `timesync` feature but you do not want router-managed time
sync for a particular instance, construct `Router(..., timesync_enabled=False)`.

See python-example/main.py
([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/python-example/main.py))
for a more complete multi-process example.
Time sync is demonstrated in python-example/timesync_example.py
([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/python-example/timesync_example.py)).
See [Time-Sync](Time-Sync) for the time sync packet flow and roles.

With `timesync` enabled, `Router` keeps an internal network clock. `TIME_SYNC` packets are
handled internally, `network_time()` / `network_time_ms()` expose the merged current time, and
source/master nodes can set partial or complete local time with `set_local_network_time(...)`,
`set_local_network_date(...)`, and the `set_local_network_*` datetime helpers.
For normal application loops, call `router.periodic(timeout_ms)` to run time sync, discovery, and
queue draining together. If you need to skip time sync for a cycle while keeping the feature
enabled, call `router.periodic_no_timesync(timeout_ms)` instead.
`router.poll_timesync()` remains available as a lower-level non-blocking hook when you want to
manage the maintenance phases manually.

With `discovery` enabled, both `Router` and `Relay` also expose `announce_discovery()` and
`poll_discovery()`. `poll_discovery()` remains available as a lower-level hook when you want to
manage discovery separately, while `Relay.periodic(timeout_ms)` bundles discovery polling and
queue draining into one call. `announce_discovery()` still forces an immediate advertise cycle.

## Logging API

The Python API exposes typed log helpers that mirror the Rust API:

- `log_f32`, `log_i16`, `log_u32`, etc.
- `log_string` for UTF-8 payloads.
- `log_binary` for raw bytes.

If you already have a packet or bytes, use:

- `tx_serialized(bytes)`
- `rx_serialized(bytes)`

## Handlers

Handlers are registered as tuples:

```
(endpoint_id, handler_fn, user)
```

`handler_fn` receives `(packet)`.

## Queue processing

The router can queue RX/TX operations. If you use the queue variants, call:

- `process_rx_queue()`
- `process_tx_queue()`
- `process_all_queues()`
- `periodic(timeout_ms)`
- `periodic_no_timesync(timeout_ms)` for router loops that should skip time sync on that cycle

## Sides

Routers use **named sides** (UART/CAN/RADIO/etc.). Register sides with:

- `add_side_serialized(name, tx_cb)`
- `add_side_packet(name, tx_cb)`

As of v3.0.0, side tracking is internal, so most apps call `rx_serialized(bytes)` without passing
a side ID. Use side-aware ingress only when you need to override ingress explicitly.

Side-aware ingress:

- `receive_serialized_from_side(side_id, bytes)`
- `receive_packet_from_side(side_id, packet)`

Runtime side policy and routing controls:

- `remove_side(side_id)`
- `set_side_ingress_enabled(side_id, enabled)`
- `set_side_egress_enabled(side_id, enabled)`
- `set_route(src_side_id, dst_side_id, enabled)`
- `clear_route(src_side_id, dst_side_id)`
- `set_typed_route(src_side_id, ty, dst_side_id, enabled)`
- `clear_typed_route(src_side_id, ty, dst_side_id)`
- `set_source_route_mode(src_side_id, mode)`
- `set_route_weight(src_side_id, dst_side_id, weight)`
- `set_route_priority(src_side_id, dst_side_id, priority)`

Use `None` for `src_side_id` when you want to control locally-originated router TX rather than
traffic received from a specific side. `Relay` exposes the same side lifecycle, side-policy, and
route-override methods, including type-specific route allowlists, with the same `None`
convention for locally-originated discovery TX.

Use `RouteSelectionMode.Fanout` to preserve current behavior, `RouteSelectionMode.Weighted` for
weighted multi-path splitting, and `RouteSelectionMode.Failover` to prefer one path until
discovery says it is gone.

For a dedicated command network where two links both reach the same remote destination and you do
not want load balancing, keep the default fanout mode and use typed routes as a manual allowlist:

```python
router = seds.Router(handlers=[(int(EP.RADIO), lambda pkt: None, None)], mode=RM.Sink)

telemetry = router.add_side_packet("TELEMETRY", lambda pkt: print("[TELEMETRY]", pkt))
command_a = router.add_side_packet("COMMAND_A", lambda pkt: print("[COMMAND_A]", pkt))
command_b = router.add_side_packet("COMMAND_B", lambda pkt: print("[COMMAND_B]", pkt))

router.set_route(None, command_a, False)
router.set_route(None, command_b, False)

router.set_typed_route(None, int(DT.MESSAGE_DATA), command_a, True)
router.set_typed_route(None, int(DT.MESSAGE_DATA), command_b, True)

router.log_f32(int(DT.GPS_DATA), [1.0, 2.0, 3.0])          # TELEMETRY only
router.log_bytes(int(DT.MESSAGE_DATA), b"ARM PAYLOAD")     # COMMAND_A + COMMAND_B

_ = telemetry
```

`MESSAGE_DATA` is just a stand-in for a command-type payload. Replace it with your schema's real
command or abort `DataType`.

## Debugging tips

- `print(pkt)` uses the packet's string formatter.
- If a log call fails, check the schema for payload size/type mismatches.
- Ensure your Python environment matches the one used by `maturin develop`.
