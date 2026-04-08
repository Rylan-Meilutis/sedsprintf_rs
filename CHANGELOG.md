# Changelog

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
- Default routing behavior still matches the existing model seeded by `RouterMode`: `Relay`
  initializes as a full side-to-side mesh, while `Sink` keeps RX-side forwarding disabled unless
  routing is explicitly enabled.
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
