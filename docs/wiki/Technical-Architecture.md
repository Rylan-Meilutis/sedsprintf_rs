# Architecture

This page explains how the core Rust library is organized, how telemetry moves through it, and why the data structures
look the way they do. It assumes no prior knowledge of the codebase.

## Goals and constraints

- **Runtime schema**: endpoints, message types, and payload layouts live in a runtime registry that can be seeded,
  queried, exported, and synced between nodes.
- **Compact on-the-wire representation**: a small header, endpoint bitmaps, and optional compression minimize bandwidth.
- **Embedded-friendly**: no_std support, bounded queues, and stack/heap tradeoffs.
- **Multi-language**: Rust, C, and Python expose the same runtime schema and packet APIs.
- **Deterministic behavior**: validation and dedupe rules are consistent across languages.

## Module map (what lives where)

-

src/config.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/config.rs)):
compile-time configuration values plus the runtime schema registry for `DataType` and `DataEndpoint` metadata.

-

src/lib.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/lib.rs)):
schema metadata (`MessageMeta`, `MessageElement`, `MessageDataType`, `MessageClass`).

-

src/telemetry_packet.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/telemetry_packet.rs)):
`Packet` validation, formatting, and packet IDs.

-

src/small_payload.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/small_payload.rs)):
inline-optimized payload storage (`SmallPayload`).

-

src/serialize.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/serialize.rs)):
compact wire format, ULEB128 helpers, envelope peek, packet IDs from wire.

-

src/router.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/router.rs)):
router core, queues, endpoint handlers, side-based routing.

-

src/relay.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/relay.rs)):
schema-agnostic fanout relay between sides.

-

src/queue.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/queue.rs)):
bounded deque used by router and relay.

-

src/c_api.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/c_api.rs))
and
src/python_api.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/python_api.rs)):
FFI bindings (C ABI and pyo3).

## Runtime schema and metadata pipeline

The registry starts with built-in internal endpoints/types for telemetry errors, reliable control,
discovery, and time sync. User endpoints and data types are added at runtime through direct APIs,
optional JSON seeding, handler registration, or discovery schema sync.

Core schema types:

- `DataEndpoint`: transparent runtime ID (`u32`) for logical destinations.
- `DataType`: transparent runtime ID (`u32`) for message types.
- `MessageMeta`: per-`DataType` metadata (name, element layout, allowed endpoints).
- `MessageElement`: `Static(count, MessageDataType, MessageClass)` or `Dynamic(MessageDataType, MessageClass)`.
- `MessageDataType`: primitive element type (Float32, UInt16, String, Binary, etc.).
- `MessageClass`: logical class (`Data`, `Warning`, `Error`).

Why these structures exist:

- **MessageMeta is registry-backed** so it can change as peers add or announce schema over time.
- **String lookup APIs** (`DataEndpoint::named`, `DataType::named`) keep application code readable while the wire format
  remains numeric.
- **MessageElement separates shape from data type** so the same primitive type can be used for static and dynamic
  payloads.
- **MessageClass is tied to element layout** so formatting and error generation know the intent of the message.
- **Discovery schema sync** exports endpoint/type definitions, merges compatible remote schema, and resolves conflicts
  deterministically.

## Packet (the core runtime type)

`Packet` is the validated, shareable container used by the router and FFI layers. It contains:

- `ty: DataType` (logical message type).
- `data_size: usize` (cached payload length; must match `payload.len()`).
- `sender: Arc<str>` (logical sender identifier).
- `endpoints: Arc<[DataEndpoint]>` (destinations; must be non-empty).
- `timestamp: u64` (ms; treated as uptime below 1e12, or epoch ms otherwise).
- `payload: StandardSmallPayload` (inline optimized; see below).

Validation rules enforced by `Packet::new` and `Packet::validate`:

- Endpoints list must be non-empty.
- For static layouts, payload length must be exactly `count * data_type_size`.
- For dynamic layouts:
    - Numeric/bool types: payload length must be a multiple of element width.
    - String types: trailing NULs are ignored and remaining bytes must be valid UTF-8.
    - Binary types: no UTF-8 requirement (raw bytes).

Packet IDs are **not** serialized. `Packet::packet_id` hashes:

- sender bytes
- message name (`DataType` as string)
- endpoint names (in order)
- timestamp + data_size (little-endian)
- payload bytes

This produces the same ID across different links, which enables de-duplication.

### SmallPayload and inline storage

`SmallPayload<INLINE>` stores short payloads directly on the stack and spills to `Arc<[u8]>` when larger. The inline
size is controlled by `MAX_STACK_PAYLOAD` via `define_stack_payload!`, and the default inline capacity is 64 bytes.

Why this matters:

- Small telemetry values avoid heap allocation and `Arc` traffic.
- Larger payloads still have cheap clone semantics because they are `Arc`-backed.

## Serialization and wire format

The compact v2 wire format is implemented in
src/serialize.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/serialize.rs)).
A packet is encoded as:

