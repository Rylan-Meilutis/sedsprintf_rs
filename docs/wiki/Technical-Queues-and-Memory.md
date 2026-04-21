# Queues and Memory (Technical)

This page documents the bounded queue implementation in
src/queue.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/queue.rs))
and how it affects router and relay behavior.

## BoundedDeque

`BoundedDeque<T>` is a byte-budgeted queue used by both Router and Relay.

Key properties:

- **Byte budget**: each item reports its `byte_cost` via the `ByteCost` trait.
- **Hard element cap**: capacity never exceeds `max_elems`, derived from `size_of::<T>()`.
- **Eviction policy**: evict from the front until a new item fits in the byte budget.
- **Growth policy**: multiplicative growth using `QUEUE_GROW_STEP`.

This keeps memory use bounded and avoids unbounded `VecDeque` growth in embedded builds.

## Shared Queue Budget

`MAX_QUEUE_BUDGET` is the overall router/relay queue budget, not a separate allowance for each
internal queue.

The budget is shared dynamically across:

- router/relay RX queues
- router/relay TX queues
- relay replay queues
- recent packet ID caches used for dedupe; these preallocate
  `min(MAX_RECENT_RX_IDS * sizeof(u64), MAX_QUEUE_BUDGET)` bytes and reserve that amount immediately
- ordered reliable out-of-order receive buffers
- reliable retransmit/replay state
- learned discovery route and topology state

If TX is busy and RX is quiet, TX can use most of the budget that is not already reserved by the
recent-ID cache. If RX later becomes the pressure point, RX can take available budget back. When
multiple areas fill at the same time, eviction comes from the largest queue-backed area first so the
total stays under `MAX_QUEUE_BUDGET`.

Discovery topology also counts against this budget. In `std` builds, the router/relay emits a
warning when discovery/topology entries have to be evicted because the shared queue budget is
exhausted.

## Growth details

Growth uses a rational multiplier instead of floats:

- `float_to_ratio` clamps the multiplier (1.01 to 16.0).
- Capacity growth is `ceil(cap * grow_num / grow_den)`.
- If the queue is at its hard cap, it evicts instead of growing.

## ByteCost accounting

`ByteCost` is used throughout:

- `RouterTxItem::Broadcast` / `RouterTxItem::ToSide` include packet byte cost plus metadata.
- `QueueItem::Serialized` includes `Arc<[u8]>` overhead and length.
- `RelayRxItem` and `RelayTxItem` account for their payload sizes.
- `SmallPayload` accounts for inline vs heap sizes.

Because `ByteCost` is approximate, the `max_elems` hard cap prevents pathological growth.

## Router and Relay queues

Router:

- RX queue: holds incoming items before processing.
- TX queue: holds items queued for sending.
- Recent-ID cache: preallocated `BoundedDeque<u64>` used for dedupe.
- Reliable out-of-order and retransmit buffers.
- Discovery route/topology state when the `discovery` feature is enabled.

Relay:

- RX queue: items received from any side.
- TX queue: items to send to all other sides.
- Replay queue: explicit reliable retransmits requested by peers.
- Recent-ID cache: similar to Router.
- Reliable out-of-order buffers.
- Discovery route/topology state when the `discovery` feature is enabled.

## Configuration knobs

These values are set at compile time via
src/config.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/config.rs)):

- `STARTING_QUEUE_SIZE`
- `MAX_QUEUE_BUDGET`
- `QUEUE_GROW_STEP`
- `MAX_RECENT_RX_IDS`

They can be overridden using `build.py env:KEY=VALUE` (
build.py: [source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.py))
or `.cargo/config.toml`. (
build.py: [source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.py))

## Tuning guidance

- Increase `MAX_QUEUE_BUDGET` if you see dropped packets, reliable buffer eviction, or topology
  eviction warnings under bursty traffic.
- Increase `MAX_RECENT_RX_IDS` if you have many duplicates across links. This cache is
  preallocated, so increasing it reserves more of `MAX_QUEUE_BUDGET` immediately.
- Reduce `QUEUE_GROW_STEP` if you want smaller memory spikes.

## Failure modes

- If a single item exceeds `MAX_QUEUE_BUDGET`, it is rejected.
- If the shared queue budget is full, older queued state is evicted to make room.
- If discovery topology consumes too much of the budget, older topology entries can be evicted and
  a warning is emitted in `std` builds.
- If handlers are slow, RX queues may accumulate and evict earlier items.
