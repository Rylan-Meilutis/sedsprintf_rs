# C/C++ Usage

The C API is exposed via
C-Headers/sedsprintf.h ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/C-Headers/sedsprintf.h))
and a static library built by Cargo.

## CMake integration (recommended)

```cmake
# Example: building for an embedded target
set(SEDSPRINTF_RS_TARGET "thumbv7em-none-eabihf" CACHE STRING "" FORCE)
set(SEDSPRINTF_EMBEDDED_BUILD ON CACHE BOOL "" FORCE)

# set the sender name
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "FC26_MAIN" CACHE STRING "" FORCE)

# optional compile-time env overrides
set(SEDSPRINTF_RS_MAX_STACK_PAYLOAD "256" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_ENV_MAX_QUEUE_SIZE "65536" CACHE STRING "" FORCE)

add_subdirectory(${CMAKE_SOURCE_DIR}/sedsprintf_rs sedsprintf_rs_build)

target_link_libraries(${CMAKE_PROJECT_NAME} PRIVATE sedsprintf_rs::sedsprintf_rs)
```

Important CMake variables:

- `SEDSPRINTF_EMBEDDED_BUILD` (ON/OFF)
- `SEDSPRINTF_RS_TARGET` (Rust target triple)
- `SEDSPRINTF_RS_DEVICE_IDENTIFIER`
- `SEDSPRINTF_RS_MAX_STACK_PAYLOAD`
- `SEDSPRINTF_RS_ENV_<KEY>` for any config env var

## Manual build (no CMake)

If you want to call Cargo directly:

```
DEVICE_IDENTIFIER=FC26_MAIN cargo build --release
```

The static library will be under `target/release/` (or under `target/<triple>/release` for embedded targets).

## Minimal C example

```C
#include "sedsprintf.h"

static uint64_t now_ms(void *user) { (void)user; return 0; }
static SedsResult tx_send(const uint8_t *bytes, size_t len, void *user)
{
    (void)bytes; (void)len; (void)user;
    return SEDS_OK;
}

static SedsResult on_packet(const SedsPacketView *pkt, void *user)
{
    (void)user;
    char buf[seds_pkt_to_string_len(pkt)];
    seds_pkt_to_string(pkt, buf, sizeof(buf));
    return SEDS_OK;
}

int main(void)
{
    const SedsLocalEndpointDesc locals[] = {
        { .endpoint = SEDS_EP_SD_CARD, .packet_handler = on_packet, .user = NULL },
    };

    SedsRouter *r = seds_router_new(
        Seds_RM_Sink,
        NULL,
        NULL,
        locals,
        sizeof(locals) / sizeof(locals[0])
    );
    seds_router_add_side_serialized(r, "TX", 2, tx_send, NULL, true);

    float data[3] = {1.0f, 2.0f, 3.0f};
    seds_router_log(r, SEDS_DT_GPS_DATA, data, sizeof(data));
    seds_router_process_all_queues(r);

    seds_router_free(r);
    return 0;
}
```

On `std` builds, passing `NULL` for `now_ms_cb` makes the router use its own internal monotonic
clock. On `no_std` builds, provide a monotonic clock callback.

See c-example-code/
([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/tree/main/c-example-code))
for a more complete example. Time sync is demonstrated in c-example-code/src/timesync_example.c
([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/c-example-code/src/timesync_example.c)).
See [Time-Sync](Time-Sync) for the time sync packet flow and roles.

With `timesync` enabled, the router owns an internal network clock and handles `TIME_SYNC`
packets internally. Use `seds_router_get_network_time_ms` / `seds_router_get_network_time` to
read the current synthesized network time. Source/master nodes can seed that clock directly with
the `seds_router_set_local_network_*` functions for date-only, time-only, millisecond, or
nanosecond precision inputs.
For normal application loops, call `seds_router_periodic(...)` to run time sync, discovery, and
queue draining together. If you need to skip time sync for a cycle while keeping the feature
enabled, call `seds_router_periodic_no_timesync(...)` instead.
`seds_router_poll_timesync(...)` remains available as a lower-level non-blocking hook when you
want to manage maintenance phases manually.

