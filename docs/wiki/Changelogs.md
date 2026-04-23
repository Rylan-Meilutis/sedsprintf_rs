# Changelogs

## Version 4.0.0 highlights

- Runtime-only schema:
    - User `DataEndpoint` and `DataType` entries are no longer generated at compile time.
    - `build.rs` no longer compiles application schema JSON into Rust enum variants or binding
      constants.
    - The crate can be built/published without a local application `telemetry_config.json`.
- Runtime schema APIs:
    - Rust now supports endpoint/type registration, lookup by ID, lookup by name, export, and
      removal.
    - `DataEndpoint::named("RADIO")` and `DataType::named("GPS_DATA")` provide readable runtime
      references for application code and tests.
    - C and Python expose matching register/info/info-by-name/remove APIs.
- JSON seeding:
    - Static JSON is optional runtime input, not compile-time codegen.
    - Host builds can seed via `SEDSPRINTF_RS_STATIC_SCHEMA_PATH`,
      `SEDSPRINTF_RS_STATIC_IPC_SCHEMA_PATH`, explicit path APIs, or explicit bytes APIs.
    - Embedded builds include `telemetry_config.json` bytes only if the file exists, then decode
      them through the same runtime parser.
- Network schema sync:
    - Discovery advertises the current endpoint/type schema.
    - Nodes merge compatible schemas and resolve ID/name conflicts deterministically.
    - Direct registration still rejects a data type name/ID that already exists with a different
      shape.
- Metadata and memory:
    - Endpoints and data types now carry human-readable descriptions.
    - Runtime JSON accepts both `description` and legacy `doc`.
    - Schema registry memory counts against the same shared router/relay queue budget as RX/TX
      queues, reliable state, dedupe caches, and discovery topology.
- Tests and examples:
    - Rust tests and benches now use readable string-backed lookups instead of raw legacy IDs.
    - Added regression coverage for schema sync, conflict resolution, budget accounting, string
      lookup, metadata, and removal.

## Version 3.12.0 highlights

- Shared queue budgeting:
    - Router and relay internals now share one dynamic `MAX_QUEUE_BUDGET` instead of using
      isolated caps for each internal queue.
    - RX queues, TX queues, recent packet IDs, reliable replay/out-of-order buffers, and discovery
      topology state all count against the same budget.
    - Recent packet ID caches now preallocate their final storage and reserve their byte cost from
      the shared budget immediately.
    - Discovery topology state is now bounded by that budget, with warnings emitted in `std` builds
      when topology entries must be evicted because the queue budget is exhausted.
- Reliable recovery traffic reduction:
    - Ordered reliable RX now partial-ACKs out-of-order packets.
    - A partial ACK suppresses timeout retransmission for that packet, while still allowing an
      explicit packet request to retransmit it later if needed.
    - Buffered packets after a missing sequence are released immediately when the gap is filled.
- Runtime robustness:
    - Router and relay side-TX contention is now handled as transient backpressure by requeueing
      pending work for retry instead of returning an intermittent handler failure.
- Regression coverage:
    - Added tests for shared queue-budget accounting, discovery topology budget pressure, partial
      ACK behavior, side-TX busy retry handling, and the threaded system flow that previously
      failed intermittently.

## Version 3.11.1 highlights

- Discovery topology fidelity:
    - Added built-in `DISCOVERY_TOPOLOGY` advertisements so routers and relays can propagate
      router identity, endpoint ownership, time-sync source ownership, and inter-router
      connections.
    - Discovery no longer loses router-level attribution after one hop by collapsing everything
      into only side-level endpoint sets.
- Topology export parity:
    - `export_topology()` now includes a top-level `routers` graph plus per-side announcer detail.
    - Python now exposes `Router.export_topology()` and `Relay.export_topology()` directly.
    - C now exposes `seds_router_export_topology[_len]` and
      `seds_relay_export_topology[_len]`, returning JSON snapshots.
- Documentation refresh:
    - Updated the README plus Rust, Python, C/C++, router-detail, and time-sync docs to describe
      the richer exported topology model.

## Version 3.11.0 highlights

- Router model cleanup:
    - Removed `RouterMode` from the active router API and forwarding logic.
    - Routers now use the same runtime route-rule model as relays.
    - With no explicit route rules, routers default to a full forwarding mesh.
