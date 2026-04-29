//! C FFI bindings for the SEDS telemetry router.
//!
//! This module exposes a stable C ABI for:
//! - Router lifecycle (create / free)
//! - Logging (typed, bytes, strings)
//! - RX / TX queue processing
//! - Packet serialization / deserialization
//! - Fixed-size schema queries
//! - Typed payload extraction
//!
//! Router sides are registered explicitly (like the Relay), and RX can specify
//! an ingress side for relay-style behavior.

#[cfg(feature = "timesync")]
use crate::timesync::{NetworkTimeReading, PartialNetworkTime, TimeSyncConfig};
use crate::{
    DataType, MessageElement, RouteSelectionMode, TelemetryError, TelemetryErrorCode,
    TelemetryResult,
    config::{
        DataEndpoint, data_type_definition, data_type_definition_by_name, data_type_exists,
        endpoint_definition, endpoint_definition_by_name, endpoint_exists, message_class_code,
        message_class_from_code, message_data_type_code, message_data_type_from_code,
        register_data_type_id, register_data_type_id_with_description, register_endpoint_id,
        register_endpoint_id_with_description, register_schema_json_bytes, reliable_code,
        reliable_from_code, remove_data_type, remove_data_type_by_name, remove_endpoint,
        remove_endpoint_by_name,
    },
    do_vec_log_typed, get_needed_message_size, message_meta,
    packet::Packet,
    router::{Clock, LeBytes, RouterSideOptions, endpoint_is_router_internal},
    router::{EndpointHandler, Router, RouterConfig},
    serialize::{deserialize_packet, packet_wire_size, peek_envelope, serialize_packet},
};
use crate::{MessageDataType::NoData, get_data_type};
use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
use core::{ffi::c_char, ffi::c_void, mem::size_of, ptr, slice, str::from_utf8};

use crate::relay::{Relay, RelaySideId, RelaySideOptions};
// ============================================================================
//  Constants / basic types shared with the C side
// ============================================================================

/// Element-kind tags for typed logging / extraction.
const SEDS_EK_UNSIGNED: u32 = 0;
const SEDS_EK_SIGNED: u32 = 1;
const SEDS_EK_FLOAT: u32 = 2;

/// Small stack buffer size for endpoint lists in callbacks.
const STACK_EPS: usize = 16; // number of endpoints to store on stack for callback

/// Generic "OK/ERR" status returned for simple FFI entry points.
#[repr(i32)]
enum SedsResult {
    SedsOk = 0,
    SedsErr = 1,
}

/// Opaque owned packet for C. Keeps Rust allocations alive across calls.
#[repr(C)]
pub struct SedsOwnedPacket {
    inner: Packet,
    // cache endpoints.as_u32() so the view can point at stable memory
    endpoints_u32: Vec<u32>,
}

/// Opaque owned header/envelope for C.
/// Owns only header pieces (no payload and no size).
#[repr(C)]
pub struct SedsOwnedHeader {
    ty: u32,
    sender: Arc<str>,        // own the sender so view can borrow it
    endpoints_u32: Vec<u32>, // own endpoints.as_u32() for stable pointers
    timestamp: u64,
}

/// Opaque relay handle exposed to C.
#[repr(C)]
pub struct SedsRelay {
    inner: Arc<Relay>,
}

#[cfg(feature = "timesync")]
#[repr(C)]
pub struct SedsNetworkTime {
    pub has_unix_time_ms: bool,
    pub unix_time_ms: u64,
    pub has_year: bool,
    pub year: i32,
    pub has_month: bool,
    pub month: u8,
    pub has_day: bool,
    pub day: u8,
    pub has_hour: bool,
    pub hour: u8,
    pub has_minute: bool,
    pub minute: u8,
    pub has_second: bool,
    pub second: u8,
    pub has_nanosecond: bool,
    pub nanosecond: u32,
}

// ============================================================================
//  Status / error helpers (shared for all FFI functions)
// ============================================================================

#[inline]
fn status_from_result_code(e: SedsResult) -> i32 {
    match e {
        SedsResult::SedsOk => 0,
        SedsResult::SedsErr => 1,
    }
}

#[inline]
fn status_from_err(e: TelemetryError) -> i32 {
    e.to_error_code() as i32
}

#[inline]
fn ok_or_status(r: TelemetryResult<()>) -> i32 {
    match r {
        Ok(()) => status_from_result_code(SedsResult::SedsOk),
        Err(e) => status_from_err(e),
    }
}

#[inline]
#[allow(clippy::unnecessary_cast)]
fn c_char_ptr_as_u8(ptr: *const c_char) -> *const u8 {
    ptr as *const u8
}

#[inline]
#[allow(clippy::unnecessary_cast)]
fn c_char_mut_ptr_as_u8(ptr: *mut c_char) -> *mut u8 {
    ptr as *mut u8
}

/// Returns the fixed payload size (in bytes) for a static schema, or `None`
/// if the message type is dynamically sized.
#[inline]
fn fixed_payload_size_if_static(ty: DataType) -> Option<usize> {
    match message_meta(ty).element {
        MessageElement::Static(_, _, _) => Some(get_needed_message_size(ty)),
        MessageElement::Dynamic(_, _) => None,
    }
}

/// Convert an optional pointer to an `Option<u64>` timestamp.
#[inline]
fn opt_ts(ts_ptr: *const u64) -> Option<u64> {
    if ts_ptr.is_null() {
        None
    } else {
        Some(unsafe { *ts_ptr })
    }
}

#[cfg(feature = "timesync")]
fn write_network_time(out: *mut SedsNetworkTime, reading: NetworkTimeReading) {
    unsafe {
        (*out).has_unix_time_ms = reading.unix_time_ms.is_some();
        (*out).unix_time_ms = reading.unix_time_ms.unwrap_or(0);
        (*out).has_year = reading.time.year.is_some();
        (*out).year = reading.time.year.unwrap_or(0);
        (*out).has_month = reading.time.month.is_some();
        (*out).month = reading.time.month.unwrap_or(0);
        (*out).has_day = reading.time.day.is_some();
        (*out).day = reading.time.day.unwrap_or(0);
        (*out).has_hour = reading.time.hour.is_some();
        (*out).hour = reading.time.hour.unwrap_or(0);
        (*out).has_minute = reading.time.minute.is_some();
        (*out).minute = reading.time.minute.unwrap_or(0);
        (*out).has_second = reading.time.second.is_some();
        (*out).second = reading.time.second.unwrap_or(0);
        (*out).has_nanosecond = reading.time.nanosecond.is_some();
        (*out).nanosecond = reading.time.nanosecond.unwrap_or(0);
    }
}

/// Convert a C-side `u32` type tag into a Rust `DataType`.
#[inline]
fn dtype_from_u32(x: u32) -> TelemetryResult<DataType> {
    DataType::try_from_u32(x).ok_or(TelemetryError::InvalidType)
}

#[inline]
fn route_selection_mode_from_i32(x: i32) -> TelemetryResult<RouteSelectionMode> {
    match x {
        0 => Ok(RouteSelectionMode::Fanout),
        1 => Ok(RouteSelectionMode::Weighted),
        2 => Ok(RouteSelectionMode::Failover),
        _ => Err(TelemetryError::BadArg),
    }
}

// ============================================================================
//  C-facing opaque types and handler descriptors
// ============================================================================

/// Opaque router handle exposed to C.
#[repr(C)]
pub struct SedsRouter {
    inner: Arc<Router>,
}

/// Must match the C header layout for `SedsPacketView`.
#[repr(C)]
pub struct SedsPacketView {
    ty: u32,
    data_size: usize,
    sender: *const c_char, // pointer
    sender_len: usize,     // length
    endpoints: *const u32,
    num_endpoints: usize,
    timestamp: u64,
    payload: *const u8,
    payload_len: usize,
}

#[repr(C)]
pub struct SedsEndpointInfo {
    exists: bool,
    id: u32,
    link_local_only: bool,
    name: *const c_char,
    name_len: usize,
    description: *const c_char,
    description_len: usize,
}

#[repr(C)]
pub struct SedsDataTypeInfo {
    exists: bool,
    id: u32,
    is_static: bool,
    element_count: usize,
    message_data_type: u8,
    message_class: u8,
    reliable: u8,
    priority: u8,
    fixed_size: usize,
    endpoints: *const u32,
    num_endpoints: usize,
    name: *const c_char,
    name_len: usize,
    description: *const c_char,
    description_len: usize,
}

/// Transmit callback signature used from C (legacy).
type CTransmit = Option<extern "C" fn(bytes: *const u8, len: usize, user: *mut c_void) -> i32>;

/// Endpoint handler callback (packet view) (legacy).
type CEndpointHandler = Option<extern "C" fn(pkt: *const SedsPacketView, user: *mut c_void) -> i32>;

/// Endpoint handler callback (serialized bytes) (legacy).
type CSerializedHandler =
    Option<extern "C" fn(bytes: *const u8, len: usize, user: *mut c_void) -> i32>;

/// C-facing endpoint descriptor (legacy, must match C header).
#[repr(C)]
pub struct SedsLocalEndpointDesc {
    endpoint: u32,                          // DataEndpoint.as_u32()
    packet_handler: CEndpointHandler,       // optional
    serialized_handler: CSerializedHandler, // optional
    user: *mut c_void,
}

// ============================================================================
//  Internal helpers: view_to_packet, string buffer writing, clock adapter
// ============================================================================

/// Convert a C `SedsPacketView` into an owned Rust `Packet`.
/// Returns `Err(())` if type/endpoints/sender are invalid or inconsistent.
#[inline]
fn view_to_packet(view: &SedsPacketView) -> Result<Packet, ()> {
    // Map type
    let ty = DataType::try_from_u32(view.ty).ok_or(())?;

    // Endpoints (u32 → DataEndpoint)
    let eps_u32 = unsafe { slice::from_raw_parts(view.endpoints, view.num_endpoints) };
    let mut eps = Vec::with_capacity(eps_u32.len());
    for &e in eps_u32 {
        let ep = DataEndpoint::try_from_u32(e).ok_or(())?;
        eps.push(ep);
    }

    // Sender as Arc<str>
    let sender_owned: &str = if view.sender.is_null() || view.sender_len == 0 {
        ""
    } else {
        let sb = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(view.sender), view.sender_len) };
        from_utf8(sb).map_err(|_| ())?
    };

    // Payload bytes
    let bytes = unsafe { slice::from_raw_parts(view.payload, view.payload_len) };

    // Optional: keep the C view honest
    if view.data_size != view.payload_len {
        return Err(());
    }

    let payload = Arc::<[u8]>::from(bytes);

    Packet::new(ty, &eps, sender_owned, view.timestamp, payload).map_err(|_| ())
}

/// Write a Rust string into a C buffer, respecting "query mode":
///
/// - If `buf` is NULL or `buf_len == 0`, returns the required size
///   (including the NUL terminator) without writing.
/// - If the buffer is too small, writes as much as fits (NUL-terminated)
///   and returns the required size.
/// - On success, returns `SEDS_OK` (0).
#[inline]
unsafe fn write_str_to_buf(s: &str, buf: *mut c_char, buf_len: usize) -> i32 {
    if buf.is_null() && buf_len != 0 {
        return status_from_err(TelemetryError::BadArg);
    }
    let needed = s.len() + 1; // include NUL

    // Query mode: tell caller required buffer size (including NUL)
    if buf.is_null() || buf_len == 0 {
        return needed as i32;
    }

    let ncopy = core::cmp::min(s.len(), buf_len.saturating_sub(1));
    unsafe {
        ptr::copy_nonoverlapping(s.as_ptr(), c_char_mut_ptr_as_u8(buf), ncopy);
        *buf.add(ncopy) = 0;
    }

    // If too small, return required size (not success)
    if buf_len < needed {
        return needed as i32;
    }

    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "discovery")]
fn json_push_escaped(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = core::fmt::Write::write_fmt(out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(feature = "discovery")]
fn topology_snapshot_to_json(snap: &crate::discovery::TopologySnapshot) -> String {
    fn push_u32_array(out: &mut String, vals: &[u32]) {
        out.push('[');
        for (idx, val) in vals.iter().enumerate() {
            if idx != 0 {
                out.push(',');
            }
            let _ = core::fmt::Write::write_fmt(out, format_args!("{val}"));
        }
        out.push(']');
    }

    fn push_string_array(out: &mut String, vals: &[String]) {
        out.push('[');
        for (idx, val) in vals.iter().enumerate() {
            if idx != 0 {
                out.push(',');
            }
            json_push_escaped(out, val);
        }
        out.push(']');
    }

    fn push_board(out: &mut String, board: &crate::discovery::TopologyBoardNode) {
        out.push('{');
        out.push_str("\"sender_id\":");
        json_push_escaped(out, &board.sender_id);
        out.push_str(",\"reachable_endpoints\":");
        push_u32_array(
            out,
            &board
                .reachable_endpoints
                .iter()
                .map(|ep| ep.as_u32())
                .collect::<Vec<u32>>(),
        );
        out.push_str(",\"reachable_timesync_sources\":");
        push_string_array(out, &board.reachable_timesync_sources);
        out.push_str(",\"connections\":");
        push_string_array(out, &board.connections);
        out.push('}');
    }

    let mut out = String::new();
    out.push('{');
    out.push_str("\"advertised_endpoints\":");
    push_u32_array(
        &mut out,
        &snap
            .advertised_endpoints
            .iter()
            .map(|ep| ep.as_u32())
            .collect::<Vec<u32>>(),
    );
    out.push_str(",\"advertised_timesync_sources\":");
    push_string_array(&mut out, &snap.advertised_timesync_sources);
    out.push_str(",\"routers\":[");
    for (idx, board) in snap.routers.iter().enumerate() {
        if idx != 0 {
            out.push(',');
        }
        push_board(&mut out, board);
    }
    out.push(']');
    out.push_str(",\"routes\":[");
    for (route_idx, route) in snap.routes.iter().enumerate() {
        if route_idx != 0 {
            out.push(',');
        }
        out.push('{');
        let _ =
            core::fmt::Write::write_fmt(&mut out, format_args!("\"side_id\":{},", route.side_id));
        out.push_str("\"side_name\":");
        json_push_escaped(&mut out, route.side_name);
        out.push_str(",\"reachable_endpoints\":");
        push_u32_array(
            &mut out,
            &route
                .reachable_endpoints
                .iter()
                .map(|ep| ep.as_u32())
                .collect::<Vec<u32>>(),
        );
        out.push_str(",\"reachable_timesync_sources\":");
        push_string_array(&mut out, &route.reachable_timesync_sources);
        out.push_str(",\"announcers\":[");
        for (ann_idx, announcer) in route.announcers.iter().enumerate() {
            if ann_idx != 0 {
                out.push(',');
            }
            out.push('{');
            out.push_str("\"sender_id\":");
            json_push_escaped(&mut out, &announcer.sender_id);
            out.push_str(",\"reachable_endpoints\":");
            push_u32_array(
                &mut out,
                &announcer
                    .reachable_endpoints
                    .iter()
                    .map(|ep| ep.as_u32())
                    .collect::<Vec<u32>>(),
            );
            out.push_str(",\"reachable_timesync_sources\":");
            push_string_array(&mut out, &announcer.reachable_timesync_sources);
            out.push_str(",\"routers\":[");
            for (board_idx, board) in announcer.routers.iter().enumerate() {
                if board_idx != 0 {
                    out.push(',');
                }
                push_board(&mut out, board);
            }
            let _ = core::fmt::Write::write_fmt(
                &mut out,
                format_args!(
                    "],\"last_seen_ms\":{},\"age_ms\":{}",
                    announcer.last_seen_ms, announcer.age_ms
                ),
            );
            out.push('}');
        }
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "],\"last_seen_ms\":{},\"age_ms\":{}",
                route.last_seen_ms, route.age_ms
            ),
        );
        out.push('}');
    }
    let _ = core::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "],\"current_announce_interval_ms\":{},\"next_announce_ms\":{}",
            snap.current_announce_interval_ms, snap.next_announce_ms
        ),
    );
    out.push('}');
    out
}

#[cfg(feature = "discovery")]
fn route_selection_mode_name(mode: RouteSelectionMode) -> &'static str {
    match mode {
        RouteSelectionMode::Fanout => "Fanout",
        RouteSelectionMode::Weighted => "Weighted",
        RouteSelectionMode::Failover => "Failover",
    }
}

