# Wire Format (Technical)

This page documents the compact v2 wire format implemented in
src/serialize.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/serialize.rs)).

## Goals

- Compact prelude with a fixed 2-byte start.
- ULEB128 integers for small-on-the-wire metadata.
- Endpoint bitmaps instead of repeated endpoint IDs.
- Optional sender and payload compression.
- CRC32 trailer for frame integrity.
- A compact in-flight wire contract so packets already on the wire remain routable and decodable
  while topology and runtime-schema changes are still propagating.

## Frame layout

```text
[FLAGS: u8]
    bit0: payload compressed
    bit1: sender compressed
    bit2: wire contract present
    bit3..7: reserved
[NEP: u8]                         // number of set bits in the endpoint bitmap
VARINT(ty: u32 as u64)            // ULEB128
VARINT(data_size: u64)            // logical payload size after decompression
VARINT(timestamp_ms: u64)
VARINT(sender_len: u64)           // logical sender length after decompression
[VARINT(sender_wire_len: u64)]    // only when sender is compressed
ENDPOINT_BITMAP                   // fixed width, 1 bit per possible endpoint ID
SENDER_BYTES                      // raw or compressed
[VARINT(contract_len: u64)]       // only when bit2 is set
[WIRE_CONTRACT_BYTES]
[RELIABLE_HEADER]                 // present for reliable schema types or when contract says so
PAYLOAD_BYTES                     // raw or compressed
[CRC32: u32 LE]                   // checksum of every prior byte in the frame
```

## Flags

Top-level frame flags:

- `0x01`: payload compressed
- `0x02`: sender compressed
- `0x04`: wire contract present

Reliable-header flags:

- `0x01`: ACK-only reliable control frame
- `0x02`: reliable but unordered
- `0x80`: unsequenced best-effort reliable wrapper

ACK-only reliable control frames are emitted by the router or relay reliable layer. They are not
valid application `Packet` values and are consumed before normal packet deserialization.

## Endpoint bitmap

The endpoint bitmap is fixed-width for the build, not sized from the currently registered runtime
schema.

- `EP_BITMAP_BITS = MAX_VALUE_DATA_ENDPOINT + 1`
- `EP_BITMAP_BYTES = ceil(EP_BITMAP_BITS / 8)`

Packing is LSB-first within each byte. `NEP` is the popcount of the bitmap and is used as a sanity
check during decode.

Important implications:

- The bitmap width is stable for a given build.
- Removing or adding runtime schema entries does not change bitmap width.
- New packets use the current endpoint IDs, but old packets still parse because the bitmap layout is
  fixed and the contract can carry extra delivery/decode metadata when needed.

## Wire contract

When `FLAG_WIRE_CONTRACT` is set, the frame carries a compact contract immediately after the sender
bytes.

```text
VARINT(contract_len)
[contract flags: u8]
[wire shape bytes]                // if contract flag 0x02 set
[target count: ULEB128]           // if contract flag 0x01 set
[target sender hash 0: u64 LE]
[target sender hash 1: u64 LE]
...
```

Contract flags:

- `0x01`: explicit frozen destination sender hashes are present
- `0x02`: inline payload shape is present
- `0x04`: a reliable header is present even if current schema lookup would no longer imply that

### Inline payload shape

The shape is packed compactly:

```text
[packed: u8]
    bits 0..3: MessageDataType code
    bits 4..5: MessageClass code
    bit 6: static-layout flag
[static_count: ULEB128]           // only when bit 6 is set
```

This lets a packet remain decodable after runtime schema changes such as:

- the current type layout changing
- the type being removed from the local runtime registry

In those cases deserialization constructs the `Packet` against the inline wire shape instead of the
current registry definition.

### Frozen destination sender hashes

The target list contains `u64` sender hashes for the destination holders the source intended when
that packet was serialized.

Routers and relays use that list to:

- keep in-flight packets pointed only at the intended holders
- avoid delivering a packet to the wrong board while discovery/topology updates are still
  converging
- allow new packets to immediately use the latest topology while old packets continue with the
  original delivery contract

## Reliable header

The reliable header is a fixed 9-byte block:

```text
[REL_FLAGS: u8]
[SEQ: u32 LE]
[ACK: u32 LE]
```

For normal reliable data frames this header appears after the optional wire contract.

Reliable control traffic now primarily uses built-in internal packet types such as:

- `ReliableAck`
- `ReliablePartialAck`
- `ReliablePacketRequest`

Those are router/relay-owned control packets. Applications should not model them as user endpoint
traffic.

## Varints

All integer metadata fields use unsigned LEB128.

This includes:

- type ID
- payload size
- timestamp
- sender lengths
- contract length
- target count
- static wire-shape count

`read_uleb128` rejects values that require more than 10 bytes.

## Compression

Sender and payload compression are evaluated independently.

- Compression is only used when it is actually smaller on the wire.
- The logical uncompressed length is still transmitted in the frame metadata.
- Decode validates the decompressed size against that logical length.

Compressed sender and payload bytes use the crate's `payload_compression` backend.

## Decode flow

High-level decode order:

1. Verify CRC32.
2. Parse the fixed prelude and varints.
3. Expand the fixed-width endpoint bitmap.
4. Decode sender bytes.
5. Decode the optional wire contract.
6. Decode the reliable header if current schema or contract says it is present.
7. Decode the payload bytes.
8. Construct `Packet::new_with_wire_contract(...)`.

The contract is what keeps decode and delivery stable across runtime topology/schema churn.

## Envelope peek

`peek_envelope(...)` parses only the envelope and returns:

- `ty`
- `endpoints`
- `sender`
- `timestamp_ms`
- `wire_shape`
- `target_senders`

`peek_frame_info(...)` extends that with the reliable header when present.

These helpers are what the router and relay use to make routing and reliable-layer decisions without
fully decoding payload data.

## Packet ID from wire

`packet_id_from_wire(...)` computes the same ID as `Packet::packet_id()` from a serialized frame.
It hashes:

- sender bytes after decompression
- message name
- endpoint names in ascending bitmap order
- timestamp
- logical payload size
- payload bytes after decompression

This makes dedupe stable across compressed and uncompressed links.

The wire contract is intentionally not part of the packet ID. It preserves delivery/decode intent
for in-flight packets without changing duplicate detection for the underlying telemetry payload.

## CRC32 trailer

Every frame ends with a 4-byte little-endian CRC32 computed over all preceding bytes.

On CRC failure the frame is rejected before normal decode. Recovery behavior after that depends on
whether the surrounding router/relay reliable layer is active for that hop.

## Common decode failures

- short prelude or short read
- CRC32 mismatch
- invalid endpoint bit set
- malformed or overlong ULEB128
- bad wire-shape type/class/count
- malformed wire-contract target list
- reliable control frame passed into full packet decode
- decompression failure or decompressed-size mismatch
- sender not valid UTF-8
