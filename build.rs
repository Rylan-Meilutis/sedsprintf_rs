// build.rs
//
// Generates C header + Python .pyi enums directly from telemetry_config.json,
// by *discovering the JSON path* from the Rust source that invokes:
//
//   define_telemetry_schema!(path = "telemetry_config.json");
//
// Also generates a C-friendly SedsResult enum from the TelemetryErrorCode enum
// found in src/lib.rs.
//
// No cbindgen is used. No intermediate temp header is written.
// Output:
//   - C-Headers/sedsprintf.h                     (injects enums into template marker)
//   - python-files/sedsprintf_rs/sedsprintf_rs.pyi (injects enums into template marker)
//
// Rebuild triggers:
//   - src/config.rs (or overridden)
//   - discovered telemetry_config.json
//   - src/lib.rs (or overridden)
//   - the header templates
//
// Optional env vars:
//   - SEDSPRINTF_RS_SKIP_ENUMGEN=1          -> skip all enum generation
//   - SEDSPRINTF_RS_CONFIG_RS=path/to.rs    -> override source file to scan (default: src/config.rs)
//   - SEDSPRINTF_RS_LIB_RS=path/to.rs       -> override lib.rs to scan (default: src/lib.rs)
//   - SEDSPRINTF_RS_SCHEMA_PATH=path        -> override telemetry_config.json path
//   - SEDSPRINTF_RS_IPC_SCHEMA_PATH=path    -> merge a board-local IPC/link-local overlay schema

use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const HEADER_ENUMS_MARKER: &str = "/* {{AUTOGEN:ENUMS}} */";
const HEADER_ABI_MARKER: &str = "/* {{AUTOGEN:ABI}} */";
const PY_ENUMS_MARKER: &str = "/* {{AUTOGEN:PY_ENUMS}} */";
const SCHEMA_PATH_ENV: &str = "SEDSPRINTF_RS_SCHEMA_PATH";
const IPC_SCHEMA_PATH_ENV: &str = "SEDSPRINTF_RS_IPC_SCHEMA_PATH";
const TEST_SCHEMA_FILE: &str = "telemetry_config.test.json";
const TEST_IPC_SCHEMA_FILE: &str = "telemetry_config.ipc.test.json";

// ========================= JSON schema =========================

#[derive(Debug, Deserialize)]
struct TelemetryConfig {
    endpoints: Vec<JsonEndpoint>,
    types: Vec<JsonType>,
}

#[derive(Debug, Deserialize)]
struct JsonEndpoint {
    /// Rust enum variant, e.g. "SdCard"
    rust: String,
    /// Schema string name, e.g. "SD_CARD" (ALL CAPS)
    name: String,
    /// Optional docstring for the enum variant
    #[serde(default)]
    doc: Option<String>,
    /// Deprecated legacy field; normalized into `link_local_only`.
    #[serde(default, alias = "broadcast_mode")]
    deprecated_broadcast_mode: Option<String>,
    /// Whether this endpoint is restricted to link-local/software-bus sides.
    #[serde(default, alias = "link_local_only")]
    _link_local_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct JsonType {
    /// Rust enum variant, e.g. "TelemetryError"
    rust: String,
    /// Schema string name, e.g. "TELEMETRY_ERROR" (ALL CAPS)
    name: String,
    #[serde(default)]
    doc: Option<String>,

    #[serde(default, alias = "reliable")]
    _reliable: Option<bool>,

    element: JsonElement,
    class: String,

    /// list of DataEndpoint rust variants, e.g. ["SdCard","Radio"]
    endpoints: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum JsonElement {
    Static { data_type: String },
    Dynamic { data_type: String },
}

// ========================= main =========================

fn main() {
    if env::var_os("SEDSPRINTF_RS_SKIP_ENUMGEN").is_some() {
        println!("cargo:warning=Skipping enum generation (SEDSPRINTF_RS_SKIP_ENUMGEN set)");
        return;
    }

    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));

    let config_rs_rel =
        env::var("SEDSPRINTF_RS_CONFIG_RS").unwrap_or_else(|_| "src/config.rs".to_string());
    let config_rs_path = crate_dir.join(&config_rs_rel);

    let lib_rs_rel = env::var("SEDSPRINTF_RS_LIB_RS").unwrap_or_else(|_| "src/lib.rs".to_string());
    let lib_rs_path = crate_dir.join(&lib_rs_rel);
    let c_api_rs_path = crate_dir.join("src/c_api.rs");

