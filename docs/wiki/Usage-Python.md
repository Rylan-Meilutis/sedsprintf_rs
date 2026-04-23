# Python Usage

Python bindings are built with `pyo3` and `maturin`. The module name is `sedsprintf_rs`.

## Build and install

Recommended in this repo:

```bash
./build.py python
```

Direct `maturin` is also supported:

```bash
maturin develop
```

## Minimal example

```python
import sedsprintf_rs as seds


def tx(bytes_buf):
    pass


def on_packet(pkt):
    print(pkt)


schema = b"""
{
  "endpoints": [
    {"rust": "SdCard", "name": "SD_CARD", "description": "Local storage endpoint"},
    {"rust": "Radio", "name": "RADIO", "description": "External radio link"}
  ],
  "types": [
    {
      "rust": "GpsData",
      "name": "GPS_DATA",
      "description": "Three f32 GPS values",
      "priority": 80,
      "class": "Data",
      "element": {"kind": "Static", "data_type": "Float32", "count": 3},
      "endpoints": ["Radio", "SdCard"]
    }
  ]
}
"""
seds.register_schema_json_bytes(schema)
sd_card = seds.endpoint_info_by_name("SD_CARD")["id"]
gps_data = seds.data_type_info_by_name("GPS_DATA")["id"]

router = seds.Router(
    handlers=[(sd_card, on_packet, None)],
)

router.add_side_serialized("RADIO", tx, reliable_enabled=True)
router.log_f32(gps_data, [1.0, 2.0, 3.0])
router.process_all_queues()
```

If you need a custom monotonic source for tests or simulation, pass `now_ms=...`.

## Runtime schema

Python exposes the same runtime registry as Rust and C:

- `register_endpoint(...)` and `register_data_type(...)` add explicit entries
- `register_schema_json_file(...)` and `register_schema_json_bytes(...)` seed entries from JSON
- `endpoint_info_by_name(...)` and `data_type_info_by_name(...)` return IDs and metadata
- `remove_endpoint_by_name(...)` and `remove_data_type_by_name(...)` remove user entries

The `DataType` and `DataEndpoint` enums only contain built-in control IDs. Application schema IDs
should be looked up by string name after registration or JSON seeding.

## Routing model

There is no Python `RouterMode` anymore.

- `Router` now uses the same rule-driven forwarding model as the Rust API
- routers and relays both default to a full forwarding mesh across eligible sides
- runtime route rules are how you restrict forwarding

Useful controls:

- `set_side_ingress_enabled(...)`
- `set_side_egress_enabled(...)`
- `set_route(...)`
- `clear_route(...)`
- `set_typed_route(...)`
- `clear_typed_route(...)`
- `set_source_route_mode(...)`
- `set_route_weight(...)`
- `set_route_priority(...)`

Use `None` for `src_side_id` when controlling locally-originated traffic.

## Discovery and reliability

With `discovery` enabled:

- routers and relays learn endpoint reachability per side
- discovery also propagates a transitive router graph, not just flattened endpoint sets
- normal traffic defaults to adaptive discovered-path load balancing
- reliable traffic still fans out across all known discovered candidates

`export_topology()` is available on both `Router` and `Relay`.

- it returns a Python `dict`
- the top-level `routers` key lists each discovered router, the endpoints/source IDs it owns, and
  its connections
- each route entry also includes `announcers` so you can see which upstream router advertised each
  piece of topology

Reliable delivery is enabled on a per-side basis with `reliable_enabled=True` for serialized
sides.

As of `3.11.0`, reliable delivery is end-to-end verified:

- the source router tracks reliable packets it originated
- each discovered destination holder emits an end-to-end acknowledgement after local delivery
- routers and relays route that acknowledgement back only along the learned return path
- unrelated sides do not receive those end-to-end acknowledgements
- the source keeps retransmitting only toward holders that are still missing an acknowledgement
- if a discovered holder ages out of topology, the source removes it from the pending holder set
- newer reliable packets on the same side still do not block while those end-to-end ACKs are pending

For ordered reliable links, later packets that arrive after a missing sequence are buffered and
partial-ACKed. Partial ACKs suppress timeout retransmit for packets already received, but explicit
packet requests can still replay them. The buffered packets are dispatched as soon as the missing
sequence arrives.

## Queue processing

Useful maintenance calls:

- `process_rx_queue()`
- `process_tx_queue()`
- `process_all_queues()`
- `periodic(timeout_ms)`
- `periodic_no_timesync(timeout_ms)` when time sync is enabled but should be skipped for one loop

Router and relay queue-backed state shares one dynamic `MAX_QUEUE_BUDGET`. RX work, TX work,
recent packet IDs, reliable buffers/replay state, discovery topology, and runtime schema registry
memory all count against it.
Recent packet ID caches preallocate their final storage and reserve that byte cost immediately.
Discovery topology eviction emits a warning in `std` builds.

## Time sync

When built with `timesync`, `Router` keeps an internal network clock and handles `TIME_SYNC`
traffic internally.

Construct `Router(..., timesync_enabled=False)` if the extension was built with `timesync` but you
do not want time sync for a particular instance.

See [Time-Sync](Time-Sync) for protocol details.
