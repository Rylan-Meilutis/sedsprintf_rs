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

The size of the header in a serialized packet is around 20 bytes (the size will change based on the total number of
endpoints in your system and the length of the sender string), plus a 4-byte CRC32 trailer. As a rough example, a packet
containing three floats is on the order of mid-30s bytes total. This small size makes it ideal for use in low bandwidth
environments.

---

## Version 3.5.0 highlights

- Endpoint routing no longer depends on schema `broadcast_mode`; discovery state and link-local scope now decide whether
  traffic stays local, targets discovered remote sides, or falls back to relay flooding during bootstrap.
- Older telemetry schemas are upgraded automatically during code generation, so legacy `broadcast_mode` configs continue
  to build while migrating to the newer `link_local_only` model.
- `build.rs` now regenerates the public C header with the current ABI wrappers, avoiding drift between exported Rust
  symbols and `C-Headers/sedsprintf.h`.
- `build.py` now has a dedicated `check` mode for strict clippy validation, and `build.py test` now includes that lint
  coverage before running the broader test/build checks.
- The generated C and Python stubs, plus the C telemetry examples, were refreshed to match the current logging and
  discovery API surface.
- Full changelog: [CHANGELOG.md](./CHANGELOG.md)

## Version 3.4.2 highlights

- Time sync producers now participate in leader election instead of always serving unconditionally.
- Routers now keep per-remote-source time state, and non-winning producers follow the elected
  leader.
- Failover now uses monotonic holdover plus slew, so network time stays continuous while
  converging toward the new leader.
- Same-priority producers now resolve leadership deterministically, and consumers can optionally
  self-promote when no producers remain but a usable network clock is still available.
- C callers can now force a discovery announce or poll for due discovery traffic on both routers
  and relays through the public ABI.
- Wiki docs now keep GitHub `source` links in-repo, and the wiki sync script rewrites them to the
  target GitLab repo path when publishing there.
- Full changelog: [v3.4.1...v3.4.2](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.4.1...v3.4.2)

## Version 3.4.1 highlights

- Discovery can now advertise reachable time source sender IDs in addition to endpoint reachability.
- `TIME_SYNC` routing now prefers the exact discovered path to the selected source instead of sending requests to every
  side that merely exposes `TIME_SYNC`.
- Same-priority time source failover now keeps standby sources active so backup selection can happen immediately when
  the current winner times out.
- Source-side time sync responses now return to the requesting ingress side, reducing unnecessary traffic.
- Full changelog: [v3.4.0...v3.4.1](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.4.0...v3.4.1)

## Version 3.4.0 highlights

- Time sync is now router-managed: `TIME_SYNC` packets are consumed internally, routers maintain a
  separate internal network clock, and packet timestamps prefer that clock when available.
- Added merged partial network time support, so routers can combine date, time-of-day, and
  subsecond precision from different sources.
- Added local/master network time setter APIs across Rust, C, and Python for date-only, HMS,
  millisecond, and nanosecond precision updates.
- On `std` builds, routers now use an internal monotonic clock by default; explicit clock hooks
  moved to `Router::new_with_clock(...)` and remain available for tests, simulation, and
  `no_std`.
- C and Python constructors now treat the router clock callback as optional on `std` builds, and
  C system-test handling was hardened to prevent indefinite hangs on regressions.
- Full changelog: [v3.3.0...v3.4.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.3.0...v3.4.0)

---

## Version 3.3.0 highlights

- Added built-in discovery/routing control traffic for routers and relays, including learned endpoint reachability,
  selective forwarding, adaptive discovery cadence, and topology export.
- Added link-local/software-bus endpoint support for IPC, including routing restrictions so link-local traffic stays on
  link-local sides and is hidden from normal network discovery advertisements.
- Added board-local IPC schema overlays via `SEDSPRINTF_RS_IPC_SCHEMA_PATH`, so shared telemetry schemas can stay fixed
  while per-board IPC endpoints/types are merged at build time.
- Updated the telemetry config editor to manage split base/IPC schema files independently, including external IPC
  overlay paths for CMake or `.cargo/config.toml` driven builds.
- Added regression coverage for discovery routing, link-local routing boundaries, IPC overlay schema merging, and the
  split-file editor path flow.
- Full changelog: [v3.2.3...v3.3.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.2.3...v3.3.0)

---

## Version 3.2.3 highlights

- Compression backend is now `zstd-safe` (single backend), with bounded output buffers in the compression path and
  no compression-level build option.
- Queueing and re-entrancy behavior was hardened for RTOS-like usage, with added stress tests for deadlock scenarios.
- Time sync system coverage expanded with multi-node C tests, including grandmaster/consumer topologies and failover.
- C system test builds on macOS now enforce a consistent deployment target to avoid linker target-mismatch warnings.
- Full changelog: [v3.2.2...v3.2.3](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.2.2...v3.2.3)

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
./build.py maturin-install env:MAX_RECENT_RX_IDS=256 env:MAX_STACK_PAYLOAD=128
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

`./build.py test` now starts with the same strict clippy checks as `./build.py check`, then runs the Cargo test suite,
a short Criterion smoke pass, and the python plus embedded build validation steps. The benchmark smoke pass uses Cargo
`--profile release`.

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
set(SEDSPRINTF_RS_ENV_MAX_QUEUE_SIZE "65536" CACHE STRING "" FORCE)

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
set(SEDSPRINTF_RS_ENV_MAX_QUEUE_SIZE "65536" CACHE STRING "" FORCE)

# or

# Ground station app
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "GS26" CACHE STRING "" FORCE)
set(SEDSPRINTF_RS_TARGET "" CACHE STRING "" FORCE)
set(SEDSPRINTF_EMBEDDED_BUILD OFF CACHE BOOL "" FORCE)
set(SEDSPRINTF_RS_ENV_MAX_QUEUE_SIZE "65536" CACHE STRING "" FORCE)
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
set(SEDSPRINTF_RS_ENV_MAX_QUEUE_SIZE "65536" CACHE STRING "" FORCE)

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
