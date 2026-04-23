# Sedsprintf_rs Documentation

Sedsprintf_rs is a Rust telemetry transport and logging library with a shared schema, compact wire format, routing, and
multi-language bindings (C/C++ and Python). It targets embedded and host environments and supports optional compression
for senders and payloads.

See [Changelogs](Changelogs) for version highlights and release notes.

## Start here (easy overview)

These pages are written for readers who want a clear mental model before digging into code.

- [Overview](Overview)
- [Concepts](Concepts)
- [Examples](Examples)
- [Changelogs](Changelogs)

## How-to guides (practical steps)

Step-by-step setup and usage by language.

- [Build-and-Configure](Build-and-Configure)
- [Time-Sync](Time-Sync)
- [Usage-Rust](Usage-Rust)
- [Usage-C-Cpp](Usage-C-Cpp)
- [Usage-Python](Usage-Python)
- [Troubleshooting](Troubleshooting)

## Technical reference (deep dive)

Detailed pages that describe internals, data structures, and formats.

- [Technical-Index](Technical-Index)
- [Technical-Architecture](Technical-Architecture)
- [Technical-Telemetry-Schema](Technical-Telemetry-Schema)
- [Technical-Wire-Format](Technical-Wire-Format)
- [Technical-Router-Details](Technical-Router-Details)
- [Technical-Queues-and-Memory](Technical-Queues-and-Memory)
- [Technical-Packet-Details](Technical-Packet-Details)
- [Technical-Bindings-and-FFI](Technical-Bindings-and-FFI)

## Repo layout (high level)

-

src/ ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/tree/main/src)):
core Rust library (schema, packet types, serialization, router/relay).

-

sedsprintf_macros/ ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/tree/main/sedsprintf_macros)):
support macros for Rust integrations.

-

telemetry_config.json ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/telemetry_config.json)):
optional runtime schema seed (endpoints + data types).

-

build.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/build.rs)):
tracks build metadata and optional embedded schema bytes; user schemas are not compiled into normal
crate builds.

-

C-Headers/ ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/tree/main/C-Headers)):
C ABI header (`sedsprintf.h`).

-

python-files/ ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/tree/main/python-files)):
Python package assets and type hints.

-

c-example-code/ ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/tree/main/c-example-code))
and
python-example/ ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/tree/main/python-example)):
runnable examples.

## Data flow at a glance

```
log(data)        rx(bytes)
    |               |
    v               v
  Router <---- deserialize ---- wire ---- serialize ----> Router
    |  \                                         |  \
    |   \-- local endpoints                      |   \-- local endpoints
    |                                            |
    +-- tx(bytes) ------------------------------>+
```

Core ideas:

- Telemetry packets carry a runtime-schema-defined type, endpoints, sender name, and payload.
- Routers deliver packets to local endpoints and optionally relay them outward.
- User schema is registered at runtime by API, optional JSON seed, endpoint handler registration,
  or discovery sync.

If you want an implementation-level tour, go to [Technical-Architecture](Technical-Architecture).
