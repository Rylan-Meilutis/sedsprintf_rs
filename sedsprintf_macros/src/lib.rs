use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{parse_macro_input, Ident, LitBool, LitInt, LitStr, Token};

// NOTE:
// This proc-macro crate must depend on:
//   serde = { version = "1", features = ["derive"] }
//   serde_json = "1"
//   proc-macro2, quote, syn
//
// And in Cargo.toml:
//   [lib]
//   proc-macro = true

// ============================================================
// define_stack_payload!
// ============================================================

/// define_stack_payload!(env = "MAX_STACK_PAYLOAD", default = 64);
///
/// Reads the *build-time* environment variable while the proc-macro runs.
/// - If missing or invalid, falls back to `default`.
///
/// Expands to:
/// - `pub const STACK_PAYLOAD_SIZE: usize = <resolved>;`
/// - `pub enum StandardSmallPayload { Inline1(SmallPayload<1>), ..., InlineN(...), Heap(Arc<[u8]>) }`
/// - impls with `new()`, `as_slice()`, `len()`, `to_arc()`, `is_inline()`, `Deref`, `Debug`.
struct StackArgs {
    env_name: String,
    default: usize,
}

impl Parse for StackArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        // env = "..."
        let k1: Ident = input.parse()?;
        if k1 != "env" {
            return Err(syn::Error::new_spanned(k1, "expected `env`"));
        }
        input.parse::<Token![=]>()?;
        let env_lit: LitStr = input.parse()?;

        input.parse::<Token![,]>()?;

        // default = 8
        let k2: Ident = input.parse()?;
        if k2 != "default" {
            return Err(syn::Error::new_spanned(k2, "expected `default`"));
        }
        input.parse::<Token![=]>()?;
        let default_lit: LitInt = input.parse()?;
        let default = default_lit.base10_parse::<usize>()?;

        Ok(Self {
            env_name: env_lit.value(),
            default,
        })
    }
}

fn read_max_from_env(env_key: &str, default: usize) -> usize {
    match std::env::var(env_key) {
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => default, // invalid -> default
        },
        Err(_) => default, // missing -> default
    }
}

