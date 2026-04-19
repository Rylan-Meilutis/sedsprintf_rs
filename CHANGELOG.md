# Changelog

## Unreleased

- Queue sizing now uses one shared dynamic `MAX_QUEUE_BUDGET` across router and relay internals
  instead of fixed per-queue caps.
- The compile-time queue knob has been renamed from `MAX_QUEUE_SIZE` to `MAX_QUEUE_BUDGET` to
  match its current meaning; the old environment name remains accepted as a legacy alias.
- Router and relay receive queues, transmit queues, reliable replay/out-of-order buffers, recent
  packet ID tracking, and discovery topology state now all draw from that same budget.
- Recent packet ID caches now preallocate their final storage at construction and reserve
  `min(MAX_RECENT_RX_IDS * sizeof(u64), MAX_QUEUE_BUDGET)` bytes from the shared queue budget.
- Discovery topology growth is bounded by the shared queue budget, and `std` builds emit a warning
  when topology state has to be evicted because the budget is exhausted.
- Ordered reliable receive paths now partial-ACK out-of-order packets so those packets do not get
  timeout-retransmitted while a gap is being recovered.
- Explicit `RELIABLE_PACKET_REQUEST` retransmits still work for partial-ACKed packets, so a packet
  can be held back from timeout traffic without becoming impossible to request again.
- Buffered reliable packets after a missing sequence are retained under the shared queue budget and
  released immediately once the missing packet arrives.
- Router and relay side-TX contention is now treated as transient backpressure: pending work is
  requeued and retried instead of surfacing an intermittent handler failure.
- Added regression coverage for shared queue-budget accounting, discovery topology budget pressure,
  partial reliable ACK behavior, side-TX busy retry handling, and the previously flaky threaded
  system flow.

## 3.11.1

- Discovery now carries a full transitive router graph with the built-in
  `DISCOVERY_TOPOLOGY` control packet instead of exporting only flattened side reachability.
- Routers and relays now preserve which sender IDs own which endpoints, which time-sync source IDs
  belong to which router, and which routers are connected to each other when that topology is
  forwarded across the network.
- `export_topology()` now exposes:
  - a top-level `routers` list with per-router endpoints, time-sync source IDs, and connections
  - per-side announcer detail so applications can see which upstream router advertised each
    portion of the graph
- Added client-facing topology export parity:
  - Python `Router.export_topology()` / `Relay.export_topology()`
  - C `seds_router_export_topology[_len]` / `seds_relay_export_topology[_len]` JSON exports
- Updated discovery, time-sync, Rust, Python, and C/C++ documentation to describe the richer
  topology model and export surfaces.

## 3.11.0

- Removed `RouterMode` from the active router model and moved router forwarding fully onto the
  same runtime route-rule system already used by `Relay`.
- Routers now default to a full forwarding mesh when no explicit route rules are installed, and
  discovery-driven multi-path routing now defaults to adaptive load balancing for normal traffic.
- Reliable delivery is now end-to-end verified in addition to the existing per-link reliable
  transport. Source routers retain locally-originated reliable packets until every currently
  discovered destination holder confirms local delivery.
- Destination routers now emit directed end-to-end reliable acknowledgements, and routers/relays
  learn return paths from reliable ingress traffic so those acknowledgements are routed only to the
  side that needs them instead of flooding unrelated links.
- When one discovered destination holder has already acknowledged, retries are narrowed to only the
  holders that are still outstanding instead of replaying to all holders again.
- If discovery later ages out one of those holders, the source drops that holder from the
  in-flight obligation set so the transaction completes instead of replaying forever toward a board
  that has disappeared from the topology view.
- Relays now prune their learned per-packet holder-ACK state against the same discovery view, so
  stale holder confirmations do not survive topology expiry or distort later route decisions.
- The end-to-end layer keeps the non-blocking reliable-stream behavior from `3.10.0`: waiting for
  holder ACKs does not stall the side/type lane for newer reliable packets.
