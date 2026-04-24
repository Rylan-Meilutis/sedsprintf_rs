# Packet Details (Technical)

This page documents `Packet` in
src/packet.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/packet.rs))
and how payload validation works.

## Structure

`Packet` contains:

- `ty: DataType`
- `data_size: usize`
- `sender: Arc<str>`
- `endpoints: Arc<[DataEndpoint]>`
- `timestamp: u64`
- `payload: StandardSmallPayload`
- `wire_shape: Option<MessageElement>`
- `wire_target_senders: Arc<[u64]>`

The first six fields are the normal logical packet data. The last two fields are populated when the
packet came from a migration-safe wire contract.

## Effective schema element

`Packet` validates and formats itself against an effective element:

- `wire_shape`, when present
- otherwise the current runtime `message_meta(ty).element`

That distinction matters when runtime schema changes while packets are still in flight. Old packets
can continue to validate and decode against the inline wire shape they were serialized with.

## Validation rules

`Packet::new` and `Packet::validate` enforce:

- endpoints must be non-empty
- `payload.len() == data_size`
- static layouts: `data_size == count * data_type_size(...)`
- dynamic numeric and bool layouts: payload length must be a multiple of the element width
- dynamic string layouts: trailing NULs are ignored for validation, remaining bytes must be UTF-8
- dynamic binary layouts: any byte content is accepted

`Packet::new_with_wire_contract(...)` uses the same validation logic, but against the effective
wire shape when one is present.

## Element widths

For dynamic layouts, element width is derived from `MessageDataType`:

- `UInt8`, `Int8`, `Bool`: 1 byte
- `UInt16`, `Int16`: 2 bytes
- `UInt32`, `Int32`, `Float32`: 4 bytes
- `UInt64`, `Int64`, `Float64`: 8 bytes
- `UInt128`, `Int128`: 16 bytes
- `String`, `Binary`: byte-granular
- `NoData`: 0 bytes

## Packet IDs

`Packet::packet_id()` generates a stable 64-bit dedupe ID. It hashes:

- sender bytes
- message name
- endpoint names in packet order
- timestamp bytes
- `data_size` bytes
- payload bytes

It intentionally does not hash:

- ingress side
- route choice
- `wire_shape`
- `wire_target_senders`

That keeps duplicate detection tied to the telemetry payload itself rather than transport details or
migration metadata.

## Wire-contract fields

### `wire_shape`

`wire_shape` is an inline `MessageElement` copied from the serialized frame when the wire contract
carried one.

Effects:

- `validate()` uses it instead of current runtime schema when present
- `data_as_*` helpers use it to choose the effective primitive type
- `as_string()` and `header_string()` format according to that effective shape

### `wire_target_senders`

`wire_target_senders` is a frozen list of destination sender hashes carried from the wire contract.

It is not application payload data. It exists so routers and relays can keep an in-flight packet
bound to the intended destination holders while topology changes are still propagating.

## Payload helpers

`Packet` exposes typed payload helpers such as:

- `data_as_f32`, `data_as_i16`, `data_as_u64`, etc.
- `data_as_bool`
- `data_as_string`
- `data_as_binary`
- `data_as_utf8_ref`

These first check the effective message data type, then decode little-endian payload bytes.

## Formatting

`header_string()` includes:

- message name
- data size
- sender
- endpoint list
- raw timestamp value
- human-readable uptime or UTC rendering

`as_string()` adds decoded payload content using the effective wire shape when present.

Message-class labeling comes from the effective shape too:

- `Data`
- `Warning`
- `Error`

Binary payloads are formatted with `to_hex_string()`.

## Timestamp formatting

Timestamps below `1_000_000_000_000` are rendered as uptime-style durations.
Larger values are treated as epoch milliseconds and rendered as UTC date-time.

This threshold is formatting-only. The raw stored timestamp is always just `u64` milliseconds.

## Constructors

Common constructor paths:

- `Packet::new(...)`: validate against current runtime schema
- `Packet::new_with_wire_contract(...)`: validate against inline wire shape when present
- `Packet::from_*_slice(...)`: typed numeric constructors
- `Packet::from_bool_slice(...)`
- `Packet::from_string(...)`
- `Packet::from_binary(...)`
- `Packet::from_no_data(...)`

The typed constructors are convenience helpers over the same validation rules.