```
[FLAGS: u8]
    bit0: payload compressed
    bit1: sender compressed
[NEP: u8]                      // number of endpoints (bits set)
VARINT(ty: u32 as u64)          // ULEB128
VARINT(data_size: u64)          // logical (uncompressed) payload size
VARINT(timestamp_ms: u64)
VARINT(sender_len: u64)         // logical sender length
[VARINT(sender_wire_len: u64)]  // only if sender compressed
ENDPOINTS_BITMAP               // 1 bit per DataEndpoint discriminant
SENDER BYTES                   // raw or compressed
PAYLOAD BYTES                  // raw or compressed
```

Design choices:

- **ULEB128 varints** minimize size for small values.
- **Endpoint bitmap** avoids repeated endpoint IDs; size is based on the highest runtime endpoint
  ID known to the schema registry.
- **Sender/payload compression** uses `zstd` (`zstd-safe`) and is applied only when it makes the payload smaller.

`packet_id_from_wire` parses only as much as needed to compute the same packet ID as `Packet::packet_id`. It
always hashes **decompressed** sender/payload bytes, so dedupe works across compressed and uncompressed links.

`TelemetryEnvelope` is a header-only view that can be obtained by `peek_envelope` without copying the payload.

## Queues and bounded memory

Router and relay queue storage is built on `BoundedDeque<T>`:

- Byte-budgeted: each item reports a `byte_cost` via the `ByteCost` trait.
- Hard element cap: capacity never exceeds `max_elems` (computed from `size_of::<T>()`).
- Eviction policy: pop from the front until new item fits in byte budget.
- Growth policy: multiplicative growth controlled by `QUEUE_GROW_STEP`.

The public `MAX_QUEUE_BUDGET` is shared dynamically across router/relay RX queues, TX queues,
recent-ID caches, reliable replay/out-of-order buffers, discovery topology state, and runtime
schema registry memory. Recent-ID caches preallocate their final storage at construction and
reserve that amount from the shared budget immediately. This design keeps memory use bounded and
avoids unbounded `VecDeque` growth in embedded targets while letting the active part of the system
use available queue budget when other internal queues are quiet.

## Router architecture

The router is the main API for logging, receiving, dispatching, and (optionally) relaying.

Key structures:

- `RouterConfig`: holds local `EndpointHandler` definitions in an `Arc<[EndpointHandler]>`.
- `EndpointHandler`: packet or serialized handler for a specific `DataEndpoint`.
- `RouterSideId`: identifies a named side (UART/CAN/RADIO/etc.).
- `RouterItem`: either `Packet` or serialized bytes.

Receive flow:

1) RX bytes or packet are processed immediately or queued (`rx_*` vs `rx_*_queue`).
2) Reliable side headers are handled first when the ingress side has reliable delivery enabled.
3) Packet ID is computed. For serialized bytes, the router tries `packet_id_from_wire` and falls back to hashing raw
   bytes if needed.
4) If the packet ID is already in the recent cache, it is dropped.
5) Local endpoint handlers are invoked.
6) The router forwards the packet according to the current route rules and discovery/path-selection state.

Forwarding is driven by:

- the active side ingress/egress policy
- route overrides and typed route overrides
- discovery-learned endpoint reachability
- link-local side scope
- route-selection policy (`Fanout`, `Weighted`, `Failover`)

Transmit flow:

- `log*` builds a new `Packet` from typed data, validates sizes, and serializes it.
- `tx*` sends a pre-built `Packet` (validated) or serialized bytes.
- Queue variants (`*_queue`) defer the work until `process_*_queue`.

Error handling:

- Local handler failures are retried (`MAX_HANDLER_RETRIES`).
- If a handler ultimately fails, the packet ID is removed from the dedupe cache so a resend can be processed later.
- The router emits a `TelemetryError` packet for local handlers when possible.

## Relay architecture

`Relay` is schema-agnostic and purely forwards packets between named sides (UART/CAN/RADIO/etc.).

- Each side registers a TX handler for serialized bytes or full packets.
- Each side can opt out of reliable sequencing/ACKs (useful for TCP links).
- The relay maintains RX/TX queues and a recent-ID cache like the router.
- Fanout clones the `Arc` for payload sharing; it does not decode unless needed for a handler.
- Dedupe ignores the source side so duplicates from different links are dropped.

## Thread safety and time

Routers and relays use an internal `RouterMutex` so that most APIs can take `&self` while still protecting shared state.
The clock source is injected via the `Clock` trait so embedded builds can supply their own timing source.

## End-to-end lifecycle (summary)

```
Producer                      Router/Relay                         Consumer
--------                      -------------                         --------
log()/tx()  -> validate -> serialize -> tx(bytes, link) -> transport -> rx_serialized()
rx(bytes)   -> deserialize -> dedupe -> handlers -> (optional) relay forward
```

If you need build-time or schema details, see [Build-and-Configure](Build-and-Configure) and
[Technical-Telemetry-Schema](Technical-Telemetry-Schema).