- Discovery and path selection:
    - Discovery-driven multi-path routing now defaults to adaptive load balancing for normal
      traffic.
    - Reliable traffic still fans out across all discovered candidates so a single weak path does
      not suppress successful remote delivery.
- End-to-end reliability:
    - Reliable delivery is now end-to-end verified instead of stopping at per-link ACKs.
    - Destination routers emit directed end-to-end acknowledgements after local delivery.
    - Source routers keep reliable packets in flight until every currently discovered holder has
      acknowledged.
    - Routers and relays learn reliable return paths from ingress traffic and route those ACKs only
      where needed instead of flooding unrelated sides.
    - Retries are narrowed to only the holders that are still outstanding instead of replaying to
      holders that already acknowledged.
    - If discovery ages out a holder, that holder is removed from the in-flight obligation set so
      the transaction completes instead of replaying forever toward a disappeared board.
    - Relay-side learned holder-ACK state is also pruned against discovery expiry so stale
      confirmations do not linger after topology changes.
    - This still uses the non-blocking reliable send path introduced in `3.10.0`; waiting for
      end-to-end ACKs does not stall newer reliable packets on that side/type lane.
- Test and docs coverage:
    - Added regression coverage for the new routing defaults, sink-style route disabling, adaptive
      discovery balancing, and end-to-end reliable acknowledgement loss/retry behavior, including a
      multi-holder system test plus holder-expiry cleanup unit coverage for both routers and relays.
    - Added dedicated testing documentation covering unit tests, Rust system tests, C system
      tests, and local coverage reporting with `cargo llvm-cov`.

## Version 3.10.0 highlights

- Reliable delivery in both `Router` and `Relay` now uses built-in internal
  `RELIABLE_ACK` and `RELIABLE_PACKET_REQUEST` packet types instead of wire-only ACK-only frames.
- Reliable senders no longer stall a side/type stream on one inflight packet. New reliable packets
  can keep flowing while ordered gaps are requested and retransmitted.
- Ordered reliable RX now buffers out-of-order frames, requests the exact missing sequence, and
  releases buffered packets once the gap is filled.
- Retransmits are requeued with elevated priority rather than monopolizing the stream, improving
  mixed-traffic behavior and multi-board fanout recovery.
- Added router, relay, and system-style regression coverage for non-blocking reliable send and the
  new internal reliable control flow.

## Version 3.9.1 highlights

- Router-internal control endpoints:
    - Reserved `DISCOVERY` and `TIME_SYNC` for built-in router control traffic.
    - `RouterConfig`, `EndpointHandler`, and `seds_router_new(...)` now reject attempts to
      register user handlers on those endpoints.
- Queue maintenance reliability:
    - Nonzero `process_all_queues_with_timeout(...)` budgets are now split between TX and RX so
      slow TX work cannot starve queued RX discovery processing.
    - Zero-timeout queue processing still drains both queues completely.
- Discovery regression coverage:
    - Added tests proving queued discovery updates the exported route table, queued time-sync
      discovery sources are learned correctly, and reserved-endpoint registration is rejected in
      both Rust and the C ABI.
- Documentation refresh:
    - Updated Rust and C/C++ usage docs to state that discovery and time sync are router-owned
      internals rather than user-registerable endpoints.
- Full
  changelog: [v3.9.0...v3.9.1](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.9.0...v3.9.1)

## Version 3.9.0 highlights

- Manual typed-link routing:
    - Added `set_typed_route(...)` / `clear_typed_route(...)` for both routers and relays.
    - Typed route rules let one `DataType` use one or many explicitly selected sides for a given
      local-TX or ingress-side source.
    - This supports dedicated links for commands, aborts, or other special traffic classes
      without rebuilding the router or relay.
- Routing precedence and compatibility:
    - Typed route rules act as allowlists layered on top of the existing side-level route policy,
      ingress/egress policy, and discovery/path-selection logic.
    - Existing routing behavior is unchanged for traffic that does not have a typed route rule.
- ABI and binding parity:
    - Added matching Rust, C ABI, and Python APIs for typed routing on both `Router` and `Relay`.
- Regression coverage and docs:
    - Added Rust and C ABI tests for typed-route selection, multi-side fanout, fallback after
      clearing typed rules, and precedence against base route disables.
    - Updated Rust, Python, C/C++, and technical router documentation to describe typed-route
      behavior and precedence.
- Full
  changelog: [v3.8.0...v3.9.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.8.0...v3.9.0)

## Version 3.8.0 highlights

