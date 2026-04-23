# Telemetry Schema

As of v4.0.0, user telemetry schema is runtime state. `DataEndpoint` and
`DataType` are stable numeric IDs on the wire, but the library no longer
generates application-specific Rust enum variants or binding constants at
compile time.

The default registry contains only built-in internal entries:

- telemetry error endpoint/type
- reliable-control packet types
- discovery endpoint/types
- time-sync endpoint/types when the `timesync` feature is enabled

Applications add user endpoints and data types at runtime through APIs, JSON
seeding, or discovery schema sync.

## Runtime IDs and Names

`DataEndpoint(pub u32)` and `DataType(pub u32)` are transparent runtime IDs.
Use names for readability and only use raw IDs where you are assigning a wire
ID intentionally.

```rust
let radio = DataEndpoint::named("RADIO");
let gps = DataType::named("GPS_DATA");

let maybe_radio = DataEndpoint::try_named("RADIO");
let maybe_gps = DataType::try_named("GPS_DATA");
```

Definitions carry:

- numeric ID
- string name
- human-readable description
- endpoint link-local flag
- data type shape, allowed endpoints, reliability mode, and priority

Lookup/export APIs include:

- `endpoint_definition(...)`
- `endpoint_definition_by_name(...)`
- `data_type_definition(...)`
- `data_type_definition_by_name(...)`
- `known_endpoints()`
- `known_data_types()`
- `export_schema()`

## Registering Schema

Rust:

```rust
let radio = register_endpoint_id_with_description(
    DataEndpoint(100),
    "RADIO",
    "Downlink radio",
    false,
)?;

register_data_type_id_with_description(
    DataType(100),
    "GPS_DATA",
    "Latitude, longitude, altitude",
    MessageElement::Static(3, MessageDataType::Float32, MessageClass::Data),
    &[radio],
    ReliableMode::Ordered,
    80,
)?;
```

Direct registration rejects conflicts:

- same endpoint ID/name with different endpoint metadata
- same data type ID/name with a different shape, endpoint set, reliability mode,
  or priority
- data types referencing endpoints that do not exist

Endpoint handler registration also creates missing endpoints in `std` builds:

```rust
EndpointHandler::new_packet_handler(DataEndpoint(250), |_pkt| Ok(()));
```

If that endpoint ID did not exist, it is registered as `ENDPOINT_250` and can be
advertised through schema discovery.

## Removing Schema

User endpoints and data types can be removed at runtime:

- `remove_endpoint(...)`
- `remove_endpoint_by_name(...)`
- `remove_data_type(...)`
- `remove_data_type_by_name(...)`

Removing an endpoint also removes user data types that reference it. Built-in
internal entries cannot be removed.

## JSON Runtime Seeding

JSON is optional runtime input. It is not used to generate user schema constants.

Host/std builds can seed from:

- `SEDSPRINTF_RS_STATIC_SCHEMA_PATH`
- `SEDSPRINTF_RS_STATIC_IPC_SCHEMA_PATH`
- Rust `register_schema_json_path(...)` / `register_schema_json_bytes(...)`
- C `seds_schema_register_json_file(...)` / `seds_schema_register_json_bytes(...)`
- Python `register_schema_json_file(...)` / `register_schema_json_bytes(...)`

Embedded builds include `telemetry_config.json` bytes only if that file exists
at build time, then decode those bytes through the same runtime parser. The
crate remains buildable/publishable without an application JSON file.

JSON shape:

```json
{
  "endpoints": [
    {
      "rust": "Radio",
      "name": "RADIO",
      "description": "Downlink radio"
    }
  ],
  "types": [
    {
      "rust": "GpsData",
      "name": "GPS_DATA",
      "description": "GPS data",
      "reliable_mode": "Ordered",
      "priority": 80,
      "class": "Data",
      "element": { "kind": "Static", "data_type": "Float32", "count": 3 },
      "endpoints": ["Radio"]
    }
  ]
}
```

Notes:

- `description` is preferred.
- Legacy `doc` is still accepted as an alias.
- Legacy `broadcast_mode = "Never"` is accepted and maps to link-local behavior.
- IPC JSON loaded through the IPC seed path is applied as link-local overlay data.

## Network Schema Sync

Discovery includes schema advertisements. When nodes connect, they can exchange
the current endpoint/type list and merge compatible definitions.

Merge behavior:

- new endpoint/type definitions are added
- equivalent definitions are kept
- ID/name conflicts are resolved deterministically so nodes converge on the same
  winner
- data types with missing endpoint dependencies are skipped until those
  endpoints are known

Direct local registration remains stricter than network merge. If local code
tries to register an existing data type with a different shape, registration
returns `BadArg`.

## Memory Budgeting

Runtime schema memory counts against the shared router/relay `MAX_QUEUE_BUDGET`.
That same budget also covers RX/TX queues, reliable replay/out-of-order state,
recent packet ID caches, and discovery topology.

If a received schema snapshot would exceed the budget, the merge is rejected and
the current registry is left unchanged.

## Payload Layouts

Schema shape still controls packet validation:

- **Static + numeric/bool**: payload size must equal `count * element_width`.
- **Dynamic + numeric/bool**: payload size must be a multiple of element width.
- **String**: dynamic UTF-8 bytes; trailing NULs are ignored for validation.
- **Binary**: raw bytes.
- **NoData**: zero-length payload.

For static `String` or `Binary` payloads, the configured limits are:

- `STATIC_STRING_LENGTH`
- `STATIC_HEX_LENGTH`

## Compatibility Checklist

For v4 deployments:

- Prefer named runtime lookup in application code: `DataEndpoint::named(...)` and
  `DataType::named(...)`.
- Seed required endpoints/types at router startup if they must always exist.
- Let discovery sync optional or peer-defined schema over time.
- Treat data type shape changes as incompatible unless you intentionally create
  a new type ID/name.
- Budget for schema memory if you expect many dynamic endpoints/types.