#[cfg(feature = "discovery")]
fn runtime_stats_snapshot_to_json(snap: &crate::diagnostics::RuntimeStatsSnapshot) -> String {
    fn push_bool(out: &mut String, val: bool) {
        out.push_str(if val { "true" } else { "false" });
    }

    fn push_optional_usize(out: &mut String, val: Option<usize>) {
        match val {
            Some(v) => {
                let _ = core::fmt::Write::write_fmt(out, format_args!("{v}"));
            }
            None => out.push_str("null"),
        }
    }

    fn push_optional_u64(out: &mut String, val: Option<u64>) {
        match val {
            Some(v) => {
                let _ = core::fmt::Write::write_fmt(out, format_args!("{v}"));
            }
            None => out.push_str("null"),
        }
    }

    fn push_optional_route_mode(out: &mut String, val: Option<RouteSelectionMode>) {
        match val {
            Some(mode) => json_push_escaped(out, route_selection_mode_name(mode)),
            None => out.push_str("null"),
        }
    }

    let mut out = String::new();
    out.push('{');

    out.push_str("\"sides\":[");
    for (side_idx, side) in snap.sides.iter().enumerate() {
        if side_idx != 0 {
            out.push(',');
        }
        out.push('{');
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!("\"side_id\":{},\"side_name\":", side.side_id,),
        );
        json_push_escaped(&mut out, side.side_name);
        out.push_str(",\"reliable_enabled\":");
        push_bool(&mut out, side.reliable_enabled);
        out.push_str(",\"link_local_enabled\":");
        push_bool(&mut out, side.link_local_enabled);
        out.push_str(",\"ingress_enabled\":");
        push_bool(&mut out, side.ingress_enabled);
        out.push_str(",\"egress_enabled\":");
        push_bool(&mut out, side.egress_enabled);
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                ",\"tx_packets\":{},\"tx_bytes\":{},\"rx_packets\":{},\"rx_bytes\":{},\
                 \"relayed_tx_packets\":{},\"relayed_tx_bytes\":{},\
                 \"relayed_rx_packets\":{},\"relayed_rx_bytes\":{},\
                 \"local_delivery_packets\":{},\"tx_retries\":{},\
                 \"tx_handler_failures\":{},\"local_handler_failures\":{},\
                 \"total_handler_retries\":{}",
                side.tx_packets,
                side.tx_bytes,
                side.rx_packets,
                side.rx_bytes,
                side.relayed_tx_packets,
                side.relayed_tx_bytes,
                side.relayed_rx_packets,
                side.relayed_rx_bytes,
                side.local_delivery_packets,
                side.tx_retries,
                side.tx_handler_failures,
                side.local_handler_failures,
                side.total_handler_retries
            ),
        );
        out.push_str(",\"adaptive\":{");
        out.push_str("\"auto_balancing_enabled\":");
        push_bool(&mut out, side.adaptive.auto_balancing_enabled);
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                ",\"estimated_capacity_bps\":{},\"peak_capacity_bps\":{},\
                 \"current_usage_bps\":{},\"peak_usage_bps\":{},\
                 \"available_headroom_bps\":{},\"effective_weight\":{},\
                 \"last_observed_ms\":{},\"sample_count\":{}",
                side.adaptive.estimated_capacity_bps,
                side.adaptive.peak_capacity_bps,
                side.adaptive.current_usage_bps,
                side.adaptive.peak_usage_bps,
                side.adaptive.available_headroom_bps,
                side.adaptive.effective_weight,
                side.adaptive.last_observed_ms,
                side.adaptive.sample_count
            ),
        );
        out.push('}');
        out.push_str(",\"data_types\":[");
        for (type_idx, data_type) in side.data_types.iter().enumerate() {
            if type_idx != 0 {
                out.push(',');
            }
            let _ = core::fmt::Write::write_fmt(
                &mut out,
                format_args!(
                    "{{\"data_type\":{},\"tx_packets\":{},\"tx_bytes\":{},\
                      \"rx_packets\":{},\"rx_bytes\":{},\
                      \"relayed_tx_packets\":{},\"relayed_tx_bytes\":{},\
                      \"relayed_rx_packets\":{},\"relayed_rx_bytes\":{},\
                      \"tx_retries\":{},\"handler_failures\":{}}}",
                    data_type.data_type.as_u32(),
                    data_type.tx_packets,
                    data_type.tx_bytes,
                    data_type.rx_packets,
                    data_type.rx_bytes,
                    data_type.relayed_tx_packets,
                    data_type.relayed_tx_bytes,
                    data_type.relayed_rx_packets,
                    data_type.relayed_rx_bytes,
                    data_type.tx_retries,
                    data_type.handler_failures
                ),
            );
        }
        out.push_str("]}");
    }
    out.push(']');

    out.push_str(",\"route_modes\":[");
    for (idx, mode) in snap.route_modes.iter().enumerate() {
        if idx != 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str("\"src_side_id\":");
        push_optional_usize(&mut out, mode.src_side_id);
        out.push_str(",\"selection_mode\":");
        push_optional_route_mode(&mut out, mode.selection_mode);
        let _ = core::fmt::Write::write_fmt(&mut out, format_args!(",\"cursor\":{}", mode.cursor));
        out.push('}');
    }
    out.push(']');

    out.push_str(",\"route_overrides\":[");
    for (idx, route) in snap.route_overrides.iter().enumerate() {
        if idx != 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str("\"src_side_id\":");
        push_optional_usize(&mut out, route.src_side_id);
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!(",\"dst_side_id\":{},\"enabled\":", route.dst_side_id),
        );
        push_bool(&mut out, route.enabled);
        out.push('}');
    }
    out.push(']');

    out.push_str(",\"typed_route_overrides\":[");
    for (idx, route) in snap.typed_route_overrides.iter().enumerate() {
        if idx != 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str("\"src_side_id\":");
        push_optional_usize(&mut out, route.src_side_id);
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                ",\"data_type\":{},\"dst_side_id\":{},\"enabled\":",
                route.data_type.as_u32(),
                route.dst_side_id
            ),
        );
        push_bool(&mut out, route.enabled);
        out.push('}');
    }
    out.push(']');

    out.push_str(",\"route_weights\":[");
    for (idx, weight) in snap.route_weights.iter().enumerate() {
        if idx != 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str("\"src_side_id\":");
        push_optional_usize(&mut out, weight.src_side_id);
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                ",\"dst_side_id\":{},\"weight\":{}",
                weight.dst_side_id, weight.weight
            ),
        );
        out.push('}');
    }
    out.push(']');

    out.push_str(",\"route_priorities\":[");
    for (idx, priority) in snap.route_priorities.iter().enumerate() {
        if idx != 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str("\"src_side_id\":");
        push_optional_usize(&mut out, priority.src_side_id);
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                ",\"dst_side_id\":{},\"priority\":{}",
                priority.dst_side_id, priority.priority
            ),
        );
        out.push('}');
    }
    out.push(']');

    let _ = core::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            ",\"queues\":{{\"rx_len\":{},\"rx_bytes\":{},\"tx_len\":{},\"tx_bytes\":{},\
              \"replay_len\":{},\"replay_bytes\":{},\"recent_rx_len\":{},\"recent_rx_bytes\":{},\
              \"reliable_rx_buffered_len\":{},\"reliable_rx_buffered_bytes\":{},\
              \"shared_queue_bytes_used\":{}}}",
            snap.queues.rx_len,
            snap.queues.rx_bytes,
            snap.queues.tx_len,
            snap.queues.tx_bytes,
            snap.queues.replay_len,
            snap.queues.replay_bytes,
            snap.queues.recent_rx_len,
            snap.queues.recent_rx_bytes,
            snap.queues.reliable_rx_buffered_len,
            snap.queues.reliable_rx_buffered_bytes,
            snap.queues.shared_queue_bytes_used
        ),
    );

    let _ = core::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            ",\"reliable\":{{\"reliable_return_route_count\":{},\
              \"end_to_end_pending_count\":{},\
              \"end_to_end_pending_destination_count\":{},\
              \"end_to_end_acked_cache_count\":{}}}",
            snap.reliable.reliable_return_route_count,
            snap.reliable.end_to_end_pending_count,
            snap.reliable.end_to_end_pending_destination_count,
            snap.reliable.end_to_end_acked_cache_count
        ),
    );

    out.push_str(",\"discovery\":{");
    let _ = core::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "\"route_count\":{},\"announcer_count\":{},\"current_announce_interval_ms\":",
            snap.discovery.route_count, snap.discovery.announcer_count
        ),
    );
    push_optional_u64(&mut out, snap.discovery.current_announce_interval_ms);
    out.push_str(",\"next_announce_ms\":");
    push_optional_u64(&mut out, snap.discovery.next_announce_ms);
    out.push('}');

    let _ = core::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            ",\"total_handler_failures\":{},\"total_handler_retries\":{}",
            snap.total_handler_failures, snap.total_handler_retries
        ),
    );
    out.push('}');
    out
}

/// Validate that a width is one of the allowed sizes.
#[inline]
fn width_is_valid(width: usize) -> bool {
    matches!(width, 0 | 1 | 2 | 4 | 8 | 16)
}

/// FFI-facing clock adapter that calls back into C when present.
type CNowMs = Option<extern "C" fn(user: *mut c_void) -> u64>;

struct FfiClock {
    cb: CNowMs,
    user_addr: usize,
}

impl Clock for FfiClock {
    fn now_ms(&self) -> u64 {
        if let Some(f) = self.cb {
            f(self.user_addr as *mut c_void)
        } else {
            0
        }
    }
}

// ============================================================================
//  FFI: String / error formatting helpers
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_header_string_len(pkt: *const SedsPacketView) -> i32 {
    if pkt.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*pkt };
    let tpkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };
    let s = tpkt.header_string();
    (s.len() + 1) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_to_string_len(pkt: *const SedsPacketView) -> i32 {
    let result = packet_to_string(pkt);
    if let Err(err) = result {
        return err;
    }
    let s = result.unwrap();
    (s.len() + 1) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_error_to_string_len(error_code: i32) -> i32 {
    let s = error_code_to_string(error_code);
    (s.len() + 1) as i32
}

fn packet_to_string(pkt: *const SedsPacketView) -> Result<String, i32> {
    if pkt.is_null() {
        return Err(status_from_err(TelemetryError::BadArg));
    }
    let view = unsafe { &*pkt };
    let tpkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return Err(status_from_err(TelemetryError::BadArg)),
    };
    Ok(tpkt.as_string())
}