- Multi-path traffic steering:
    - Added `RouteSelectionMode` with `Fanout`, `Weighted`, and `Failover` modes for both routers
      and relays.
    - Added runtime per-route weights and priorities so traffic can be split unevenly or pinned to
      a preferred primary path with ordered backups.
    - Discovery reachability now acts as the liveness signal for failover, so expired paths stop
      receiving traffic automatically.
- ABI and binding parity:
    - Added matching Rust, C ABI, and Python controls for source route mode, route weight, and
      route priority configuration.
- Regression coverage:
    - Added router and relay tests for weighted split and failover behavior, plus C ABI coverage
      for weighted local routing.
- Full
  changelog: [v3.7.0...v3.8.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.7.0...v3.8.0)

## Version 3.7.0 highlights

- Runtime router and relay routing tables:
    - `Relay` now supports `remove_side(...)` too, so router and relay side lifecycle controls
      match across Rust, C ABI, and Python.
    - Added per-side ingress and egress controls so routers and relays can accept traffic from
      many links while transmitting only on the links you allow.
    - Added runtime route overrides for both local TX (`None` / `-1`) and side-to-side relay
      paths, enabling asymmetric policies like `A -> B` but not `B -> A`.
- Default behavior stays familiar:
    - `RouterMode::Relay` still seeds a full mesh between sides by default.
    - `RouterMode::Sink` still disables side-to-side forwarding by default, but routing can now be
      enabled selectively at runtime without rebuilding the router.
- Topology and ABI updates:
    - Discovery announcements now follow the active egress and local-route policy for both routers
      and relays.
    - Added matching Rust, C ABI, and Python APIs for runtime side policy and routing changes.
- Regression coverage:
    - Added tests for asymmetric router and relay paths, ingress-disabled sides, and C ABI
      local-route overrides.
- Full
  changelog: [v3.6.0...v3.7.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.6.0...v3.7.0)

## Version 3.6.0 highlights

- Router side lifecycle management:
    - Added `remove_side(...)` for routers in Rust, C, and Python so TCP-style peer-per-side
      topologies can drop disconnected peers without rebuilding the router.
    - Removed sides keep existing side IDs stable for the rest of the router and are no longer
      eligible for transmit, ingress, reliable state, or topology export.
- Discovery convergence after topology changes:
    - Router side add/remove continues to trigger the adaptive discovery fast path, and removal now
      also clears learned reachability for that side before announcing the remaining topology.
    - Discovery exports and announces now omit removed sides while continuing to advertise the
      local plus still-reachable endpoint set on the surviving links.
- Regression coverage:
    - Added Rust tests for router side removal, discovery topology shrinkage, and invalid ingress
      rejection for removed side IDs.
    - Added C ABI coverage for removing a router side and verifying discovery only transmits on
      the remaining side.
- Full
  changelog: [v3.5.2...v3.6.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.5.2...v3.6.0)

## Version 3.5.2 highlights

- Time sync failover recovery:
    - Fixed router-managed time sync so a consumer drops stale pending requests when the selected
      remote source times out or leadership changes.
    - Consumers now re-issue `TIME_SYNC_REQUEST` traffic toward the newly elected replacement
      source instead of remaining stuck on holdover until restart.
    - Added Rust system-test coverage for the disconnect/reconnect failover path where a
      replacement source must be requested and accepted after timeout-driven re-election.
- Full
  changelog: [v3.5.1...v3.5.2](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.5.1...v3.5.2)

## Version 3.5.1 highlights

- Main-loop maintenance API cleanup:
    - Added `Router::periodic(timeout_ms)` and `Router::periodic_no_timesync(timeout_ms)` so
      applications can drive discovery, optional time sync, and queue draining through one call.
    - Added `Relay::periodic(timeout_ms)` so relays can drive discovery plus queue draining through
      one call as well.
    - Added matching C ABI and Python binding entry points for the new periodic helpers.
- Documentation refresh:
    - Updated Rust, Python, C/C++, and time-sync usage docs to recommend the periodic helpers for
      normal application loops.
    - Kept the lower-level `poll_timesync()` and `poll_discovery()` APIs documented for advanced
      callers that want explicit control over maintenance phases.
- Full
  changelog: [v3.5.0...v3.5.1](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.5.0...v3.5.1)

## Version 3.4.2 highlights

