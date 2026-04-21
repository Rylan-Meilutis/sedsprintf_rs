# SEDSPRINTF_RS

An implementation of the `sedsprintf` telemetry protocol in Rust.

---

## Authors

- [@Rylan-Meilutis](https://github.com/rylan-meilutis) (Original Author, Maintainer, and co-creator of the protocol)
- [@origami-yoda](https://github.com/origami-yoda) (Co-creator of the protocol and co-author of the original C++
  implementation)

---

## About

This library started out as a rewrite of the original sedsprintf C++ library
found [here](https://github.com/University-at-Buffalo-SEDS/sedsprintf).

After the initial rewrite, many improvements were made to the rust implementation including better safety, easier
extension, and improved performance.
This caused the C++ implementation to be rewritten to keep feature parity with the rust version.
After about of month of this, we decided that we were no longer going to use the C++ version, and thus the project was
archived and is no longer being maintained.
With the Rust version being the sole implementation, we have continued to improve it and add new features like python
bindings, packet compression, and a bitmap for endpoints to further reduce packet size.
This library is now being used in multiple projects including embedded code on the rocket and on the rust based ground
station. Sedsprintf_rs is now capable of acting as a new network, passing telemetry data to endpoints across hardware
and software networks (uart, can ethernet, etc.) and across differing platforms and protocols (tcp, udp, etc.).

---

## Overview

This library provides a safe and efficient way to handle telemetry data including serializing, routing, and converting
into strings for logging. The purpose of this library is to provide an easy and consistent way to create and transport
any data to any destination without needing to implement the core routing on every platform, the library handles the
creation and routing of the data while allowing for easy extension to support new data types and destinations. The user
of the library is responsible for implementing 2 core functions and one function per local data endpoint.
The core functions are as follows:

- A function to send raw bytes to the all other nodes (e.g. UART, SPI, I2C, CAN, etc.)
- A function to receive raw bytes from all other nodes and passes the packet to the router
  (e.g. UART, SPI, I2C, CAN, etc.)
- A function to handle local data endpoints (e.g. logging to console, writing to file, sending over radio, etc.)
  (Note: each local endpoint needs its own function)

Sedsprintf_rs also provides helpers to convert the telemetry data into strings for logging purposes.
The library also handles the serialization and deserialization of the telemetry data.

Sedsprintf_rs is platform-agnostic and can be used on any platform that supports Rust. The library is primarily designed
to be used in embedded systems and used by a C program, but can also be used in desktop applications and other rust
codebases.

Sedsprintf_rs also supports python bindings via pyo3. to use you need maturin installed to build the python package.

With the optional `discovery` feature, routers and relays can exchange built-in discovery packets, learn which
endpoints are reachable through which sides, adapt the announce rate as the topology changes, and export a live topology
snapshot for inspection. When `timesync` is also enabled, discovery can advertise concrete time source sender IDs so
`TIME_SYNC` requests prefer exact source paths instead of generic endpoint flooding. When a route is known, forwarding
becomes more selective; when it is not known, the system falls back to ordinary flooding. `DISCOVERY` and `TIME_SYNC`
are reserved internal router endpoints: applications can use the discovery and time-sync APIs, but must not register
local endpoint handlers for those endpoints or try to override their built-in handling.

Queue memory is bounded by the compile-time `MAX_QUEUE_BUDGET`. Router and relay internals share that budget
dynamically across RX work, TX work, reliable replay/out-of-order buffers, recent packet ID tracking, and learned
discovery topology state. The recent packet ID cache preallocates its final storage because it is expected to fill
during normal operation, so its reserved bytes come out of the shared budget immediately. If one active queue area is
idle, another can use more of the remaining budget; if several areas fill at once, older queued state is evicted so
total queue-owned memory stays bounded.

Reliable delivery uses internal ACK/request control packets. Ordered reliable receivers buffer out-of-order packets,
partial-ACK packets that arrived after a gap, request the missing sequence, and then release the buffered run as soon as
the gap is filled. A partial ACK suppresses timeout retransmission for that exact packet, but an explicit packet request
can still retransmit it later.

The size of the header in a serialized packet is around 20 bytes (the size will change based on the total number of
endpoints in your system and the length of the sender string), plus a 4-byte CRC32 trailer. As a rough example, a packet
containing three floats is on the order of mid-30s bytes total. This small size makes it ideal for use in low bandwidth
environments.

---

## Recent changelog milestones

## Version 3.12.0 highlights

- Router and relay queue-backed state now shares one dynamic `MAX_QUEUE_BUDGET` instead of using
  isolated per-queue caps.
- `MAX_QUEUE_SIZE` has been renamed to `MAX_QUEUE_BUDGET`; the old environment name remains
  accepted as a legacy alias.
- Recent packet ID caches, reliable replay/out-of-order buffers, and discovery topology state now
  count against the same shared budget, with topology eviction warnings in `std` builds.
- Ordered reliable receive paths now partial-ACK out-of-order packets to reduce timeout
  retransmission traffic while still allowing explicit packet requests.
- Router and relay side-TX contention is now retried as transient backpressure instead of surfacing
  intermittent handler failures.
- Full changelog: [CHANGELOG.md](./CHANGELOG.md)

## Version 3.11.1 highlights

- Discovery now propagates a full router graph with `DISCOVERY_TOPOLOGY`, so routers and relays
  keep track of which sender IDs own which endpoints and how remote routers connect to each other.
- `export_topology()` now includes router-level topology plus per-side announcer detail instead of
  only aggregated reachable endpoint lists.
- Topology export is now available to clients across all supported surfaces:
  Rust `export_topology()`, Python `Router.export_topology()` / `Relay.export_topology()`, and C
  JSON exports via `seds_router_export_topology*` / `seds_relay_export_topology*`.
- Full changelog: [CHANGELOG.md](./CHANGELOG.md)

## Version 3.11.0 highlights

- Removed `RouterMode` from the active router model. Routers now use the same runtime routing-rule
  model as relays, and with no explicit route rules they default to a full forwarding mesh.
- Discovery-driven multi-path routing now defaults to adaptive load balancing for normal traffic,
  while reliable discovered traffic still fans out across all known candidate paths.
- Reliable delivery is now end-to-end verified in addition to the existing per-link ACK/retransmit
  layer. Destination routers emit directed end-to-end acknowledgements, and discovery-informed
  return-path learning routes those ACKs only toward the source instead of flooding them.
- When a discovered destination holder disappears from topology, the source retires that holder
  from the in-flight obligation set instead of replaying forever toward a vanished board.
- Relays also prune their learned holder-ACK state against discovery expiry so stale confirmations
  do not keep affecting later discovered routing choices.
- Reliable streams still stay non-blocking while those end-to-end acknowledgements are outstanding.
- Added expanded testing documentation covering unit tests, Rust system tests, C system tests,
  local coverage reporting, and the new end-to-end reliability regression tests.
- Full changelog: [CHANGELOG.md](./CHANGELOG.md)

## Version 3.10.0 highlights

- Reliable delivery in both `Router` and `Relay` now uses built-in internal
  `RELIABLE_ACK` and `RELIABLE_PACKET_REQUEST` packet types instead of wire-only ACK-only frames.
- Reliable streams no longer block on one missing reliable packet. Ordered gaps are requested
  explicitly, out-of-order packets are buffered, and retransmits are requeued with elevated
  priority.
- This improves recovery on asymmetric links and multi-destination fanout where different boards
  progress at different rates.
- Full changelog: [CHANGELOG.md](./CHANGELOG.md)

## Version 3.9.1 highlights

- Reserved the built-in `DISCOVERY` and `TIME_SYNC` endpoints for router-owned control traffic.
- User handlers can no longer shadow internal discovery or time-sync behavior through Rust or C
  configuration APIs.
- Queue timeout handling was tightened so TX/RX work shares nonzero budgets more predictably.
- Full changelog: [CHANGELOG.md](./CHANGELOG.md)

## Version 3.0.0 highlights

- Introduced internal router-side tracking so most applications can use the plain RX APIs and only
  opt into side-aware ingress when they actually need it.
- Added TCP-like reliable delivery for schema types marked `reliable` or `reliable_mode`, with
  ACKs, retransmits, and optional ordering.
- This established the modern router/reliability model that later releases expanded.
- Full changelog: [v2.4.0...v3.0.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v2.4.0...v3.0.0)

## Version 1.0.0 highlights

- First stable release with routing, serialization, and packet creation across Rust, C, and Python.
- Marked the API as stable and established the base wire-format and packet model the later versions
  built on.

---

## Building

To build the library in a C project, just include the library as a submodule or subtree and link it in your
cmakelists.txt as shown below.
For other build systems, you can build the library as a static or dynamic library using cargo and link it to your
project.

Building with python bindings can be done with the build script on posix systems:

```
./build.py release maturin-develop
```

When building in an embedded environment the library will compile to a static library that can be linked to your C code.
this library takes up about 100kb of flash and does require heap allocation to be available through either freertos, or
by creating shims that expose pvPortMalloc and vPortFree.

### build.py usage

```
./build.py [OPTIONS]

Options:
  release                 Build in release mode.
  check                   Run cargo clippy with -D warnings for default, python, and embedded builds.
  test                    Run the clippy checks, cargo tests, a short Criterion benchmark smoke pass, and also validate python plus embedded builds when the cross C toolchain exists.
  embedded                Build for the embedded target (enables embedded feature).
  python                  Build with Python bindings (enables python feature).
  timesync                Build with time sync helpers (enables timesync feature).
  maturin-build           Run maturin build with the .pyi .gitignore hack.
  maturin-develop         Run maturin develop with the .pyi .gitignore hack.
  maturin-install         Build wheel and install it with uv pip install.
  target=<triple>         Set Rust compilation target (e.g. target=thumbv7em-none-eabihf).
  device_id=<id>          Set DEVICE_IDENTIFIER env var for the build.
  schema_path=<path>      Set SEDSPRINTF_RS_SCHEMA_PATH for the build.
  ipc_schema_path=<path>  Set SEDSPRINTF_RS_IPC_SCHEMA_PATH for a board-local IPC overlay.
  max_queue_budget=<n>    Set MAX_QUEUE_BUDGET for the shared router/relay queue budget.
  max_recent_rx_ids=<n>   Set MAX_RECENT_RX_IDS for the preallocated recent-ID cache.
  max_stack_payload=<n>   Set MAX_STACK_PAYLOAD for define_stack_payload!(env="MAX_STACK_PAYLOAD", ...).
  env:KEY=VALUE           Set arbitrary environment variable(s) for the build (repeatable).
```

Examples:

```
./build.py release
./build.py check
./build.py check release
./build.py embedded release target=thumbv7em-none-eabihf device_id=FC
./build.py python
./build.py test release
./build.py maturin-install max_recent_rx_ids=256 env:MAX_STACK_PAYLOAD=128
```

## Dependencies

- Rust → https://rustup.rs/
- CMake
- A C++ compiler
- A C compiler

## Performance benchmarking

Criterion benchmarks are available through Cargo benches. The current benchmark targets exercise packet construction,
serialization, header peeking, deserialization, and router/relay flows that mirror the Rust system-test path under the
default host feature set.

Run:

```bash
cargo bench --bench packet_paths
cargo bench --bench router_system_paths
```

If you want profiler-friendly output while iterating locally:

```bash
cargo bench --bench packet_paths -- --profile-time=5
```

`./build.py test` now starts with the same strict clippy checks as `./build.py check`, then runs:

- `cargo test --features timesync`, which includes the unit tests in `src/tests.rs`, the Rust system tests under
  `tests/rust-system-test/`, and the C integration tests under `tests/c-system-test/`
- a short Criterion smoke pass for `packet_paths` and `router_system_paths`
- a `cargo build` validation for the `python` feature
- a `cargo build` validation for the `embedded` feature when a matching cross C toolchain is available

The benchmark smoke pass uses Cargo `--profile release`. The C system-test harness waits for all asserted endpoint hits
before exiting, so it does not fail early while one side is still draining forwarded traffic.

Coverage is regression-oriented rather than percentage-gated in CI today. If you want a local line/branch coverage
number, the supported path is:

```bash
cargo llvm-cov --features timesync --workspace --html
```

That writes an HTML report under `target/llvm-cov/html/` when `cargo-llvm-cov` is installed.

More detail on the test layers, what each suite covers, and the intended commands is in
[docs/wiki/Testing.md](./docs/wiki/Testing.md).

## Usage

### Linking from a C/C++ CMake project

```
# Example: building for an embedded target
set(SEDSPRINTF_RS_TARGET "thumbv7m-none-eabi" CACHE STRING "" FORCE)
set(SEDSPRINTF_EMBEDDED_BUILD ON CACHE BOOL "" FORCE)

# Optional: always build the Rust crate in release mode, even if the parent CMake
# configuration is Debug. Useful when your top-level project stays Debug but you
# want an optimized telemetry library.
# set(SEDSPRINTF_RS_FORCE_RELEASE ON CACHE BOOL "" FORCE)

# set the sender name
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "FC26_MAIN" CACHE STRING "" FORCE)

# optional compile-time env overrides
set(SEDSPRINTF_RS_MAX_STACK_PAYLOAD "256" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_MAX_QUEUE_BUDGET "65536" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_MAX_RECENT_RX_IDS "256" CACHE STRING "" FORCE)

# Use the provided CMake glue
add_subdirectory(${CMAKE_SOURCE_DIR}/sedsprintf_rs sedsprintf_rs_build)

# Optional: prefer static linking even on host builds
# set(SEDSPRINTF_RS_PREFER_DYNAMIC OFF CACHE BOOL "" FORCE)

# Link against the imported target
target_link_libraries(${CMAKE_PROJECT_NAME} PRIVATE sedsprintf_rs::sedsprintf_rs)
```

Host CMake builds now prefer the shared Rust library when supported. Embedded builds still use the static library.
If you want the Rust crate to use the release profile regardless of the parent CMake config,
set `SEDSPRINTF_RS_FORCE_RELEASE=ON` before `add_subdirectory(...)`. Otherwise the wrapper follows
`CMAKE_BUILD_TYPE` for single-config generators and defaults to debug for Debug builds.

- Configure telemetry schema via `telemetry_config.json` (endpoints + message types). The Rust enum metadata is
  generated
  from this JSON by `define_telemetry_schema!` in `src/config.rs`.
  NOTE: (ON EVERY SYSTEM THIS LIBRARY IS USED, THE CONFIG ENUMS MUST BE THE SAME OR UNDEFINED BEHAVIOR MAY OCCUR). So
  for
  most applications I would recommend making a fork and setting the config values you need for your application.

---

## Setting the device / platform name

Each build of `sedsprintf_rs` embeds a **device identifier** which appears in every telemetry packet header.

Rust resolves it using:

```
pub const DEVICE_IDENTIFIER: &str = match option_env!("DEVICE_IDENTIFIER") {
    Some(v) => v,
    None => "TEST_PLATFORM",
};
```

### Set it globally using `.cargo/config.toml` (recommended)

Create:

```
# .cargo/config.toml
[env]
DEVICE_IDENTIFIER = "GROUND_STATION_26"
```

After this, any `cargo build`, `cargo run`, or CI build will embed `"GROUND_STATION_26"` automatically.

No build script changes required.

---

### Setting the name from CMake

```
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "FC26_MAIN" CACHE STRING "" FORCE)
```

Note: This must be set **before** including the sedsprintf_rs CMake as a subdirectory.

Typical examples:

```cmake
# Flight computer firmware
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "FC26_MAIN" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_TARGET "thumbv7em-none-eabihf" CACHE STRING "" FORCE)
set(SEDSPRINTF_EMBEDDED_BUILD ON CACHE BOOL "" FORCE)
set(SEDSPRINTF_RS_MAX_STACK_PAYLOAD "256" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_MAX_QUEUE_BUDGET "65536" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_MAX_RECENT_RX_IDS "256" CACHE STRING "" FORCE)

# or

# Ground station app
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "GS26" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_TARGET "" CACHE STRING "" FORCE)
set(SEDSPRINTF_EMBEDDED_BUILD OFF CACHE BOOL "" FORCE)
set(SEDSPRINTF_RS_MAX_QUEUE_BUDGET "65536" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_MAX_RECENT_RX_IDS "256" CACHE STRING "" FORCE)
```

### Manually via build.py

```bash
# Host build
./build.py release device_id=GROUND_STATION
# Embedded build
./build.py embedded release target=thumbv7em-none-eabihf device_id=FC
```

---

## Telemetry config (JSON + GUI editor)

The telemetry schema lives in `telemetry_config.json` and drives the generated `DataEndpoint` and `DataType` enums.
You can edit it directly or use the GUI editor:

```bash
./telemetry_config_editor.py
```

The editor auto-discovers the base JSON path from `src/config.rs` (or `SEDSPRINTF_RS_SCHEMA_PATH`) and the IPC overlay
path from `SEDSPRINTF_RS_IPC_SCHEMA_PATH`. It can switch between the shared base schema and the board-local IPC overlay
and edit/save them independently.

For board-local IPC/software-bus endpoints, keep the shared schema fixed and provide a second JSON file through
`SEDSPRINTF_RS_IPC_SCHEMA_PATH` (or `./build.py ipc_schema_path=path/to/ipc_config.json`). That overlay is merged at
build time. Endpoints from the IPC overlay are treated as link-local automatically; endpoints from the base schema are
treated as non-link-local automatically.

Note: `TelemetryError` (data type and endpoint) is built-in and must not appear in the JSON schema.

Note: The editor uses Tkinter. On some Linux distros you may need to install it
(e.g. `sudo apt install python3-tk`).

Example `telemetry_config.json`:

```json
{
  "endpoints": [
    {
      "rust": "Radio",
      "name": "RADIO",
      "doc": "Downlink radio"
    },
    {
      "rust": "SdCard",
      "name": "SD_CARD",
      "doc": "Onboard logging"
    }
  ],
  "types": [
    {
      "rust": "GpsData",
      "name": "GPS_DATA",
      "doc": "Lat/Lon/Alt",
      "class": "Data",
      "element": {
        "kind": "Static",
        "data_type": "Float32",
        "count": 3
      },
      "endpoints": [
        "Radio",
        "SdCard"
      ]
    }
  ]
}
```

---

## Example CMakeLists.txt

```cmake
cmake_minimum_required(VERSION 3.22)
project(my_app C CXX)

add_executable(my_app
    src/main.c
)

# ---- sedsprintf_rs configuration ----
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "FC26_MAIN" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_TARGET "thumbv7em-none-eabihf" CACHE STRING "" FORCE)
set(SEDSPRINTF_EMBEDDED_BUILD ON CACHE BOOL "" FORCE)
set(SEDSPRINTF_RS_MAX_STACK_PAYLOAD "256" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_MAX_QUEUE_BUDGET "65536" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_MAX_RECENT_RX_IDS "256" CACHE STRING "" FORCE)

# Add the submodule/subtree root (adjust path as needed)
add_subdirectory(${CMAKE_SOURCE_DIR}/sedsprintf_rs sedsprintf_rs_build)

target_link_libraries(my_app PRIVATE sedsprintf_rs::sedsprintf_rs)
```

---

## Using this repo as a subtree

```
git remote add sedsprintf-upstream https://github.com/Rylan-Meilutis/sedsprintf_rs.git
git fetch sedsprintf-upstream

git config subtree.sedsprintf_rs.remote sedsprintf-upstream
git config subtree.sedsprintf_rs.branch main

git subtree add --prefix=sedsprintf_rs sedsprintf-upstream main
```

To Switch branches:

```bash
git config subtree.sedsprintf_rs.branch <the-new-branch>
```

Update:

```bash
git subtree pull --prefix=sedsprintf_rs sedsprintf-upstream main \
    -m "Merge sedsprintf_rs upstream main"
```

Helper scripts:

```bash
./scripts/subtree_update_no_stash.py
./scripts/subtree_update.py            # stash → update → stash-pop
```

---

## Using this repo as a submodule

If you prefer a **submodule** instead of a subtree:

```bash
git submodule add -b main https://github.com/Rylan-Meilutis/sedsprintf_rs.git sedsprintf_rs

git config submodule.sedsprintf_rs.branch main   # (or dev, etc.)
```

Initialize:

```bash
git submodule update --init --recursive
```

Update using helper scripts:

The scripts:

- read `submodule.sedsprintf_rs.branch`
- fetch `origin/<branch>`
- fast-forward the submodule repo
- stage & commit the updated submodule pointer in the parent repo

---

## Embedded allocator + lock hook examples (C)

For embedded (`--features embedded`) builds, provide these symbols:

- `void *telemetryMalloc(size_t)`
- `void telemetryFree(void *)`
- `void telemetry_lock(void)`
- `void telemetry_unlock(void)`
- `void seds_error_msg(const char *, size_t)`
- `void telemetry_panic_hook(const char *, size_t)`

Notes:

- `telemetry_lock`/`telemetry_unlock` must be recursive-safe.
- Do not call router/logging APIs from ISR context (hooks may block).
- Keep allocator non-blocking/fail-fast on RTOS targets (`NO_WAIT` style).

```C
// telemetry_hooks.c
#include <stddef.h>
#include <stdlib.h>
#include <stdio.h>

/*
 * Rust expects these functions to exist for heap allocations:
 *
 *   void *telemetryMalloc(size_t);
 *   void telemetryFree(void *);
 *   void telemetry_lock(void);
 *   void telemetry_unlock(void);
 *   void seds_error_msg(const char *, const size_t);
 *   void telemetry_panic_hook(const char *, const size_t);
 *
 */

void telemetry_lock(void)
{
    /* Optional on bare metal / single-threaded targets. */
}

void telemetry_unlock(void)
{
    /* Optional on bare metal / single-threaded targets. */
}

void *telemetryMalloc(size_t xSize)
{
    if (xSize == 0) {
        xSize = 1;
    }
    return malloc(xSize);
}

void telemetryFree(void *pv)
{
    free(pv);
}

void seds_error_msg(const char *str, const size_t len)
{
    // Implement your logging mechanism here, for example:
    fwrite(str, 1, len, stderr);
    fwrite("\n", 1, 1, stderr);
}

void telemetry_panic_hook(const char *str, const size_t len)
{
    // Called from Rust panic handler in embedded/no_std builds.
    fwrite("PANIC: ", 1, 7, stderr);
    fwrite(str, 1, len, stderr);
    fwrite("\n", 1, 1, stderr);
}
```

### FreeRTOS example

```C
// telemetry_hooks_freertos.c
#include "FreeRTOS.h"
#include "semphr.h"
#include <stddef.h>
#include <stdio.h>

/* Example allocator backend; replace with heap_4/5 or your own allocator. */
void *pvPortMalloc(size_t xSize);
void vPortFree(void *pv);

static SemaphoreHandle_t g_telemetry_lock = NULL;

void telemetry_init_lock(void)
{
    if (g_telemetry_lock == NULL) {
        g_telemetry_lock = xSemaphoreCreateRecursiveMutex();
    }
}

void telemetry_lock(void)
{
    if (g_telemetry_lock != NULL && xPortIsInsideInterrupt() == pdFALSE) {
        (void)xSemaphoreTakeRecursive(g_telemetry_lock, portMAX_DELAY);
    }
}

void telemetry_unlock(void)
{
    if (g_telemetry_lock != NULL && xPortIsInsideInterrupt() == pdFALSE) {
        (void)xSemaphoreGiveRecursive(g_telemetry_lock);
    }
}

void *telemetryMalloc(size_t xSize)
{
    if (xSize == 0) {
        xSize = 1;
    }
    return pvPortMalloc(xSize);
}

void telemetryFree(void *pv)
{
    vPortFree(pv);
}

void seds_error_msg(const char *str, size_t len)
{
    (void)len;
    printf("%s\r\n", str);
}

void telemetry_panic_hook(const char *str, size_t len)
{
    (void)len;
    printf("PANIC: %s\r\n", str ? str : "(null)");
    taskDISABLE_INTERRUPTS();
    for (;;)
    {
    }
}
```

### ThreadX example

```C
// telemetry_hooks_threadx.c
#include "tx_api.h"
#include <stddef.h>
#include <stdio.h>

static TX_BYTE_POOL *rust_byte_pool_external = NULL;
static TX_MUTEX g_telemetry_mutex;
static UINT g_telemetry_mutex_ready = 0U;
static TX_THREAD *g_telemetry_mutex_owner = TX_NULL;
static UINT g_telemetry_mutex_recursion = 0U;

void telemetry_set_byte_pool(TX_BYTE_POOL *pool)
{
    rust_byte_pool_external = pool;
}

void telemetry_init_lock(void)
{
    if (g_telemetry_mutex_ready == 0U) {
        if (tx_mutex_create(&g_telemetry_mutex, "telemetry_mutex", TX_INHERIT) == TX_SUCCESS) {
            g_telemetry_mutex_ready = 1U;
        }
    }
}

void telemetry_lock(void)
{
    if (g_telemetry_mutex_ready == 0U) {
        return;
    }

    TX_THREAD *self = tx_thread_identify();
    if (self == TX_NULL) {
        /* Not in thread context; do not block in ISR/startup contexts. */
        return;
    }

    if (g_telemetry_mutex_owner == self) {
        g_telemetry_mutex_recursion++;
        return;
    }

    if (tx_mutex_get(&g_telemetry_mutex, TX_WAIT_FOREVER) == TX_SUCCESS) {
        g_telemetry_mutex_owner = self;
        g_telemetry_mutex_recursion = 1U;
    }
}

void telemetry_unlock(void)
{
    if (g_telemetry_mutex_ready == 0U) {
        return;
    }

    TX_THREAD *self = tx_thread_identify();
    if (self == TX_NULL) {
        return;
    }

    if (g_telemetry_mutex_owner != self) {
        return;
    }

    if (g_telemetry_mutex_recursion > 1U) {
        g_telemetry_mutex_recursion--;
        return;
    }

    g_telemetry_mutex_owner = TX_NULL;
    g_telemetry_mutex_recursion = 0U;
    (void)tx_mutex_put(&g_telemetry_mutex);
}

void *telemetryMalloc(size_t xSize)
{
    void *ptr = NULL;
    if (rust_byte_pool_external == NULL) {
        return NULL;
    }

    if (xSize == 0U) {
        xSize = 1U;
    }

    if (tx_byte_allocate(rust_byte_pool_external, &ptr, xSize, TX_NO_WAIT) != TX_SUCCESS) {
        return NULL;
    }
    return ptr;
}

void telemetryFree(void *pv)
{
    if (pv != NULL) {
        (void)tx_byte_release(pv);
    }
}

void seds_error_msg(const char *str, size_t len)
{
    (void)len;
    printf("%s\r\n", str);
}

void telemetry_panic_hook(const char *str, size_t len)
{
    (void)len;
    printf("PANIC: %s\r\n", str ? str : "(null)");
    for (;;)
    {
    }
}
```

Call `telemetry_init_lock()` and `telemetry_set_byte_pool(...)` before any telemetry/router API usage.
