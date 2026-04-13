# Build and Configure

This page explains how to build the library and how compile-time configuration works across Rust, C/C++, and Python.

## Build tooling (build.py)

The repo includes
build.py ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.py)),
a wrapper around Cargo and Maturin that:

- Sets compile-time environment variables (e.g., `DEVICE_IDENTIFIER`).
- Enables feature flags (`embedded`, `python`).
- Optionally installs missing Rust targets via `rustup`.
- Produces consistent output for CI and local builds.

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

Useful options:

- `check` runs `cargo clippy -D warnings` for the default, python, and embedded builds.
- `test` runs the same clippy checks, then:
  - `cargo test --features timesync`
  - a short Criterion smoke pass for `packet_paths` and `router_system_paths`
  - `cargo build --features python`
  - `cargo build --no-default-features --target <embedded-target> --features embedded` when a matching cross C
    toolchain is available
- `device_id=<id>` sets `DEVICE_IDENTIFIER` for the build.
- `schema_path=<path>` sets `SEDSPRINTF_RS_SCHEMA_PATH`.
- `ipc_schema_path=<path>` sets `SEDSPRINTF_RS_IPC_SCHEMA_PATH` for a board-local IPC overlay.
- `max_stack_payload=<n>` sets `MAX_STACK_PAYLOAD` for inline payload storage.
- `env:KEY=VALUE` passes any compile-time env var used by
  src/config.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/config.rs)).
- `target=<triple>` sets the Rust target triple for embedded builds.

## Cargo features

From
Cargo.toml ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/Cargo.toml)):

- `std` (default): host build with std.
- `embedded`: enables embedded defaults, `timesync`, and no_std-friendly behavior.
- `python`: enables pyo3 bindings.
- `compression` (default): enables payload compression (implemented with `zstd-safe`).
- `timesync`: enables time sync helpers and built-in time sync packet types.

Examples:

- Disable compression: `default-features = false` and omit `compression`.
- Embedded + compression: enable both `embedded` and `compression`.

Compression notes:

- Compression is opportunistic (only used when it reduces size).
- Backend is fixed to `zstd-safe` for simplicity/consistency across builds.
- There is no compression-level build option.
- For cross-target embedded builds, enabling `compression` requires a usable target C toolchain
  (for `zstd-sys`, e.g. `arm-none-eabi-gcc` or `CC_<target>` override).

When `timesync` is enabled, the build adds the `TIME_SYNC` endpoint and
`TIME_SYNC_*` packet types directly in code (like `TelemetryError`), plus the router-managed
internal network clock and FFI accessors for current network time. See [Time-Sync](Time-Sync)
for roles, packet fields, internal clock behavior, and master-side setter APIs.

Python builds via `maturin` in this repo enable `timesync` by default (see
pyproject.toml ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/pyproject.toml))).

## Test coverage and what runs

`./build.py test` is the intended top-level validation command for local development and CI-style checks in this repo.
It covers four layers:

- Static analysis: strict `cargo clippy -D warnings` for default, `python`, and embedded variants.
- Rust unit and integration tests: `cargo test --features timesync`, including `src/tests.rs`, Rust system tests in
  `tests/rust-system-test/`, and the Rust harness that configures and runs the C system tests in
  `tests/c-system-test/c_system_test.rs`.
- Benchmark smoke: short Criterion runs for `benches/packet_paths.rs` and `benches/router_system_paths.rs`.
- Build validation: host `python` feature build and embedded-feature build when an embedded cross C toolchain is
  present.

The C system tests exercise the generated C ABI, multi-endpoint routing, relay forwarding, discovery, and time-sync
behavior through compiled executables in `c-system-test/`. The main multi-node C test now waits for every asserted
endpoint count before shutdown so it does not fail early when one simulated board drains slightly slower than another.

This repo does not currently publish or gate on a single required coverage percentage in `build.py test`. Coverage is
tracked primarily through regression tests across unit, Rust system, and C system layers. If you want a local
percentage/HTML report, use `cargo-llvm-cov`:

```bash
cargo llvm-cov --features timesync --workspace --html
```

That produces a local report under `target/llvm-cov/html/`.

## Device identifier

Every build embeds `DEVICE_IDENTIFIER` into telemetry packets.

Recommended (Rust):

```
# .cargo/config.toml
[env]
DEVICE_IDENTIFIER = "GROUND_STATION_26"
```

CMake:

```
set(SEDSPRINTF_RS_DEVICE_IDENTIFIER "FC26_MAIN" CACHE STRING "" FORCE)
```

build.py ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.py)):

```
./build.py release device_id=GROUND_STATION
```

## Compile-time configuration

Configuration values are read via `option_env!` in
src/config.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/config.rs)).
You can set them via `.cargo/config.toml`,
`build.py env:KEY=VALUE`, or CMake `SEDSPRINTF_RS_ENV_<KEY>` variables.

Supported keys (defaults shown):

- `DEVICE_IDENTIFIER` (TEST_PLATFORM)
- `MAX_RECENT_RX_IDS` (128)
- `STARTING_RECENT_RX_IDS` (32)
- `STARTING_QUEUE_SIZE` (64 bytes)
- `MAX_QUEUE_SIZE` (51200 bytes)
- `QUEUE_GROW_STEP` (3.2)
- `PAYLOAD_COMPRESS_THRESHOLD` (16 bytes)
- `STATIC_STRING_LENGTH` (1024)
- `STATIC_HEX_LENGTH` (1024)
- `STRING_PRECISION` (8)
- `MAX_STACK_PAYLOAD` (64, via `define_stack_payload!`)
- `MAX_HANDLER_RETRIES` (3)

## CMake integration

CMakeLists.txt ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/CMakeLists.txt))
invokes
build.py ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.py))
and exposes variables for embedded builds.

Common CMake variables:

- `SEDSPRINTF_EMBEDDED_BUILD` (ON/OFF)
- `SEDSPRINTF_RS_FORCE_RELEASE` (ON/OFF, forces Cargo release profile even under a Debug parent build)
- `SEDSPRINTF_RS_TARGET` (Rust target triple)
- `SEDSPRINTF_RS_DEVICE_IDENTIFIER`
- `SEDSPRINTF_RS_MAX_STACK_PAYLOAD`
- `SEDSPRINTF_RS_ENV_<KEY>` for any config env var

After `add_subdirectory`, link the target:

```
target_link_libraries(${CMAKE_PROJECT_NAME} PRIVATE sedsprintf_rs::sedsprintf_rs)
```

## Python builds

Python bindings are built with `maturin`.

Options:

- `./build.py python` (develop build)
- `./build.py maturin-build` (wheel)
- `./build.py maturin-install` (build + install)

If you use `maturin develop` directly, ensure you are in the correct virtualenv.

## Build.rs overrides (advanced)

build.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.rs))
can be directed to alternate sources or disabled:

- `SEDSPRINTF_RS_SKIP_ENUMGEN=1` skips enum generation.
- `SEDSPRINTF_RS_SCHEMA_PATH=path/to/telemetry_config.json` overrides the base schema source.
- `SEDSPRINTF_RS_IPC_SCHEMA_PATH=path/to/ipc_config.json` adds a board-local IPC overlay schema.
- `SEDSPRINTF_RS_LIB_RS=path/to/lib.rs` overrides error enum source.

## Embedded allocator hooks

Bare-metal builds expect the following symbols to be provided by the host environment:

- `void *telemetryMalloc(size_t)`
- `void telemetryFree(void *)`
- `void telemetry_lock(void)`
- `void telemetry_unlock(void)`
- `void seds_error_msg(const char *, size_t)`
- `void telemetry_panic_hook(const char *, size_t)`

See [Usage-C-Cpp](Usage-C-Cpp) for an example stub implementation.