fn error_code_to_string(error_code: i32) -> &'static str {
    let result = TelemetryErrorCode::try_from_i32(error_code);

    match result {
        Some(s) => s.as_str(),
        None => {
            if error_code == SedsResult::SedsOk as i32 {
                "SEDS OK"
            } else if error_code == SedsResult::SedsErr as i32 {
                "SEDS ERROR"
            } else {
                "Unknown error"
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_header_string(
    pkt: *const SedsPacketView,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    if pkt.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*pkt };
    let tpkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };
    let s = tpkt.header_string();
    unsafe { write_str_to_buf(&s, buf, buf_len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_to_string(
    pkt: *const SedsPacketView,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    let result = packet_to_string(pkt);
    if let Err(err) = result {
        return err;
    }
    let s = result.unwrap();
    unsafe { write_str_to_buf(&s, buf, buf_len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_error_to_string(error_code: i32, buf: *mut c_char, buf_len: usize) -> i32 {
    let s = error_code_to_string(error_code);
    unsafe { write_str_to_buf(s, buf, buf_len) }
}

// ============================================================================
//  FFI: Router lifecycle (new / free)
// ============================================================================

/// Router constructor (no TX callback; sides are added separately).
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_new(
    mode: u8,
    now_ms_cb: CNowMs,
    user: *mut c_void,
    handlers: *const SedsLocalEndpointDesc,
    n_handlers: usize,
) -> *mut SedsRouter {
    // Build handler vector
    let mut v: Vec<EndpointHandler> = Vec::new();
    if n_handlers > 0 && !handlers.is_null() {
        v.reserve(n_handlers.saturating_mul(2));
        let slice = unsafe { slice::from_raw_parts(handlers, n_handlers) };
        for desc in slice {
            let endpoint = DataEndpoint(desc.endpoint);
            if endpoint_is_router_internal(endpoint) {
                return ptr::null_mut();
            }

            // Common user ctx for either callback kind
            let user_addr = desc.user as usize;

            // If a PACKET handler is provided, register it
            if let Some(cb_fn) = desc.packet_handler {
                let eh = EndpointHandler::new_packet_handler(endpoint, move |pkt: &Packet| {
                    // Fast path: up to STACK_EPS endpoints, no heap allocation
                    let mut stack_eps: [u32; STACK_EPS] = [0; STACK_EPS];

                    let (endpoints_ptr, num_endpoints, _owned_vec): (
                        *const u32,
                        usize,
                        Option<Vec<u32>>,
                    ) = if pkt.endpoints().len() <= STACK_EPS {
                        for (i, e) in pkt.endpoints().iter().enumerate() {
                            stack_eps[i] = e.as_u32();
                        }
                        (stack_eps.as_ptr(), pkt.endpoints().len(), None)
                    } else {
                        let mut eps_u32 = Vec::with_capacity(pkt.endpoints().len());
                        for e in pkt.endpoints().iter() {
                            eps_u32.push(e.as_u32());
                        }
                        let ptr = eps_u32.as_ptr();
                        let len = eps_u32.len();
                        (ptr, len, Some(eps_u32))
                    };

                    let sender_bytes = pkt.sender().as_bytes();
                    let view = SedsPacketView {
                        ty: pkt.data_type().as_u32(),
                        data_size: pkt.data_size(),
                        sender: sender_bytes.as_ptr() as *const c_char,
                        sender_len: sender_bytes.len(),
                        endpoints: endpoints_ptr,
                        num_endpoints,
                        timestamp: pkt.timestamp(),
                        payload: pkt.payload().as_ptr(),
                        payload_len: pkt.payload().len(),
                    };

                    let code = cb_fn(&view as *const _, user_addr as *mut c_void);
                    if code == status_from_result_code(SedsResult::SedsOk) {
                        Ok(())
                    } else {
                        Err(TelemetryError::Io("handler error"))
                    }
                });

                v.push(eh);
            }

            // If a SERIALIZED handler is provided, register it
            if let Some(cb_fn) = desc.serialized_handler {
                let eh =
                    EndpointHandler::new_serialized_handler(endpoint, move |bytes: &[u8]| {
                        let code = cb_fn(bytes.as_ptr(), bytes.len(), user_addr as *mut c_void);
                        if code == status_from_result_code(SedsResult::SedsOk) {
                            Ok(())
                        } else {
                            Err(TelemetryError::Io("handler error"))
                        }
                    });

                v.push(eh);
            }
        }
    }

    let cfg = {
        let cfg = RouterConfig::new(v);
        #[cfg(feature = "timesync")]
        let cfg = cfg.with_timesync(TimeSyncConfig::default());
        cfg
    };
    let _ = mode;

    #[cfg(feature = "std")]
    let router = if now_ms_cb.is_some() {
        Router::new_with_clock(
            cfg,
            Box::new(FfiClock {
                cb: now_ms_cb,
                user_addr: user as usize,
            }),
        )
    } else {
        Router::new(cfg)
    };

    #[cfg(not(feature = "std"))]
    let router = Router::new_with_clock(
        cfg,
        Box::new(FfiClock {
            cb: now_ms_cb,
            user_addr: user as usize,
        }),
    );

    Box::into_raw(Box::new(SedsRouter {
        inner: Arc::from(router),
    }))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_free(r: *mut SedsRouter) {
    if r.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(r));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_sender_id(
    r: *mut SedsRouter,
    sender: *const c_char,
    sender_len: usize,
) -> i32 {
    if r.is_null() || (sender_len > 0 && sender.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let sender_id = if sender_len == 0 {
        ""
    } else {
        let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(sender), sender_len) };
        match from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return status_from_err(TelemetryError::BadArg),
        }
    };
    let router = unsafe { &mut *r };
    router.inner.set_sender(sender_id);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_get_network_time_ms(r: *mut SedsRouter, out_ms: *mut u64) -> i32 {
    if r.is_null() || out_ms.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let Some(ms) = router.network_time_ms() else {
        return status_from_err(TelemetryError::Io("network time unavailable"));
    };
    unsafe { *out_ms = ms };
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_get_network_time(
    r: *mut SedsRouter,
    out: *mut SedsNetworkTime,
) -> i32 {
    if r.is_null() || out.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let Some(reading) = router.network_time() else {
        return status_from_err(TelemetryError::Io("network time unavailable"));
    };
    write_network_time(out, reading);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_configure_timesync(
    r: *mut SedsRouter,
    enabled: bool,
    role: u32,
    priority: u64,
    source_timeout_ms: u64,
    announce_interval_ms: u64,
    request_interval_ms: u64,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    if !enabled {
        router.set_timesync_config(None);
        return status_from_result_code(SedsResult::SedsOk);
    }

    let role = match role {
        0 => crate::timesync::TimeSyncRole::Consumer,
        1 => crate::timesync::TimeSyncRole::Source,
        2 => crate::timesync::TimeSyncRole::Auto,
        _ => return status_from_err(TelemetryError::BadArg),
    };

    router.set_timesync_config(Some(TimeSyncConfig {
        role,
        priority,
        source_timeout_ms,
        announce_interval_ms,
        request_interval_ms,
        consumer_promotion_enabled: true,
        max_slew_ppm: 50_000,
    }));
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_poll_timesync(r: *mut SedsRouter, out_did_queue: *mut bool) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    match router.poll_timesync() {
        Ok(did_queue) => {
            if !out_did_queue.is_null() {
                unsafe { *out_did_queue = did_queue };
            }
            status_from_result_code(SedsResult::SedsOk)
        }
        Err(e) => status_from_err(e),
    }
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_announce_discovery(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.announce_discovery())
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_poll_discovery(r: *mut SedsRouter, out_did_queue: *mut bool) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    match router.poll_discovery() {
        Ok(did_queue) => {
            if !out_did_queue.is_null() {
                unsafe { *out_did_queue = did_queue };
            }
            status_from_result_code(SedsResult::SedsOk)
        }
        Err(e) => status_from_err(e),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_periodic(r: *mut SedsRouter, timeout_ms: u32) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.periodic(timeout_ms))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_periodic_no_timesync(r: *mut SedsRouter, timeout_ms: u32) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.periodic_no_timesync(timeout_ms))
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_local_network_time(
    r: *mut SedsRouter,
    has_year: bool,
    year: i32,
    has_month: bool,
    month: u8,
    has_day: bool,
    day: u8,
    has_hour: bool,
    hour: u8,
    has_minute: bool,
    minute: u8,
    has_second: bool,
    second: u8,
    has_nanosecond: bool,
    nanosecond: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.set_local_network_time(PartialNetworkTime {
        year: has_year.then_some(year),
        month: has_month.then_some(month),
        day: has_day.then_some(day),
        hour: has_hour.then_some(hour),
        minute: has_minute.then_some(minute),
        second: has_second.then_some(second),
        nanosecond: has_nanosecond.then_some(nanosecond),
    });
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_local_network_date(
    r: *mut SedsRouter,
    year: i32,
    month: u8,
    day: u8,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.set_local_network_date(year, month, day);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_local_network_time_hm(
    r: *mut SedsRouter,
    hour: u8,
    minute: u8,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.set_local_network_time_hm(hour, minute);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_local_network_time_hms(
    r: *mut SedsRouter,
    hour: u8,
    minute: u8,
    second: u8,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.set_local_network_time_hms(hour, minute, second);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_local_network_time_hms_millis(
    r: *mut SedsRouter,
    hour: u8,
    minute: u8,
    second: u8,
    millisecond: u16,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.set_local_network_time_hms_millis(hour, minute, second, millisecond);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_local_network_time_hms_nanos(
    r: *mut SedsRouter,
    hour: u8,
    minute: u8,
    second: u8,
    nanosecond: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.set_local_network_time_hms_nanos(hour, minute, second, nanosecond);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_local_network_datetime(
    r: *mut SedsRouter,
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.set_local_network_datetime(year, month, day, hour, minute, second);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_local_network_datetime_millis(
    r: *mut SedsRouter,
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    millisecond: u16,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.set_local_network_datetime_millis(year, month, day, hour, minute, second, millisecond);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "timesync")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_local_network_datetime_nanos(
    r: *mut SedsRouter,
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    nanosecond: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.set_local_network_datetime_nanos(year, month, day, hour, minute, second, nanosecond);
    status_from_result_code(SedsResult::SedsOk)
}

// ============================================================================
//  FFI: Router side registration
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_add_side_serialized(
    r: *mut SedsRouter,
    name: *const c_char,
    name_len: usize,
    tx: CTransmit,
    tx_user: *mut c_void,
    reliable_enabled: bool,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    let side_name: &'static str = if name.is_null() || name_len == 0 {
        ""
    } else {
        let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
        match from_utf8(bytes) {
            Ok(s) => {
                let owned = String::from(s);
                Box::leak(owned.into_boxed_str())
            }
            Err(_) => "",
        }
    };

    let router = unsafe { &(*r).inner };

    let tx_closure = tx.map(|f| {
        let user_addr = tx_user as usize;
        move |bytes: &[u8]| -> TelemetryResult<()> {
            let code = f(bytes.as_ptr(), bytes.len(), user_addr as *mut c_void);
            if code == status_from_result_code(SedsResult::SedsOk) {
                Ok(())
            } else {
                Err(TelemetryError::Io("router side tx error"))
            }
        }
    });

    let Some(tx_fn) = tx_closure else {
        return status_from_err(TelemetryError::BadArg);
    };

    let opts = RouterSideOptions {
        reliable_enabled,
        link_local_enabled: false,
        ..RouterSideOptions::default()
    };

    let side_id = router.add_side_serialized_with_options(side_name, tx_fn, opts);
    side_id as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_add_side_packet(
    r: *mut SedsRouter,
    name: *const c_char,
    name_len: usize,
    tx: CEndpointHandler,
    tx_user: *mut c_void,
    reliable_enabled: bool,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    let side_name: &'static str = if name.is_null() || name_len == 0 {
        ""
    } else {
        let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
        match from_utf8(bytes) {
            Ok(s) => {
                let owned = String::from(s);
                Box::leak(owned.into_boxed_str())
            }
            Err(_) => "",
        }
    };

    let router = unsafe { &(*r).inner };

    let Some(cb_fn) = tx else {
        return status_from_err(TelemetryError::BadArg);
    };

    let user_addr = tx_user as usize;

    let tx_closure = move |pkt: &Packet| -> TelemetryResult<()> {
        let mut stack_eps: [u32; STACK_EPS] = [0; STACK_EPS];
        let (endpoints_ptr, num_endpoints, _owned_vec): (*const u32, usize, Option<Vec<u32>>) =
            if pkt.endpoints().len() <= STACK_EPS {
                for (i, e) in pkt.endpoints().iter().enumerate() {
                    stack_eps[i] = e.as_u32();
                }
                (stack_eps.as_ptr(), pkt.endpoints().len(), None)
            } else {
                let mut eps_u32 = Vec::with_capacity(pkt.endpoints().len());
                for e in pkt.endpoints().iter() {
                    eps_u32.push(e.as_u32());
                }
                let ptr = eps_u32.as_ptr();
                let len = eps_u32.len();
                (ptr, len, Some(eps_u32))
            };

        let sender_bytes = pkt.sender().as_bytes();
        let view = SedsPacketView {
            ty: pkt.data_type().as_u32(),
            data_size: pkt.data_size(),
            sender: sender_bytes.as_ptr() as *const c_char,
            sender_len: sender_bytes.len(),
            endpoints: endpoints_ptr,
            num_endpoints,
            timestamp: pkt.timestamp(),
            payload: pkt.payload().as_ptr(),
            payload_len: pkt.payload().len(),
        };

        let code = cb_fn(&view as *const _, user_addr as *mut c_void);
        if code == status_from_result_code(SedsResult::SedsOk) {
            Ok(())
        } else {
            Err(TelemetryError::Io("router side tx error"))
        }
    };

    let opts = RouterSideOptions {
        reliable_enabled,
        link_local_enabled: false,
        ..RouterSideOptions::default()
    };

    let side_id = router.add_side_packet_with_options(side_name, tx_closure, opts);
    side_id as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_remove_side(r: *mut SedsRouter, side_id: i32) -> i32 {
    if r.is_null() || side_id < 0 {
        return status_from_err(TelemetryError::BadArg);
    }

    let router = unsafe { &(*r).inner };
    ok_or_status(router.remove_side(side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_side_ingress_enabled(
    r: *mut SedsRouter,
    side_id: i32,
    enabled: bool,
) -> i32 {
    if r.is_null() || side_id < 0 {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.set_side_ingress_enabled(side_id as usize, enabled))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_side_egress_enabled(
    r: *mut SedsRouter,
    side_id: i32,
    enabled: bool,
) -> i32 {
    if r.is_null() || side_id < 0 {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.set_side_egress_enabled(side_id as usize, enabled))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_route(
    r: *mut SedsRouter,
    src_side_id: i32,
    dst_side_id: i32,
    enabled: bool,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.set_route(src, dst_side_id as usize, enabled))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_route(
    r: *mut SedsRouter,
    src_side_id: i32,
    dst_side_id: i32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.clear_route(src, dst_side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_typed_route(
    r: *mut SedsRouter,
    src_side_id: i32,
    ty: u32,
    dst_side_id: i32,
    enabled: bool,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty) {
        Ok(ty) => ty,
        Err(err) => return status_from_err(err),
    };
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.set_typed_route(src, ty, dst_side_id as usize, enabled))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_typed_route(
    r: *mut SedsRouter,
    src_side_id: i32,
    ty: u32,
    dst_side_id: i32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty) {
        Ok(ty) => ty,
        Err(err) => return status_from_err(err),
    };
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.clear_typed_route(src, ty, dst_side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_source_route_mode(
    r: *mut SedsRouter,
    src_side_id: i32,
    mode: i32,
) -> i32 {
    if r.is_null() || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let mode = match route_selection_mode_from_i32(mode) {
        Ok(mode) => mode,
        Err(err) => return status_from_err(err),
    };
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.set_source_route_mode(src, mode))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_source_route_mode(r: *mut SedsRouter, src_side_id: i32) -> i32 {
    if r.is_null() || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.clear_source_route_mode(src))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_route_weight(
    r: *mut SedsRouter,
    src_side_id: i32,
    dst_side_id: i32,
    weight: u32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.set_route_weight(src, dst_side_id as usize, weight))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_route_weight(
    r: *mut SedsRouter,
    src_side_id: i32,
    dst_side_id: i32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.clear_route_weight(src, dst_side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_set_route_priority(
    r: *mut SedsRouter,
    src_side_id: i32,
    dst_side_id: i32,
    priority: u32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.set_route_priority(src, dst_side_id as usize, priority))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_route_priority(
    r: *mut SedsRouter,
    src_side_id: i32,
    dst_side_id: i32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(router.clear_route_priority(src, dst_side_id as usize))
}

// ============================================================================
//  FFI: Schema helper (fixed payload size)
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_dtype_expected_size(ty_u32: u32) -> i32 {
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };

    match fixed_payload_size_if_static(ty) {
        Some(sz) => sz as i32,
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_endpoint_exists(endpoint: u32) -> bool {
    endpoint_exists(DataEndpoint(endpoint))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_dtype_exists(ty: u32) -> bool {
    data_type_exists(DataType(ty))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_endpoint_register(
    endpoint: u32,
    name: *const c_char,
    name_len: usize,
    link_local_only: bool,
) -> i32 {
    if name.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
    let Ok(name) = from_utf8(bytes) else {
        return status_from_err(TelemetryError::Deserialize("endpoint name"));
    };
    ok_or_status(register_endpoint_id(DataEndpoint(endpoint), name, link_local_only).map(|_| ()))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_endpoint_register_ex(
    endpoint: u32,
    name: *const c_char,
    name_len: usize,
    description: *const c_char,
    description_len: usize,
    link_local_only: bool,
) -> i32 {
    if name.is_null() || (description.is_null() && description_len != 0) {
        return status_from_err(TelemetryError::BadArg);
    }
    let name_bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
    let Ok(name) = from_utf8(name_bytes) else {
        return status_from_err(TelemetryError::Deserialize("endpoint name"));
    };
    let description = if description_len == 0 {
        ""
    } else {
        let bytes =
            unsafe { slice::from_raw_parts(c_char_ptr_as_u8(description), description_len) };
        let Ok(description) = from_utf8(bytes) else {
            return status_from_err(TelemetryError::Deserialize("endpoint description"));
        };
        description
    };
    ok_or_status(
        register_endpoint_id_with_description(
            DataEndpoint(endpoint),
            name,
            description,
            link_local_only,
        )
        .map(|_| ()),
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_dtype_register(
    ty: u32,
    name: *const c_char,
    name_len: usize,
    is_static: bool,
    element_count: usize,
    message_data_type: u8,
    message_class: u8,
    reliable: u8,
    priority: u8,
    endpoints: *const u32,
    num_endpoints: usize,
) -> i32 {
    if name.is_null() || (num_endpoints > 0 && endpoints.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
    let Ok(name) = from_utf8(bytes) else {
        return status_from_err(TelemetryError::Deserialize("data type name"));
    };
    let Some(data_type) = message_data_type_from_code(message_data_type) else {
        return status_from_err(TelemetryError::BadArg);
    };
    let Some(class) = message_class_from_code(message_class) else {
        return status_from_err(TelemetryError::BadArg);
    };
    let Some(reliable) = reliable_from_code(reliable) else {
        return status_from_err(TelemetryError::BadArg);
    };
    let element = if is_static {
        MessageElement::Static(element_count, data_type, class)
    } else {
        MessageElement::Dynamic(data_type, class)
    };
    let endpoint_ids = if num_endpoints == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(endpoints, num_endpoints) }
    };
    let eps: Vec<DataEndpoint> = endpoint_ids.iter().copied().map(DataEndpoint).collect();
    ok_or_status(
        register_data_type_id(DataType(ty), name, element, &eps, reliable, priority).map(|_| ()),
    )
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn seds_dtype_register_ex(
    ty: u32,
    name: *const c_char,
    name_len: usize,
    description: *const c_char,
    description_len: usize,
    is_static: bool,
    element_count: usize,
    message_data_type: u8,
    message_class: u8,
    reliable: u8,
    priority: u8,
    endpoints: *const u32,
    num_endpoints: usize,
) -> i32 {
    if name.is_null()
        || (description.is_null() && description_len != 0)
        || (num_endpoints > 0 && endpoints.is_null())
    {
        return status_from_err(TelemetryError::BadArg);
    }
    let name_bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
    let Ok(name) = from_utf8(name_bytes) else {
        return status_from_err(TelemetryError::Deserialize("data type name"));
    };
    let description = if description_len == 0 {
        ""
    } else {
        let bytes =
            unsafe { slice::from_raw_parts(c_char_ptr_as_u8(description), description_len) };
        let Ok(description) = from_utf8(bytes) else {
            return status_from_err(TelemetryError::Deserialize("data type description"));
        };
        description
    };
    let Some(data_type) = message_data_type_from_code(message_data_type) else {
        return status_from_err(TelemetryError::BadArg);
    };
    let Some(class) = message_class_from_code(message_class) else {
        return status_from_err(TelemetryError::BadArg);
    };
    let Some(reliable) = reliable_from_code(reliable) else {
        return status_from_err(TelemetryError::BadArg);
    };
    let element = if is_static {
        MessageElement::Static(element_count, data_type, class)
    } else {
        MessageElement::Dynamic(data_type, class)
    };
    let endpoint_ids = if num_endpoints == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(endpoints, num_endpoints) }
    };
    let eps: Vec<DataEndpoint> = endpoint_ids.iter().copied().map(DataEndpoint).collect();
    ok_or_status(
        register_data_type_id_with_description(
            DataType(ty),
            name,
            description,
            element,
            &eps,
            reliable,
            priority,
        )
        .map(|_| ()),
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_schema_register_json_bytes(json: *const u8, json_len: usize) -> i32 {
    if json.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let bytes = unsafe { slice::from_raw_parts(json, json_len) };
    ok_or_status(register_schema_json_bytes(bytes))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_schema_register_json_file(path: *const c_char, path_len: usize) -> i32 {
    if path.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(path), path_len) };
    let Ok(path) = from_utf8(bytes) else {
        return status_from_err(TelemetryError::Deserialize("schema json path"));
    };
    #[cfg(feature = "std")]
    {
        ok_or_status(crate::config::register_schema_json_path(path))
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = path;
        status_from_err(TelemetryError::BadArg)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_endpoint_get_info(endpoint: u32, out: *mut SedsEndpointInfo) -> i32 {
    if out.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let Some(def) = endpoint_definition(DataEndpoint(endpoint)) else {
        unsafe {
            *out = SedsEndpointInfo {
                exists: false,
                id: endpoint,
                link_local_only: false,
                name: ptr::null(),
                name_len: 0,
                description: ptr::null(),
                description_len: 0,
            };
        }
        return status_from_result_code(SedsResult::SedsOk);
    };
    unsafe {
        *out = SedsEndpointInfo {
            exists: true,
            id: def.id.as_u32(),
            link_local_only: def.link_local_only,
            name: def.name.as_ptr() as *const c_char,
            name_len: def.name.len(),
            description: def.description.as_ptr() as *const c_char,
            description_len: def.description.len(),
        };
    }
    status_from_result_code(SedsResult::SedsOk)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_endpoint_get_info_by_name(
    name: *const c_char,
    name_len: usize,
    out: *mut SedsEndpointInfo,
) -> i32 {
    if name.is_null() || out.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
    let Ok(name) = from_utf8(bytes) else {
        return status_from_err(TelemetryError::Deserialize("endpoint name"));
    };
    let Some(def) = endpoint_definition_by_name(name) else {
        unsafe {
            *out = SedsEndpointInfo {
                exists: false,
                id: 0,
                link_local_only: false,
                name: ptr::null(),
                name_len: 0,
                description: ptr::null(),
                description_len: 0,
            };
        }
        return status_from_result_code(SedsResult::SedsOk);
    };
    unsafe {
        *out = SedsEndpointInfo {
            exists: true,
            id: def.id.as_u32(),
            link_local_only: def.link_local_only,
            name: def.name.as_ptr() as *const c_char,
            name_len: def.name.len(),
            description: def.description.as_ptr() as *const c_char,
            description_len: def.description.len(),
        };
    }
    status_from_result_code(SedsResult::SedsOk)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_dtype_get_info(
    ty: u32,
    endpoints_out: *mut u32,
    endpoints_cap: usize,
    out: *mut SedsDataTypeInfo,
) -> i32 {
    if out.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let Some(def) = data_type_definition(DataType(ty)) else {
        unsafe {
            *out = SedsDataTypeInfo {
                exists: false,
                id: ty,
                is_static: false,
                element_count: 0,
                message_data_type: 0,
                message_class: 0,
                reliable: 0,
                priority: 0,
                fixed_size: 0,
                endpoints: ptr::null(),
                num_endpoints: 0,
                name: ptr::null(),
                name_len: 0,
                description: ptr::null(),
                description_len: 0,
            };
        }
        return status_from_result_code(SedsResult::SedsOk);
    };
    if def.endpoints.len() > endpoints_cap || (!def.endpoints.is_empty() && endpoints_out.is_null())
    {
        return status_from_err(TelemetryError::BadArg);
    }
    for (idx, ep) in def.endpoints.iter().enumerate() {
        unsafe {
            *endpoints_out.add(idx) = ep.as_u32();
        }
    }
    let (is_static, count, data_type, class) = match def.element {
        MessageElement::Static(count, data_type, class) => (true, count, data_type, class),
        MessageElement::Dynamic(data_type, class) => (false, 0, data_type, class),
    };
    unsafe {
        *out = SedsDataTypeInfo {
            exists: true,
            id: def.id.as_u32(),
            is_static,
            element_count: count,
            message_data_type: message_data_type_code(data_type),
            message_class: message_class_code(class),
            reliable: reliable_code(def.reliable),
            priority: def.priority,
            fixed_size: fixed_payload_size_if_static(def.id).unwrap_or(0),
            endpoints: endpoints_out,
            num_endpoints: def.endpoints.len(),
            name: def.name.as_ptr() as *const c_char,
            name_len: def.name.len(),
            description: def.description.as_ptr() as *const c_char,
            description_len: def.description.len(),
        };
    }
    status_from_result_code(SedsResult::SedsOk)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_dtype_get_info_by_name(
    name: *const c_char,
    name_len: usize,
    endpoints_out: *mut u32,
    endpoints_cap: usize,
    out: *mut SedsDataTypeInfo,
) -> i32 {
    if name.is_null() || out.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
    let Ok(name) = from_utf8(bytes) else {
        return status_from_err(TelemetryError::Deserialize("data type name"));
    };
    let Some(def) = data_type_definition_by_name(name) else {
        unsafe {
            *out = SedsDataTypeInfo {
                exists: false,
                id: 0,
                is_static: false,
                element_count: 0,
                message_data_type: 0,
                message_class: 0,
                reliable: 0,
                priority: 0,
                fixed_size: 0,
                endpoints: ptr::null(),
                num_endpoints: 0,
                name: ptr::null(),
                name_len: 0,
                description: ptr::null(),
                description_len: 0,
            };
        }
        return status_from_result_code(SedsResult::SedsOk);
    };
    if def.endpoints.len() > endpoints_cap || (!def.endpoints.is_empty() && endpoints_out.is_null())
    {
        return status_from_err(TelemetryError::BadArg);
    }
    for (idx, ep) in def.endpoints.iter().enumerate() {
        unsafe {
            *endpoints_out.add(idx) = ep.as_u32();
        }
    }
    let (is_static, count, data_type, class) = match def.element {
        MessageElement::Static(count, data_type, class) => (true, count, data_type, class),
        MessageElement::Dynamic(data_type, class) => (false, 0, data_type, class),
    };
    unsafe {
        *out = SedsDataTypeInfo {
            exists: true,
            id: def.id.as_u32(),
            is_static,
            element_count: count,
            message_data_type: message_data_type_code(data_type),
            message_class: message_class_code(class),
            reliable: reliable_code(def.reliable),
            priority: def.priority,
            fixed_size: fixed_payload_size_if_static(def.id).unwrap_or(0),
            endpoints: endpoints_out,
            num_endpoints: def.endpoints.len(),
            name: def.name.as_ptr() as *const c_char,
            name_len: def.name.len(),
            description: def.description.as_ptr() as *const c_char,
            description_len: def.description.len(),
        };
    }
    status_from_result_code(SedsResult::SedsOk)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_endpoint_remove(endpoint: u32) -> i32 {
    ok_or_status(remove_endpoint(DataEndpoint(endpoint)).map(|_| ()))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_endpoint_remove_by_name(name: *const c_char, name_len: usize) -> i32 {
    if name.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
    let Ok(name) = from_utf8(bytes) else {
        return status_from_err(TelemetryError::Deserialize("endpoint name"));
    };
    ok_or_status(remove_endpoint_by_name(name).map(|_| ()))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_dtype_remove(ty: u32) -> i32 {
    ok_or_status(remove_data_type(DataType(ty)).map(|_| ()))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_dtype_remove_by_name(name: *const c_char, name_len: usize) -> i32 {
    if name.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
    let Ok(name) = from_utf8(bytes) else {
        return status_from_err(TelemetryError::Deserialize("data type name"));
    };
    ok_or_status(remove_data_type_by_name(name).map(|_| ()))
}

// ============================================================================
//  FFI: Relay lifecycle (new / free)
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_new(now_ms_cb: CNowMs, user: *mut c_void) -> *mut SedsRelay {
    let clock = FfiClock {
        cb: now_ms_cb,
        user_addr: user as usize,
    };

    let relay = Relay::new(Box::new(clock));
    Box::into_raw(Box::new(SedsRelay {
        inner: Arc::new(relay),
    }))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_free(r: *mut SedsRelay) {
    if r.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(r));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_set_sender_id(
    r: *mut SedsRelay,
    sender: *const c_char,
    sender_len: usize,
) -> i32 {
    if r.is_null() || (sender_len > 0 && sender.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let sender_id = if sender_len == 0 {
        ""
    } else {
        let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(sender), sender_len) };
        match from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return status_from_err(TelemetryError::BadArg),
        }
    };
    let relay = unsafe { &mut *r };
    relay.inner.set_sender(sender_id);
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_announce_discovery(r: *mut SedsRelay) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.announce_discovery())
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_poll_discovery(r: *mut SedsRelay, out_did_queue: *mut bool) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    match relay.poll_discovery() {
        Ok(did_queue) => {
            if !out_did_queue.is_null() {
                unsafe { *out_did_queue = did_queue };
            }
            status_from_result_code(SedsResult::SedsOk)
        }
        Err(e) => status_from_err(e),
    }
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_export_topology_len(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let json = topology_snapshot_to_json(&router.export_topology());
    (json.len() + 1) as i32
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_export_topology(
    r: *mut SedsRouter,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let json = topology_snapshot_to_json(&router.export_topology());
    unsafe { write_str_to_buf(&json, buf, buf_len) }
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_export_runtime_stats_len(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let json = runtime_stats_snapshot_to_json(&router.export_runtime_stats());
    (json.len() + 1) as i32
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_router_export_runtime_stats(
    r: *mut SedsRouter,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let json = runtime_stats_snapshot_to_json(&router.export_runtime_stats());
    unsafe { write_str_to_buf(&json, buf, buf_len) }
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_export_topology_len(r: *mut SedsRelay) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let json = topology_snapshot_to_json(&relay.export_topology());
    (json.len() + 1) as i32
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_export_topology(
    r: *mut SedsRelay,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let json = topology_snapshot_to_json(&relay.export_topology());
    unsafe { write_str_to_buf(&json, buf, buf_len) }
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_export_runtime_stats_len(r: *mut SedsRelay) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let json = runtime_stats_snapshot_to_json(&relay.export_runtime_stats());
    (json.len() + 1) as i32
}

#[cfg(feature = "discovery")]
#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_export_runtime_stats(
    r: *mut SedsRelay,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let json = runtime_stats_snapshot_to_json(&relay.export_runtime_stats());
    unsafe { write_str_to_buf(&json, buf, buf_len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_periodic(r: *mut SedsRelay, timeout_ms: u32) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.periodic(timeout_ms))
}

// ============================================================================
//  FFI: Relay side registration
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_add_side_serialized(
    r: *mut SedsRelay,
    name: *const c_char,
    name_len: usize,
    tx: CTransmit,
    tx_user: *mut c_void,
    reliable_enabled: bool,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    let side_name: &'static str = if name.is_null() || name_len == 0 {
        ""
    } else {
        let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
        match from_utf8(bytes) {
            Ok(s) => {
                let owned = String::from(s);
                Box::leak(owned.into_boxed_str())
            }
            Err(_) => "",
        }
    };

    let relay = unsafe { &(*r).inner };

    let tx_closure = tx.map(|f| {
        let user_addr = tx_user as usize;
        move |bytes: &[u8]| -> TelemetryResult<()> {
            let code = f(bytes.as_ptr(), bytes.len(), user_addr as *mut c_void);
            if code == status_from_result_code(SedsResult::SedsOk) {
                Ok(())
            } else {
                Err(TelemetryError::Io("relay tx error"))
            }
        }
    });

    let Some(tx_fn) = tx_closure else {
        return status_from_err(TelemetryError::BadArg);
    };

    let opts = RelaySideOptions {
        reliable_enabled,
        link_local_enabled: false,
        ..RelaySideOptions::default()
    };
    let side_id: RelaySideId = relay.add_side_serialized_with_options(side_name, tx_fn, opts);
    side_id as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_add_side_packet(
    r: *mut SedsRelay,
    name: *const c_char,
    name_len: usize,
    tx: CEndpointHandler,
    tx_user: *mut c_void,
    reliable_enabled: bool,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    let side_name: &'static str = if name.is_null() || name_len == 0 {
        ""
    } else {
        let bytes = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(name), name_len) };
        match from_utf8(bytes) {
            Ok(s) => {
                let owned = String::from(s);
                Box::leak(owned.into_boxed_str())
            }
            Err(_) => "",
        }
    };

    let relay = unsafe { &(*r).inner };

    let Some(cb_fn) = tx else {
        return status_from_err(TelemetryError::BadArg);
    };

    let user_addr = tx_user as usize;

    let tx_closure = move |pkt: &Packet| -> TelemetryResult<()> {
        let mut stack_eps: [u32; STACK_EPS] = [0; STACK_EPS];
        let (endpoints_ptr, num_endpoints, _owned_vec): (*const u32, usize, Option<Vec<u32>>) =
            if pkt.endpoints().len() <= STACK_EPS {
                for (i, e) in pkt.endpoints().iter().enumerate() {
                    stack_eps[i] = e.as_u32();
                }
                (stack_eps.as_ptr(), pkt.endpoints().len(), None)
            } else {
                let mut eps_u32 = Vec::with_capacity(pkt.endpoints().len());
                for e in pkt.endpoints().iter() {
                    eps_u32.push(e.as_u32());
                }
                let ptr = eps_u32.as_ptr();
                let len = eps_u32.len();
                (ptr, len, Some(eps_u32))
            };

        let sender_bytes = pkt.sender().as_bytes();
        let view = SedsPacketView {
            ty: pkt.data_type().as_u32(),
            data_size: pkt.data_size(),
            sender: sender_bytes.as_ptr() as *const c_char,
            sender_len: sender_bytes.len(),
            endpoints: endpoints_ptr,
            num_endpoints,
            timestamp: pkt.timestamp(),
            payload: pkt.payload().as_ptr(),
            payload_len: pkt.payload().len(),
        };

        let code = cb_fn(&view as *const _, user_addr as *mut c_void);
        if code == status_from_result_code(SedsResult::SedsOk) {
            Ok(())
        } else {
            Err(TelemetryError::Io("relay packet tx error"))
        }
    };

    let opts = RelaySideOptions {
        reliable_enabled,
        link_local_enabled: false,
        ..RelaySideOptions::default()
    };
    let side_id: RelaySideId = relay.add_side_packet_with_options(side_name, tx_closure, opts);
    side_id as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_remove_side(r: *mut SedsRelay, side_id: i32) -> i32 {
    if r.is_null() || side_id < 0 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.remove_side(side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_set_side_ingress_enabled(
    r: *mut SedsRelay,
    side_id: i32,
    enabled: bool,
) -> i32 {
    if r.is_null() || side_id < 0 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.set_side_ingress_enabled(side_id as usize, enabled))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_set_side_egress_enabled(
    r: *mut SedsRelay,
    side_id: i32,
    enabled: bool,
) -> i32 {
    if r.is_null() || side_id < 0 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.set_side_egress_enabled(side_id as usize, enabled))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_set_route(
    r: *mut SedsRelay,
    src_side_id: i32,
    dst_side_id: i32,
    enabled: bool,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.set_route(src, dst_side_id as usize, enabled))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_clear_route(
    r: *mut SedsRelay,
    src_side_id: i32,
    dst_side_id: i32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.clear_route(src, dst_side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_set_typed_route(
    r: *mut SedsRelay,
    src_side_id: i32,
    ty: u32,
    dst_side_id: i32,
    enabled: bool,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty) {
        Ok(ty) => ty,
        Err(err) => return status_from_err(err),
    };
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.set_typed_route(src, ty, dst_side_id as usize, enabled))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_clear_typed_route(
    r: *mut SedsRelay,
    src_side_id: i32,
    ty: u32,
    dst_side_id: i32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty) {
        Ok(ty) => ty,
        Err(err) => return status_from_err(err),
    };
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.clear_typed_route(src, ty, dst_side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_set_source_route_mode(
    r: *mut SedsRelay,
    src_side_id: i32,
    mode: i32,
) -> i32 {
    if r.is_null() || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let mode = match route_selection_mode_from_i32(mode) {
        Ok(mode) => mode,
        Err(err) => return status_from_err(err),
    };
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.set_source_route_mode(src, mode))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_clear_source_route_mode(r: *mut SedsRelay, src_side_id: i32) -> i32 {
    if r.is_null() || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.clear_source_route_mode(src))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_set_route_weight(
    r: *mut SedsRelay,
    src_side_id: i32,
    dst_side_id: i32,
    weight: u32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.set_route_weight(src, dst_side_id as usize, weight))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_clear_route_weight(
    r: *mut SedsRelay,
    src_side_id: i32,
    dst_side_id: i32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.clear_route_weight(src, dst_side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_set_route_priority(
    r: *mut SedsRelay,
    src_side_id: i32,
    dst_side_id: i32,
    priority: u32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.set_route_priority(src, dst_side_id as usize, priority))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_clear_route_priority(
    r: *mut SedsRelay,
    src_side_id: i32,
    dst_side_id: i32,
) -> i32 {
    if r.is_null() || dst_side_id < 0 || src_side_id < -1 {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    let src = if src_side_id < 0 {
        None
    } else {
        Some(src_side_id as usize)
    };
    ok_or_status(relay.clear_route_priority(src, dst_side_id as usize))
}

// ============================================================================
//  FFI: Relay RX / TX queueing
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_rx_serialized_from_side(
    r: *mut SedsRelay,
    side_id: u32,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if r.is_null() || (len > 0 && bytes.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }

    let relay = unsafe { &(*r).inner };
    let slice = unsafe { slice::from_raw_parts(bytes, len) };

    ok_or_status(relay.rx_serialized_from_side(side_id as usize, slice))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_rx_packet_from_side(
    r: *mut SedsRelay,
    side_id: u32,
    view: *const SedsPacketView,
) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    let relay = unsafe { &(*r).inner };

    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };

    ok_or_status(relay.rx_from_side(side_id as usize, pkt))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_process_rx_queue(r: *mut SedsRelay) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.process_rx_queue())
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_process_tx_queue(r: *mut SedsRelay) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.process_tx_queue())
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_process_all_queues(r: *mut SedsRelay) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.process_all_queues())
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_clear_queues(r: *mut SedsRelay) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    relay.clear_queues();
    status_from_result_code(SedsResult::SedsOk)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_process_rx_queue_with_timeout(
    r: *mut SedsRelay,
    timeout_ms: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.process_rx_queue_with_timeout(timeout_ms))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_process_tx_queue_with_timeout(
    r: *mut SedsRelay,
    timeout_ms: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.process_tx_queue_with_timeout(timeout_ms))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_relay_process_all_queues_with_timeout(
    r: *mut SedsRelay,
    timeout_ms: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let relay = unsafe { &(*r).inner };
    ok_or_status(relay.process_all_queues_with_timeout(timeout_ms))
}

// ============================================================================
//  Internal logging helper: dispatch queue vs immediate, with optional ts
// ============================================================================

fn call_log_or_queue<T: LeBytes>(
    router: *mut SedsRouter,
    ty: DataType,
    ts: Option<u64>,
    data: &[T],
    queue: bool,
) -> TelemetryResult<()> {
    unsafe {
        let r = &(*router).inner;
        if queue {
            match ts {
                Some(t) => r.log_queue_ts::<T>(ty, t, data),
                None => r.log_queue::<T>(ty, data),
            }
        } else {
            match ts {
                Some(t) => r.log_ts::<T>(ty, t, data),
                None => r.log::<T>(ty, data),
            }
        }
    }
}

fn finish_with<T: LeBytes + Copy>(
    r: *mut SedsRouter,
    ty: DataType,
    ts: Option<u64>,
    queue: bool,
    padded: &[u8],
    required_elems: usize,
    elem_size: usize,
) -> i32 {
    if get_data_type(ty) == NoData {
        return ok_or_status(unsafe {
            let router = &(*r).inner;
            if queue {
                match ts {
                    Some(t) => router.log_queue_ts::<T>(ty, t, &[]),
                    None => router.log_queue::<T>(ty, &[]),
                }
            } else {
                match ts {
                    Some(t) => router.log_ts::<T>(ty, t, &[]),
                    None => router.log::<T>(ty, &[]),
                }
            }
        });
    }

    let mut tmp: Vec<T> = Vec::with_capacity(required_elems);
    if vectorize_data::<T>(padded.as_ptr(), required_elems, elem_size, &mut tmp).is_err() {
        return status_from_err(TelemetryError::Io("vectorize_data failed"));
    }

    ok_or_status(unsafe {
        let router = &(*r).inner;
        if queue {
            match ts {
                Some(t) => router.log_queue_ts::<T>(ty, t, &tmp),
                None => router.log_queue::<T>(ty, &tmp),
            }
        } else {
            match ts {
                Some(t) => router.log_ts::<T>(ty, t, &tmp),
                None => router.log::<T>(ty, &tmp),
            }
        }
    })
}

// ============================================================================
//  FFI: Unified logging entry points (typed / bytes / string)
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_bytes_ex(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const u8,
    len: usize,
    timestamp_ms_opt: *const u64,
    queue: bool,
) -> i32 {
    if r.is_null() || (len > 0 && data.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };

    let src = unsafe { slice::from_raw_parts(data, len) };
    let ts = opt_ts(timestamp_ms_opt);

    if let Some(required) = fixed_payload_size_if_static(ty) {
        if src.len() == required {
            return ok_or_status(call_log_or_queue::<u8>(r, ty, ts, src, queue));
        }

        let mut tmp = vec![0u8; required];
        let ncopy = core::cmp::min(src.len(), required);
        if ncopy > 0 {
            tmp[..ncopy].copy_from_slice(&src[..ncopy]);
        }

        return ok_or_status(call_log_or_queue::<u8>(r, ty, ts, &tmp, queue));
    }

    ok_or_status(call_log_or_queue::<u8>(r, ty, ts, src, queue))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_f32_ex(
    r: *mut SedsRouter,
    ty_u32: u32,
    vals: *const f32,
    n_vals: usize,
    timestamp_ms_opt: *const u64,
    queue: bool,
) -> i32 {
    if r.is_null() || (n_vals > 0 && vals.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    let slice = unsafe { slice::from_raw_parts(vals, n_vals) };
    ok_or_status(call_log_or_queue::<f32>(
        r,
        ty,
        opt_ts(timestamp_ms_opt),
        slice,
        queue,
    ))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_string_ex(
    r: *mut SedsRouter,
    ty_u32: u32,
    bytes: *const c_char,
    len: usize,
    timestamp_ms_opt: *const u64,
    queue: bool,
) -> i32 {
    if r.is_null() || (len > 0 && bytes.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };

    let src = unsafe { slice::from_raw_parts(c_char_ptr_as_u8(bytes), len) };
    let ts = opt_ts(timestamp_ms_opt);

    if let Some(required) = fixed_payload_size_if_static(ty) {
        if src.len() == required {
            return ok_or_status(call_log_or_queue::<u8>(r, ty, ts, src, queue));
        }
        let mut tmp = vec![0u8; required];
        let ncopy = core::cmp::min(src.len(), required);
        if ncopy > 0 {
            tmp[..ncopy].copy_from_slice(&src[..ncopy]);
        }
        return ok_or_status(call_log_or_queue::<u8>(r, ty, ts, &tmp, queue));
    }
    ok_or_status(call_log_or_queue::<u8>(r, ty, ts, src, queue))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_typed_ex(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const c_void,
    count: usize,
    elem_size: usize,
    elem_kind: u32,
    timestamp_ms_opt: *const u64,
    queue: bool,
) -> i32 {
    if r.is_null() || (count > 0 && data.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    if !width_is_valid(elem_size) {
        return status_from_err(TelemetryError::BadArg);
    }
    let ty = match dtype_from_u32(ty_u32) {
        Ok(t) => t,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    let ts = opt_ts(timestamp_ms_opt);

    if let Some(required_bytes) = fixed_payload_size_if_static(ty) {
        if required_bytes % elem_size != 0 {
            return status_from_err(TelemetryError::BadArg);
        }

        let src_bytes_len = count.saturating_mul(elem_size);
        let src = unsafe { slice::from_raw_parts(data as *const u8, src_bytes_len) };

        let mut padded = vec![0u8; required_bytes];
        let ncopy = core::cmp::min(src.len(), required_bytes);
        if ncopy > 0 {
            padded[..ncopy].copy_from_slice(&src[..ncopy]);
        }

        let required_elems = required_bytes / elem_size;

        return match (elem_kind, elem_size) {
            (SEDS_EK_UNSIGNED, 1) => {
                finish_with::<u8>(r, ty, ts, queue, &padded, required_elems, 1)
            }
            (SEDS_EK_UNSIGNED, 2) => {
                finish_with::<u16>(r, ty, ts, queue, &padded, required_elems, 2)
            }
            (SEDS_EK_UNSIGNED, 4) => {
                finish_with::<u32>(r, ty, ts, queue, &padded, required_elems, 4)
            }
            (SEDS_EK_UNSIGNED, 8) => {
                finish_with::<u64>(r, ty, ts, queue, &padded, required_elems, 8)
            }
            (SEDS_EK_UNSIGNED, 16) => {
                finish_with::<u128>(r, ty, ts, queue, &padded, required_elems, 16)
            }

            (SEDS_EK_SIGNED, 1) => finish_with::<i8>(r, ty, ts, queue, &padded, required_elems, 1),
            (SEDS_EK_SIGNED, 2) => finish_with::<i16>(r, ty, ts, queue, &padded, required_elems, 2),
            (SEDS_EK_SIGNED, 4) => finish_with::<i32>(r, ty, ts, queue, &padded, required_elems, 4),
            (SEDS_EK_SIGNED, 8) => finish_with::<i64>(r, ty, ts, queue, &padded, required_elems, 8),
            (SEDS_EK_SIGNED, 16) => {
                finish_with::<i128>(r, ty, ts, queue, &padded, required_elems, 16)
            }

            (SEDS_EK_FLOAT, 4) => finish_with::<f32>(r, ty, ts, queue, &padded, required_elems, 4),
            (SEDS_EK_FLOAT, 8) => finish_with::<f64>(r, ty, ts, queue, &padded, required_elems, 8),

            _ => status_from_err(TelemetryError::BadArg),
        };
    }

    match (elem_kind, elem_size) {
        (SEDS_EK_UNSIGNED, 1) => do_vec_log_typed!(r, ty, ts, queue, data, count, u8),
        (SEDS_EK_UNSIGNED, 2) => do_vec_log_typed!(r, ty, ts, queue, data, count, u16),
        (SEDS_EK_UNSIGNED, 4) => do_vec_log_typed!(r, ty, ts, queue, data, count, u32),
        (SEDS_EK_UNSIGNED, 8) => do_vec_log_typed!(r, ty, ts, queue, data, count, u64),
        (SEDS_EK_UNSIGNED, 16) => do_vec_log_typed!(r, ty, ts, queue, data, count, u128),

        (SEDS_EK_SIGNED, 1) => do_vec_log_typed!(r, ty, ts, queue, data, count, i8),
        (SEDS_EK_SIGNED, 2) => do_vec_log_typed!(r, ty, ts, queue, data, count, i16),
        (SEDS_EK_SIGNED, 4) => do_vec_log_typed!(r, ty, ts, queue, data, count, i32),
        (SEDS_EK_SIGNED, 8) => do_vec_log_typed!(r, ty, ts, queue, data, count, i64),
        (SEDS_EK_SIGNED, 16) => do_vec_log_typed!(r, ty, ts, queue, data, count, i128),

        (SEDS_EK_FLOAT, 4) => do_vec_log_typed!(r, ty, ts, queue, data, count, f32),
        (SEDS_EK_FLOAT, 8) => do_vec_log_typed!(r, ty, ts, queue, data, count, f64),

        _ => status_from_err(TelemetryError::BadArg),
    }
}

// ---------- Legacy logging wrappers (preserve existing ABI) ----------

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_bytes(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const u8,
    len: usize,
) -> i32 {
    seds_router_log_bytes_ex(r, ty_u32, data, len, ptr::null(), false)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_f32(
    r: *mut SedsRouter,
    ty_u32: u32,
    vals: *const f32,
    n_vals: usize,
) -> i32 {
    seds_router_log_f32_ex(r, ty_u32, vals, n_vals, ptr::null(), false)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_typed(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const c_void,
    count: usize,
    elem_size: usize,
    elem_kind: u32,
) -> i32 {
    seds_router_log_typed_ex(
        r,
        ty_u32,
        data,
        count,
        elem_size,
        elem_kind,
        ptr::null(),
        false,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_log_queue_typed(
    r: *mut SedsRouter,
    ty_u32: u32,
    data: *const c_void,
    count: usize,
    elem_size: usize,
    elem_kind: u32,
) -> i32 {
    seds_router_log_typed_ex(
        r,
        ty_u32,
        data,
        count,
        elem_size,
        elem_kind,
        ptr::null(),
        true,
    )
}

// ============================================================================
//  FFI: Receive / queue RX and process queues
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_receive_serialized(
    r: *mut SedsRouter,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if r.is_null() || (len > 0 && bytes.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    ok_or_status(router.rx_serialized(slice))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_receive(r: *mut SedsRouter, view: *const SedsPacketView) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    ok_or_status(router.rx(&pkt))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_transmit_message_queue(
    r: *mut SedsRouter,
    view: *const SedsPacketView,
) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    ok_or_status(router.tx_queue(pkt))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_transmit_message(
    r: *mut SedsRouter,
    view: *const SedsPacketView,
) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    ok_or_status(router.tx(pkt))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_transmit_serialized_message_queue(
    r: *mut SedsRouter,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if r.is_null() || bytes.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    let data = Arc::from(slice);
    ok_or_status(router.tx_serialized_queue(data))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_transmit_serialized_message(
    r: *mut SedsRouter,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if r.is_null() || bytes.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    let data = Arc::from(slice);
    ok_or_status(router.tx_serialized(data))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_tx_queue(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.process_tx_queue())
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_rx_queue(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.process_rx_queue())
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_rx_serialized_packet_to_queue(
    r: *mut SedsRouter,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if r.is_null() || (len > 0 && bytes.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    ok_or_status(router.rx_serialized_queue(slice))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_rx_packet_to_queue(
    r: *mut SedsRouter,
    view: *const SedsPacketView,
) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    ok_or_status(router.rx_queue(pkt))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_all_queues(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.process_all_queues())
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_queues(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.clear_queues();
    status_from_result_code(SedsResult::SedsOk)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_rx_queue(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.clear_rx_queue();
    status_from_result_code(SedsResult::SedsOk)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_clear_tx_queue(r: *mut SedsRouter) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    router.clear_tx_queue();
    status_from_result_code(SedsResult::SedsOk)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_tx_queue_with_timeout(
    r: *mut SedsRouter,
    timeout_ms: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.process_tx_queue_with_timeout(timeout_ms))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_rx_queue_with_timeout(
    r: *mut SedsRouter,
    timeout_ms: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.process_rx_queue_with_timeout(timeout_ms))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_process_all_queues_with_timeout(
    r: *mut SedsRouter,
    timeout_ms: u32,
) -> i32 {
    if r.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    ok_or_status(router.process_all_queues_with_timeout(timeout_ms))
}

// ============================================================================
//  FFI: Receive / queue RX (explicit ingress side)
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_receive_serialized_from_side(
    r: *mut SedsRouter,
    side_id: u32,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if r.is_null() || (len > 0 && bytes.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    ok_or_status(router.rx_serialized_from_side(slice, side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_receive_from_side(
    r: *mut SedsRouter,
    side_id: u32,
    view: *const SedsPacketView,
) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    ok_or_status(router.rx_from_side(&pkt, side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_rx_serialized_packet_to_queue_from_side(
    r: *mut SedsRouter,
    side_id: u32,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if r.is_null() || (len > 0 && bytes.is_null()) {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    ok_or_status(router.rx_serialized_queue_from_side(slice, side_id as usize))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_router_rx_packet_to_queue_from_side(
    r: *mut SedsRouter,
    side_id: u32,
    view: *const SedsPacketView,
) -> i32 {
    if r.is_null() || view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let router = unsafe { &(*r).inner };
    let pkt = match view_to_packet(unsafe { &*view }) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::InvalidType),
    };
    ok_or_status(router.rx_queue_from_side(pkt, side_id as usize))
}

// ============================================================================
//  FFI: Payload pointer & copy helpers
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_bytes_ptr(
    pkt: *const SedsPacketView,
    out_len: *mut usize,
) -> *const c_void {
    if pkt.is_null() {
        return ptr::null();
    }
    let view = unsafe { &*pkt };
    if !out_len.is_null() {
        unsafe { *out_len = view.payload_len };
    }
    view.payload as *const c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_data_ptr(
    pkt: *const SedsPacketView,
    elem_size: usize,
    out_count: *mut usize,
) -> *const c_void {
    if pkt.is_null() || !width_is_valid(elem_size) {
        return ptr::null();
    }
    let view = unsafe { &*pkt };

    if elem_size == 0 || view.payload_len % elem_size != 0 {
        if !out_count.is_null() {
            unsafe { *out_count = 0 };
        }
        return ptr::null();
    }

    let count = view.payload_len / elem_size;
    if !out_count.is_null() {
        unsafe { *out_count = count };
    }

    view.payload as *const c_void
}

macro_rules! impl_seds_pkt_get_typed_from_packet {
    ($fname:ident, $method:ident, $ty:ty) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $fname(
            pkt: *const SedsPacketView,
            out: *mut $ty,
            out_elems: usize,
        ) -> i32 {
            if pkt.is_null() {
                return status_from_err(TelemetryError::BadArg);
            }

            let view = unsafe { &*pkt };
            let tpkt = match view_to_packet(view) {
                Ok(p) => p,
                Err(_) => return status_from_err(TelemetryError::BadArg),
            };

            let vals = match tpkt.$method() {
                Ok(v) => v,
                Err(e) => return status_from_err(e),
            };

            let needed = vals.len();
            if needed == 0 {
                return 0;
            }

            if out.is_null() || out_elems == 0 || out_elems < needed {
                return needed as i32;
            }

            unsafe {
                ptr::copy_nonoverlapping(vals.as_ptr(), out, needed);
            }

            needed as i32
        }
    };
}

impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_f32, data_as_f32, f32);
impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_f64, data_as_f64, f64);

impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_u8, data_as_u8, u8);
impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_u16, data_as_u16, u16);
impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_u32, data_as_u32, u32);
impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_u64, data_as_u64, u64);

impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_i8, data_as_i8, i8);
impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_i16, data_as_i16, i16);
impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_i32, data_as_i32, i32);
impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_i64, data_as_i64, i64);

impl_seds_pkt_get_typed_from_packet!(seds_pkt_get_bool, data_as_bool, bool);

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_get_string(
    pkt: *const SedsPacketView,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    if pkt.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    let view = unsafe { &*pkt };
    let tpkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };

    let s = match tpkt.data_as_string() {
        Ok(s) => s,
        Err(e) => return status_from_err(e),
    };

    unsafe { write_str_to_buf(&s, buf, buf_len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_get_string_len(pkt: *const SedsPacketView) -> i32 {
    if pkt.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    let view = unsafe { &*pkt };
    let tpkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };

    let s = match tpkt.data_as_string() {
        Ok(s) => s,
        Err(e) => return status_from_err(e),
    };

    (s.len() + 1) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_copy_bytes(
    pkt: *const SedsPacketView,
    dst: *mut u8,
    dst_len: usize,
) -> i32 {
    if pkt.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*pkt };
    let needed = view.payload_len;

    if needed == 0 {
        return 0;
    }
    if view.payload.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    if dst.is_null() || dst_len < needed {
        return needed as i32;
    }

    unsafe {
        ptr::copy_nonoverlapping(view.payload, dst, needed);
    }
    needed as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_copy_data(
    pkt: *const SedsPacketView,
    elem_size: usize,
    dst: *mut c_void,
    dst_elems: usize,
) -> i32 {
    if pkt.is_null() || !width_is_valid(elem_size) {
        return status_from_err(TelemetryError::BadArg);
    }

    let view = unsafe { &*pkt };

    if elem_size == 0 || view.payload_len % elem_size != 0 {
        return status_from_err(TelemetryError::BadArg);
    }

    let count = view.payload_len / elem_size;
    if count == 0 {
        return 0;
    }

    if dst.is_null() || dst_elems == 0 || dst_elems < count {
        return count as i32;
    }

    if count > 0 && view.payload.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    let total_bytes = match count.checked_mul(elem_size) {
        Some(n) => n,
        None => return status_from_err(TelemetryError::BadArg),
    };

    unsafe {
        ptr::copy_nonoverlapping(view.payload, dst as *mut u8, total_bytes);
    }
    count as i32
}

// ============================================================================
//  Typed extraction support: vectorize_data + seds_pkt_get_typed
// ============================================================================

#[derive(Debug)]
#[allow(dead_code)]
pub enum VectorizeError {
    NullBasePtr,
    ElemSizeMismatch { elem_size: usize, expected: usize },
    Overflow,
    ZeroCount,
}

fn vectorize_data<T: LeBytes + Copy>(
    base: *const u8,
    count: usize,
    elem_size: usize,
    tmp: &mut Vec<T>,
) -> Result<(), VectorizeError> {
    if base.is_null() {
        return Err(VectorizeError::NullBasePtr);
    }
    if elem_size != size_of::<T>() {
        return Err(VectorizeError::ElemSizeMismatch {
            elem_size,
            expected: size_of::<T>(),
        });
    }
    if count == 0 {
        return Err(VectorizeError::ZeroCount);
    }
    let _ = count
        .checked_mul(elem_size)
        .ok_or(VectorizeError::Overflow)?;

    tmp.reserve_exact(count);
    unsafe {
        let mut p = base;
        let dst = tmp.as_mut_ptr().add(tmp.len());
        for i in 0..count {
            let v = ptr::read_unaligned(p as *const T);
            dst.add(i).write(v);
            p = p.add(elem_size);
        }
        tmp.set_len(tmp.len() + count);
    }
    Ok(())
}

fn extract_typed_into<T: LeBytes + Copy>(
    view: &SedsPacketView,
    elem_size: usize,
    count: usize,
    out: *mut T,
) -> Result<(), VectorizeError> {
    let mut tmp: Vec<T> = Vec::with_capacity(count);
    vectorize_data::<T>(view.payload, count, elem_size, &mut tmp)?;
    unsafe {
        ptr::copy_nonoverlapping(tmp.as_ptr(), out, count);
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_get_typed(
    pkt: *const SedsPacketView,
    out: *mut c_void,
    count: usize,
    elem_size: usize,
    elem_kind: u32,
) -> i32 {
    if pkt.is_null() || !width_is_valid(elem_size) {
        return status_from_err(TelemetryError::BadArg);
    }

    let view = unsafe { &*pkt };

    if view.payload_len == 0 {
        return 0;
    }

    if view.payload.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }

    if elem_size == 0 || view.payload_len % elem_size != 0 {
        return status_from_err(TelemetryError::BadArg);
    }

    let needed = view.payload_len / elem_size;

    if out.is_null() || count == 0 || count < needed {
        return needed as i32;
    }

    let res = match (elem_kind, elem_size) {
        (SEDS_EK_UNSIGNED, 1) => extract_typed_into::<u8>(view, elem_size, needed, out as *mut u8),
        (SEDS_EK_UNSIGNED, 2) => {
            extract_typed_into::<u16>(view, elem_size, needed, out as *mut u16)
        }
        (SEDS_EK_UNSIGNED, 4) => {
            extract_typed_into::<u32>(view, elem_size, needed, out as *mut u32)
        }
        (SEDS_EK_UNSIGNED, 8) => {
            extract_typed_into::<u64>(view, elem_size, needed, out as *mut u64)
        }
        (SEDS_EK_UNSIGNED, 16) => {
            extract_typed_into::<u128>(view, elem_size, needed, out as *mut u128)
        }

        (SEDS_EK_SIGNED, 1) => extract_typed_into::<i8>(view, elem_size, needed, out as *mut i8),
        (SEDS_EK_SIGNED, 2) => extract_typed_into::<i16>(view, elem_size, needed, out as *mut i16),
        (SEDS_EK_SIGNED, 4) => extract_typed_into::<i32>(view, elem_size, needed, out as *mut i32),
        (SEDS_EK_SIGNED, 8) => extract_typed_into::<i64>(view, elem_size, needed, out as *mut i64),
        (SEDS_EK_SIGNED, 16) => {
            extract_typed_into::<i128>(view, elem_size, needed, out as *mut i128)
        }

        (SEDS_EK_FLOAT, 4) => extract_typed_into::<f32>(view, elem_size, needed, out as *mut f32),
        (SEDS_EK_FLOAT, 8) => extract_typed_into::<f64>(view, elem_size, needed, out as *mut f64),

        _ => Err(VectorizeError::ElemSizeMismatch {
            elem_size,
            expected: elem_size,
        }),
    };

    match res {
        Ok(()) => needed as i32,
        Err(_) => status_from_err(TelemetryError::BadArg),
    }
}

// ============================================================================
//  Serialization / deserialization helpers
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_serialize_len(view: *const SedsPacketView) -> i32 {
    if view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*view };
    let pkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };
    if let Err(e) = pkt.validate() {
        return status_from_err(e);
    }
    packet_wire_size(&pkt) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_serialize(
    view: *const SedsPacketView,
    out: *mut u8,
    out_len: usize,
) -> i32 {
    if view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let view = unsafe { &*view };
    let pkt = match view_to_packet(view) {
        Ok(p) => p,
        Err(_) => return status_from_err(TelemetryError::BadArg),
    };
    if let Err(e) = pkt.validate() {
        return status_from_err(e);
    }

    let bytes = serialize_packet(&pkt);
    let needed = bytes.len();

    if out.is_null() || out_len == 0 || out_len < needed {
        return needed as i32;
    }

    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), out, needed);
    }
    needed as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_deserialize_owned(bytes: *const u8, len: usize) -> *mut SedsOwnedPacket {
    if len > 0 && bytes.is_null() {
        return ptr::null_mut();
    }
    let slice = unsafe { slice::from_raw_parts(bytes, len) };

    let tpkt = match deserialize_packet(slice) {
        Ok(p) => p,
        Err(_) => return ptr::null_mut(),
    };
    if tpkt.validate().is_err() {
        return ptr::null_mut();
    }

    let endpoints_u32: Vec<u32> = tpkt.endpoints().iter().map(|e| e.as_u32()).collect();
    let owned = SedsOwnedPacket {
        inner: tpkt,
        endpoints_u32,
    };
    Box::into_raw(Box::new(owned))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_owned_pkt_free(p: *mut SedsOwnedPacket) {
    if p.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(p));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_owned_pkt_view(
    pkt: *const SedsOwnedPacket,
    out_view: *mut SedsPacketView,
) -> i32 {
    if pkt.is_null() || out_view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let pkt = unsafe { &*pkt };
    let inner = &pkt.inner;

    let sender_bytes = inner.sender().as_bytes();

    let view = SedsPacketView {
        ty: inner.data_type().as_u32(),
        data_size: inner.data_size(),
        sender: sender_bytes.as_ptr() as *const c_char,
        sender_len: sender_bytes.len(),
        endpoints: pkt.endpoints_u32.as_ptr(),
        num_endpoints: pkt.endpoints_u32.len(),
        timestamp: inner.timestamp(),
        payload: inner.payload().as_ptr(),
        payload_len: inner.payload().len(),
    };

    unsafe {
        *out_view = view;
    }
    status_from_result_code(SedsResult::SedsOk)
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_validate_serialized(bytes: *const u8, len: usize) -> i32 {
    if len > 0 && bytes.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    match deserialize_packet(slice) {
        Ok(p) => match p.validate() {
            Ok(()) => status_from_result_code(SedsResult::SedsOk),
            Err(e) => status_from_err(e),
        },
        Err(_) => status_from_err(TelemetryError::Deserialize("bad packet")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_pkt_deserialize_header_owned(
    bytes: *const u8,
    len: usize,
) -> *mut SedsOwnedHeader {
    if len > 0 && bytes.is_null() {
        return ptr::null_mut();
    }
    let slice = unsafe { slice::from_raw_parts(bytes, len) };

    let env = match peek_envelope(slice) {
        Ok(e) => e,
        Err(_) => return ptr::null_mut(),
    };

    let endpoints_u32: Vec<u32> = env.endpoints.iter().map(|&e| e.as_u32()).collect();
    let owned = SedsOwnedHeader {
        ty: env.ty.as_u32(),
        sender: env.sender,
        endpoints_u32,
        timestamp: env.timestamp_ms,
    };
    Box::into_raw(Box::new(owned))
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_owned_header_free(h: *mut SedsOwnedHeader) {
    if h.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(h));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn seds_owned_header_view(
    h: *const SedsOwnedHeader,
    out_view: *mut SedsPacketView,
) -> i32 {
    if h.is_null() || out_view.is_null() {
        return status_from_err(TelemetryError::BadArg);
    }
    let h = unsafe { &*h };
    let sender_bytes = h.sender.as_bytes();

    let view = SedsPacketView {
        ty: h.ty,
        data_size: 0,
        sender: sender_bytes.as_ptr() as *const c_char,
        sender_len: sender_bytes.len(),
        endpoints: h.endpoints_u32.as_ptr(),
        num_endpoints: h.endpoints_u32.len(),
        timestamp: h.timestamp,
        payload: ptr::null(),
        payload_len: 0,
    };

    unsafe {
        *out_view = view;
    }
    status_from_result_code(SedsResult::SedsOk)
}

#[cfg(all(test, feature = "discovery"))]
mod tests {
    use super::*;
    use crate::discovery::{DISCOVERY_FAST_INTERVAL_MS, build_discovery_announce};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use serde_json::Value;
    use std::sync::Mutex;

    struct TestClock {
        now_ms: Arc<AtomicU64>,
    }

    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.now_ms.load(Ordering::SeqCst)
        }
    }

    extern "C" fn pkt_counter_cb(pkt: *const SedsPacketView, user: *mut c_void) -> i32 {
        if pkt.is_null() || user.is_null() {
            return status_from_result_code(SedsResult::SedsErr);
        }
        let hits = unsafe { &*(user as *const AtomicUsize) };
        hits.fetch_add(1, Ordering::SeqCst);
        status_from_result_code(SedsResult::SedsOk)
    }

    struct PacketTypeCounter {
        hits: AtomicUsize,
        ty: DataType,
    }

    extern "C" fn pkt_type_counter_cb(pkt: *const SedsPacketView, user: *mut c_void) -> i32 {
        if pkt.is_null() || user.is_null() {
            return status_from_result_code(SedsResult::SedsErr);
        }
        let counter = unsafe { &*(user as *const PacketTypeCounter) };
        let pkt = unsafe { &*pkt };
        if pkt.ty == counter.ty.as_u32() {
            counter.hits.fetch_add(1, Ordering::SeqCst);
        }
        status_from_result_code(SedsResult::SedsOk)
    }

    #[derive(Default)]
    struct SerializedTxState {
        attempts: AtomicUsize,
        delivered: Mutex<Vec<Vec<u8>>>,
    }

    fn count_serialized_frames_of_type(frames: &[Vec<u8>], ty: DataType) -> usize {
        frames
            .iter()
            .filter(|bytes| peek_envelope(bytes.as_slice()).ok().map(|env| env.ty) == Some(ty))
            .count()
    }

    extern "C" fn serialized_retry_once_cb(bytes: *const u8, len: usize, user: *mut c_void) -> i32 {
        if bytes.is_null() || user.is_null() {
            return status_from_result_code(SedsResult::SedsErr);
        }
        let state = unsafe { &*(user as *const SerializedTxState) };
        let attempt = state.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return status_from_result_code(SedsResult::SedsErr);
        }
        let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
        state.delivered.lock().unwrap().push(slice.to_vec());
        status_from_result_code(SedsResult::SedsOk)
    }

    extern "C" fn serialized_ok_cb(bytes: *const u8, _len: usize, _user: *mut c_void) -> i32 {
        if bytes.is_null() {
            return status_from_result_code(SedsResult::SedsErr);
        }
        status_from_result_code(SedsResult::SedsOk)
    }

    fn export_router_json(
        router: *mut SedsRouter,
        len_fn: extern "C" fn(*mut SedsRouter) -> i32,
        export_fn: extern "C" fn(*mut SedsRouter, *mut c_char, usize) -> i32,
    ) -> String {
        let len = len_fn(router);
        assert!(len > 0);
        let mut buf = vec![0u8; len as usize];
        assert_eq!(export_fn(router, buf.as_mut_ptr().cast(), buf.len()), 0);
        let nul = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8(buf[..nul].to_vec()).unwrap()
    }

    fn export_relay_json(
        relay: *mut SedsRelay,
        len_fn: extern "C" fn(*mut SedsRelay) -> i32,
        export_fn: extern "C" fn(*mut SedsRelay, *mut c_char, usize) -> i32,
    ) -> String {
        let len = len_fn(relay);
        assert!(len > 0);
        let mut buf = vec![0u8; len as usize];
        assert_eq!(export_fn(relay, buf.as_mut_ptr().cast(), buf.len()), 0);
        let nul = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8(buf[..nul].to_vec()).unwrap()
    }

    fn assert_runtime_json_shape(doc: &Value, expected_side_name: &str) {
        assert!(doc.get("sides").unwrap().is_array());
        assert!(doc.get("route_modes").unwrap().is_array());
        assert!(doc.get("route_overrides").unwrap().is_array());
        assert!(doc.get("typed_route_overrides").unwrap().is_array());
        assert!(doc.get("route_weights").unwrap().is_array());
        assert!(doc.get("route_priorities").unwrap().is_array());
        assert!(doc.get("queues").unwrap().is_object());
        assert!(doc.get("reliable").unwrap().is_object());
        assert!(doc.get("discovery").unwrap().is_object());
        assert!(doc.get("total_handler_failures").unwrap().is_u64());
        assert!(doc.get("total_handler_retries").unwrap().is_u64());

        let sides = doc.get("sides").unwrap().as_array().unwrap();
        let side = sides
            .iter()
            .find(|side| side.get("side_name").and_then(Value::as_str) == Some(expected_side_name))
            .unwrap();
        assert!(side.get("side_id").unwrap().is_u64());
        assert!(side.get("reliable_enabled").unwrap().is_boolean());
        assert!(side.get("link_local_enabled").unwrap().is_boolean());
        assert!(side.get("ingress_enabled").unwrap().is_boolean());
        assert!(side.get("egress_enabled").unwrap().is_boolean());
        assert!(side.get("tx_packets").unwrap().is_u64());
        assert!(side.get("tx_bytes").unwrap().is_u64());
        assert!(side.get("rx_packets").unwrap().is_u64());
        assert!(side.get("rx_bytes").unwrap().is_u64());
        assert!(side.get("relayed_tx_packets").unwrap().is_u64());
        assert!(side.get("relayed_rx_packets").unwrap().is_u64());
        assert!(side.get("tx_retries").unwrap().is_u64());
        assert!(side.get("data_types").unwrap().is_array());

        let adaptive = side.get("adaptive").unwrap();
        assert!(adaptive.get("auto_balancing_enabled").unwrap().is_boolean());
        assert!(adaptive.get("estimated_capacity_bps").unwrap().is_u64());
        assert!(adaptive.get("peak_capacity_bps").unwrap().is_u64());
        assert!(adaptive.get("current_usage_bps").unwrap().is_u64());
        assert!(adaptive.get("peak_usage_bps").unwrap().is_u64());
        assert!(adaptive.get("available_headroom_bps").unwrap().is_u64());
        assert!(adaptive.get("effective_weight").unwrap().is_u64());
        assert!(adaptive.get("last_observed_ms").unwrap().is_u64());
        assert!(adaptive.get("sample_count").unwrap().is_u64());

        let queues = doc.get("queues").unwrap();
        assert!(queues.get("rx_len").unwrap().is_u64());
        assert!(queues.get("tx_len").unwrap().is_u64());
        assert!(queues.get("shared_queue_bytes_used").unwrap().is_u64());

        let reliable = doc.get("reliable").unwrap();
        assert!(
            reliable
                .get("reliable_return_route_count")
                .unwrap()
                .is_u64()
        );
        assert!(reliable.get("end_to_end_pending_count").unwrap().is_u64());

        let discovery = doc.get("discovery").unwrap();
        assert!(discovery.get("route_count").unwrap().is_u64());
        assert!(discovery.get("announcer_count").unwrap().is_u64());
        assert!(discovery.get("current_announce_interval_ms").is_some());
        assert!(discovery.get("next_announce_ms").is_some());
    }

    fn assert_topology_json_shape(doc: &Value, local_sender: &str) {
        assert!(doc.get("advertised_endpoints").unwrap().is_array());
        assert!(doc.get("advertised_timesync_sources").unwrap().is_array());
        assert!(doc.get("routers").unwrap().is_array());
        assert!(doc.get("routes").unwrap().is_array());

        let local = doc
            .get("routers")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|router| router.get("sender_id").and_then(Value::as_str) == Some(local_sender))
            .unwrap();
        assert!(local.get("connections").unwrap().is_array());
        assert!(local.get("reachable_endpoints").unwrap().is_array());
        assert!(local.get("reachable_timesync_sources").unwrap().is_array());

        let route = doc
            .get("routes")
            .unwrap()
            .as_array()
            .unwrap()
            .first()
            .unwrap();
        assert!(route.get("side_id").unwrap().is_u64());
        assert!(route.get("side_name").unwrap().is_string());
        assert!(route.get("announcers").unwrap().is_array());
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn router_c_abi_rejects_reserved_discovery_endpoint_handler() {
        let desc = SedsLocalEndpointDesc {
            endpoint: DataEndpoint::Discovery.as_u32(),
            packet_handler: Some(pkt_counter_cb),
            serialized_handler: None,
            user: ptr::null_mut(),
        };

        let router = seds_router_new(0, None, ptr::null_mut(), &desc, 1);
        assert!(router.is_null());
    }

    #[cfg(feature = "timesync")]
    #[test]
    fn router_c_abi_rejects_reserved_timesync_endpoint_handler() {
        let desc = SedsLocalEndpointDesc {
            endpoint: DataEndpoint::TimeSync.as_u32(),
            packet_handler: Some(pkt_counter_cb),
            serialized_handler: None,
            user: ptr::null_mut(),
        };

        let router = seds_router_new(0, None, ptr::null_mut(), &desc, 1);
        assert!(router.is_null());
    }

    #[test]
    fn router_c_abi_can_announce_and_poll_discovery() {
        let now_ms = Arc::new(AtomicU64::new(0));
        let hits = AtomicUsize::new(0);
        let side_name = b"NET";
        let mut did_queue = false;

        let router = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint(101),
                |_pkt| Ok(()),
            )]),
            Box::new(TestClock {
                now_ms: now_ms.clone(),
            }),
        );
        let router = Box::into_raw(Box::new(SedsRouter {
            inner: Arc::from(router),
        }));

        let side_id = seds_router_add_side_packet(
            router,
            side_name.as_ptr() as *const c_char,
            side_name.len(),
            Some(pkt_counter_cb),
            (&hits as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        assert!(side_id >= 0);

        assert_eq!(seds_router_announce_discovery(router), 0);
        assert_eq!(seds_router_process_tx_queue(router), 0);
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        now_ms.store(DISCOVERY_FAST_INTERVAL_MS, Ordering::SeqCst);
        assert_eq!(seds_router_poll_discovery(router, &mut did_queue), 0);
        assert!(did_queue);
        assert_eq!(seds_router_process_tx_queue(router), 0);
        assert_eq!(hits.load(Ordering::SeqCst), 6);

        seds_router_free(router);
    }

    #[test]
    fn router_c_abi_queued_serialized_ingress_retries_side_tx_and_relays() {
        let side_name_a = b"can";
        let side_name_b = b"uart";
        let tx_state = SerializedTxState::default();

        let router = seds_router_new(1, None, ptr::null_mut(), ptr::null(), 0);
        assert!(!router.is_null());

        let side_a = seds_router_add_side_serialized(
            router,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(serialized_ok_cb),
            ptr::null_mut(),
            true,
        );
        assert!(side_a >= 0);

        let side_b = seds_router_add_side_serialized(
            router,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(serialized_retry_once_cb),
            (&tx_state as *const SerializedTxState).cast_mut().cast(),
            true,
        );
        assert!(side_b >= 0);

        let pkt = Packet::from_f32_slice(DataType(100), &[1.0, 2.0, 3.0], &[DataEndpoint(101)], 33)
            .unwrap();
        let wire = serialize_packet(&pkt);

        assert_eq!(
            seds_router_rx_serialized_packet_to_queue_from_side(
                router,
                side_a as u32,
                wire.as_ptr(),
                wire.len(),
            ),
            0
        );
        assert_eq!(seds_router_process_all_queues_with_timeout(router, 0), 0);

        assert!(tx_state.attempts.load(Ordering::SeqCst) >= 2);
        let delivered = tx_state.delivered.lock().unwrap().clone();
        assert_eq!(
            count_serialized_frames_of_type(&delivered, DataType(100)),
            1
        );
        assert!(!delivered[0].is_empty());

        seds_router_free(router);
    }

    #[test]
    fn router_c_abi_remove_side_stops_discovery_tx() {
        let hits_a = AtomicUsize::new(0);
        let hits_b = AtomicUsize::new(0);
        let side_name_a = b"A";
        let side_name_b = b"B";

        let router = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint(101),
                |_pkt| Ok(()),
            )]),
            Box::new(TestClock {
                now_ms: Arc::new(AtomicU64::new(0)),
            }),
        );
        let router = Box::into_raw(Box::new(SedsRouter {
            inner: Arc::from(router),
        }));

        let side_a = seds_router_add_side_packet(
            router,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(pkt_counter_cb),
            (&hits_a as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        let side_b = seds_router_add_side_packet(
            router,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(pkt_counter_cb),
            (&hits_b as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        assert!(side_a >= 0);
        assert!(side_b >= 0);
        assert_eq!(seds_router_remove_side(router, side_a), 0);
        assert_eq!(
            seds_router_remove_side(router, side_a),
            status_from_err(TelemetryError::BadArg)
        );

        assert_eq!(seds_router_announce_discovery(router), 0);
        assert_eq!(seds_router_process_tx_queue(router), 0);
        assert_eq!(hits_a.load(Ordering::SeqCst), 0);
        assert!(hits_b.load(Ordering::SeqCst) > 0);

        seds_router_free(router);
    }

    #[test]
    fn relay_c_abi_remove_side_stops_discovery_tx() {
        let hits_a = AtomicUsize::new(0);
        let hits_b = AtomicUsize::new(0);
        let side_name_a = b"A";
        let side_name_b = b"B";

        let relay = Relay::new(Box::new(TestClock {
            now_ms: Arc::new(AtomicU64::new(0)),
        }));
        let relay = Box::into_raw(Box::new(SedsRelay {
            inner: Arc::new(relay),
        }));

        let side_a = seds_relay_add_side_packet(
            relay,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(pkt_counter_cb),
            (&hits_a as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        let side_b = seds_relay_add_side_packet(
            relay,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(pkt_counter_cb),
            (&hits_b as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        assert!(side_a >= 0);
        assert!(side_b >= 0);
        assert_eq!(seds_relay_remove_side(relay, side_a), 0);
        assert_eq!(
            seds_relay_remove_side(relay, side_a),
            status_from_err(TelemetryError::BadArg)
        );

        let discovery_pkt = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint(101)]).unwrap();
        unsafe {
            (*relay)
                .inner
                .rx_from_side(side_b as RelaySideId, discovery_pkt)
                .unwrap();
        }
        assert_eq!(seds_relay_process_rx_queue(relay), 0);
        hits_a.store(0, Ordering::SeqCst);
        hits_b.store(0, Ordering::SeqCst);

        assert_eq!(seds_relay_announce_discovery(relay), 0);
        assert_eq!(seds_relay_process_tx_queue(relay), 0);
        assert_eq!(hits_a.load(Ordering::SeqCst), 0);
        assert!(hits_b.load(Ordering::SeqCst) > 0);

        seds_relay_free(relay);
    }

    #[test]
    fn router_c_abi_runtime_routes_can_limit_local_tx_to_one_side() {
        let hits = AtomicUsize::new(0);
        let side_name_a = b"A";
        let side_name_b = b"B";

        let router = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint(101),
                |_pkt| Ok(()),
            )]),
            Box::new(TestClock {
                now_ms: Arc::new(AtomicU64::new(0)),
            }),
        );
        let router = Box::into_raw(Box::new(SedsRouter {
            inner: Arc::from(router),
        }));

        let side_a = seds_router_add_side_packet(
            router,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(pkt_counter_cb),
            (&hits as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        let side_b = seds_router_add_side_packet(
            router,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(pkt_counter_cb),
            (&hits as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        assert!(side_a >= 0);
        assert!(side_b >= 0);
        assert_eq!(seds_router_set_route(router, -1, side_b, false), 0);

        assert_eq!(seds_router_announce_discovery(router), 0);
        assert_eq!(seds_router_process_tx_queue(router), 0);
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        seds_router_free(router);
    }

    #[test]
    fn router_c_abi_weighted_route_mode_splits_local_tx() {
        let hits_a = AtomicUsize::new(0);
        let hits_b = AtomicUsize::new(0);
        let side_name_a = b"A";
        let side_name_b = b"B";

        let router = Router::new_with_clock(
            RouterConfig::default(),
            Box::new(TestClock {
                now_ms: Arc::new(AtomicU64::new(0)),
            }),
        );
        let router = Box::into_raw(Box::new(SedsRouter {
            inner: Arc::from(router),
        }));

        let side_a = seds_router_add_side_packet(
            router,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(pkt_counter_cb),
            (&hits_a as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        let side_b = seds_router_add_side_packet(
            router,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(pkt_counter_cb),
            (&hits_b as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        let discovery_pkt_a =
            build_discovery_announce("REMOTE_A", 0, &[DataEndpoint(101)]).unwrap();
        let discovery_pkt_b =
            build_discovery_announce("REMOTE_B", 1, &[DataEndpoint(101)]).unwrap();
        unsafe {
            (*router)
                .inner
                .rx_from_side(&discovery_pkt_a, side_a as usize)
                .unwrap();
            (*router)
                .inner
                .rx_from_side(&discovery_pkt_b, side_b as usize)
                .unwrap();
        }
        hits_a.store(0, Ordering::SeqCst);
        hits_b.store(0, Ordering::SeqCst);

        assert_eq!(seds_router_set_source_route_mode(router, -1, 1), 0);
        assert_eq!(seds_router_set_route_weight(router, -1, side_a, 2), 0);
        assert_eq!(seds_router_set_route_weight(router, -1, side_b, 1), 0);

        for seq in 0..6 {
            let pkt = Packet::from_f32_slice(
                DataType(100),
                &[seq as f32, seq as f32 + 1.0, seq as f32 + 2.0],
                &[DataEndpoint(101)],
                seq as u64,
            )
            .unwrap();
            unsafe {
                (*router).inner.tx(pkt).unwrap();
            }
        }

        assert_eq!(hits_a.load(Ordering::SeqCst), 4);
        assert_eq!(hits_b.load(Ordering::SeqCst), 2);

        seds_router_free(router);
    }

    #[test]
    fn router_c_abi_typed_routes_can_target_selected_sides() {
        let hits_a = PacketTypeCounter {
            hits: AtomicUsize::new(0),
            ty: DataType(100),
        };
        let hits_b = PacketTypeCounter {
            hits: AtomicUsize::new(0),
            ty: DataType(100),
        };
        let hits_c = PacketTypeCounter {
            hits: AtomicUsize::new(0),
            ty: DataType(100),
        };
        let side_name_a = b"A";
        let side_name_b = b"B";
        let side_name_c = b"C";

        let router = Router::new_with_clock(
            RouterConfig::default(),
            Box::new(TestClock {
                now_ms: Arc::new(AtomicU64::new(0)),
            }),
        );
        let router = Box::into_raw(Box::new(SedsRouter {
            inner: Arc::from(router),
        }));

        let side_a = seds_router_add_side_packet(
            router,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(pkt_type_counter_cb),
            (&hits_a as *const PacketTypeCounter).cast_mut().cast(),
            false,
        );
        let side_b = seds_router_add_side_packet(
            router,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(pkt_type_counter_cb),
            (&hits_b as *const PacketTypeCounter).cast_mut().cast(),
            false,
        );
        let side_c = seds_router_add_side_packet(
            router,
            side_name_c.as_ptr() as *const c_char,
            side_name_c.len(),
            Some(pkt_type_counter_cb),
            (&hits_c as *const PacketTypeCounter).cast_mut().cast(),
            false,
        );
        assert!(side_a >= 0);
        assert!(side_b >= 0);
        assert!(side_c >= 0);

        let discovery_b = build_discovery_announce("REMOTE_B", 0, &[DataEndpoint(101)]).unwrap();
        let discovery_c = build_discovery_announce("REMOTE_C", 1, &[DataEndpoint(101)]).unwrap();
        unsafe {
            assert_eq!(
                ok_or_status((*router).inner.rx_from_side(&discovery_b, side_b as usize)),
                0
            );
            assert_eq!(
                ok_or_status((*router).inner.rx_from_side(&discovery_c, side_c as usize)),
                0
            );
        }

        assert_eq!(
            seds_router_set_typed_route(router, -1, DataType(100).as_u32(), side_b, true),
            0
        );
        assert_eq!(
            seds_router_set_typed_route(router, -1, DataType(100).as_u32(), side_c, true),
            0
        );
        assert_eq!(seds_router_set_source_route_mode(router, -1, 0), 0);

        let pkt = Packet::from_f32_slice(DataType(100), &[1.0, 2.0, 3.0], &[DataEndpoint(101)], 1)
            .unwrap();
        unsafe {
            assert_eq!(ok_or_status((*router).inner.tx(pkt)), 0);
        }
        assert_eq!(hits_a.hits.load(Ordering::SeqCst), 0);
        assert_eq!(hits_b.hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits_c.hits.load(Ordering::SeqCst), 1);

        assert_eq!(
            seds_router_clear_typed_route(router, -1, DataType(100).as_u32(), side_b),
            0
        );
        assert_eq!(
            seds_router_clear_typed_route(router, -1, DataType(100).as_u32(), side_c),
            0
        );

        let pkt = Packet::from_f32_slice(DataType(100), &[4.0, 5.0, 6.0], &[DataEndpoint(101)], 2)
            .unwrap();
        unsafe {
            assert_eq!(ok_or_status((*router).inner.tx(pkt)), 0);
        }
        assert_eq!(hits_a.hits.load(Ordering::SeqCst), 0);
        assert_eq!(hits_b.hits.load(Ordering::SeqCst) + hits_c.hits.load(Ordering::SeqCst), 4);

        seds_router_free(router);
    }

    #[test]
    fn router_c_abi_periodic_runs_discovery_and_queue_processing() {
        let hits = AtomicUsize::new(0);
        let side_name = b"NET";

        let router = Router::new_with_clock(
            RouterConfig::new(vec![EndpointHandler::new_packet_handler(
                DataEndpoint(101),
                |_pkt| Ok(()),
            )]),
            Box::new(TestClock {
                now_ms: Arc::new(AtomicU64::new(0)),
            }),
        );
        let router = Box::into_raw(Box::new(SedsRouter {
            inner: Arc::from(router),
        }));

        let side_id = seds_router_add_side_packet(
            router,
            side_name.as_ptr() as *const c_char,
            side_name.len(),
            Some(pkt_counter_cb),
            (&hits as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        assert!(side_id >= 0);

        assert_eq!(seds_router_periodic_no_timesync(router, 0), 0);
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        seds_router_free(router);
    }

    #[test]
    fn relay_c_abi_can_announce_and_poll_discovery() {
        let now_ms = Arc::new(AtomicU64::new(0));
        let hits = AtomicUsize::new(0);
        let side_name_a = b"A";
        let side_name_b = b"B";
        let mut did_queue = false;

        let relay = Relay::new(Box::new(TestClock {
            now_ms: now_ms.clone(),
        }));
        let relay = Box::into_raw(Box::new(SedsRelay {
            inner: Arc::new(relay),
        }));

        let side_a = seds_relay_add_side_packet(
            relay,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(pkt_counter_cb),
            (&hits as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        let side_b = seds_relay_add_side_packet(
            relay,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(pkt_counter_cb),
            (&hits as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        assert!(side_a >= 0);
        assert!(side_b >= 0);
        assert_eq!(seds_relay_set_route(relay, side_a, side_b, false), 0);

        let discovery_pkt = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint(101)]).unwrap();
        unsafe {
            (*relay)
                .inner
                .rx_from_side(side_a as RelaySideId, discovery_pkt)
                .unwrap();
        }
        assert_eq!(seds_relay_process_rx_queue(relay), 0);
        assert_eq!(seds_relay_process_tx_queue(relay), 0);
        let hits_after_learning = hits.load(Ordering::SeqCst);

        assert_eq!(seds_relay_announce_discovery(relay), 0);
        assert_eq!(seds_relay_process_tx_queue(relay), 0);
        assert!(hits.load(Ordering::SeqCst) > hits_after_learning);

        now_ms.store(DISCOVERY_FAST_INTERVAL_MS, Ordering::SeqCst);
        assert_eq!(seds_relay_poll_discovery(relay, &mut did_queue), 0);
        if did_queue {
            assert_eq!(seds_relay_process_tx_queue(relay), 0);
            assert!(hits.load(Ordering::SeqCst) > hits_after_learning);
        }

        seds_relay_free(relay);
    }

    #[test]
    fn relay_c_abi_periodic_runs_discovery_and_queue_processing() {
        let hits = AtomicUsize::new(0);
        let side_name_a = b"A";
        let side_name_b = b"B";

        let relay = Relay::new(Box::new(TestClock {
            now_ms: Arc::new(AtomicU64::new(0)),
        }));
        let relay = Box::into_raw(Box::new(SedsRelay {
            inner: Arc::new(relay),
        }));

        let side_a = seds_relay_add_side_packet(
            relay,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(pkt_counter_cb),
            (&hits as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        let side_b = seds_relay_add_side_packet(
            relay,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(pkt_counter_cb),
            (&hits as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        assert!(side_a >= 0);
        assert!(side_b >= 0);

        let discovery_pkt = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint(101)]).unwrap();
        unsafe {
            (*relay)
                .inner
                .rx_from_side(side_a as RelaySideId, discovery_pkt)
                .unwrap();
        }

        assert_eq!(seds_relay_periodic(relay, 0), 0);
        let hits_after_learning = hits.load(Ordering::SeqCst);
        assert_eq!(seds_relay_periodic(relay, 0), 0);
        assert_eq!(hits.load(Ordering::SeqCst), hits_after_learning + 6);

        seds_relay_free(relay);
    }

    #[test]
    fn router_c_abi_runtime_stats_json_has_expected_schema() {
        let router = seds_router_new(1, None, ptr::null_mut(), ptr::null(), 0);
        assert!(!router.is_null());

        let side_name = b"UPLINK";
        let side_id = seds_router_add_side_serialized(
            router,
            side_name.as_ptr() as *const c_char,
            side_name.len(),
            Some(serialized_ok_cb),
            ptr::null_mut(),
            false,
        );
        assert!(side_id >= 0);

        let discovery = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint(101)]).unwrap();
        unsafe {
            assert_eq!(
                ok_or_status((*router).inner.rx_from_side(&discovery, side_id as usize)),
                0
            );
        }

        let topology: Value = serde_json::from_str(&export_router_json(
            router,
            seds_router_export_topology_len,
            seds_router_export_topology,
        ))
        .unwrap();
        assert_topology_json_shape(&topology, "TEST_PLATFORM");

        let runtime: Value = serde_json::from_str(&export_router_json(
            router,
            seds_router_export_runtime_stats_len,
            seds_router_export_runtime_stats,
        ))
        .unwrap();
        assert_runtime_json_shape(&runtime, "UPLINK");

        seds_router_free(router);
    }

    #[test]
    fn relay_c_abi_runtime_stats_json_has_expected_schema() {
        let relay = seds_relay_new(None, ptr::null_mut());
        assert!(!relay.is_null());

        let side_name = b"UPLINK";
        let side_id = seds_relay_add_side_serialized(
            relay,
            side_name.as_ptr() as *const c_char,
            side_name.len(),
            Some(serialized_ok_cb),
            ptr::null_mut(),
            false,
        );
        assert!(side_id >= 0);

        let discovery = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint(101)]).unwrap();
        unsafe {
            (*relay)
                .inner
                .rx_from_side(side_id as RelaySideId, discovery)
                .unwrap();
        }
        assert_eq!(seds_relay_process_rx_queue(relay), 0);

        let topology: Value = serde_json::from_str(&export_relay_json(
            relay,
            seds_relay_export_topology_len,
            seds_relay_export_topology,
        ))
        .unwrap();
        assert_topology_json_shape(&topology, "RELAY");

        let runtime: Value = serde_json::from_str(&export_relay_json(
            relay,
            seds_relay_export_runtime_stats_len,
            seds_relay_export_runtime_stats,
        ))
        .unwrap();
        assert_runtime_json_shape(&runtime, "UPLINK");

        seds_relay_free(relay);
    }

    #[test]
    fn c_abi_sender_id_setters_update_exported_topology() {
        let router = seds_router_new(1, None, ptr::null_mut(), ptr::null(), 0);
        assert!(!router.is_null());
        let side_name = b"UPLINK";
        let side_id = seds_router_add_side_serialized(
            router,
            side_name.as_ptr() as *const c_char,
            side_name.len(),
            Some(serialized_ok_cb),
            ptr::null_mut(),
            false,
        );
        assert!(side_id >= 0);
        let discovery = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint(101)]).unwrap();
        unsafe {
            assert_eq!(
                ok_or_status((*router).inner.rx_from_side(&discovery, side_id as usize)),
                0
            );
        }
        let router_sender = b"ROUTER_RUNTIME";
        assert_eq!(
            seds_router_set_sender_id(
                router,
                router_sender.as_ptr() as *const c_char,
                router_sender.len(),
            ),
            0
        );
        let router_topology: Value = serde_json::from_str(&export_router_json(
            router,
            seds_router_export_topology_len,
            seds_router_export_topology,
        ))
        .unwrap();
        assert_topology_json_shape(&router_topology, "ROUTER_RUNTIME");
        seds_router_free(router);

        let relay = seds_relay_new(None, ptr::null_mut());
        assert!(!relay.is_null());
        let relay_side_id = seds_relay_add_side_serialized(
            relay,
            side_name.as_ptr() as *const c_char,
            side_name.len(),
            Some(serialized_ok_cb),
            ptr::null_mut(),
            false,
        );
        assert!(relay_side_id >= 0);
        let relay_discovery =
            build_discovery_announce("REMOTE_A", 0, &[DataEndpoint(101)]).unwrap();
        unsafe {
            (*relay)
                .inner
                .rx_from_side(relay_side_id as RelaySideId, relay_discovery)
                .unwrap();
        }
        assert_eq!(seds_relay_process_rx_queue(relay), 0);
        let relay_sender = b"RELAY_RUNTIME";
        assert_eq!(
            seds_relay_set_sender_id(
                relay,
                relay_sender.as_ptr() as *const c_char,
                relay_sender.len(),
            ),
            0
        );
        let relay_topology: Value = serde_json::from_str(&export_relay_json(
            relay,
            seds_relay_export_topology_len,
            seds_relay_export_topology,
        ))
        .unwrap();
        assert_topology_json_shape(&relay_topology, "RELAY_RUNTIME");
        seds_relay_free(relay);
    }

    #[test]
    fn relay_c_abi_runtime_routes_can_limit_discovery_to_one_side() {
        let hits_a = AtomicUsize::new(0);
        let hits_b = AtomicUsize::new(0);
        let side_name_a = b"A";
        let side_name_b = b"B";

        let relay = Relay::new(Box::new(TestClock {
            now_ms: Arc::new(AtomicU64::new(0)),
        }));
        let relay = Box::into_raw(Box::new(SedsRelay {
            inner: Arc::new(relay),
        }));

        let side_a = seds_relay_add_side_packet(
            relay,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(pkt_counter_cb),
            (&hits_a as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        let side_b = seds_relay_add_side_packet(
            relay,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(pkt_counter_cb),
            (&hits_b as *const AtomicUsize).cast_mut().cast(),
            false,
        );
        assert!(side_a >= 0);
        assert!(side_b >= 0);

        let discovery_pkt = build_discovery_announce("REMOTE_A", 0, &[DataEndpoint(101)]).unwrap();
        unsafe {
            (*relay)
                .inner
                .rx_from_side(side_a as RelaySideId, discovery_pkt)
                .unwrap();
        }
        assert_eq!(seds_relay_process_rx_queue(relay), 0);
        assert_eq!(seds_relay_process_tx_queue(relay), 0);
        hits_a.store(0, Ordering::SeqCst);
        hits_b.store(0, Ordering::SeqCst);

        assert_eq!(seds_relay_set_route(relay, -1, side_b, false), 0);
        assert_eq!(seds_relay_announce_discovery(relay), 0);
        assert_eq!(seds_relay_process_tx_queue(relay), 0);
        assert!(hits_a.load(Ordering::SeqCst) > 0);
        assert_eq!(hits_b.load(Ordering::SeqCst), 0);

        seds_relay_free(relay);
    }

    #[test]
    fn relay_c_abi_typed_routes_can_target_selected_sides() {
        let hits_a = PacketTypeCounter {
            hits: AtomicUsize::new(0),
            ty: DataType(100),
        };
        let hits_b = PacketTypeCounter {
            hits: AtomicUsize::new(0),
            ty: DataType(100),
        };
        let hits_c = PacketTypeCounter {
            hits: AtomicUsize::new(0),
            ty: DataType(100),
        };
        let hits_d = PacketTypeCounter {
            hits: AtomicUsize::new(0),
            ty: DataType(100),
        };
        let side_name_a = b"A";
        let side_name_b = b"B";
        let side_name_c = b"C";
        let side_name_d = b"D";

        let relay = Relay::new(Box::new(TestClock {
            now_ms: Arc::new(AtomicU64::new(0)),
        }));
        let relay = Box::into_raw(Box::new(SedsRelay {
            inner: Arc::new(relay),
        }));

        let side_a = seds_relay_add_side_packet(
            relay,
            side_name_a.as_ptr() as *const c_char,
            side_name_a.len(),
            Some(pkt_type_counter_cb),
            (&hits_a as *const PacketTypeCounter).cast_mut().cast(),
            false,
        );
        let side_b = seds_relay_add_side_packet(
            relay,
            side_name_b.as_ptr() as *const c_char,
            side_name_b.len(),
            Some(pkt_type_counter_cb),
            (&hits_b as *const PacketTypeCounter).cast_mut().cast(),
            false,
        );
        let side_c = seds_relay_add_side_packet(
            relay,
            side_name_c.as_ptr() as *const c_char,
            side_name_c.len(),
            Some(pkt_type_counter_cb),
            (&hits_c as *const PacketTypeCounter).cast_mut().cast(),
            false,
        );
        let side_d = seds_relay_add_side_packet(
            relay,
            side_name_d.as_ptr() as *const c_char,
            side_name_d.len(),
            Some(pkt_type_counter_cb),
            (&hits_d as *const PacketTypeCounter).cast_mut().cast(),
            false,
        );
        assert!(side_a >= 0);
        assert!(side_b >= 0);
        assert!(side_c >= 0);
        assert!(side_d >= 0);
        let discovery_b = build_discovery_announce("REMOTE_B", 0, &[DataEndpoint(101)]).unwrap();
        let discovery_d = build_discovery_announce("REMOTE_D", 1, &[DataEndpoint(101)]).unwrap();
        unsafe {
            (*relay)
                .inner
                .rx_from_side(side_b as RelaySideId, discovery_b)
                .unwrap();
            (*relay)
                .inner
                .rx_from_side(side_d as RelaySideId, discovery_d)
                .unwrap();
        }
        assert_eq!(seds_relay_process_all_queues(relay), 0);
        hits_a.hits.store(0, Ordering::SeqCst);
        hits_b.hits.store(0, Ordering::SeqCst);
        hits_c.hits.store(0, Ordering::SeqCst);
        hits_d.hits.store(0, Ordering::SeqCst);

        assert_eq!(
            seds_relay_set_typed_route(relay, side_a, DataType(100).as_u32(), side_b, true),
            0
        );
        assert_eq!(
            seds_relay_set_typed_route(relay, side_a, DataType(100).as_u32(), side_d, true),
            0
        );
        assert_eq!(seds_relay_set_source_route_mode(relay, side_a, 0), 0);

        let pkt = Packet::from_f32_slice(DataType(100), &[1.0, 2.0, 3.0], &[DataEndpoint(101)], 1)
            .unwrap();
        unsafe {
            (*relay)
                .inner
                .rx_from_side(side_a as RelaySideId, pkt)
                .unwrap();
        }
        assert_eq!(seds_relay_process_all_queues(relay), 0);
        assert_eq!(hits_a.hits.load(Ordering::SeqCst), 0);
        assert_eq!(hits_b.hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits_c.hits.load(Ordering::SeqCst), 0);
        assert_eq!(hits_d.hits.load(Ordering::SeqCst), 1);

        seds_relay_free(relay);
    }
}
