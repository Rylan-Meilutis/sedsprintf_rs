# Rust Usage

This is the primary API and the source of truth for the Rust-facing behavior.

## Add as a dependency

```toml
sedsprintf_rs = { path = "path/to/sedsprintf_rs" }
```

Or from git:

```toml
sedsprintf_rs = { git = "https://github.com/Rylan-Meilutis/sedsprintf_rs.git", branch = "main" }
```

## Minimal router example

```rust
use sedsprintf_rs::router::{EndpointHandler, Router, RouterConfig};
use sedsprintf_rs::{DataEndpoint, DataType, TelemetryResult};

fn main() -> TelemetryResult<()> {
    let handler = EndpointHandler::new_packet_handler(DataEndpoint::SdCard, |pkt| {
        println!("rx: {pkt}");
        Ok(())
    });

    let router = Router::new(RouterConfig::new([handler]));

    router.add_side_serialized("RADIO", |bytes| {
        let _ = bytes;
        Ok(())
    });

    router.log(DataType::GpsData, &[1.0_f32, 2.0, 3.0])?;
    router.process_all_queues()?;
    Ok(())
}
```

On `std` builds, `Router::new(...)` uses an internal monotonic clock. For tests, simulation, or
`no_std`, use `Router::new_with_clock(...)`.

## Sides and routing

Routers and relays use named sides such as `UART`, `CAN`, or `RADIO`.

- `add_side_serialized(...)` and `add_side_packet(...)` register egress handlers
- `remove_side(...)` tombstones a side without renumbering the remaining side ids
- `set_side_ingress_enabled(...)` and `set_side_egress_enabled(...)` control directional policy
- `set_route(...)` and `set_typed_route(...)` define runtime forwarding rules

There is no `RouterMode` anymore.

- `Router` now defaults to rule-driven full-mesh forwarding between eligible sides
- `Relay` keeps the same full-mesh default
- if you want sink-like behavior, disable the specific routes you do not want rather than choosing a
  separate constructor mode

Example:

```rust
use sedsprintf_rs::router::Router;

let router = Router::new(RouterConfig::default());
let side_a = router.add_side_serialized("A", tx_a);
let side_b = router.add_side_serialized("B", tx_b);
let side_c = router.add_side_serialized("C", tx_c);

router.set_route(None, side_b, false)?;        // local TX does not go to B
router.set_route(Some(side_a), side_b, true)?; // allow A -> B
router.set_route(Some(side_b), side_a, false)?;// block B -> A
router.set_typed_route(None, DataType::GpsData, side_c, true)?;
router.set_side_egress_enabled(side_c, false)?; // ingress only
```

## Discovery and multi-path routing

With the `discovery` feature enabled, routers and relays learn which endpoints are reachable
through which sides.

- known paths are preferred over flooding
- unknown paths still fall back to flood/bootstrap behavior
- link-local-only endpoints stay on sides marked `link_local_enabled`
- local plus source-side route rules still gate what discovery is allowed to use
- discovery also carries a transitive router graph, so exported topology keeps sender ownership and
  router-to-router connections instead of only flattening reachability per side

When discovery reports multiple candidate paths:

- normal traffic defaults to adaptive load balancing based on observed transmit bandwidth
- reliable traffic still fans out across all discovered candidates so one weak path does not hide a
  successful delivery on another path
- `set_source_route_mode(...)`, `set_route_weight(...)`, and `set_route_priority(...)` can still
  override the defaults

## Reliable delivery

Reliable delivery has two switches:

- the schema type itself must be marked reliable
- the router/relay side must opt in with `reliable_enabled: true`

That side option is per hop, not global. It controls what happens between the router/relay and
that side's TX callback.

```rust
use sedsprintf_rs::router::{Router, RouterConfig, RouterSideOptions};

let router = Router::new(RouterConfig::default());
router.add_side_serialized_with_options(
    "RADIO",
    tx,
    RouterSideOptions {
        reliable_enabled: true,
        link_local_enabled: false,
        ..RouterSideOptions::default()
    },
);
```

If the underlying transport is already reliable, disable the router-level reliable layer with
`RouterConfig::with_reliable_enabled(false)`.

What `reliable_enabled` means on a side:

- `reliable_enabled: true` on a serialized side wraps reliable schema traffic in the router/relay's
  hop-level reliable framing for that side only
- that hop-level framing adds sequence numbers, ACKs, packet requests, and retransmits
- `reliable_enabled: false` sends the application packet once on that side without the router's
  hop-level reliable wrapper
- packet-output sides (`add_side_packet*`) receive decoded `Packet` values, so they cannot carry
  the serialized hop-level reliable wrapper even if `reliable_enabled` is set

