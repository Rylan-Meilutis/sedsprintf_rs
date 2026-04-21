# Concepts (Non-Technical)

This page explains the core ideas using terms you can understand without reading code.

## Schema

A schema is a shared dictionary of message types and destinations. Every system in the telemetry network must use **the
same schema** so that packets can be decoded consistently.

Why this matters:

- It prevents mismatched enum values between devices.
- It ensures each packet is interpreted the same way everywhere.
- It lets you generate bindings for multiple languages from one source of truth.

## Endpoints

Endpoints are logical destinations for messages (e.g., RADIO, STORAGE, GROUND). A packet can target multiple endpoints
at once.

Endpoints help answer two questions:

- "Who should receive this message locally?"
- "Should this message be forwarded off‑device?"

The router uses endpoint rules to decide whether to forward a message:

- **Always**: send off‑device even if a local handler exists.
- **Never**: never forward; only handle locally.
- **Default**: forward only if no local handler exists for that endpoint.

## Types

A type is the logical name of a message, like GPS_DATA or BATTERY_STATUS. Each type has:

- A data layout (how bytes are interpreted).
- A class (data, warning, error).
- A list of allowed endpoints.

Types are stable IDs on the wire. Changing their order in the schema changes their IDs.

## Telemetry packet

A packet is a single unit of telemetry. It always includes:

- Sender ID (what device produced it).
- Timestamp (milliseconds).
- Type (what data it is).
- Endpoints (where it should go).
- Payload bytes (the data itself).

You can think of packets as "typed envelopes" that carry raw bytes plus enough metadata to decode them safely.

## Router

The router is the central component. It performs three main jobs:

1) **Validate** outgoing data against the schema.
2) **Dispatch** incoming packets to local handlers.
3) **Forward** packets off‑device when configured.

If you only want local logging, create a router with no sides or disable the relevant routes. Routers
and relays use the same full-mesh side model by default once sides are registered, and runtime route
controls decide which paths are allowed.

## Relay

The relay is a simple fan‑out switch. It does not know about the schema. It just forwards packets between sides while
avoiding duplicates. It is useful when you want to bridge links without decoding payloads.

With the optional `discovery` feature, routers and relays can also exchange built-in discovery packets and learn which
endpoints are reachable through which sides. When they know a route, they can forward only toward matching sides instead
of flooding every side. When they do not know a route yet, they fall back to the normal flood behavior.

## Discovery

Discovery is an optional built-in control plane, similar in spirit to time sync:

- Routers and relays exchange internal `DISCOVERY_ANNOUNCE` packets.
- They learn reachable endpoints per side and keep that information as soft state with expiry.
- Discovery traffic is adaptive: it is sent more often when topology changes and less often when the network is stable.
- Apps can export the current discovered topology for inspection.
- Link-local-only endpoints stay on software-bus / IPC links and are not advertised onto normal network links.

Discovery is an optimization, not a correctness requirement. Unknown or expired routes fall back to normal forwarding so
packets are still delivered while the network converges.

## Time sync

Time sync is an optional feature that adds built‑in packets and helpers for clock alignment between devices. A time
source announces itself, and consumers exchange request/response timestamps to estimate offset and delay.

If you need the details, see [Time-Sync](Time-Sync).

## Reliability

Reliability is opt‑in and only applies to types marked reliable in the schema. When enabled on a link, the router uses
ACKs, retransmits, and optional ordering to deliver messages more like TCP.

This is useful on lossy links, but you can disable it for transports that are already reliable.

With discovery enabled, reliable packets are sent to all currently known candidate sides for the target endpoints. In
`3.11.0` and later, the source router also keeps the packet in flight until every currently discovered holder has
returned an end-to-end acknowledgement after local delivery, and retransmits are narrowed to only the holders that are
still outstanding. If one holder later disappears from discovery, that holder is removed from the pending obligation
set so topology loss does not keep the end-to-end transaction alive forever.

For ordered reliable links, packets that arrive after a missing sequence are buffered and partially
acknowledged. That tells the sender not to retransmit those already-seen packets on timeout, while
still allowing them to be sent again if the receiver explicitly requests them.

## Dedupe

To prevent loops (especially in relay mode), the system uses a packet ID hash. If it sees the same packet again, it
drops it. This keeps multi‑hop networks from endlessly echoing packets.

## Queueing

The router and relay can queue work. This lets you:

- Receive data in an interrupt and process later.
- Batch outgoing sends to avoid spikes.

Queues are bounded by one shared `MAX_QUEUE_BUDGET` per router or relay. RX work, TX work,
recent packet IDs, reliable buffers/replay state, and learned discovery topology all draw from that
same budget. Recent packet ID caches reserve their final storage up front. If the remaining budget
is exhausted, older queued state is evicted; discovery topology evictions emit warnings in builds
that support standard error output.

## Compression

Packets can optionally compress the sender and payload. Compression is only used when it makes the packet smaller. That
means small payloads usually stay uncompressed.

## A quick checklist

- All devices must use the same schema.
- Endpoints decide local vs remote handling.
- Routers can validate, dispatch, and forward.
- Relays can fan out without decoding.