#[proc_macro]
pub fn define_stack_payload(input: TokenStream) -> TokenStream {
    let StackArgs { env_name, default } = parse_macro_input!(input as StackArgs);

    let max = read_max_from_env(&env_name, default);

    // Compute powers of two up to `max`
    let mut caps = Vec::new();
    let mut cap = 1usize;
    while cap <= max {
        caps.push(cap);
        cap *= 2;
    }

    // Variants like `Inline1(SmallPayload<1>), Inline2(...), ...`
    let variants = caps.iter().map(|c| {
        let vname = syn::Ident::new(&format!("Inline{}", c), Span::call_site());
        quote! {
            #vname(crate::small_payload::SmallPayload<#c>)
        }
    });

    // new() selection arms
    let new_arms = caps.iter().map(|c| {
        let vname = syn::Ident::new(&format!("Inline{}", c), Span::call_site());
        quote! {
            if len <= #c {
                return StandardSmallPayload::#vname(
                    crate::small_payload::SmallPayload::<#c>::new(data)
                );
            }
        }
    });

    // match arms for as_slice()
    let as_slice_arms = caps.iter().map(|c| {
        let vname = syn::Ident::new(&format!("Inline{}", c), Span::call_site());
        quote! {
            StandardSmallPayload::#vname(inner) => inner.as_slice(),
        }
    });

    // match arms for len()
    let len_arms = caps.iter().map(|c| {
        let vname = syn::Ident::new(&format!("Inline{}", c), Span::call_site());
        quote! {
            StandardSmallPayload::#vname(inner) => inner.len(),
        }
    });

    // match arms for is_empty()
    let is_empty_arms = caps.iter().map(|c| {
        let vname = syn::Ident::new(&format!("Inline{}", c), Span::call_site());
        quote! {
            StandardSmallPayload::#vname(inner) => inner.is_empty(),
        }
    });

    // match arms for to_arc()
    let to_arc_arms = caps.iter().map(|c| {
        let vname = syn::Ident::new(&format!("Inline{}", c), Span::call_site());
        quote! {
            StandardSmallPayload::#vname(inner) => inner.to_arc(),
        }
    });

    // match arms for is_inline()
    let is_inline_arms = caps.iter().map(|c| {
        let vname = syn::Ident::new(&format!("Inline{}", c), Span::call_site());
        quote! {
            StandardSmallPayload::#vname(inner) => inner.is_inline(),
        }
    });

    // Debug arms
    let debug_arms = caps.iter().map(|c| {
        let vname = syn::Ident::new(&format!("Inline{}", c), Span::call_site());
        quote! {
            StandardSmallPayload::#vname(inner) => core::fmt::Debug::fmt(inner, f),
        }
    });

    // match arms for byte_cost()
    let byte_cost_arms = caps.iter().map(|c| {
        let vname = syn::Ident::new(&format!("Inline{}", c), Span::call_site());
        quote! {
            StandardSmallPayload::#vname(inner) => crate::queue::ByteCost::byte_cost(inner),
        }
    });

    let expanded = quote! {
        /// Max stack payload size generated by `define_stack_payload!`.
        ///
        /// This value is resolved at compile time by the proc-macro by reading
        /// the build environment variable specified in the macro invocation.
        pub const STACK_PAYLOAD_SIZE: usize = #max;

        #[derive(Clone)]
        pub enum StandardSmallPayload {
            #(#variants),*,
            Heap(alloc::sync::Arc<[u8]>),
        }

        impl StandardSmallPayload {
            #[inline]
            pub fn new(data: &[u8]) -> Self {
                let len = data.len();
                #(#new_arms)*
                StandardSmallPayload::Heap(alloc::sync::Arc::from(data))
            }

            #[inline]
            pub fn as_slice(&self) -> &[u8] {
                match self {
                    #(#as_slice_arms)*
                    StandardSmallPayload::Heap(arc) => arc,
                }
            }

            #[inline]
            pub fn len(&self) -> usize {
                match self {
                    #(#len_arms)*
                    StandardSmallPayload::Heap(arc) => arc.len(),
                }
            }

            #[inline]
            pub fn is_empty(&self) -> bool {
                match self {
                    #(#is_empty_arms)*
                    StandardSmallPayload::Heap(arc) => arc.is_empty(),
                }
            }

            #[inline]
            pub fn to_arc(&self) -> alloc::sync::Arc<[u8]> {
                match self {
                    #(#to_arc_arms)*
                    StandardSmallPayload::Heap(arc) => arc.clone(),
                }
            }

            #[inline]
            pub fn is_inline(&self) -> bool {
                match self {
                    #(#is_inline_arms)*
                    StandardSmallPayload::Heap(_) => false,
                }
            }
        }

        impl crate::queue::ByteCost for StandardSmallPayload {
            #[inline]
            fn byte_cost(&self) -> usize {
                match self {
                    #(#byte_cost_arms)*
                    StandardSmallPayload::Heap(arc) => {
                        core::mem::size_of::<StandardSmallPayload>() + arc.len()
                    }
                }
            }
        }

        impl core::cmp::PartialEq for StandardSmallPayload {
            #[inline]
            fn eq(&self, other: &Self) -> bool {
                self.as_slice() == other.as_slice()
            }
        }
        impl core::cmp::Eq for StandardSmallPayload {}

        impl core::cmp::PartialEq<[u8]> for StandardSmallPayload {
            #[inline]
            fn eq(&self, other: &[u8]) -> bool {
                self.as_slice() == other
            }
        }

        impl core::cmp::PartialEq<StandardSmallPayload> for [u8] {
            #[inline]
            fn eq(&self, other: &StandardSmallPayload) -> bool {
                self == other.as_slice()
            }
        }

        impl core::cmp::PartialEq<alloc::sync::Arc<[u8]>> for StandardSmallPayload {
            #[inline]
            fn eq(&self, other: &alloc::sync::Arc<[u8]>) -> bool {
                self.as_slice() == other.as_ref()
            }
        }

        impl core::ops::Deref for StandardSmallPayload {
            type Target = [u8];
            #[inline]
            fn deref(&self) -> &[u8] {
                self.as_slice()
            }
        }

        impl core::fmt::Debug for StandardSmallPayload {
            #[inline]
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                match self {
                    #(#debug_arms)*
                    StandardSmallPayload::Heap(arc) => write!(
                        f,
                        "{}::Heap({} bytes)",
                        stringify!(StandardSmallPayload),
                        arc.len()
                    ),
                }
            }
        }
    };

    expanded.into()
}

// ============================================================
// define_telemetry_schema!
// ============================================================
//
// Usage (in caller crate):
//   define_telemetry_schema!(path = "telemetry_config.json");
//
// Path resolution:
// - First tries: <caller crate>/CARGO_MANIFEST_DIR + path
// - If that env var is missing (non-Cargo builds), errors with a helpful message.
//
// Naming requirements:
// - JsonEndpoint.name and JsonType.name MUST be ALL CAPS / underscores (e.g. "RADIO", "GPS_DATA")
// - JsonEndpoint.rust and JsonType.rust MUST be valid Rust PascalCase identifiers (e.g. "Radio", "GpsData")

#[derive(Debug, serde::Deserialize)]
struct TelemetryConfig {
    endpoints: Vec<JsonEndpoint>,
    types: Vec<JsonType>,
}

