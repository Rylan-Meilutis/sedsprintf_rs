# Troubleshooting

## Schema names are missing

- v4.0.0 does not generate application endpoint/type constants at compile time.
- Register endpoints and data types at startup, seed them from JSON, or wait for discovery sync.
- For runtime JSON seeding, use `SEDSPRINTF_RS_STATIC_SCHEMA_PATH`,
  `SEDSPRINTF_RS_STATIC_IPC_SCHEMA_PATH`, or the explicit JSON file/bytes API for your language.
- Use name lookup helpers such as `DataEndpoint::try_named(...)`,
  `seds_endpoint_get_info_by_name(...)`, or `endpoint_info_by_name(...)` before logging.

## Schema mismatch between systems

Nodes synchronize runtime schema through discovery when that feature is enabled. Compatible
endpoint/type definitions merge as nodes learn from each other. Direct registration of the same
name or ID with a different shape returns an error.

Symptoms:

- Unknown `DataType` / `DataEndpoint` errors before discovery converges.
- Registration errors for an existing type with a different size or element shape.
- Discovery warnings for conflicting schema definitions.

Fix:

- Prefer stable names and IDs for long-lived schemas.
- Seed shared required entries from JSON on every node when you need them available immediately.
- If two nodes define the same type differently, rename one of them or change the payload shape so
  the definition is identical everywhere.

## Size mismatch errors

If you see `TelemetryError::SizeMismatch`:

- Check the schema definition for the message type.
- Ensure static payloads match exact size.
- Ensure dynamic payloads are a multiple of the element width.

## UTF-8 errors for strings

String payloads must be valid UTF-8. Trailing NULs are ignored but invalid byte sequences will fail validation.

## Compression errors

If a receiver was built without the `compression` feature, it cannot decode compressed payloads. Ensure all nodes share
the same feature set or disable compression everywhere.

## Packets not forwarding

Check:

- The router has active sides and the relevant routes are enabled.
- Link-local-only endpoints stay on link-local/software-bus sides.
- Non-local endpoints depend on discovery state for selective remote routing.
- Your TX callback is installed (non-NULL) and returns OK.

## Echo loops

If you see packets bouncing endlessly:

- Ensure your side TX callback does not immediately re-inject back into the same side.
- Confirm dedupe cache sizes are large enough for your traffic patterns.

## Dropped packets or queue evictions

Queues are bounded by one shared `MAX_QUEUE_BUDGET` per router or relay. RX work, TX work,
recent packet IDs, reliable buffers/replay state, discovery topology, and runtime schema registry
memory all draw from that same budget. Recent packet ID caches reserve their final storage up
front, so a large
`MAX_RECENT_RX_IDS` leaves less budget for active packet queues. If traffic is bursty:

- Increase `MAX_QUEUE_BUDGET` or `QUEUE_GROW_STEP`.
- Process queues more frequently.
- Lower `MAX_RECENT_RX_IDS` if dedupe history is reserving too much of the shared budget.
- If you see topology eviction warnings, either increase `MAX_QUEUE_BUDGET`, reduce discovery churn,
  or process queues more often so topology/control traffic does not compete with backed-up data.

## Embedded build fails with missing symbols

Bare-metal targets must provide `telemetryMalloc`, `telemetryFree`, and `seds_error_msg`.

## macOS C system test linker target warnings

If C system tests print warnings like "object file ... built for newer 'macOS' version than being linked":

- Ensure Rust staticlib and CMake link use the same deployment target.
- In this repo, `c-system-test/CMakeLists.txt` pins `CMAKE_OSX_DEPLOYMENT_TARGET` and forwards it into Rust builds.
- If you maintain a custom CMake integration, set `CMAKE_OSX_DEPLOYMENT_TARGET` explicitly and keep it consistent.

## Python import fails

- Ensure you built the extension: `./build.py python` or `maturin develop`. (
  build.py: [source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.py))
- Verify you are using the same Python interpreter/venv used for the build.
- If runtime schema names are missing, seed/register the schema in that process; rebuilding is only
  needed after Python API changes.