With `discovery` enabled, `seds_router_poll_discovery(...)` remains available as a lower-level
hook to queue due discovery advertisements, and `seds_router_announce_discovery(...)` still forces
an immediate announce. Relays now expose `seds_relay_periodic(...)` for the normal main-loop path,
alongside the lower-level `seds_relay_poll_discovery(...)` and `seds_relay_announce_discovery(...)`
functions.

## Sending and receiving

Common calls:

- `seds_router_log` / `seds_router_log_ts`: log typed payloads.
- `seds_router_transmit_serialized_message`: send raw bytes.
- `seds_router_receive_serialized`: receive bytes immediately.
- `seds_router_rx_serialized_packet_to_queue`: enqueue for later processing.
- `seds_router_process_all_queues`: process queued RX/TX.

As of v3.0.0, most applications should call the plain receive APIs above. Side IDs are tracked
internally by the router. If you need to explicitly override ingress (custom relay or bridge),
use the side-aware variants:

- `seds_router_receive_serialized_from_side`
- `seds_router_receive_from_side`
- `seds_router_rx_serialized_packet_to_queue_from_side`
- `seds_router_rx_packet_to_queue_from_side`

Runtime side policy and routing controls are also available:

- `seds_router_remove_side`
- `seds_router_set_side_ingress_enabled`
- `seds_router_set_side_egress_enabled`
- `seds_router_set_route`
- `seds_router_clear_route`
- `seds_router_set_typed_route`
- `seds_router_clear_typed_route`
- `seds_router_set_source_route_mode`
- `seds_router_set_route_weight`
- `seds_router_set_route_priority`
- `seds_relay_remove_side`
- `seds_relay_set_side_ingress_enabled`
- `seds_relay_set_side_egress_enabled`
- `seds_relay_set_route`
- `seds_relay_clear_route`
- `seds_relay_set_typed_route`
- `seds_relay_clear_typed_route`
- `seds_relay_set_source_route_mode`
- `seds_relay_set_route_weight`
- `seds_relay_set_route_priority`

Pass `-1` as the source side to `seds_router_set_route` / `seds_router_clear_route` when you want
to control locally-originated router TX rather than traffic received from a specific side. The
same `-1` convention also applies to `seds_router_set_typed_route` /
`seds_router_clear_typed_route`. The relay route APIs use the same `-1` convention for
locally-originated discovery TX.

Typed route rules act as per-`DataType` allowlists for a given source side. If any typed rules
exist for `(src_side, ty)`, only the enabled destination sides for that type remain eligible.
That allows dedicated command, abort, or other special-purpose links while keeping ordinary
traffic on the default routing policy.

`SedsRouteSelectionMode` controls multi-path behavior:

- `Seds_RSM_Fanout`: send to all eligible paths.
- `Seds_RSM_Weighted`: send one packet on one eligible path using weighted round-robin.
- `Seds_RSM_Failover`: send only on the lowest-priority eligible path.

## Payload layout expectations

Payloads are little-endian. The schema defines element type and count. For dynamic payloads, sizes must be a multiple of
element width.

Strings must be valid UTF-8. For static strings, the payload is padded or truncated to `STATIC_STRING_LENGTH`.

## Embedded allocator hooks

Bare-metal builds must provide:

- `void *telemetryMalloc(size_t)`
- `void telemetryFree(void *)`
- `void telemetry_lock(void)`
- `void telemetry_unlock(void)`
- `void seds_error_msg(const char *, size_t)`
- `void telemetry_panic_hook(const char *, size_t)`

A simple stub is shown in `README.md` and can be adapted for your platform.

## Threading and reentrancy

The router uses internal locking, so the C API is safe to call from multiple threads if your platform supports it. In
bare-metal contexts, you may still want to serialize access around interrupts.

Do not call router/logging APIs from ISR context on RTOS targets, because platform lock hooks may block.