#[derive(Debug, serde::Deserialize)]
struct JsonEndpoint {
    /// Rust enum variant, e.g. "Radio"
    rust: String,
    /// Schema string name, e.g. "RADIO" (MUST be all caps)
    name: String,
    /// Optional docstring for the enum variant
    #[serde(default)]
    doc: Option<String>,
    /// Deprecated legacy field; normalized into `link_local_only`.
    #[serde(default, alias = "broadcast_mode")]
    deprecated_broadcast_mode: Option<String>,
    #[serde(default)]
    link_local_only: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
struct JsonType {
    /// Rust enum variant, e.g. "GpsData"
    rust: String,
    /// Schema string name, e.g. "GPS_DATA" (MUST be all caps)
    name: String,
    #[serde(default)]
    doc: Option<String>,

    #[serde(default)]
    reliable: Option<bool>,
    #[serde(default)]
    reliable_mode: Option<String>,
    #[serde(default)]
    priority: Option<u8>,

    element: JsonElement,
    class: String,

    /// list of DataEndpoint rust variants, e.g. ["SdCard","Radio"]
    endpoints: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "kind")]
enum JsonElement {
    Static { count: usize, data_type: String },
    Dynamic { data_type: String },
}

struct SchemaArgs {
    path: String,
    timesync: bool,
    discovery: bool,
}

impl Parse for SchemaArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut path: Option<String> = None;
        let mut timesync = false;
        let mut discovery = false;

        while !input.is_empty() {
            let k: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            if k == "path" {
                let path_lit: LitStr = input.parse()?;
                path = Some(path_lit.value());
            } else if k == "timesync" {
                let ts_lit: LitBool = input.parse()?;
                timesync = ts_lit.value();
            } else if k == "discovery" {
                let disc_lit: LitBool = input.parse()?;
                discovery = disc_lit.value();
            } else {
                return Err(syn::Error::new_spanned(
                    k,
                    "expected `path`, `timesync`, or `discovery`",
                ));
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        let path = path.ok_or_else(|| syn::Error::new(Span::call_site(), "missing `path`"))?;
        Ok(Self {
            path,
            timesync,
            discovery,
        })
    }
}

fn ensure_valid_ident(s: &str) -> Result<syn::Ident, String> {
    syn::parse_str::<syn::Ident>(s).map_err(|_| format!("invalid Rust identifier: {s:?}"))
}

fn ensure_caps_name(kind: &str, s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("{kind}.name is empty"));
    }
    let ok = s
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(format!(
            "{kind}.name must be ALL CAPS/underscore (e.g. \"RADIO\", \"GPS_DATA\"); got {s:?}"
        ));
    }
    Ok(())
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
            | (_, "DISCOVERY_TIMESYNC_SOURCES")
    )
}

fn caller_manifest_dir() -> Result<std::path::PathBuf, String> {
    let m = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "CARGO_MANIFEST_DIR not set; define_telemetry_schema! must be built via Cargo (or switch to an include_str!-based macro)".to_string())?;
    Ok(std::path::PathBuf::from(m))
}

fn resolve_schema_path(path_rel: &str) -> Result<std::path::PathBuf, String> {
    let root = caller_manifest_dir()?;
    if let Ok(override_path) = std::env::var("SEDSPRINTF_RS_SCHEMA_PATH") {
        if override_path.trim().is_empty() {
            return Err("SEDSPRINTF_RS_SCHEMA_PATH is set but empty".to_string());
        }
        let p = std::path::PathBuf::from(override_path);
        if p.is_absolute() {
            return Ok(p);
        }
        return Ok(root.join(p));
    }
    Ok(root.join(path_rel))
}

fn resolve_optional_ipc_schema_path() -> Result<Option<std::path::PathBuf>, String> {
    let root = caller_manifest_dir()?;
    let Ok(override_path) = std::env::var("SEDSPRINTF_RS_IPC_SCHEMA_PATH") else {
        return Ok(None);
    };
    if override_path.trim().is_empty() {
        return Err("SEDSPRINTF_RS_IPC_SCHEMA_PATH is set but empty".to_string());
    }
    let path = std::path::PathBuf::from(override_path);
    Ok(Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    }))
}

fn load_schema(path_rel: &str) -> Result<TelemetryConfig, String> {
    let root = caller_manifest_dir()?;
    let path = resolve_schema_path(path_rel)?;

    let bytes = std::fs::read(&path).map_err(|e| {
        format!(
            "failed to read telemetry schema at {}: {e}\n\
             note: path is resolved as CARGO_MANIFEST_DIR ({}) + {:?} (override with SEDSPRINTF_RS_SCHEMA_PATH)",
            path.display(),
            root.display(),
            path_rel
        )
    })?;

    serde_json::from_slice::<TelemetryConfig>(&bytes)
        .map_err(|e| format!("failed to parse {} as JSON: {e}", path.display()))
}

