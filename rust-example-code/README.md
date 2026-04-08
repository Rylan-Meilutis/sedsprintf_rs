# Rust Examples

Build with the features you need, for example:

- `cargo run --features timesync --bin timesync_example`
- `cargo run --features compression --bin reliable_example`

These examples are standalone source files; wire them into your own binary target
or copy the snippets as needed.

Additional example files in this folder:

- `load_balancing_example.rs`: weighted split and failover route selection.
- `typed_routing_example.rs`: route one packet type through two dedicated sides without load balancing.
- `relay_example.rs`: basic relay side wiring.
- `multinode_sim_example.rs`: multi-node simulation.