- Time sync election and continuity:
    - `Source` routers now participate in producer election instead of always acting as masters.
    - Routers keep per-remote-source time state instead of collapsing all remotes into one shared
      internal source slot.
    - Non-winning producers now follow the elected leader rather than continuing to serve
      independently.
    - Failover now uses monotonic holdover plus slew so network time does not jump backward during
      source changes.
- Tie breaking and consumer promotion:
    - Same-priority producers now resolve leadership by selecting one winner and advertising a
      temporary boosted priority while the standby producers keep their configured priorities.
    - Consumers with a non-uptime-based clock can optionally self-promote when no producers remain,
      allowing the network to stay aligned during complete producer loss.
- Docs and tooling updates:
    - Added C ABI discovery maintenance hooks so C callers can force a discovery announce or poll
      for due discovery traffic on both routers and relays.
    - Added Rust system-test coverage for election, priority ties, consumer promotion, and failover
      monotonicity.
    - Wiki source links now default to GitHub in-repo, while the wiki sync script rewrites them to
      the target GitLab repo path when publishing to GitLab.
- Full
  changelog: [v3.4.1...v3.4.2](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.4.1...v3.4.2)

## Version 3.4.1 highlights

- Discovery and time sync routing integration:
    - Added built-in `DISCOVERY_TIMESYNC_SOURCES` advertisements so routers and relays can learn
      concrete reachable time source sender IDs instead of only generic `TIME_SYNC` endpoint
      reachability.
    - `TIME_SYNC` requests now prefer exact discovered source paths when the current selected
      source is known through discovery.
    - `export_topology()` now includes advertised and reachable time source IDs alongside endpoint
      reachability.
- Time sync failover and traffic reduction:
    - `TimeSyncTracker` now keeps the active source set and can fail over immediately to a
      same-priority or lower-priority standby source that is still active.
    - Source-generated `TIME_SYNC_RESPONSE` traffic now returns to the requesting ingress side
      instead of being broadcast to every side.
    - Fixed an internal request-serving deadlock by avoiding timesync mutex re-entry while
      sampling source-side timestamps.
    - Full
      changelog: [v3.4.0...v3.4.1](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.4.0...v3.4.1)

## Version 3.4.0 highlights

- Router-managed time sync and network clock:
    - Time sync is now handled internally by the router instead of through normal local `TIME_SYNC`
      endpoint handlers.
    - Routers maintain an internal non-monotonic network clock separate from their monotonic timing
      source.
    - Packet timestamps now prefer the internal network clock when one is available.
- Partial time-source merging and master clock injection:
    - The internal network clock can merge partial sources, such as date from one source and
      time-of-day or subsecond precision from another.
    - Added master/local time setter APIs for complete or partial network time, including
      date-only, hour/minute, hour/minute/second, millisecond, and nanosecond variants.
    - Local time setters are anchored at commit time so short context switches during updates do
      not leave complete absolute times stale.
- Constructor and FFI clock model updates:
    - On `std` builds, `Router::new(...)` now uses an internal monotonic clock by default.
    - Added `Router::new_with_clock(...)` for tests, simulation, and `no_std` / embedded clock
      injection.
    - C and Python router constructors now treat the monotonic clock callback as optional on
      `std` builds and fall back to the internal router clock when it is omitted.
- C system-test and harness improvements:
    - Fixed a relay timing issue in the C system-test path that could cause shutdown to stall.
    - Updated C time-sync tests to follow the internal router-managed time-sync model.
    - Added bounded timeout handling in the Rust C-test harness so future regressions fail fast
      instead of hanging indefinitely.
- Documentation refresh:
    - Updated Rust, C, and Python usage docs to reflect the new router constructor model.
    - Expanded time-sync documentation to cover the internal network clock, merged partial
      sources, current network time accessors, and master-side setter APIs.
- Full
  changelog: [v3.3.0...v3.4.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.3.0...v3.4.0)

## Version 3.3.0 highlights

- Discovery/routing control plane:
    - Added built-in discovery advertisements for routers and relays under the `discovery` feature.
    - Routers and relays now learn endpoint reachability, export topology snapshots, and use adaptive announce
      intervals that speed up after topology changes and back off when stable.
    - Selective forwarding now uses discovered reachability first and falls back to ordinary flooding when routes are
      unknown.
- Reliability and forwarding integration:
    - Reliable packets are now fanned out to all discovered candidate sides instead of relying only on blind flooding.
    - Added additional routing tests to ensure discovery does not cause link-local traffic to leak onto normal network
      sides.