- Added regression coverage for the new default routing model, adaptive discovery balancing, route
  disabling in place of sink-mode behavior, end-to-end reliable acknowledgement recovery for both
  single-destination and multi-holder delivery in the Rust system tests, and holder-expiry cleanup
  in both router and relay unit tests.
- Expanded documentation for testing, including unit tests, Rust system tests, C system tests, and
  local code-coverage reporting with `cargo llvm-cov`.

## 3.10.0

- Reworked reliable delivery in both `Router` and `Relay` to use built-in internal
  `RELIABLE_ACK` and `RELIABLE_PACKET_REQUEST` packet types instead of wire-only ACK-only frames.
- Reliable senders no longer block a side/type lane on one inflight packet. New reliable packets can
  continue sending while missing ordered packets are requested and retransmitted.
- Ordered reliable receive paths now buffer out-of-order frames, request the specific missing
  sequence, and release buffered packets once the gap is filled.
- Retransmits are now requeued with temporarily elevated priority instead of being sent as an
  exclusive inflight retry path, which improves recovery under mixed traffic and multi-destination
  fanout.
- Added regression coverage for non-blocking reliable send, router and relay retransmit recovery,
  and the new internal reliable control flow in both unit and system-style tests.
- Updated README and technical docs to describe the new internal reliable control packets and
  non-blocking retransmit behavior.

## 3.9.1

- Reserved the built-in `DISCOVERY` and `TIME_SYNC` endpoints for router-owned control traffic so
  user code can no longer register local handlers that shadow or distort internal discovery and
  time-sync behavior.
- `RouterConfig`, `EndpointHandler`, and the C router constructor now reject attempts to register
  handlers on those internal endpoints.
- Tightened combined queue processing so nonzero timeout budgets are split across TX and RX work,
  while `timeout_ms == 0` still drains both queues fully.
- Added regression coverage for queued discovery route learning, time-sync source learning, queue
  starvation prevention, and reserved-endpoint rejection in both Rust and the C ABI.
- Updated Rust and C/C++ docs to document that discovery and time sync are router-internal and
  not user-overridable.

## 3.9.0

- Added manual `DataType`-specific routing controls for both `Router` and `Relay` across Rust, C,
  and Python.
- Typed route rules now let a deployment restrict a given `(local TX or source side, data type)`
  to one or many explicitly selected destination sides, enabling dedicated links for commands,
  aborts, or other special traffic classes.
- Typed route allowlists layer on top of the existing side-level route policy, ingress/egress
  policy, and discovery/path-selection logic rather than bypassing them.
- Added regression coverage for router and relay typed-route fanout, fallback after clearing typed
  rules, precedence against base route disables, and matching C ABI coverage.
- Updated Rust, Python, C/C++, and technical routing documentation to describe the new typed-route
  APIs and routing precedence.

## 3.8.0

- Added path-selection policies for both `Router` and `Relay` across Rust, C, and Python.
- New source-side route modes let traffic keep current fanout behavior, split across multiple
  discovered paths with weighted round-robin, or use single-active failover routing.
- Per-route weights and priorities can now be configured at runtime, so deployments can do
  non-50/50 load balancing or choose a preferred primary link with ordered backups.
- Path failover now follows discovery liveness, side removal, and side disable state, so traffic
  automatically shifts to remaining eligible paths when a discovered path disappears.
- Added regression coverage for weighted split and failover behavior in both router and relay
  paths, including C ABI coverage.

## 3.7.0

- Added runtime side routing controls for both `Router` and `Relay` across Rust, C, and Python.
- `Relay` now also supports `remove_side(...)`, bringing side lifecycle parity with `Router`
  across Rust, C, and Python while preserving stable side IDs.
- Routers and relays now support per-side `ingress_enabled` and `egress_enabled` policy, so a
  deployment can use many RX sides while limiting TX to one or a selected subset of sides.