fn normalize_base_schema(cfg: &mut TelemetryConfig) {
    for ep in &mut cfg.endpoints {
        normalize_endpoint_legacy_fields(ep);
        if ep.deprecated_broadcast_mode.as_deref() != Some("Never") {
            ep.link_local_only = Some(false);
        }
    }
}

fn normalize_endpoint_legacy_fields(ep: &mut JsonEndpoint) {
    let Some(mode) = ep.deprecated_broadcast_mode.as_deref() else {
        return;
    };

    if mode == "Never" {
        ep.link_local_only = Some(true);
    }
}

fn load_merged_schema(path_rel: &str) -> Result<TelemetryConfig, String> {
    let mut cfg = load_schema(path_rel)?;
    normalize_base_schema(&mut cfg);
    let Some(overlay_path) = resolve_optional_ipc_schema_path()? else {
        return Ok(cfg);
    };

    let bytes = std::fs::read(&overlay_path).map_err(|e| {
        format!(
            "failed to read IPC overlay schema at {}: {e}\n\
             note: set SEDSPRINTF_RS_IPC_SCHEMA_PATH to a JSON file containing only board-local link-local endpoints/types",
            overlay_path.display()
        )
    })?;

    let overlay = serde_json::from_slice::<TelemetryConfig>(&bytes)
        .map_err(|e| format!("failed to parse {} as JSON: {e}", overlay_path.display()))?;
    merge_ipc_overlay(&mut cfg, overlay, &overlay_path)?;
    Ok(cfg)
}

fn merge_ipc_overlay(
    base: &mut TelemetryConfig,
    overlay: TelemetryConfig,
    overlay_path: &std::path::Path,
) -> Result<(), String> {
    let mut overlay = overlay;
    let mut endpoint_rust = std::collections::HashSet::<&str>::new();
    let mut endpoint_name = std::collections::HashSet::<&str>::new();
    let mut type_rust = std::collections::HashSet::<&str>::new();
    let mut type_name = std::collections::HashSet::<&str>::new();

    for ep in &base.endpoints {
        endpoint_rust.insert(ep.rust.as_str());
        endpoint_name.insert(ep.name.as_str());
    }
    for ty in &base.types {
        type_rust.insert(ty.rust.as_str());
        type_name.insert(ty.name.as_str());
    }

    for ep in &mut overlay.endpoints {
        ep.link_local_only = Some(true);
    }

    for ep in &overlay.endpoints {
        if !endpoint_rust.insert(ep.rust.as_str()) {
            return Err(format!(
                "{}: IPC overlay endpoint rust variant {:?} collides with the base schema",
                overlay_path.display(),
                ep.rust
            ));
        }
        if !endpoint_name.insert(ep.name.as_str()) {
            return Err(format!(
                "{}: IPC overlay endpoint name {:?} collides with the base schema",
                overlay_path.display(),
                ep.name
            ));
        }
    }

    for ty in &overlay.types {
        if !type_rust.insert(ty.rust.as_str()) {
            return Err(format!(
                "{}: IPC overlay type rust variant {:?} collides with the base schema",
                overlay_path.display(),
                ty.rust
            ));
        }
        if !type_name.insert(ty.name.as_str()) {
            return Err(format!(
                "{}: IPC overlay type name {:?} collides with the base schema",
                overlay_path.display(),
                ty.name
            ));
        }
    }

    base.endpoints.extend(overlay.endpoints);
    base.types.extend(overlay.types);
    Ok(())
}