- Link-local/software-bus IPC support:
    - Added link-local-only endpoint support for software-bus / IPC traffic.
    - Discovery advertisements are filtered per-side so IPC endpoints are not exposed on non-link-local links.
    - Routers and relays now enforce link-local routing boundaries even when discovery data is overly broad.
- Split schema support for per-board IPC:
    - Added `SEDSPRINTF_RS_IPC_SCHEMA_PATH` for board-local IPC overlays that merge with the shared base schema.
    - IPC overlay endpoints are treated as link-local automatically; base-schema endpoints are treated as non-link-local
      automatically.
    - Added proc-macro/build-script tests for overlay merging, collision rejection, and link-local normalization.
- Telemetry config editor updates:
    - The GUI editor can now open, edit, and save the base schema and IPC overlay as separate files.
    - IPC overlay paths can live outside the repository and be supplied by environment-driven build systems such as
      CMake or `.cargo/config.toml`.
    - Link-local scope is now derived from which file is being edited rather than being a user-editable checkbox.
- Full
  changelog: [v3.2.3...v3.3.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.2.3...v3.3.0)

## Version 3.2.3 highlights

- Compression backend consolidation:
    - Switched to a single backend (`zstd-safe`) for sender/payload compression.
    - Removed compression-level configuration knobs from build/docs/examples.
    - Kept bounded compression behavior and added constrained-memory regression tests.
- Router/FFI queueing and re-entrancy hardening:
    - RX queue paths and lock behavior were tightened to avoid deadlocks under RTOS-like concurrency.
    - Added tests for handler re-entry into router APIs and mixed ingress/processing concurrency.
- Time sync validation expansion:
    - Added C system tests for multi-node time sync and board-topology scenarios (grandmaster + consumers).
    - Added failover coverage where backup sources are selected after source timeout.
- C/C++ integration updates:
    - Expanded C header template function descriptions.
    - macOS C system-test builds now align deployment target settings with Rust staticlib builds to avoid linker
      mismatch warnings.
- Full
  changelog: [v3.2.2...v3.2.3](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.2.2...v3.2.3)

## Version 3.2.2 highlights

- Script reliability and UX improvements: better error handling with actionable failure hints across update/build/docs
  helper scripts.
- Formatting cleanup across scripts and docs for more consistent output.
- Additional wiki documentation updates and wording cleanup.
- Full
  changelog: [v3.2.1...v3.2.2](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.2.1...v3.2.2)

## Version 3.2.1 highlights

- Wiki overhaul: broad documentation refresh, structure cleanup, and improved navigation/discoverability.
- GUI updates to the telemetry config editor.
- Full
  changelog: [v3.2.0...v3.2.1](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.2.0...v3.2.1)

## Version 3.2.0 highlights

- Time Sync feature: built-in `TIME_SYNC` endpoint and `TIME_SYNC_*` packet types (enabled via `timesync` feature).
- New Time Sync helpers and improved failover handling in `TimeSyncTracker`.
- New Rust, C, and Python time sync examples plus additional Rust examples for relay, reliability, timeouts, and
  multi-node simulation.
- RTOS time sync example code for FreeRTOS and ThreadX.
- Updated wiki docs to surface new examples and feature behavior.
- Full
  changelog: [v3.1.0...v3.2.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.1.0...v3.2.0)

What's included:

- Feature: `timesync` adds `TIME_SYNC` endpoint and `TIME_SYNC_ANNOUNCE/REQUEST/RESPONSE` types (built-in like
  `TelemetryError`).
