# Troubleshooting

## Header or enums not updating

- Run a build that triggers
  build.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.rs)) (
  for example `cargo build` or `./build.py release`).
- Ensure
  telemetry_config.json ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/telemetry_config.json))
  is valid JSON.
- If you override the schema path, set `SEDSPRINTF_RS_SCHEMA_PATH`.
- If you use a board-local IPC overlay, also check `SEDSPRINTF_RS_IPC_SCHEMA_PATH`.

## Schema mismatch between systems

All nodes must use the exact same
telemetry_config.json ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/telemetry_config.json))
order and definitions. Mismatches cause decode errors or
undefined behavior.

Symptoms:

- Invalid DataType/DataEndpoint errors.
- Packets decode into the wrong types.

Fix:

- Redeploy the same schema and regenerated bindings everywhere.

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
recent packet IDs, reliable buffers/replay state, and discovery topology all draw from that same
budget. Recent packet ID caches reserve their final storage up front, so a large
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
- If the module loads but symbols are missing, rebuild after schema changes.