fn msg_datatype_token(name: &str) -> proc_macro2::TokenStream {
    // Must match caller crate's MessageDataType variant names exactly.
    let id = syn::Ident::new(name, Span::call_site());
    quote!(MessageDataType::#id)
}

fn msg_class_token(name: &str) -> proc_macro2::TokenStream {
    // Must match caller crate's MessageClass variant names exactly.
    let id = syn::Ident::new(name, Span::call_site());
    quote!(MessageClass::#id)
}

fn reliable_mode_token(mode: &str) -> Result<proc_macro2::TokenStream, String> {
    let mode_lc = mode.to_ascii_lowercase();
    let ts = match mode_lc.as_str() {
        "none" => quote!(crate::ReliableMode::None),
        "ordered" => quote!(crate::ReliableMode::Ordered),
        "unordered" => quote!(crate::ReliableMode::Unordered),
        _ => {
            return Err(format!(
                "invalid reliable_mode: {mode:?} (expected \"None\", \"Ordered\", or \"Unordered\")"
            ))
        }
    };
    Ok(ts)
}

#[proc_macro]
pub fn define_telemetry_schema(input: TokenStream) -> TokenStream {
    let SchemaArgs {
        path,
        timesync,
        discovery,
    } = parse_macro_input!(input as SchemaArgs);

    let cfg = match load_merged_schema(&path) {
        Ok(v) => v,
        Err(e) => return syn::Error::new(Span::call_site(), e).to_compile_error().into(),
    };
    let timesync_enabled = timesync;
    let discovery_enabled = discovery;

    for ty in &cfg.types {
        if ty.rust == "TelemetryError" || ty.name == "TELEMETRY_ERROR" {
            return syn::Error::new(
                Span::call_site(),
                "telemetry_config.json: TelemetryError is built-in and must not be defined in the schema",
            )
                .to_compile_error()
                .into();
        }
        if is_timesync_type(ty) {
            return syn::Error::new(
                Span::call_site(),
                "telemetry_config.json: TimeSync types are built-in and must not be defined in the schema",
            )
                .to_compile_error()
                .into();
        }
        if is_discovery_type(ty) {
            return syn::Error::new(
                Span::call_site(),
                "telemetry_config.json: Discovery types are built-in and must not be defined in the schema",
            )
                .to_compile_error()
                .into();
        }
    }
    for ep in &cfg.endpoints {
        if ep.rust == "TelemetryError" || ep.name == "TELEMETRY_ERROR" {
            return syn::Error::new(
                Span::call_site(),
                "telemetry_config.json: TelemetryError endpoint is built-in and must not be defined in the schema",
            )
                .to_compile_error()
                .into();
        }
        if is_timesync_endpoint(ep) {
            return syn::Error::new(
                Span::call_site(),
                "telemetry_config.json: TimeSync endpoint is built-in and must not be defined in the schema",
            )
                .to_compile_error()
                .into();
        }
        if is_discovery_endpoint(ep) {
            return syn::Error::new(
                Span::call_site(),
                "telemetry_config.json: Discovery endpoint is built-in and must not be defined in the schema",
            )
                .to_compile_error()
                .into();
        }
    }

    // ----------------------------
    // Validate + prep endpoints
    // ----------------------------
    let mut ep_idents = Vec::<syn::Ident>::new();
    let mut ep_docs = Vec::<String>::new();
    let mut ep_names = Vec::<String>::new();
    let mut ep_link_local = Vec::<bool>::new();

    for ep in &cfg.endpoints {
        if let Err(e) = ensure_caps_name("endpoint", &ep.name) {
            return syn::Error::new(Span::call_site(), e).to_compile_error().into();
        }

        let id = match ensure_valid_ident(&ep.rust) {
            Ok(id) => id,
            Err(e) => return syn::Error::new(Span::call_site(), e).to_compile_error().into(),
        };

        ep_idents.push(id);
        ep_docs.push(ep.doc.clone().unwrap_or_default());
        ep_names.push(ep.name.clone());
        ep_link_local.push(ep.link_local_only.unwrap_or(false));
    }

    if timesync_enabled {
        ep_idents.push(syn::Ident::new("TimeSync", Span::call_site()));
        ep_docs.push("Time sync routing endpoint (always forwarded).".to_string());
        ep_names.push("TIME_SYNC".to_string());
        ep_link_local.push(false);
    }
    if discovery_enabled {
        ep_idents.push(syn::Ident::new("Discovery", Span::call_site()));
        ep_docs.push("Discovery control endpoint for internal route advertisements.".to_string());
        ep_names.push("DISCOVERY".to_string());
        ep_link_local.push(false);
    }

    ep_idents.push(syn::Ident::new("TelemetryError", Span::call_site()));
    ep_docs.push(
        "Encoded telemetry error text (string payload) (CRITICAL FOR SYSTEM FUNCTIONALITY, DO NOT REMOVE)"
            .to_string(),
    );
    ep_names.push("TELEMETRY_ERROR".to_string());
    ep_link_local.push(false);

    let max_ep_value = cfg.endpoints.len() as u32
        + if timesync_enabled { 1 } else { 0 }
        + if discovery_enabled { 1 } else { 0 };

    let ep_variants = ep_idents.iter().zip(ep_docs.iter()).map(|(id, doc)| {
        if doc.is_empty() {
            quote!(#id,)
        } else {
            quote!(
                #[doc = #doc]
                #id,
            )
        }
    });

    // DataEndpoint::as_str() uses JSON "name" (RADIO, SD_CARD, ...)
    let ep_meta_arms = ep_idents
        .iter()
        .zip(ep_names.iter())
        .zip(ep_link_local.iter())
        .map(|((id, name), link_local_only)| {
            let link_local_only = *link_local_only;
            quote! {
                DataEndpoint::#id => EndpointMeta {
                    name: #name,
                    link_local_only: #link_local_only,
                },
            }
        });

    // ----------------------------
    // Validate + prep types
    // ----------------------------
    let mut ty_entries: Vec<(syn::Ident, String)> = Vec::new();

    for ty in &cfg.types {
        if let Err(e) = ensure_caps_name("type", &ty.name) {
            return syn::Error::new(Span::call_site(), e).to_compile_error().into();
        }

        let mut saw_link_local = false;
        let mut saw_non_link_local = false;
        for eprust in &ty.endpoints {
            if let Some(ep) = cfg.endpoints.iter().find(|ep| ep.rust == *eprust) {
                if ep.link_local_only.unwrap_or(false) {
                    saw_link_local = true;
                } else {
                    saw_non_link_local = true;
                }
            }
        }
        if saw_link_local && saw_non_link_local {
            return syn::Error::new(
                Span::call_site(),
                format!(
                    "telemetry_config.json: type {} ({}) mixes link-local-only and normal endpoints; split it into separate types",
                    ty.rust, ty.name
                ),
            )
                .to_compile_error()
                .into();
        }

        let id = match ensure_valid_ident(&ty.rust) {
            Ok(id) => id,
            Err(e) => return syn::Error::new(Span::call_site(), e).to_compile_error().into(),
        };

        ty_entries.push((id, ty.doc.clone().unwrap_or_default()));
    }

    if timesync_enabled {
        ty_entries.push((
            syn::Ident::new("TimeSyncAnnounce", Span::call_site()),
            "Time source announce (priority, time_ms).".to_string(),
        ));
        ty_entries.push((
            syn::Ident::new("TimeSyncRequest", Span::call_site()),
            "Time sync request (seq, t1_ms).".to_string(),
        ));
        ty_entries.push((
            syn::Ident::new("TimeSyncResponse", Span::call_site()),
            "Time sync response (seq, t1_ms, t2_ms, t3_ms).".to_string(),
        ));
    }
    if discovery_enabled {
        ty_entries.push((
            syn::Ident::new("DiscoveryAnnounce", Span::call_site()),
            "Endpoint discovery advertisement (dynamic list of endpoint IDs).".to_string(),
        ));
        ty_entries.push((
            syn::Ident::new("DiscoveryTimeSyncSources", Span::call_site()),
            "Time sync source discovery advertisement (dynamic list of sender IDs).".to_string(),
        ));
    }
    ty_entries.push((
        syn::Ident::new("ReliableAck", Span::call_site()),
        "Internal reliable-delivery acknowledgement (type, seq).".to_string(),
    ));
    ty_entries.push((
        syn::Ident::new("ReliablePacketRequest", Span::call_site()),
        "Internal reliable-delivery retransmit request (type, seq).".to_string(),
    ));

    let max_ty_value = cfg.types.len() as u32
        + if timesync_enabled { 3 } else { 0 }
        + if discovery_enabled { 2 } else { 0 }
        + 2;

    let ty_variants = ty_entries
        .iter()
        .cloned()
        .chain(std::iter::once((
            syn::Ident::new("TelemetryError", Span::call_site()),
            "Encoded telemetry error text (string payload) (CRITICAL FOR SYSTEM FUNCTIONALITY, DO NOT REMOVE)"
                .to_string(),
        )))
        .map(|(id, doc)| {
            if doc.is_empty() {
                quote!(#id,)
            } else {
                quote!(
                #[doc = #doc]
                #id,
            )
            }
        });

    let builtin_ty_meta = {
        let endpoints_tokens: Vec<proc_macro2::TokenStream> =
            vec![quote!(DataEndpoint::TelemetryError)];
        quote! {
            DataType::ReliableAck => MessageMeta {
                name: "RELIABLE_ACK",
                element: MessageElement::Static(2, MessageDataType::UInt32, MessageClass::Data),
                endpoints: &[#(#endpoints_tokens),*],
                reliable: crate::ReliableMode::None,
                priority: 250,
            },
            DataType::ReliablePacketRequest => MessageMeta {
                name: "RELIABLE_PACKET_REQUEST",
                element: MessageElement::Static(2, MessageDataType::UInt32, MessageClass::Data),
                endpoints: &[#(#endpoints_tokens),*],
                reliable: crate::ReliableMode::None,
                priority: 250,
            },
            DataType::TelemetryError => MessageMeta {
                name: "TELEMETRY_ERROR",
                element: MessageElement::Dynamic(MessageDataType::String, MessageClass::Error),
                endpoints: &[#(#endpoints_tokens),*],
                reliable: crate::ReliableMode::Ordered,
                priority: 200,
            },
        }
    };

    let ty_meta_arms = cfg.types.iter().map(|ty| {
        let rust_id = syn::Ident::new(&ty.rust, Span::call_site());
        let name = &ty.name;
        let reliable_mode = match &ty.reliable_mode {
            Some(mode) => match reliable_mode_token(mode) {
                Ok(ts) => ts,
                Err(e) => return syn::Error::new(Span::call_site(), e).to_compile_error(),
            },
            None => {
                if ty.reliable.unwrap_or(false) {
                    quote!(crate::ReliableMode::Ordered)
                } else {
                    quote!(crate::ReliableMode::None)
                }
            }
        };

        let class_ts = msg_class_token(&ty.class);
        let priority = ty.priority.unwrap_or(0);

        let endpoints_tokens: Vec<proc_macro2::TokenStream> = ty
            .endpoints
            .iter()
            .map(|eprust| {
                let e = syn::Ident::new(eprust, Span::call_site());
                quote!(DataEndpoint::#e)
            })
            .collect();

        let element_ts = match &ty.element {
            JsonElement::Static { count, data_type } => {
                let dt = msg_datatype_token(data_type);
                quote!(MessageElement::Static(#count, #dt, #class_ts))
            }
            JsonElement::Dynamic { data_type } => {
                let dt = msg_datatype_token(data_type);
                quote!(MessageElement::Dynamic(#dt, #class_ts))
            }
        };

        quote! {
            DataType::#rust_id => MessageMeta {
                name: #name,
                element: #element_ts,
                endpoints: &[#(#endpoints_tokens),*],
                reliable: #reliable_mode,
                priority: #priority,
            },
        }
    });

    let timesync_ty_meta = if timesync_enabled {
        quote! {
            DataType::TimeSyncAnnounce => MessageMeta {
                name: "TIME_SYNC_ANNOUNCE",
                element: MessageElement::Static(2, MessageDataType::UInt64, MessageClass::Data),
                endpoints: &[DataEndpoint::TimeSync],
                reliable: crate::ReliableMode::None,
                priority: 255,
            },
            DataType::TimeSyncRequest => MessageMeta {
                name: "TIME_SYNC_REQUEST",
                element: MessageElement::Static(2, MessageDataType::UInt64, MessageClass::Data),
                endpoints: &[DataEndpoint::TimeSync],
                reliable: crate::ReliableMode::None,
                priority: 255,
            },
            DataType::TimeSyncResponse => MessageMeta {
                name: "TIME_SYNC_RESPONSE",
                element: MessageElement::Static(4, MessageDataType::UInt64, MessageClass::Data),
                endpoints: &[DataEndpoint::TimeSync],
                reliable: crate::ReliableMode::None,
                priority: 255,
            },
        }
    } else {
        quote! {}
    };

    let discovery_ty_meta = if discovery_enabled {
        quote! {
            DataType::DiscoveryAnnounce => MessageMeta {
                name: "DISCOVERY_ANNOUNCE",
                element: MessageElement::Dynamic(MessageDataType::UInt32, MessageClass::Data),
                endpoints: &[DataEndpoint::Discovery],
                reliable: crate::ReliableMode::None,
                priority: 240,
            },
            DataType::DiscoveryTimeSyncSources => MessageMeta {
                name: "DISCOVERY_TIMESYNC_SOURCES",
                element: MessageElement::Dynamic(MessageDataType::UInt8, MessageClass::Data),
                endpoints: &[DataEndpoint::Discovery],
                reliable: crate::ReliableMode::None,
                priority: 240,
            },
        }
    } else {
        quote! {}
    };

    let expanded = quote! {
        // Auto-generated by `define_telemetry_schema!`.
        // Source: telemetry schema JSON.

        pub const MAX_VALUE_DATA_ENDPOINT: u32 = #max_ep_value;
        pub const MAX_VALUE_DATA_TYPE: u32 = #max_ty_value;

        // -----------------------------------------------------------------------------
        // Endpoints
        // -----------------------------------------------------------------------------

        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, EnumCount)]
        #[repr(u32)]
        pub enum DataEndpoint {
            #(#ep_variants)*
        }

        pub const fn get_endpoint_meta(endpoint_type: DataEndpoint) -> EndpointMeta {
            match endpoint_type {
                #(#ep_meta_arms)*
            }
        }

        // -----------------------------------------------------------------------------
        // Data types
        // -----------------------------------------------------------------------------

        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, EnumCount)]
        #[repr(u32)]
        pub enum DataType {
            #(#ty_variants)*
        }

        pub const fn get_message_meta(data_type: DataType) -> MessageMeta {
            match data_type {
                #builtin_ty_meta
                #timesync_ty_meta
                #discovery_ty_meta
                #(#ty_meta_arms)*
            }
        }
    };

    expanded.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn endpoint(rust: &str, name: &str, link_local_only: bool) -> JsonEndpoint {
        JsonEndpoint {
            rust: rust.to_string(),
            name: name.to_string(),
            doc: None,
            deprecated_broadcast_mode: None,
            link_local_only: Some(link_local_only),
        }
    }

    fn datatype(rust: &str, name: &str, endpoints: &[&str]) -> JsonType {
        JsonType {
            rust: rust.to_string(),
            name: name.to_string(),
            doc: None,
            reliable: Some(false),
            reliable_mode: Some("None".to_string()),
            priority: Some(0),
            element: JsonElement::Dynamic {
                data_type: "String".to_string(),
            },
            class: "Data".to_string(),
            endpoints: endpoints.iter().map(|ep| ep.to_string()).collect(),
        }
    }

    fn base_schema() -> TelemetryConfig {
        TelemetryConfig {
            endpoints: vec![
                endpoint("Radio", "RADIO", false),
                endpoint("SdCard", "SD_CARD", false),
            ],
            types: vec![datatype("MessageData", "MESSAGE_DATA", &["Radio", "SdCard"])],
        }
    }

    #[test]
    fn merge_ipc_overlay_appends_valid_entries() {
        let mut base = base_schema();
        let overlay = TelemetryConfig {
            endpoints: vec![endpoint("SoftwareBus", "SOFTWARE_BUS", true)],
            types: vec![datatype("IpcMessage", "IPC_MESSAGE", &["SoftwareBus"])],
        };

        merge_ipc_overlay(&mut base, overlay, Path::new("ipc.json")).unwrap();

        assert_eq!(base.endpoints.len(), 3);
        assert_eq!(base.types.len(), 2);
        assert_eq!(base.endpoints[2].rust, "SoftwareBus");
        assert_eq!(base.types[1].name, "IPC_MESSAGE");
    }

    #[test]
    fn merge_ipc_overlay_coerces_overlay_endpoints_to_link_local() {
        let mut base = base_schema();
        let overlay = TelemetryConfig {
            endpoints: vec![endpoint("SoftwareBus", "SOFTWARE_BUS", false)],
            types: vec![datatype("IpcMessage", "IPC_MESSAGE", &["SoftwareBus"])],
        };

        merge_ipc_overlay(&mut base, overlay, Path::new("ipc.json")).unwrap();
        assert_eq!(base.endpoints[2].link_local_only, Some(true));
    }

    #[test]
    fn normalize_base_schema_forces_non_link_local_endpoints() {
        let mut base = TelemetryConfig {
            endpoints: vec![endpoint("Radio", "RADIO", true)],
            types: vec![datatype("MessageData", "MESSAGE_DATA", &["Radio"])],
        };

        normalize_base_schema(&mut base);
        assert_eq!(base.endpoints[0].link_local_only, Some(false));
    }

    #[test]
    fn normalize_base_schema_upgrades_legacy_broadcast_mode_never() {
        let mut base = TelemetryConfig {
            endpoints: vec![JsonEndpoint {
                rust: "SoftwareBus".to_string(),
                name: "SOFTWARE_BUS".to_string(),
                doc: None,
                deprecated_broadcast_mode: Some("Never".to_string()),
                link_local_only: None,
            }],
            types: vec![datatype("IpcMessage", "IPC_MESSAGE", &["SoftwareBus"])],
        };

        normalize_base_schema(&mut base);
        assert_eq!(base.endpoints[0].link_local_only, Some(true));
    }

    #[test]
    fn merge_ipc_overlay_rejects_endpoint_collisions() {
        let mut base = base_schema();
        let overlay = TelemetryConfig {
            endpoints: vec![endpoint("Radio", "SOFTWARE_BUS", true)],
            types: vec![datatype("IpcMessage", "IPC_MESSAGE", &["Radio"])],
        };

        let err = merge_ipc_overlay(&mut base, overlay, Path::new("ipc.json")).unwrap_err();
        assert!(err.contains("endpoint rust variant"));
        assert!(err.contains("collides"));
    }

    #[test]
    fn merge_ipc_overlay_rejects_type_name_collisions() {
        let mut base = base_schema();
        let overlay = TelemetryConfig {
            endpoints: vec![endpoint("SoftwareBus", "SOFTWARE_BUS", true)],
            types: vec![datatype("IpcMessage", "MESSAGE_DATA", &["SoftwareBus"])],
        };

        let err = merge_ipc_overlay(&mut base, overlay, Path::new("ipc.json")).unwrap_err();
        assert!(err.contains("type name"));
        assert!(err.contains("collides"));
    }
}
