# Bindings and FFI (Technical)

This page describes how the Rust core is exposed to C/C++ and Python.

## C/C++ binding

src/c_api.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/c_api.rs))
defines the C ABI surface.

C-Headers/sedsprintf.h ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/C-Headers/sedsprintf.h))
declares the ABI structs and functions. It is not generated from an application schema.

- Built-in control IDs are exposed as constants.
- User schema entries are runtime IDs looked up by name or registered by API.

Typical usage patterns:

- Construct packets by calling into the C API helpers.
- Register or seed schema entries at process startup.
- Use `seds_endpoint_get_info_by_name(...)` and `seds_dtype_get_info_by_name(...)` to map readable
  names to numeric wire IDs.
- Register handler callbacks for endpoints.

Runtime schema APIs include:

- `seds_endpoint_register(...)` / `seds_endpoint_register_ex(...)`
- `seds_dtype_register(...)` / `seds_dtype_register_ex(...)`
- `seds_schema_register_json_file(...)` / `seds_schema_register_json_bytes(...)`
- `seds_endpoint_get_info(...)` / `seds_endpoint_get_info_by_name(...)`
- `seds_dtype_get_info(...)` / `seds_dtype_get_info_by_name(...)`
- `seds_endpoint_remove(...)` / `seds_endpoint_remove_by_name(...)`
- `seds_dtype_remove(...)` / `seds_dtype_remove_by_name(...)`

Embedded builds provide allocator and error hooks so the core can run without std.

## Python binding

src/python_api.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/src/python_api.rs))
defines the pyo3 module.

python-files/sedsprintf_rs/sedsprintf_rs.pyi ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/python-files/sedsprintf_rs/sedsprintf_rs.pyi))
contains type hints for the stable Python API surface.

- The Python package exposes built-in control IDs, router/packet helpers, and runtime schema APIs.

Typical usage patterns:

- Create a router and register handlers in Python.
- Register or seed schema entries at startup.
- Look up readable names with `endpoint_info_by_name(...)` and `data_type_info_by_name(...)`.
- Log payloads using the returned numeric IDs.
- Decode payloads using the packet helper methods.

Runtime schema functions include `register_endpoint(...)`, `register_data_type(...)`,
`register_schema_json_file(...)`, `register_schema_json_bytes(...)`, `endpoint_info_by_name(...)`,
`data_type_info_by_name(...)`, `remove_endpoint_by_name(...)`, and `remove_data_type_by_name(...)`.

## Schema consistency

Rust, C, and Python all use the same runtime registry and packet layout. Schema can be registered
directly, seeded from JSON, or learned through discovery. Compatible definitions merge across
nodes. Conflicting definitions, such as the same type name/ID with a different payload size, are
resolved deterministically by the discovery sync path and return errors when registered directly.

Default crate builds do not compile an application JSON schema into the binary. Embedded builds may
include JSON bytes when a static schema file is present, then decode those bytes through the same
runtime registration path.

## Ownership and memory

- Packet payloads are stored as byte slices; copies happen only when needed (e.g., inline payloads or when serializing).
- Many APIs use `Arc` internally to allow cheap cloning across handlers.
- C and Python bindings use the same runtime schema registry and packet layout, so the payload
  bytes are compatible across languages.

## Embedded hooks

Bare‑metal builds expect allocator/error hooks:

- `telemetryMalloc` / `telemetryFree`
- `telemetry_lock` / `telemetry_unlock`
- `seds_error_msg`
- `telemetry_panic_hook`

These allow the core to remain usable in `no_std` environments.