For routers specifically:

- hop-level side reliability is separate from the source router's end-to-end reliable tracking
- a reliable packet can still be tracked end-to-end across the network even if one specific egress
  side is configured without hop-level reliability
- when discovery reports multiple candidate holders, reliable traffic still fans out across all of
  them unless you explicitly restrict routes

As of `3.11.0`, reliability has two layers:

- per-link reliable sequencing, ACKs, packet requests, and retransmits
- end-to-end verification from the source router to every currently discovered destination holder

The end-to-end path works like this:

- the source router tracks reliable packets it originated
- when a destination router delivers a reliable packet to a local handler, it emits an end-to-end
  acknowledgement tagged with its identity
- routers and relays learn the return path from the reliable packet’s ingress side and route that
  acknowledgement only where it needs to go
- the source keeps the packet in flight until all currently discovered holders have acknowledged
- if one end-to-end acknowledgement is lost, the source retransmits only toward the holders that
  are still outstanding until they respond or the retry limit is reached
- if discovery later expires one holder, the source removes that holder from the pending set and
  finishes once the remaining discovered holders are satisfied

That means reliable delivery is now verified at the application-destination boundary, not just per
hop, while still keeping reliable send non-blocking for newer packets on the same side/type lane.

For ordered reliable links, a receiver that gets packets after a gap buffers those later packets,
emits partial ACKs for them, and requests the missing sequence. Partial ACKs stop timeout-based
retransmits for packets the receiver already has, but explicit packet requests can still replay
them. When the missing sequence arrives, the buffered packets are dispatched immediately in order.

## Receiving packets

Common receive APIs:

- `rx_serialized(bytes)`
- `rx_serialized_queue(bytes)`
- `rx(packet)`
- `rx_queue(packet)`

Meaning of the variants:

- `rx_*` processes immediately in the current call
- `rx_*_queue` only enqueues work for a later `process_*` / `periodic` call
- `*_from_side(..., side_id)` tags the ingress with an explicit side id for route/discovery logic
- the non-`from_side` variants treat the input as locally-originated rather than arriving from a
  registered side

If an immediate router receive/transmit API is called from inside a side TX callback, the router
now defers that work onto its queue instead of recursively re-entering forwarding on the same
stack.

Use side-aware ingress only when you need to override the ingress side explicitly:

- `rx_serialized_from_side(bytes, side_id)`
- `rx_from_side(packet, side_id)`

## Queue processing

The common maintenance calls are:

- `process_rx_queue()`
- `process_tx_queue()`
- `process_all_queues()`
- `periodic(timeout_ms)`
- `periodic_no_timesync(timeout_ms)` when `timesync` is enabled but you want to skip it for one
  loop

What each one does:

- `process_rx_queue()` drains queued receives only
- `process_tx_queue()` drains queued transmits only
- `process_all_queues()` drains both queues without a time budget
- `process_*_with_timeout(timeout_ms)` runs the same phase with a millisecond budget; `0` means
  drain fully
- `periodic(timeout_ms)` is the normal main-loop entry point because it also polls built-in
  discovery and, when enabled, time sync before draining queues

For relays, nested `process_tx_queue*` / `process_all_queues*` calls made from inside a side TX
callback are intentionally turned into no-ops so a side callback cannot recursively drive relay TX
on the same stack.

Router and relay queue-backed state shares the compile-time `MAX_QUEUE_BUDGET` dynamically.
That includes RX work, TX work, recent packet IDs, reliable buffers/replay state, and discovery
topology. Recent packet ID caches preallocate their final storage and reserve that byte cost
immediately. If the remaining budget is exhausted, older queued state is evicted; discovery
topology eviction emits a warning in `std` builds.

## Topology export

With discovery enabled, `export_topology()` returns the router's current learned view.

- `topology.routers` contains the top-level discovered router graph
- each router entry includes the sender ID, owned endpoints, owned time-sync source IDs, and
  connected routers
- each side route also includes `announcers`, so you can see which upstream router advertised the
  exported topology

## Reserved internal endpoints

Do not register user handlers for:

- `DataEndpoint::Discovery`
- `DataEndpoint::TimeSync` when the `timesync` feature is enabled

Those endpoints are owned by the router’s built-in control traffic.

## Time sync

When the `timesync` feature is enabled, the router maintains an internal network clock and handles
`TIME_SYNC` traffic internally.

For ordinary loops, prefer `periodic(timeout_ms)` so time sync, discovery, and queue draining run
together.

See [Time-Sync](Time-Sync) for the protocol details.