- Examples:
  rust-example-code/timesync_example.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/rust-example-code/timesync_example.rs)),
  rust-example-code/relay_example.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/rust-example-code/relay_example.rs)),
  rust-example-code/reliable_example.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/rust-example-code/reliable_example.rs)),
  rust-example-code/queue_timeout_example.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/rust-example-code/queue_timeout_example.rs)),
  rust-example-code/multinode_sim_example.rs ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/rust-example-code/multinode_sim_example.rs)),
  c-example-code/src/timesync_example.c ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/c-example-code/src/timesync_example.c)),
  python-example/timesync_example.py ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/python-example/timesync_example.py)),
  rtos-example-code/freertos_timesync.c ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/rtos-example-code/freertos_timesync.c)),
  rtos-example-code/threadx_timesync.c ([source](https://github.com/Rylan-Meilutis/sedsprintf_rs/blob/main/rtos-example-code/threadx_timesync.c)).
- C system test and examples demonstrate Time Sync announce/request/response flows.
- Python example handles Time Sync without re-entering the router from handlers.

## Version 3.1.0 highlights

- CRC support for packets to ensure validity; in reliable mode, CRC failures trigger retransmits.
- Fixes to the config editor GUI.
- Full
  changelog: [v3.0.0...v3.1.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v3.0.0...v3.1.0)

## Version 3.0.0 highlights

- Router side tracking is internal. Most applications should call the plain RX APIs (`rx_serialized` / `rx`) and only
  use side-aware variants when explicitly overriding ingress (custom relays, multi-link bridges, etc.).
- TCP-like reliability is now available for schema types marked `reliable` / `reliable_mode`, with ACKs, retransmits,
  and optional ordering. Enable per side and disable when the transport is already reliable.
- Full
  changelog: [v2.4.0...v3.0.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v2.4.0...v3.0.0)

## Version 2.4.0 highlights

- Moved config to environment + JSON schema used at compile time.
- Added a simple GUI tool for building the config.
- Full
  changelog: [v2.3.2...v2.4.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v2.3.2...v2.4.0)

## Version 2.3.2 highlights

- Added a new unsafe API for creating link IDs.
- Fixed existing bugs.
- Full
  changelog: [v2.3.1...v2.3.2](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v2.3.1...v2.3.2)

## Version 2.3.1 highlights

- Simplified config format is now live and in production.
- Full
  changelog: [v2.2.3...v2.3.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v2.2.3...v2.3.0)

## Version 2.2.3 highlights

- Build script fixes and more repo details.
- Final fix for bounded ring buffers used in routers and relays.
- Full
  changelog: [v2.2.1...v2.2.3](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v2.2.1...v2.2.3)

## Version 2.2.1 highlights

- Link-aware router for relay mode, reducing reliance on dedupe to prevent loops.
- Full
  changelog: [v2.2.0...v2.2.1](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v2.2.0...v2.2.1)

## Version 2.1.0 highlights

- Improved relay handling with side-aware routing and transmit callbacks.
- Added a new full system test.
- Full
  changelog: [v2.0.0...v2.1.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v2.0.0...v2.1.0)

## Version 2.0.0 highlights

- Added `RouterMode` (Relay vs Sink) behavior.
- Fixed a bug where packet hashes were not saved, causing double processing.
- Full
  changelog: [v1.5.2...v2.0.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v1.5.2...v2.0.0)

## Version 1.5.2 highlights

- Added max queue size controls plus ring buffer behavior to prevent unbounded growth and heap overruns.
- Full
  changelog: [v1.5.1...v1.5.2](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v1.5.1...v1.5.2)

## Version 1.5.1 highlights

- Reduced memory usage for stack-stored packet payloads and overall memory footprint.
- Full
  changelog: [v1.5.0...v1.5.1](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v1.5.0...v1.5.1)

## Version 1.5.0 highlights

- Added payload and sender string compression with configurable thresholds.
- Full
  changelog: [v1.4.0...v1.5.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v1.4.0...v1.5.0)

## Version 1.4.0 highlights

- Added packet dedupe prevention.
- Improved README and added scripts for submodule usage and compile-time sender string setting.
- Full
  changelog: [v1.2.0...v1.4.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v1.2.0...v1.4.0)

## Version 1.2.0 highlights

- Added relay support for transporting packets across protocols (e.g., CAN to UART).
- Full
  changelog: [v1.1.1...v1.2.0](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v1.1.1...v1.2.0)

## Version 1.1.1 highlights

- Fixed broadcast behavior when all consumers have handlers or endpoints.
- Added support for packets containing no data.
- Full
  changelog: [v1.1.0...v1.1.1](https://github.com/Rylan-Meilutis/sedsprintf_rs/compare/v1.1.0...v1.1.1)

## Version 1.0.6 highlights

- Renamed hex data type to binary data.
- Fixed C API handling for 128-bit integers.

## Version 1.0.5 highlights

- Improved build script and update subtree script.
- Updated documentation.

## Version 1.0.1 highlights

- Performance optimizations and documentation improvements.
- Added error logging in no_std builds (requires external function hook).

## Version 1.0.0 highlights

- First stable release with routing, serialization, and packet creation across C, Rust, and Python.
- Marked API as stable.