- Added runtime route overrides for `(local TX or source side) -> destination side`, enabling
  one-way relay paths such as `A -> B` while blocking `B -> A`, plus selective exclusion of
  specific sides from locally-originated traffic on both routers and relays.
- Default routing behavior still matched the older router model at this point: `RouterMode::Relay`
  initialized as a full side-to-side mesh, while `RouterMode::Sink` kept RX-side forwarding
  disabled unless routing was explicitly enabled.
- Discovery announcements now respect the active per-side egress policy and local route overrides,
  so advertised topology follows the currently allowed output links for both routers and relays.
- Added regression coverage for asymmetric routing, ingress-disabled sides, and the new C ABI
  runtime routing controls, including relay coverage.

## 3.6.0

- Added router-side removal APIs across Rust, C, and Python with stable side IDs preserved for
  surviving sides.
- Removed router sides are now tombstoned internally so stale side IDs stop routing traffic
  without forcing side ID renumbering.
- Removing a side now purges queued ingress/egress work plus reliable/discovery state associated
  with that side.
- Discovery topology changes caused by router side add/remove now immediately reschedule discovery
  announcements so peers learn newly available or remaining endpoint mappings faster.
- Added regression coverage for router-side removal, discovery topology export after removal, and
  the C ABI side-removal path.

## 3.5.2

- Fixed router-managed time sync failover so a consumer clears stale pending sync requests when
  the selected remote source disappears or leadership changes.
- This resolves a reconnection case where a consumer could continue holding over on an old source
  and fail to issue a new `TIME_SYNC_REQUEST` to the replacement source until rebooted.
- Added regression coverage for remote-source failover to ensure the replacement source is
  re-requested and accepted after timeout-driven re-election.

## 3.5.1

- Added consolidated router maintenance helpers: `periodic(timeout_ms)` and
  `periodic_no_timesync(timeout_ms)`. These bundle discovery polling and queue draining, and the
  latter lets applications skip time-sync maintenance for a loop iteration without disabling the
  feature globally.
- Added relay `periodic(timeout_ms)` to bundle discovery polling and queue draining into one main
  loop call.
- Exposed the new periodic APIs through the C ABI and Python bindings so Rust, C, and Python users
  have matching main-loop maintenance entry points.
- Updated Rust, C/C++, Python, and time-sync documentation to prefer the periodic helpers for
  ordinary application loops while keeping `poll_timesync()` / `poll_discovery()` documented as
  lower-level hooks.

## 3.5.0

- Removed schema-level `broadcast_mode` from the active telemetry schema model. Routing is now determined by discovery
  state and link-local scope instead of a per-endpoint broadcast policy.
- Added automatic upgrade handling for older schemas that still include `broadcast_mode`. `Never` now normalizes to
  `link_local_only = true`, while `Default` and `Always` are accepted as legacy no-ops.
- Kept proc-macro schema loading and `build.rs` schema loading behavior aligned so Rust codegen and generated bindings
  interpret legacy schemas the same way.
- Updated relay routing so discovered remote endpoint matches are targeted selectively, while non-local traffic can
  still bootstrap through fallback flooding before discovery converges.
- Restored release-test coverage for the discovery plus timesync path after the routing change;
  `./build.py test release` passes with the new behavior.
- Added `./build.py check`, which runs `cargo clippy -D warnings` across the default, python, and embedded builds, and
  folded that clippy coverage into `./build.py test`.
- Fixed generated C headers so the checked-in ABI header includes the current logging entry points, including
  `seds_router_log_typed`, `seds_router_log_queue_typed`, `seds_router_log_bytes`, and `seds_router_log_f32`.
- Updated the C header generation path in `build.rs` so `C-Headers/sedsprintf.h` is regenerated with the current ABI
  instead of drifting behind the exported symbols.
- Updated Python stub generation and example telemetry code to match the current public ABI and discovery helpers.