    // Rebuild triggers
    println!("cargo:rerun-if-changed={}", config_rs_path.display());
    println!("cargo:rerun-if-changed={}", lib_rs_path.display());
    println!("cargo:rerun-if-changed={}", c_api_rs_path.display());

    // Discover schema json path by scanning config.rs for define_telemetry_schema!(path="...")
    let schema_path = resolve_schema_path(&crate_dir, &config_rs_path);
    let ipc_schema_path = resolve_optional_overlay_path(&crate_dir);
    println!("cargo:rerun-if-changed={}", schema_path.display());
    if let Some(path) = &ipc_schema_path {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    // Re-run if the override env vars change (so switching paths rebuilds)
    println!("cargo:rerun-if-env-changed=SEDSPRINTF_RS_CONFIG_RS");
    println!("cargo:rerun-if-env-changed=SEDSPRINTF_RS_LIB_RS");
    println!("cargo:rerun-if-env-changed=SEDSPRINTF_RS_SKIP_ENUMGEN");
    println!("cargo:rerun-if-env-changed={SCHEMA_PATH_ENV}");
    println!("cargo:rerun-if-env-changed={IPC_SCHEMA_PATH_ENV}");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_TIMESYNC");

    // Make the discovered JSON path available to the crate at compile time.
    // Use in Rust as: env!("SEDSPRINTF_RS_SCHEMA_JSON")
    println!(
        "cargo:rustc-env=SEDSPRINTF_RS_SCHEMA_JSON={}",
        schema_path.display()
    );
    if let Some(path) = &ipc_schema_path {
        println!("cargo:rustc-env={IPC_SCHEMA_PATH_ENV}={}", path.display());
    }

    // Templates: rebuild if they change
    let header_tpl_path = crate_dir.join("header_templates/sedsprintf.h.txt");
    let pyi_tpl_path = crate_dir.join("header_templates/sedsprintf_rs.pyi.txt");
    println!("cargo:rerun-if-changed={}", header_tpl_path.display());
    println!("cargo:rerun-if-changed={}", pyi_tpl_path.display());

    // Load + validate JSON schema
    let mut cfg = load_schema_absolute(&schema_path);
    normalize_base_schema(&mut cfg);
    if let Some(path) = &ipc_schema_path {
        let overlay = load_schema_absolute(path);
        merge_ipc_overlay(&mut cfg, overlay, path);
    }
    let timesync_enabled = is_timesync_enabled();
    let discovery_enabled = is_discovery_enabled();
    validate_schema(&cfg, timesync_enabled, discovery_enabled);
    if timesync_enabled {
        append_timesync_builtins(&mut cfg);
    }
    if discovery_enabled {
        append_discovery_builtins(&mut cfg);
    }

    // Parse TelemetryErrorCode from lib.rs into SedsResult members
    let seds_result = parse_seds_result_from_lib_rs(&lib_rs_path);

    // Generate C enums text
    let (c_dt, c_ep) = render_c_enums(&cfg);
    let c_sr = render_c_enum_seds_result(&seds_result);
    let c_abi = render_c_abi_decls();

    let c_enums_joined = format!("{c_dt}\n\n{c_ep}\n\n{c_sr}\n");

    // Generate PYI enums text (DataType, DataEndpoint, SedsResult)
    let pyi = render_pyi_enums(&cfg, &seds_result);

    // Inject into templates
    write_injected_many(
        &header_tpl_path,
        &[
            (HEADER_ENUMS_MARKER, c_enums_joined.as_str()),
            (HEADER_ABI_MARKER, c_abi.as_str()),
        ],
        &crate_dir.join("C-Headers/sedsprintf.h"),
    );

    write_injected_many(
        &pyi_tpl_path,
        &[(PY_ENUMS_MARKER, pyi.as_str())],
        &crate_dir
            .join("python-files")
            .join("sedsprintf_rs")
            .join("sedsprintf_rs.pyi"),
    );
}

// ========================= discovery =========================

fn find_schema_path_from_config_rs(config_rs_path: &Path, crate_dir: &Path) -> PathBuf {
    let text = fs::read_to_string(config_rs_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", config_rs_path.display()));

    // Matches across newlines; captures the string literal contents
    // define_telemetry_schema!(path = "telemetry_config.json");
    let re = Regex::new(r#"(?s)define_telemetry_schema!\s*\(\s*[^)]*?\bpath\s*=\s*"([^"]+)""#)
        .expect("regex compile failed");

    let mut it = re.captures_iter(&text);
    let first = it.next().unwrap_or_else(|| {
        panic!(
            "could not find define_telemetry_schema!(path = \"...\") in {}",
            config_rs_path.display()
        )
    });
    let first_rel = first.get(1).unwrap().as_str().to_string();
    for cap in it {
        let rel = cap.get(1).unwrap().as_str();
        if rel != first_rel {
            panic!(
                "multiple define_telemetry_schema!(path = \"...\") invocations with different paths found in {} (expected identical paths)",
                config_rs_path.display()
            );
        }
    }

    crate_dir.join(first_rel)
}

fn resolve_schema_path(crate_dir: &Path, config_rs_path: &Path) -> PathBuf {
    if let Ok(val) = env::var(SCHEMA_PATH_ENV) {
        if val.trim().is_empty() {
            panic!("{SCHEMA_PATH_ENV} is set but empty");
        }
        let p = PathBuf::from(val);
        return if p.is_absolute() {
            p
        } else {
            crate_dir.join(p)
        };
    }

    if env::var_os("CARGO_CFG_TEST").is_some() {
        let test_path = crate_dir.join(TEST_SCHEMA_FILE);
        if test_path.exists() {
            // Ensure proc-macro expansion sees the test schema too.
            println!("cargo:rustc-env={SCHEMA_PATH_ENV}={}", test_path.display());
            println!("cargo:rerun-if-changed={}", test_path.display());
            return test_path;
        }
    }

    find_schema_path_from_config_rs(config_rs_path, crate_dir)
}

fn resolve_optional_overlay_path(crate_dir: &Path) -> Option<PathBuf> {
    if let Ok(val) = env::var(IPC_SCHEMA_PATH_ENV) {
        if val.trim().is_empty() {
            panic!("{IPC_SCHEMA_PATH_ENV} is set but empty");
        }
        let path = PathBuf::from(val);
        return Some(if path.is_absolute() {
            path
        } else {
            crate_dir.join(path)
        });
    }

    if env::var_os("CARGO_CFG_TEST").is_some() {
        let test_ipc_path = crate_dir.join(TEST_IPC_SCHEMA_FILE);
        if test_ipc_path.exists() {
            println!(
                "cargo:rustc-env={IPC_SCHEMA_PATH_ENV}={}",
                test_ipc_path.display()
            );
            println!("cargo:rerun-if-changed={}", test_ipc_path.display());
            return Some(test_ipc_path);
        }
    }

    None
}

// ========================= load / validate =========================

fn load_schema_absolute(path: &Path) -> TelemetryConfig {
    let bytes = fs::read(path)
        .unwrap_or_else(|e| panic!("failed to read telemetry schema {}: {e}", path.display()));

    serde_json::from_slice::<TelemetryConfig>(&bytes).unwrap_or_else(|e| {
        panic!(
            "failed to parse telemetry schema {} as JSON: {e}",
            path.display()
        )
    })
}

fn normalize_base_schema(base: &mut TelemetryConfig) {
    for ep in &mut base.endpoints {
        normalize_endpoint_legacy_fields(ep);
        if ep.deprecated_broadcast_mode.as_deref() != Some("Never") {
            ep._link_local_only = Some(false);
        }
    }
}

fn normalize_endpoint_legacy_fields(ep: &mut JsonEndpoint) {
    let Some(mode) = ep.deprecated_broadcast_mode.as_deref() else {
        return;
    };

    match mode {
        "Never" => {
            ep._link_local_only = Some(true);
            println!(
                "cargo:warning=Deprecated telemetry schema field broadcast_mode=\"Never\" on endpoint {} was normalized to link_local_only=true",
                ep.name
            );
        }
        "Default" | "Always" => {
            println!(
                "cargo:warning=Deprecated telemetry schema field broadcast_mode on endpoint {} is ignored; discovery now controls remote routing",
                ep.name
            );
        }
        other => {
            println!(
                "cargo:warning=Unknown deprecated broadcast_mode value {:?} on endpoint {}; ignoring it",
                other, ep.name
            );
        }
    }
}

fn merge_ipc_overlay(base: &mut TelemetryConfig, overlay: TelemetryConfig, overlay_path: &Path) {
    let mut overlay = overlay;
    let mut endpoint_rust: HashSet<&str> =
        base.endpoints.iter().map(|ep| ep.rust.as_str()).collect();
    let mut endpoint_name: HashSet<&str> =
        base.endpoints.iter().map(|ep| ep.name.as_str()).collect();
    let mut type_rust: HashSet<&str> = base.types.iter().map(|ty| ty.rust.as_str()).collect();
    let mut type_name: HashSet<&str> = base.types.iter().map(|ty| ty.name.as_str()).collect();

    for ep in &mut overlay.endpoints {
        ep._link_local_only = Some(true);
    }

    for ep in &overlay.endpoints {
        if !endpoint_rust.insert(ep.rust.as_str()) {
            panic!(
                "{}: IPC overlay endpoint rust variant {:?} collides with the base schema",
                overlay_path.display(),
                ep.rust
            );
        }
        if !endpoint_name.insert(ep.name.as_str()) {
            panic!(
                "{}: IPC overlay endpoint name {:?} collides with the base schema",
                overlay_path.display(),
                ep.name
            );
        }
    }

    for ty in &overlay.types {
        if !type_rust.insert(ty.rust.as_str()) {
            panic!(
                "{}: IPC overlay type rust variant {:?} collides with the base schema",
                overlay_path.display(),
                ty.rust
            );
        }
        if !type_name.insert(ty.name.as_str()) {
            panic!(
                "{}: IPC overlay type name {:?} collides with the base schema",
                overlay_path.display(),
                ty.name
            );
        }
    }

    base.endpoints.extend(overlay.endpoints);
    base.types.extend(overlay.types);
}

fn validate_schema(cfg: &TelemetryConfig, timesync_enabled: bool, discovery_enabled: bool) {
    for ty in &cfg.types {
        if ty.rust == "TelemetryError" || ty.name == "TELEMETRY_ERROR" {
            panic!(
                "telemetry_config.json: TelemetryError is built-in and must not be defined in the schema"
            );
        }
        if is_timesync_type(ty) {
            panic!(
                "telemetry_config.json: TimeSync types are built-in and must not be defined in the schema"
            );
        }
        if is_discovery_type(ty) {
            panic!(
                "telemetry_config.json: Discovery types are built-in and must not be defined in the schema"
            );
        }
    }
    for ep in &cfg.endpoints {
        if ep.rust == "TelemetryError" || ep.name == "TELEMETRY_ERROR" {
            panic!(
                "telemetry_config.json: TelemetryError endpoint is built-in and must not be defined in the schema"
            );
        }
        if is_timesync_endpoint(ep) {
            panic!(
                "telemetry_config.json: TimeSync endpoint is built-in and must not be defined in the schema"
            );
        }
        if is_discovery_endpoint(ep) {
            panic!(
                "telemetry_config.json: Discovery endpoint is built-in and must not be defined in the schema"
            );
        }
    }

    // Ensure schema names are ALL CAPS (or underscore/digit).
    for ep in &cfg.endpoints {
        ensure_rust_ident(&ep.rust, "endpoints[].rust");
        ensure_all_caps(&ep.name, "endpoints[].name");
    }

    let mut endpoint_set: HashSet<&str> = cfg.endpoints.iter().map(|e| e.rust.as_str()).collect();
    if timesync_enabled {
        endpoint_set.insert("TimeSync");
    }
    if discovery_enabled {
        endpoint_set.insert("Discovery");
    }
    endpoint_set.insert("TelemetryError");

    for ty in &cfg.types {
        ensure_rust_ident(&ty.rust, "types[].rust");
        ensure_all_caps(&ty.name, "types[].name");

        // Ensure referenced endpoints exist
        for eprust in &ty.endpoints {
            if !endpoint_set.contains(eprust.as_str()) {
                panic!(
                    "telemetry_config.json: type {} ({}) references unknown endpoint {:?}. Valid endpoints: {:?}",
                    ty.rust,
                    ty.name,
                    eprust,
                    cfg.endpoints
                        .iter()
                        .map(|e| e.rust.as_str())
                        .collect::<Vec<_>>()
                );
            }
        }

        let mut saw_link_local = false;
        let mut saw_non_link_local = false;
        for eprust in &ty.endpoints {
            if let Some(ep) = cfg.endpoints.iter().find(|ep| ep.rust == *eprust) {
                if ep._link_local_only.unwrap_or(false) {
                    saw_link_local = true;
                } else {
                    saw_non_link_local = true;
                }
            }
        }
        if saw_link_local && saw_non_link_local {
            panic!(
                "telemetry_config.json: type {} ({}) mixes link-local-only and normal endpoints; split it into separate types",
                ty.rust, ty.name
            );
        }

        // Sanity check element datatype + class
        match &ty.element {
            JsonElement::Static { data_type } | JsonElement::Dynamic { data_type } => {
                if data_type.trim().is_empty() {
                    panic!(
                        "telemetry_config.json: types[{}].element.data_type is empty",
                        ty.rust
                    );
                }
            }
        }
        if ty.class.trim().is_empty() {
            panic!("telemetry_config.json: types[{}].class is empty", ty.rust);
        }
    }
}

fn is_timesync_enabled() -> bool {
    env::var_os("CARGO_FEATURE_TIMESYNC").is_some()
}

fn is_discovery_enabled() -> bool {
    env::var_os("CARGO_FEATURE_DISCOVERY").is_some()
}

fn is_timesync_endpoint(ep: &JsonEndpoint) -> bool {
    ep.rust == "TimeSync" || ep.name == "TIME_SYNC"
}

fn is_discovery_endpoint(ep: &JsonEndpoint) -> bool {
    ep.rust == "Discovery" || ep.name == "DISCOVERY"
}

fn is_timesync_type(ty: &JsonType) -> bool {
    matches!(
        (ty.rust.as_str(), ty.name.as_str()),
        ("TimeSyncAnnounce", _)
            | ("TimeSyncRequest", _)
            | ("TimeSyncResponse", _)
            | (_, "TIME_SYNC_ANNOUNCE")
            | (_, "TIME_SYNC_REQUEST")
            | (_, "TIME_SYNC_RESPONSE")
    )
}

fn is_discovery_type(ty: &JsonType) -> bool {
    matches!(
        (ty.rust.as_str(), ty.name.as_str()),
        ("DiscoveryAnnounce", _)
            | (_, "DISCOVERY_ANNOUNCE")
            | ("DiscoveryTimeSyncSources", _)
            | (_, "DISCOVERY_TIMESYNC_SOURCES")
            | ("DiscoveryTopology", _)
            | (_, "DISCOVERY_TOPOLOGY")
    )
}

fn append_timesync_builtins(cfg: &mut TelemetryConfig) {
    cfg.endpoints.push(JsonEndpoint {
        rust: "TimeSync".to_string(),
        name: "TIME_SYNC".to_string(),
        doc: Some("Time sync routing endpoint (always forwarded).".to_string()),
        deprecated_broadcast_mode: None,
        _link_local_only: Some(false),
    });

    cfg.types.push(JsonType {
        rust: "TimeSyncAnnounce".to_string(),
        name: "TIME_SYNC_ANNOUNCE".to_string(),
        doc: Some("Time source announce (priority, time_ms).".to_string()),
        _reliable: Some(false),
        element: JsonElement::Static {
            data_type: "UInt64".to_string(),
        },
        class: "Data".to_string(),
        endpoints: vec!["TimeSync".to_string()],
    });

    cfg.types.push(JsonType {
        rust: "TimeSyncRequest".to_string(),
        name: "TIME_SYNC_REQUEST".to_string(),
        doc: Some("Time sync request (seq, t1_ms).".to_string()),
        _reliable: Some(false),
        element: JsonElement::Static {
            data_type: "UInt64".to_string(),
        },
        class: "Data".to_string(),
        endpoints: vec!["TimeSync".to_string()],
    });

    cfg.types.push(JsonType {
        rust: "TimeSyncResponse".to_string(),
        name: "TIME_SYNC_RESPONSE".to_string(),
        doc: Some("Time sync response (seq, t1_ms, t2_ms, t3_ms).".to_string()),
        _reliable: Some(false),
        element: JsonElement::Static {
            data_type: "UInt64".to_string(),
        },
        class: "Data".to_string(),
        endpoints: vec!["TimeSync".to_string()],
    });
}

fn append_discovery_builtins(cfg: &mut TelemetryConfig) {
    cfg.endpoints.push(JsonEndpoint {
        rust: "Discovery".to_string(),
        name: "DISCOVERY".to_string(),
        doc: Some("Discovery control endpoint for internal route advertisements.".to_string()),
        deprecated_broadcast_mode: None,
        _link_local_only: Some(false),
    });

    cfg.types.push(JsonType {
        rust: "DiscoveryAnnounce".to_string(),
        name: "DISCOVERY_ANNOUNCE".to_string(),
        doc: Some("Endpoint discovery advertisement (dynamic list of endpoint IDs).".to_string()),
        _reliable: Some(false),
        element: JsonElement::Dynamic {
            data_type: "UInt32".to_string(),
        },
        class: "Data".to_string(),
        endpoints: vec!["Discovery".to_string()],
    });

    cfg.types.push(JsonType {
        rust: "DiscoveryTimeSyncSources".to_string(),
        name: "DISCOVERY_TIMESYNC_SOURCES".to_string(),
        doc: Some(
            "Time sync source discovery advertisement (dynamic list of sender IDs).".to_string(),
        ),
        _reliable: Some(false),
        element: JsonElement::Dynamic {
            data_type: "UInt8".to_string(),
        },
        class: "Data".to_string(),
        endpoints: vec!["Discovery".to_string()],
    });

    cfg.types.push(JsonType {
        rust: "DiscoveryTopology".to_string(),
        name: "DISCOVERY_TOPOLOGY".to_string(),
        doc: Some(
            "Full board-topology discovery advertisement (boards, endpoints, and connections)."
                .to_string(),
        ),
        _reliable: Some(false),
        element: JsonElement::Dynamic {
            data_type: "UInt8".to_string(),
        },
        class: "Data".to_string(),
        endpoints: vec!["Discovery".to_string()],
    });
}

fn ensure_rust_ident(s: &str, field: &str) {
    let re = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap();
    if !re.is_match(s) {
        panic!("telemetry_config.json: {field} must be a valid Rust identifier, got {s:?}");
    }
}

fn ensure_all_caps(s: &str, field: &str) {
    let ok = s
        .chars()
        .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit());
    if !ok {
        panic!("telemetry_config.json: {field} must be ALL CAPS (A-Z, 0-9, _), got {s:?}");
    }
}

// ========================= lib.rs -> SedsResult =========================

#[derive(Debug, Clone)]
struct SedsResultEnum {
    // name -> value (i64)
    members: Vec<(String, i64)>, // (NAME, VALUE)
}

/// Parse TelemetryErrorCode from lib.rs and build a SedsResult enum mapping.
/// Rules:
/// - Includes all TelemetryErrorCode variants as SEDS_<NAME> with same numeric value.
/// - Ensures SEDS_OK = 0 exists (injected if missing).
/// - Ensures SEDS_ERR = -1 exists (injected if missing).
fn parse_seds_result_from_lib_rs(lib_rs_path: &Path) -> SedsResultEnum {
    let text = fs::read_to_string(lib_rs_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", lib_rs_path.display()));

    let re_enum = Regex::new(r#"(?s)\bpub\s+enum\s+TelemetryErrorCode\s*\{(.*?)}"#)
        .expect("regex compile failed");

    let caps = re_enum.captures(&text).unwrap_or_else(|| {
        panic!(
            "could not find `pub enum TelemetryErrorCode {{ ... }}` in {}",
            lib_rs_path.display()
        )
    });

    let body = caps.get(1).unwrap().as_str();

    let re_member = Regex::new(r#"(?m)^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(-?\d+)\s*,?\s*$"#)
        .expect("regex compile failed");

    let mut members: Vec<(String, i64)> = Vec::new();

    for m in re_member.captures_iter(body) {
        let rust_name = m.get(1).unwrap().as_str();
        let val: i64 = m.get(2).unwrap().as_str().parse().unwrap();
        let c_name = format!("SEDS_{}", to_screaming_snake(rust_name));
        members.push((c_name, val));
    }

    if members.is_empty() {
        panic!(
            "TelemetryErrorCode enum found, but no explicit `Variant = <int>` members were parsed in {}",
            lib_rs_path.display()
        );
    }

    // Inject SEDS_OK=0 and SEDS_ERR=-1 if missing
    let has_ok = members.iter().any(|(n, _)| n == "SEDS_OK");
    let has_err = members.iter().any(|(n, _)| n == "SEDS_ERR");
    if !has_ok {
        members.push(("SEDS_OK".to_string(), 0));
    }
    if !has_err {
        members.push(("SEDS_ERR".to_string(), -1));
    }

    // Dedup by name (keep first), then sort by value descending (0, -1, -2, ...)
    {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        members.retain(|(n, _)| seen.insert(n.clone()));
    }

    members.sort_by(|(na, va), (nb, vb)| {
        // primary: value descending
        vb.cmp(va)
            // secondary: name ascending (stable-ish)
            .then_with(|| na.cmp(nb))
    });

    SedsResultEnum { members }
}

fn to_screaming_snake(s: &str) -> String {
    // Simple PascalCase/camelCase -> SCREAMING_SNAKE.
    // Good enough for your variant names like GenericError, InvalidType, SizeMismatchError...
    let mut out = String::new();
    let mut prev_lower_or_digit = false;

    for ch in s.chars() {
        if ch.is_ascii_uppercase() {
            if prev_lower_or_digit {
                out.push('_');
            }
            out.push(ch);
            prev_lower_or_digit = false;
        } else if ch.is_ascii_lowercase() {
            out.push(ch.to_ascii_uppercase());
            prev_lower_or_digit = true;
        } else if ch.is_ascii_digit() {
            if !out.ends_with('_') && !out.is_empty() && !prev_lower_or_digit {
                // usually not needed; keep conservative
            }
            out.push(ch);
            prev_lower_or_digit = true;
        } else {
            // skip unexpected chars
        }
    }

    if out.is_empty() {
        "UNKNOWN".to_string()
    } else {
        out
    }
}

// ========================= render C enums =========================

fn render_c_enums(cfg: &TelemetryConfig) -> (String, String) {
    let dt = render_c_enum_datatype(cfg);
    let ep = render_c_enum_endpoint(cfg);
    (dt, ep)
}

fn render_c_enum_datatype(cfg: &TelemetryConfig) -> String {
    let mut lines = Vec::new();
    lines.push("typedef enum SedsDataType {".to_string());

    // Sequential discriminants from 0, JSON order then built-ins
    for (i, ty) in cfg.types.iter().enumerate() {
        // TELEMETRY_ERROR -> SEDS_DT_TELEMETRY_ERROR
        let name = format!("SEDS_DT_{}", ty.name);
        let doc = ty.doc.as_deref().unwrap_or("").trim();
        let idx = i;

        if !doc.is_empty() {
            lines.push(format!("  /* {} */", sanitize_c_comment(doc)));
        }
        lines.push(format!("  {name} = {idx},"));
    }

    lines.push("  /* Built-in TelemetryError */".to_string());
    lines.push(format!("  SEDS_DT_TELEMETRY_ERROR = {},", cfg.types.len()));

    lines.push("} SedsDataType;".to_string());
    lines.join("\n")
}

fn render_c_enum_endpoint(cfg: &TelemetryConfig) -> String {
    let mut lines = Vec::new();
    lines.push("typedef enum SedsDataEndpoint {".to_string());

    for (i, ep) in cfg.endpoints.iter().enumerate() {
        let name = format!("SEDS_EP_{}", ep.name);
        let doc = ep.doc.as_deref().unwrap_or("").trim();
        let idx = i;

        if !doc.is_empty() {
            lines.push(format!("  /* {} */", sanitize_c_comment(doc)));
        }
        lines.push(format!("  {name} = {idx},"));
    }

    lines.push("  /* Built-in TelemetryError endpoint */".to_string());
    lines.push(format!(
        "  SEDS_EP_TELEMETRY_ERROR = {},",
        cfg.endpoints.len()
    ));

    lines.push("} SedsDataEndpoint;".to_string());
    lines.join("\n")
}

fn render_c_enum_seds_result(sr: &SedsResultEnum) -> String {
    let mut lines = Vec::new();
    lines.push("typedef enum SedsResult {".to_string());

    for (name, val) in &sr.members {
        lines.push(format!("  {name} = {val},"));
    }

    lines.push("} SedsResult;".to_string());
    lines.join("\n")
}

fn sanitize_c_comment(s: &str) -> String {
    s.replace("*/", "* /")
}

// ========================= render PYI enums =========================

fn render_pyi_enums(cfg: &TelemetryConfig, sr: &SedsResultEnum) -> String {
    // This assumes your template already imports IntEnum, etc.
    let dt = render_python_intenum(
        "DataType",
        "Wire-level type tags (generated from telemetry_config.json).",
        cfg.types
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.as_str(), i as i64, t.doc.as_deref().unwrap_or("")))
            .chain(std::iter::once((
                "TELEMETRY_ERROR",
                cfg.types.len() as i64,
                "Built-in telemetry error text (string payload).",
            )))
            .collect::<Vec<_>>(),
    );

    let ep = render_python_intenum(
        "DataEndpoint",
        "Routing endpoints for packets (generated from telemetry_config.json).",
        cfg.endpoints
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.as_str(), i as i64, e.doc.as_deref().unwrap_or("")))
            .chain(std::iter::once((
                "TELEMETRY_ERROR",
                cfg.endpoints.len() as i64,
                "Built-in telemetry error endpoint.",
            )))
            .collect::<Vec<_>>(),
    );

    let sr_members = sr
        .members
        .iter()
        .map(|(name, val)| (name.as_str(), *val, ""))
        .collect::<Vec<_>>();

    let sr_txt = render_python_intenum(
        "SedsResult",
        "Result/error codes (generated from TelemetryErrorCode).",
        sr_members,
    );

    format!("{dt}\n\n{ep}\n\n{sr_txt}\n")
}

fn render_python_intenum(name: &str, doc: &str, members: Vec<(&str, i64, &str)>) -> String {
    let mut lines = Vec::new();
    lines.push(format!("class {name}(IntEnum):"));
    lines.push(format!("    \"\"\"{doc}\"\"\""));

    for (ident, value, member_doc) in members {
        if member_doc.trim().is_empty() {
            lines.push(format!("    {ident}: int = {value}"));
        } else {
            lines.push(format!(
                "    {ident}: int = {value}  #: {}",
                member_doc.trim()
            ));
        }
    }

    lines.join("\n")
}

fn render_c_abi_decls() -> String {
    [
        "/* =================================================================",
        "   Public ABI wrappers *AUTOGENERATED FROM RUST C ABI*",
        "   ================================================================= */",
        "",
        "/** @brief Legacy byte logger without timestamp or queue flag. */",
        "SedsResult seds_router_log_bytes(SedsRouter * r, SedsDataType ty, const uint8_t * data, size_t len);",
        "",
        "/** @brief Legacy f32 logger without timestamp or queue flag. */",
        "SedsResult seds_router_log_f32(SedsRouter * r, SedsDataType ty, const float * vals, size_t n_vals);",
        "",
        "/** @brief Legacy typed logger without timestamp or queue flag. */",
        "SedsResult seds_router_log_typed(SedsRouter * r,",
        "                                 SedsDataType ty,",
        "                                 const void * data,",
        "                                 size_t count,",
        "                                 size_t elem_size,",
        "                                 SedsElemKind elem_kind);",
        "",
        "/** @brief Legacy typed logger that enqueues instead of sending immediately. */",
        "SedsResult seds_router_log_queue_typed(SedsRouter * r,",
        "                                       SedsDataType ty,",
        "                                       const void * data,",
        "                                       size_t count,",
        "                                       size_t elem_size,",
        "                                       SedsElemKind elem_kind);",
        "",
        "/** @brief Byte logger with optional timestamp + queue flag. */",
        "SedsResult seds_router_log_bytes_ex(SedsRouter * r,",
        "                                    SedsDataType ty,",
        "                                    const uint8_t * data,",
        "                                    size_t len,",
        "                                    const uint64_t * timestamp_ms_opt,",
        "                                    int queue);",
        "",
        "/** @brief f32 logger with optional timestamp + queue flag. */",
        "SedsResult seds_router_log_f32_ex(SedsRouter * r,",
        "                                  SedsDataType ty,",
        "                                  const float * vals,",
        "                                  size_t n_vals,",
        "                                  const uint64_t * timestamp_ms_opt,",
        "                                  int queue);",
    ]
        .join("\n")
}

// ========================= template injection =========================

fn write_injected_many(template_path: &Path, replacements: &[(&str, &str)], out_path: &Path) {
    let tpl = fs::read_to_string(template_path)
        .unwrap_or_else(|e| panic!("read template {} failed: {e}", template_path.display()));

    let mut final_text = tpl;
    for (marker, injected) in replacements {
        if !final_text.contains(marker) {
            panic!(
                "template {} is missing marker: {}",
                template_path.display(),
                marker
            );
        }
        final_text = final_text.replace(marker, injected);
    }

    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("create dir {} failed: {e}", parent.display()));
    }

    fs::write(out_path, final_text)
        .unwrap_or_else(|e| panic!("write {} failed: {e}", out_path.display()));
}
