// v4 runtime-schema build script.
//
// Telemetry endpoints and data types are no longer generated from JSON at
// compile time. Applications can seed the runtime registry with
// SEDSPRINTF_RS_STATIC_SCHEMA_PATH / SEDSPRINTF_RS_STATIC_IPC_SCHEMA_PATH, or
// register schema entries directly as nodes join the network.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(sedsprintf_has_telemetry_config_json)");
    println!("cargo:rerun-if-changed=telemetry_config.json");
    if std::path::Path::new("telemetry_config.json").is_file() {
        println!("cargo:rustc-cfg=sedsprintf_has_telemetry_config_json");
    }
    for key in [
        "DEVICE_IDENTIFIER",
        "MAX_RECENT_RX_IDS",
        "STARTING_QUEUE_SIZE",
        "MAX_QUEUE_BUDGET",
        "MAX_QUEUE_SIZE",
        "QUEUE_GROW_STEP",
        "PAYLOAD_COMPRESS_THRESHOLD",
        "STATIC_STRING_LENGTH",
        "STATIC_HEX_LENGTH",
        "STRING_PRECISION",
        "MAX_STACK_PAYLOAD",
        "MAX_HANDLER_RETRIES",
        "RELIABLE_RETRANSMIT_MS",
        "RELIABLE_MAX_RETRIES",
        "RELIABLE_MAX_PENDING",
        "RELIABLE_MAX_RETURN_ROUTES",
        "RELIABLE_MAX_END_TO_END_PENDING",
        "RELIABLE_MAX_END_TO_END_ACK_CACHE",
        "SEDSPRINTF_RS_STATIC_SCHEMA_PATH",
        "SEDSPRINTF_RS_STATIC_IPC_SCHEMA_PATH",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
}
