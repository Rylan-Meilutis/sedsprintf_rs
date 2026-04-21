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

DT = seds.DataType
EP = seds.DataEndpoint


def tx(bytes_buf):
    pass


def on_packet(pkt):
    print(pkt)


router = seds.Router(
    handlers=[(int(EP.SD_CARD), on_packet, None)],
)

router.add_side_serialized("RADIO", tx, reliable_enabled=True)
router.log_f32(int(DT.GPS_DATA), [1.0, 2.0, 3.0])
router.process_all_queues()
```

If you need a custom monotonic source for tests or simulation, pass `now_ms=...`.

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
recent packet IDs, reliable buffers/replay state, and discovery topology all count against it.
Recent packet ID caches preallocate their final storage and reserve that byte cost immediately.
Discovery topology eviction emits a warning in `std` builds.

## Time sync

When built with `timesync`, `Router` keeps an internal network clock and handles `TIME_SYNC`
traffic internally.

Construct `Router(..., timesync_enabled=False)` if the extension was built with `timesync` but you
do not want time sync for a particular instance.

See [Time-Sync](Time-Sync) for protocol details.
