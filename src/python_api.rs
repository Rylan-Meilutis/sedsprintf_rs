//! Python bindings for the SEDS telemetry router.
//!
//! Exposes a small, opinionated API to Python via `pyo3`:
//!
//! - `Packet`
//!   - Immutable view of a `Packet`
//!   - Header fields + payload access
//!   - Serialization to bytes
//!
//! - `Router`
//!   - Logging (bytes, f32, generic typed)
//!   - RX/TX queue processing (with optional timeouts)
//!   - Python callbacks for TX, packet handlers, and serialized handlers
//!   - New link-aware APIs (`*_from`, RX queueing variants)
//!
//! - `Relay`
//!   - unchanged (fans out between sides)
//!
//! - Top-level helpers
//!   - `deserialize_packet_py(data)` → `Packet`
//!   - `peek_header_py(data)` → dict with header fields
//!   - `make_packet(...)` → `Packet`
//!
//! - Dynamic enums
//!   - `DataType`, `DataEndpoint`, `ElemKind`
//!
//! The Python module name is `sedsprintf_rs`.

use alloc::{boxed::Box, string::String, sync::Arc as AArc, vec::Vec};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyModule, PyTuple};
use std::sync::{Arc as SArc, Mutex, OnceLock};

#[cfg(feature = "timesync")]
use crate::timesync::TimeSyncConfig;
use crate::{
    config::{
        data_type_definition, data_type_definition_by_name, data_type_exists, endpoint_definition,
        endpoint_definition_by_name, endpoint_exists, message_class_code, message_class_from_code,
        message_data_type_code, message_data_type_from_code, register_data_type_id_with_description,
        register_endpoint_id_with_description, register_schema_json_bytes,
        reliable_code, reliable_from_code, remove_data_type,
        remove_data_type_by_name, remove_endpoint, remove_endpoint_by_name, DataEndpoint,
        DataType,
    }, get_message_name, get_needed_message_size, message_meta,
    packet::Packet, relay::{Relay, RelaySideOptions}, router::{Clock, EndpointHandler, LeBytes, Router, RouterConfig, RouterSideOptions},
    serialize::{deserialize_packet, packet_wire_size, peek_envelope, serialize_packet},
    try_enum_from_i32, try_enum_from_u32, MessageElement,
    RouteSelectionMode,
    TelemetryError,
    TelemetryResult,
    MAX_VALUE_DATA_ENDPOINT,
    MAX_VALUE_DATA_TYPE, MAX_VALUE_ROUTE_SELECTION_MODE,
};

static GLOBAL_ROUTER_SINGLETON: OnceLock<SArc<Mutex<Router>>> = OnceLock::new();
type EndpointHandlerBundle = (Vec<EndpointHandler>, Vec<Py<PyAny>>, Vec<Py<PyAny>>);

// ===========================================================================
// Macros
// ===========================================================================

macro_rules! impl_py_vec_accessors {
    ($( $name:ident -> $ty:ty ; $doc:expr );+ $(;)?) => {
        #[pymethods]
        impl PyPacket {
            $(
                #[doc = $doc]
                fn $name(&self) -> PyResult<Vec<$ty>> {
                    self.inner
                        .$name()
                        .map(|v| v.to_vec())
                        .map_err(py_err_from)
                }
            )+
        }
    };
}

// ============================================================================
//  Shared helpers / constants
// ============================================================================

/// Element-kind tags for typed logging (mirrors C FFI).
const EK_UNSIGNED: u32 = 0;
const EK_SIGNED: u32 = 1;
const EK_FLOAT: u32 = 2;

/// Map `TelemetryError` → `PyErr` with a consistent prefix.
fn py_err_from(e: TelemetryError) -> PyErr {
    PyRuntimeError::new_err(format!("Telemetry error: {e:?}"))
}

/// Convert Python-side `int` → `DataType`, with range/validity checks.
fn dtype_from_u32(x: u32) -> TelemetryResult<DataType> {
    DataType::try_from_u32(x).ok_or(TelemetryError::InvalidType)
}

/// Convert Python-side `int` → `DataEndpoint`, with range/validity checks.
fn endpoint_from_u32(x: u32) -> TelemetryResult<DataEndpoint> {
    DataEndpoint::try_from_u32(x).ok_or(TelemetryError::Deserialize("bad endpoint"))
}

fn build_router_config(handlers: Vec<EndpointHandler>, timesync_enabled: bool) -> RouterConfig {
    let cfg = RouterConfig::new(handlers);
    #[cfg(feature = "timesync")]
    let cfg = if timesync_enabled {
        cfg.with_timesync(TimeSyncConfig::default())
    } else {
        cfg
    };
    cfg
}

fn build_router_with_optional_clock(
    py: Python<'_>,
    cfg: RouterConfig,
    now_keep: Option<&Py<PyAny>>,
) -> Router {
    if let Some(cb) = now_keep {
        Router::new_with_clock(
            cfg,
            Box::new(PyClock {
                cb: Some(cb.clone_ref(py)),
            }),
        )
    } else {
        Router::new(cfg)
    }
}

fn build_endpoint_handlers(
    py: Python<'_>,
    handlers: Option<&Bound<'_, PyAny>>,
) -> PyResult<EndpointHandlerBundle> {
    let mut handlers_vec = Vec::new();
    let mut keep_pkt = Vec::new();
    let mut keep_ser = Vec::new();

    if let Some(hs) = handlers {
        let list = hs.cast::<PyList>().map_err(|_| {
            PyValueError::new_err("handlers must be list of (endpoint, pkt_cb, ser_cb) tuples")
        })?;

        for item in list.iter() {
            let tup = item
                .cast::<PyTuple>()
                .map_err(|_| PyValueError::new_err("handler must be a 3-tuple"))?;
            if tup.len() != 3 {
                return Err(PyValueError::new_err("tuple arity must be 3"));
            }

            let ep_u32: u32 = tup.get_item(0)?.extract()?;
            let endpoint = DataEndpoint(ep_u32);

            if !tup.get_item(1)?.is_none() {
                let cb: Py<PyAny> = tup.get_item(1)?.extract()?;
                let cb_for_closure = cb.clone_ref(py);
                keep_pkt.push(cb);

                let eh = EndpointHandler::new_packet_handler(endpoint, move |pkt| {
                    Python::attach(|py| {
                        let py_pkt = PyPacket { inner: pkt.clone() };
                        let any = Py::new(py, py_pkt)
                            .map_err(|_| TelemetryError::Io("packet wrapper"))?
                            .into_any();
                        let any = any.bind(py);

                        match cb_for_closure.call1(py, (any,)) {
                            Ok(_cb) => Ok(()),
                            Err(err) => {
                                err.restore(py);
                                Err(TelemetryError::Io("packet handler error"))
                            }
                        }
                    })
                });

                handlers_vec.push(eh);
            }

            if !tup.get_item(2)?.is_none() {
                let cb: Py<PyAny> = tup.get_item(2)?.extract()?;
                let cb_for_closure = cb.clone_ref(py);
                keep_ser.push(cb);

                let eh = EndpointHandler::new_serialized_handler(endpoint, move |bytes| {
                    Python::attach(|py| {
                        let arg = PyBytes::new(py, bytes).into_any();
                        match cb_for_closure.call1(py, (arg,)) {
                            Ok(_cb) => Ok(()),
                            Err(err) => {
                                err.restore(py);
                                Err(TelemetryError::Io("serialized handler error"))
                            }
                        }
                    })
                });

                handlers_vec.push(eh);
            }
        }
    }

    Ok((handlers_vec, keep_pkt, keep_ser))
}

/// Return the fixed payload size in bytes for a type, or `None` if dynamic.
fn required_payload_size_for(ty: DataType) -> Option<usize> {
    match message_meta(ty).element {
        MessageElement::Static(_, _, _) => Some(get_needed_message_size(ty)),
        MessageElement::Dynamic(_, _) => None,
    }
}

fn normalize_u8_payload_for_type(ty: DataType, buf: &mut Vec<u8>) {
    if let Some(required) = required_payload_size_for(ty) {
        if buf.len() < required {
            buf.resize(required, 0u8);
        } else if buf.len() > required {
            buf.truncate(required);
        }
    }
}

fn normalize_f32_payload_for_type(ty: DataType, vals: &mut Vec<f32>) -> PyResult<()> {
    if let Some(required_bytes) = required_payload_size_for(ty) {
        if required_bytes % 4 != 0 {
            return Err(py_err_from(TelemetryError::BadArg));
        }
        let need = required_bytes / 4;
        if vals.len() < need {
            vals.resize(need, 0.0);
        } else if vals.len() > need {
            vals.truncate(need);
        }
    }
    Ok(())
}

fn with_router_lock<T>(
    inner: &SArc<Mutex<Router>>,
    f: impl FnOnce(&Router) -> TelemetryResult<T>,
) -> PyResult<T> {
    let rtr = inner
        .lock()
        .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
    f(&rtr).map_err(py_err_from)
}

fn dispatch_log_slice<T: LeBytes + Copy>(
    router: &Router,
    ty: DataType,
    timestamp_ms: Option<u64>,
    queue: bool,
    values: &[T],
) -> TelemetryResult<()> {
    if queue {
        match timestamp_ms {
            Some(ts) => router.log_queue_ts::<T>(ty, ts, values),
            None => router.log_queue::<T>(ty, values),
        }
    } else {
        match timestamp_ms {
            Some(ts) => router.log_ts::<T>(ty, ts, values),
            None => router.log::<T>(ty, values),
        }
    }
}

/// Reinterpret a byte buffer as a Vec<T> using unaligned little-endian reads,
/// writing into `out` without realloc churn. Panic-safe w.r.t. `set_len`.
fn vectorize_data<T: LeBytes + Copy>(
    base: *const u8,
    count: usize,
    elem_size: usize,
    out: &mut Vec<T>,
) -> Result<(), ()> {
    use core::{mem::size_of, ptr};

    if elem_size != size_of::<T>() || base.is_null() || count == 0 {
        return Err(());
    }

    out.reserve_exact(count);
    unsafe {
        let start_len = out.len();
        let dst = out.as_mut_ptr().add(start_len);
        for i in 0..count {
            let v = ptr::read_unaligned(base.add(i * elem_size) as *const T);
            dst.add(i).write(v);
        }
        out.set_len(start_len + count);
    }
    Ok(())
}

#[cfg(feature = "discovery")]
fn topology_snapshot_to_pydict(
    py: Python<'_>,
    snap: crate::discovery::TopologySnapshot,
) -> PyResult<Py<PyDict>> {
    let out = PyDict::new(py);
    out.set_item(
        "advertised_endpoints",
        snap.advertised_endpoints
            .iter()
            .map(|ep| ep.as_u32())
            .collect::<Vec<u32>>(),
    )?;
    out.set_item(
        "advertised_timesync_sources",
        snap.advertised_timesync_sources,
    )?;

    let routers = PyList::empty(py);
    for board in snap.routers {
        let item = PyDict::new(py);
        item.set_item("sender_id", board.sender_id)?;
        item.set_item(
            "reachable_endpoints",
            board
                .reachable_endpoints
                .iter()
                .map(|ep| ep.as_u32())
                .collect::<Vec<u32>>(),
        )?;
        item.set_item(
            "reachable_timesync_sources",
            board.reachable_timesync_sources,
        )?;
        item.set_item("connections", board.connections)?;
        routers.append(item)?;
    }
    out.set_item("routers", routers)?;

    let routes = PyList::empty(py);
    for route in snap.routes {
        let route_dict = PyDict::new(py);
        route_dict.set_item("side_id", route.side_id)?;
        route_dict.set_item("side_name", route.side_name)?;
        route_dict.set_item(
            "reachable_endpoints",
            route
                .reachable_endpoints
                .iter()
                .map(|ep| ep.as_u32())
                .collect::<Vec<u32>>(),
        )?;
        route_dict.set_item(
            "reachable_timesync_sources",
            route.reachable_timesync_sources,
        )?;
        let announcers = PyList::empty(py);
        for announcer in route.announcers {
            let announcer_dict = PyDict::new(py);
            announcer_dict.set_item("sender_id", announcer.sender_id)?;
            announcer_dict.set_item(
                "reachable_endpoints",
                announcer
                    .reachable_endpoints
                    .iter()
                    .map(|ep| ep.as_u32())
                    .collect::<Vec<u32>>(),
            )?;
            announcer_dict.set_item(
                "reachable_timesync_sources",
                announcer.reachable_timesync_sources,
            )?;
            let announcer_routers = PyList::empty(py);
            for board in announcer.routers {
                let board_dict = PyDict::new(py);
                board_dict.set_item("sender_id", board.sender_id)?;
                board_dict.set_item(
                    "reachable_endpoints",
                    board
                        .reachable_endpoints
                        .iter()
                        .map(|ep| ep.as_u32())
                        .collect::<Vec<u32>>(),
                )?;
                board_dict.set_item(
                    "reachable_timesync_sources",
                    board.reachable_timesync_sources,
                )?;
                board_dict.set_item("connections", board.connections)?;
                announcer_routers.append(board_dict)?;
            }
            announcer_dict.set_item("routers", announcer_routers)?;
            announcer_dict.set_item("last_seen_ms", announcer.last_seen_ms)?;
            announcer_dict.set_item("age_ms", announcer.age_ms)?;
            announcers.append(announcer_dict)?;
        }
        route_dict.set_item("announcers", announcers)?;
        route_dict.set_item("last_seen_ms", route.last_seen_ms)?;
        route_dict.set_item("age_ms", route.age_ms)?;
        routes.append(route_dict)?;
    }
    out.set_item("routes", routes)?;
    out.set_item(
        "current_announce_interval_ms",
        snap.current_announce_interval_ms,
    )?;
    out.set_item("next_announce_ms", snap.next_announce_ms)?;
    Ok(out.unbind())
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
fn runtime_stats_snapshot_to_pydict(
    py: Python<'_>,
    snap: crate::diagnostics::RuntimeStatsSnapshot,
) -> PyResult<Py<PyDict>> {
    let out = PyDict::new(py);

    let sides = PyList::empty(py);
    for side in snap.sides {
        let side_dict = PyDict::new(py);
        side_dict.set_item("side_id", side.side_id)?;
        side_dict.set_item("side_name", side.side_name)?;
        side_dict.set_item("reliable_enabled", side.reliable_enabled)?;
        side_dict.set_item("link_local_enabled", side.link_local_enabled)?;
        side_dict.set_item("ingress_enabled", side.ingress_enabled)?;
        side_dict.set_item("egress_enabled", side.egress_enabled)?;
        side_dict.set_item("tx_packets", side.tx_packets)?;
        side_dict.set_item("tx_bytes", side.tx_bytes)?;
        side_dict.set_item("rx_packets", side.rx_packets)?;
        side_dict.set_item("rx_bytes", side.rx_bytes)?;
        side_dict.set_item("relayed_tx_packets", side.relayed_tx_packets)?;
        side_dict.set_item("relayed_tx_bytes", side.relayed_tx_bytes)?;
        side_dict.set_item("relayed_rx_packets", side.relayed_rx_packets)?;
        side_dict.set_item("relayed_rx_bytes", side.relayed_rx_bytes)?;
        side_dict.set_item("local_delivery_packets", side.local_delivery_packets)?;
        side_dict.set_item("tx_retries", side.tx_retries)?;
        side_dict.set_item("tx_handler_failures", side.tx_handler_failures)?;
        side_dict.set_item("local_handler_failures", side.local_handler_failures)?;
        side_dict.set_item("total_handler_retries", side.total_handler_retries)?;

        let adaptive = PyDict::new(py);
        adaptive.set_item(
            "auto_balancing_enabled",
            side.adaptive.auto_balancing_enabled,
        )?;
        adaptive.set_item(
            "estimated_capacity_bps",
            side.adaptive.estimated_capacity_bps,
        )?;
        adaptive.set_item("peak_capacity_bps", side.adaptive.peak_capacity_bps)?;
        adaptive.set_item("current_usage_bps", side.adaptive.current_usage_bps)?;
        adaptive.set_item("peak_usage_bps", side.adaptive.peak_usage_bps)?;
        adaptive.set_item(
            "available_headroom_bps",
            side.adaptive.available_headroom_bps,
        )?;
        adaptive.set_item("effective_weight", side.adaptive.effective_weight)?;
        adaptive.set_item("last_observed_ms", side.adaptive.last_observed_ms)?;
        adaptive.set_item("sample_count", side.adaptive.sample_count)?;
        side_dict.set_item("adaptive", adaptive)?;

        let data_types = PyList::empty(py);
        for data_type in side.data_types {
            let item = PyDict::new(py);
            item.set_item("data_type", data_type.data_type.as_u32())?;
            item.set_item("tx_packets", data_type.tx_packets)?;
            item.set_item("tx_bytes", data_type.tx_bytes)?;
            item.set_item("rx_packets", data_type.rx_packets)?;
            item.set_item("rx_bytes", data_type.rx_bytes)?;
            item.set_item("relayed_tx_packets", data_type.relayed_tx_packets)?;
            item.set_item("relayed_tx_bytes", data_type.relayed_tx_bytes)?;
            item.set_item("relayed_rx_packets", data_type.relayed_rx_packets)?;
            item.set_item("relayed_rx_bytes", data_type.relayed_rx_bytes)?;
            item.set_item("tx_retries", data_type.tx_retries)?;
            item.set_item("handler_failures", data_type.handler_failures)?;
            data_types.append(item)?;
        }
        side_dict.set_item("data_types", data_types)?;
        sides.append(side_dict)?;
    }
    out.set_item("sides", sides)?;

    let route_modes = PyList::empty(py);
    for mode in snap.route_modes {
        let item = PyDict::new(py);
        item.set_item("src_side_id", mode.src_side_id)?;
        item.set_item(
            "selection_mode",
            mode.selection_mode.map(route_selection_mode_name),
        )?;
        item.set_item("cursor", mode.cursor)?;
        route_modes.append(item)?;
    }
    out.set_item("route_modes", route_modes)?;

    let route_overrides = PyList::empty(py);
    for route in snap.route_overrides {
        let item = PyDict::new(py);
        item.set_item("src_side_id", route.src_side_id)?;
        item.set_item("dst_side_id", route.dst_side_id)?;
        item.set_item("enabled", route.enabled)?;
        route_overrides.append(item)?;
    }
    out.set_item("route_overrides", route_overrides)?;

    let typed_route_overrides = PyList::empty(py);
    for route in snap.typed_route_overrides {
        let item = PyDict::new(py);
        item.set_item("src_side_id", route.src_side_id)?;
        item.set_item("data_type", route.data_type.as_u32())?;
        item.set_item("dst_side_id", route.dst_side_id)?;
        item.set_item("enabled", route.enabled)?;
        typed_route_overrides.append(item)?;
    }
    out.set_item("typed_route_overrides", typed_route_overrides)?;

    let route_weights = PyList::empty(py);
    for weight in snap.route_weights {
        let item = PyDict::new(py);
        item.set_item("src_side_id", weight.src_side_id)?;
        item.set_item("dst_side_id", weight.dst_side_id)?;
        item.set_item("weight", weight.weight)?;
        route_weights.append(item)?;
    }
    out.set_item("route_weights", route_weights)?;

    let route_priorities = PyList::empty(py);
    for priority in snap.route_priorities {
        let item = PyDict::new(py);
        item.set_item("src_side_id", priority.src_side_id)?;
        item.set_item("dst_side_id", priority.dst_side_id)?;
        item.set_item("priority", priority.priority)?;
        route_priorities.append(item)?;
    }
    out.set_item("route_priorities", route_priorities)?;

    let queues = PyDict::new(py);
    queues.set_item("rx_len", snap.queues.rx_len)?;
    queues.set_item("rx_bytes", snap.queues.rx_bytes)?;
    queues.set_item("tx_len", snap.queues.tx_len)?;
    queues.set_item("tx_bytes", snap.queues.tx_bytes)?;
    queues.set_item("replay_len", snap.queues.replay_len)?;
    queues.set_item("replay_bytes", snap.queues.replay_bytes)?;
    queues.set_item("recent_rx_len", snap.queues.recent_rx_len)?;
    queues.set_item("recent_rx_bytes", snap.queues.recent_rx_bytes)?;
    queues.set_item(
        "reliable_rx_buffered_len",
        snap.queues.reliable_rx_buffered_len,
    )?;
    queues.set_item(
        "reliable_rx_buffered_bytes",
        snap.queues.reliable_rx_buffered_bytes,
    )?;
    queues.set_item(
        "shared_queue_bytes_used",
        snap.queues.shared_queue_bytes_used,
    )?;
    out.set_item("queues", queues)?;

    let reliable = PyDict::new(py);
    reliable.set_item(
        "reliable_return_route_count",
        snap.reliable.reliable_return_route_count,
    )?;
    reliable.set_item(
        "end_to_end_pending_count",
        snap.reliable.end_to_end_pending_count,
    )?;
    reliable.set_item(
        "end_to_end_pending_destination_count",
        snap.reliable.end_to_end_pending_destination_count,
    )?;
    reliable.set_item(
        "end_to_end_acked_cache_count",
        snap.reliable.end_to_end_acked_cache_count,
    )?;
    out.set_item("reliable", reliable)?;

    let discovery = PyDict::new(py);
    discovery.set_item("route_count", snap.discovery.route_count)?;
    discovery.set_item("announcer_count", snap.discovery.announcer_count)?;
    discovery.set_item(
        "current_announce_interval_ms",
        snap.discovery.current_announce_interval_ms,
    )?;
    discovery.set_item("next_announce_ms", snap.discovery.next_announce_ms)?;
    out.set_item("discovery", discovery)?;

    out.set_item("total_handler_failures", snap.total_handler_failures)?;
    out.set_item("total_handler_retries", snap.total_handler_retries)?;
    Ok(out.unbind())
}

// ============================================================================
//  Packet (PyPacket)
// ============================================================================

/// Python-visible wrapper around `Packet`.
///
/// Constructed indirectly via:
/// - `deserialize_packet_py`
/// - `make_packet`
/// - callbacks (router handlers)
#[pyclass(name = "Packet")]
pub struct PyPacket {
    pub(crate) inner: Packet,
}

impl_py_vec_accessors! {
    data_as_u8   -> u8;   "Decode payload as `Vec<u8>` for unsigned byte arrays.";
    data_as_u16  -> u16;  "Decode payload as `Vec<u16>` for unsigned 16-bit arrays.";
    data_as_u32  -> u32;  "Decode payload as `Vec<u32>` for unsigned 32-bit arrays.";
    data_as_u64  -> u64;  "Decode payload as `Vec<u64>` for unsigned 64-bit arrays.";

    data_as_i8   -> i8;   "Decode payload as `Vec<i8>` for signed 8-bit arrays.";
    data_as_i16  -> i16;  "Decode payload as `Vec<i16>` for signed 16-bit arrays.";
    data_as_i32  -> i32;  "Decode payload as `Vec<i32>` for signed 32-bit arrays.";
    data_as_i64  -> i64;  "Decode payload as `Vec<i64>` for signed 64-bit arrays.";

    data_as_f32  -> f32;  "Decode payload as `Vec<f32>` for float32 arrays.";
    data_as_f64  -> f64;  "Decode payload as `Vec<f64>` for float64 arrays.";

    data_as_bool -> bool; "Decode payload as `Vec<bool>` for boolean arrays.";
}

#[pymethods]
impl PyPacket {
    /// DataType as an integer (see `DataType` IntEnum).
    #[getter]
    fn ty(&self) -> u32 {
        self.inner.data_type().as_u32()
    }

    /// Declared data size for the packet payload, in bytes.
    #[getter]
    fn data_size(&self) -> usize {
        self.inner.data_size()
    }

    /// Sender identifier as a UTF-8 string.
    #[getter]
    fn sender(&self) -> String {
        self.inner.sender().to_string()
    }

    /// Endpoints as integer IDs (see `DataEndpoint` IntEnum).
    #[getter]
    fn endpoints(&self) -> Vec<u32> {
        self.inner.endpoints().iter().map(|e| e.as_u32()).collect()
    }

    /// Packet timestamp in milliseconds (source-defined semantics).
    #[getter]
    fn timestamp_ms(&self) -> u64 {
        self.inner.timestamp()
    }

    /// Raw payload bytes.
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.inner.payload())
    }

    /// Decode payload as a UTF-8 string for `DataType::String`.
    fn data_as_string(&self) -> PyResult<String> {
        self.inner.data_as_string().map_err(py_err_from)
    }

    /// Human-readable header string (no payload).
    fn header_string(&self) -> String {
        self.inner.header_string()
    }

    /// Full human-readable representation (header + payload summary).
    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    /// Wire size in bytes when serialized.
    fn wire_size(&self) -> usize {
        packet_wire_size(&self.inner)
    }

    /// Serialize to wire format bytes.
    fn serialize<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let bytes = serialize_packet(&self.inner);
        Ok(PyBytes::new(py, &bytes))
    }
}

// ============================================================================
//  Clock (PyClock)
// ============================================================================

/// Clock implementation that delegates to an optional Python callback.
///
/// The callback should be a zero-arg function returning an `int`
/// representing milliseconds.
struct PyClock {
    cb: Option<Py<PyAny>>,
}

impl Clock for PyClock {
    fn now_ms(&self) -> u64 {
        if let Some(ref cb) = self.cb {
            Python::attach(|py| match cb.call0(py) {
                Ok(v) => v.extract::<u64>(py).unwrap_or(0),
                Err(_e) => 0,
            })
        } else {
            0
        }
    }
}

// ============================================================================
//  Router (PyRouter)
// ============================================================================

/// Python-visible router wrapper.
///
/// The underlying `Router` is protected by a `Mutex` for host-side
/// concurrency. Python callbacks for TX, packet handlers, and
/// serialized handlers are kept alive in this object.
#[pyclass(name = "Router")]
pub struct PyRouter {
    // Host-side concurrency: protect the Router with a Mutex and share via Arc.
    inner: SArc<Mutex<Router>>,
    _pkt_cbs: Vec<Py<PyAny>>,
    _ser_cbs: Vec<Py<PyAny>>,
    _side_cbs: Vec<Option<Py<PyAny>>>,
}

#[pymethods]
impl PyRouter {
    // ------------------------------------------------------------------------
    //  Singleton construction
    // ------------------------------------------------------------------------

    /// Create or retrieve a per-process singleton Router.
    #[staticmethod]
    #[pyo3(signature = (now_ms=None, handlers=None, timesync_enabled = true))]
    fn new_singleton(
        py: Python<'_>,
        now_ms: Option<Py<PyAny>>,
        handlers: Option<&Bound<'_, PyAny>>,
        timesync_enabled: bool,
    ) -> PyResult<Self> {
        if let Some(existing) = GLOBAL_ROUTER_SINGLETON.get() {
            if now_ms.is_some() || handlers.is_some() || !timesync_enabled {
                return Err(PyRuntimeError::new_err(
                    "Router singleton already exists; cannot modify now_ms/handlers/timesync_enabled",
                ));
            }

            return Ok(PyRouter {
                inner: existing.clone(),
                _pkt_cbs: Vec::new(),
                _ser_cbs: Vec::new(),
                _side_cbs: Vec::new(),
            });
        }

        // Copy callbacks to keep them alive
        let now_keep = now_ms.as_ref().map(|p| p.clone_ref(py));

        let (handlers_vec, keep_pkt, keep_ser) = build_endpoint_handlers(py, handlers)?;
        let cfg = build_router_config(handlers_vec, timesync_enabled);
        let router = build_router_with_optional_clock(py, cfg, now_keep.as_ref());
        let arc = SArc::new(Mutex::new(router));

        GLOBAL_ROUTER_SINGLETON
            .set(arc.clone())
            .map_err(|_existing| PyRuntimeError::new_err("Router singleton already exists"))?;

        Ok(PyRouter {
            inner: arc,
            _pkt_cbs: keep_pkt,
            _ser_cbs: keep_ser,
            _side_cbs: Vec::new(),
        })
    }

    /// Create a new router.
    #[new]
    #[pyo3(signature = (now_ms=None, handlers=None, timesync_enabled=true))]
    fn new(
        py: Python<'_>,
        now_ms: Option<Py<PyAny>>,
        handlers: Option<&Bound<'_, PyAny>>,
        timesync_enabled: bool,
    ) -> PyResult<Self> {
        let now_keep = now_ms.as_ref().map(|p| p.clone_ref(py));

        let (handlers_vec, keep_pkt, keep_ser) = build_endpoint_handlers(py, handlers)?;
        let cfg = build_router_config(handlers_vec, timesync_enabled);
        let router = build_router_with_optional_clock(py, cfg, now_keep.as_ref());

        Ok(Self {
            inner: SArc::new(Mutex::new(router)),
            _pkt_cbs: keep_pkt,
            _ser_cbs: keep_ser,
            _side_cbs: Vec::new(),
        })
    }

    // ------------------------------------------------------------------------
    //  Side registration
    // ------------------------------------------------------------------------

    #[pyo3(signature = (name, tx, reliable_enabled=false))]
    fn add_side_serialized(
        &mut self,
        py: Python<'_>,
        name: &str,
        tx: Py<PyAny>,
        reliable_enabled: bool,
    ) -> PyResult<u32> {
        let cb_keep = tx.clone_ref(py);
        let cb_for_closure = cb_keep.clone_ref(py);

        let name_static: &'static str = Box::leak(name.to_owned().into_boxed_str());

        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;

        let opts = RouterSideOptions {
            reliable_enabled,
            link_local_enabled: false,
            ..RouterSideOptions::default()
        };

        let id = rtr.add_side_serialized_with_options(
            name_static,
            move |bytes| {
                Python::attach(|py| {
                    let arg = PyBytes::new(py, bytes);
                    match cb_for_closure.call1(py, (&arg,)) {
                        Ok(_) => Ok(()),
                        Err(err) => {
                            err.restore(py);
                            Err(TelemetryError::Io("router side tx error"))
                        }
                    }
                })
            },
            opts,
        );

        if self._side_cbs.len() <= id {
            self._side_cbs.resize_with(id + 1, || None);
        }
        self._side_cbs[id] = Some(cb_keep);

        Ok(id as u32)
    }

    #[pyo3(signature = (name, tx, reliable_enabled=false))]
    fn add_side_packet(
        &mut self,
        py: Python<'_>,
        name: &str,
        tx: Py<PyAny>,
        reliable_enabled: bool,
    ) -> PyResult<u32> {
        let cb_keep = tx.clone_ref(py);
        let cb_for_closure = cb_keep.clone_ref(py);

        let name_static: &'static str = Box::leak(name.to_owned().into_boxed_str());

        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;

        let opts = RouterSideOptions {
            reliable_enabled,
            link_local_enabled: false,
            ..RouterSideOptions::default()
        };

        let id = rtr.add_side_packet_with_options(
            name_static,
            move |pkt: &Packet| {
                Python::attach(|py| {
                    let py_pkt = PyPacket { inner: pkt.clone() };
                    let any = Py::new(py, py_pkt)
                        .map_err(|_| TelemetryError::Io("router packet wrapper"))?;
                    match cb_for_closure.call1(py, (&any,)) {
                        Ok(_) => Ok(()),
                        Err(err) => {
                            err.restore(py);
                            Err(TelemetryError::Io("router packet tx error"))
                        }
                    }
                })
            },
            opts,
        );

        if self._side_cbs.len() <= id {
            self._side_cbs.resize_with(id + 1, || None);
        }
        self._side_cbs[id] = Some(cb_keep);

        Ok(id as u32)
    }

    fn remove_side(&mut self, side_id: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.remove_side(side_id as usize).map_err(py_err_from)?;
        if let Some(slot) = self._side_cbs.get_mut(side_id as usize) {
            *slot = None;
        }
        Ok(())
    }

    fn set_side_ingress_enabled(&self, side_id: u32, enabled: bool) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_side_ingress_enabled(side_id as usize, enabled)
            .map_err(py_err_from)
    }

    fn set_side_egress_enabled(&self, side_id: u32, enabled: bool) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_side_egress_enabled(side_id as usize, enabled)
            .map_err(py_err_from)
    }

    fn set_route(&self, src_side_id: Option<u32>, dst_side_id: u32, enabled: bool) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_route(
            src_side_id.map(|id| id as usize),
            dst_side_id as usize,
            enabled,
        )
        .map_err(py_err_from)
    }

    fn clear_route(&self, src_side_id: Option<u32>, dst_side_id: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.clear_route(src_side_id.map(|id| id as usize), dst_side_id as usize)
            .map_err(py_err_from)
    }

    fn set_typed_route(
        &self,
        src_side_id: Option<u32>,
        ty: u32,
        dst_side_id: u32,
        enabled: bool,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_typed_route(
            src_side_id.map(|id| id as usize),
            dtype_from_u32(ty).map_err(py_err_from)?,
            dst_side_id as usize,
            enabled,
        )
        .map_err(py_err_from)
    }

    fn clear_typed_route(
        &self,
        src_side_id: Option<u32>,
        ty: u32,
        dst_side_id: u32,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.clear_typed_route(
            src_side_id.map(|id| id as usize),
            dtype_from_u32(ty).map_err(py_err_from)?,
            dst_side_id as usize,
        )
        .map_err(py_err_from)
    }

    fn set_source_route_mode(&self, src_side_id: Option<u32>, mode: i32) -> PyResult<()> {
        let mode = match mode {
            0 => RouteSelectionMode::Fanout,
            1 => RouteSelectionMode::Weighted,
            2 => RouteSelectionMode::Failover,
            _ => return Err(PyValueError::new_err("invalid route selection mode")),
        };
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_source_route_mode(src_side_id.map(|id| id as usize), mode)
            .map_err(py_err_from)
    }

    fn clear_source_route_mode(&self, src_side_id: Option<u32>) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.clear_source_route_mode(src_side_id.map(|id| id as usize))
            .map_err(py_err_from)
    }

    fn set_route_weight(
        &self,
        src_side_id: Option<u32>,
        dst_side_id: u32,
        weight: u32,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_route_weight(
            src_side_id.map(|id| id as usize),
            dst_side_id as usize,
            weight,
        )
        .map_err(py_err_from)
    }

    fn clear_route_weight(&self, src_side_id: Option<u32>, dst_side_id: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.clear_route_weight(src_side_id.map(|id| id as usize), dst_side_id as usize)
            .map_err(py_err_from)
    }

    fn set_route_priority(
        &self,
        src_side_id: Option<u32>,
        dst_side_id: u32,
        priority: u32,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_route_priority(
            src_side_id.map(|id| id as usize),
            dst_side_id as usize,
            priority,
        )
        .map_err(py_err_from)
    }

    fn clear_route_priority(&self, src_side_id: Option<u32>, dst_side_id: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.clear_route_priority(src_side_id.map(|id| id as usize), dst_side_id as usize)
            .map_err(py_err_from)
    }

    // ------------------------------------------------------------------------
    //  TX / RX (broadcast)
    // ------------------------------------------------------------------------

    fn transmit_message(&self, _py: Python<'_>, packet: &Bound<'_, PyAny>) -> PyResult<()> {
        let pkt_ref: PyRef<PyPacket> = packet.extract()?;

        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;

        rtr.tx(pkt_ref.inner.clone()).map_err(py_err_from)
    }

    fn transmit_message_queue(&self, _py: Python<'_>, packet: &Bound<'_, PyAny>) -> PyResult<()> {
        let pkt_ref: PyRef<PyPacket> = packet.extract()?;

        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;

        rtr.tx_queue(pkt_ref.inner.clone()).map_err(py_err_from)
    }

    fn transmit_serialized_message(
        &self,
        _py: Python<'_>,
        data: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let bytes: &[u8] = data.extract()?;
        let arc: AArc<[u8]> = AArc::from(bytes);

        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;

        rtr.tx_serialized(arc).map_err(py_err_from)
    }

    fn transmit_serialized_message_queue(
        &self,
        _py: Python<'_>,
        data: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let bytes: &[u8] = data.extract()?;
        let arc: AArc<[u8]> = AArc::from(bytes);

        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;

        rtr.tx_serialized_queue(arc).map_err(py_err_from)
    }

    fn receive_serialized(&self, _py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let bytes: &[u8] = data.extract()?;
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.rx_serialized(bytes).map_err(py_err_from)
    }

    fn receive_packet(&self, _py: Python<'_>, packet: &Bound<'_, PyAny>) -> PyResult<()> {
        let pkt_ref: PyRef<PyPacket> = packet.extract()?;
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.rx(&pkt_ref.inner).map_err(py_err_from)
    }

    // ------------------------------------------------------------------------
    //  Side-aware RX (explicit ingress side)
    // ------------------------------------------------------------------------

    fn receive_serialized_from_side(
        &self,
        _py: Python<'_>,
        side_id: u32,
        data: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let bytes: &[u8] = data.extract()?;
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.rx_serialized_from_side(bytes, side_id as usize)
            .map_err(py_err_from)
    }

    fn receive_serialized_queue(&self, _py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let bytes: &[u8] = data.extract()?;
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.rx_serialized_queue(bytes).map_err(py_err_from)
    }

    fn receive_packet_queue(&self, _py: Python<'_>, packet: &Bound<'_, PyAny>) -> PyResult<()> {
        let pkt_ref: PyRef<PyPacket> = packet.extract()?;
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.rx_queue(pkt_ref.inner.clone()).map_err(py_err_from)
    }

    fn receive_serialized_queue_from_side(
        &self,
        _py: Python<'_>,
        side_id: u32,
        data: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let bytes: &[u8] = data.extract()?;
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.rx_serialized_queue_from_side(bytes, side_id as usize)
            .map_err(py_err_from)
    }

    fn receive_packet_from_side(
        &self,
        _py: Python<'_>,
        side_id: u32,
        packet: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let pkt_ref: PyRef<PyPacket> = packet.extract()?;
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.rx_from_side(&pkt_ref.inner, side_id as usize)
            .map_err(py_err_from)
    }

    fn receive_packet_queue_from_side(
        &self,
        _py: Python<'_>,
        side_id: u32,
        packet: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let pkt_ref: PyRef<PyPacket> = packet.extract()?;
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.rx_queue_from_side(pkt_ref.inner.clone(), side_id as usize)
            .map_err(py_err_from)
    }

    // ------------------------------------------------------------------------
    //  Logging
    // ------------------------------------------------------------------------

    #[pyo3(signature = (ty, data, timestamp_ms=None, queue=false))]
    fn log_bytes(
        &self,
        _py: Python<'_>,
        ty: u32,
        data: &Bound<'_, PyAny>,
        timestamp_ms: Option<u64>,
        queue: bool,
    ) -> PyResult<()> {
        let ty = dtype_from_u32(ty).map_err(py_err_from)?;
        let mut buf: Vec<u8> = data.extract::<&[u8]>()?.to_vec();

        normalize_u8_payload_for_type(ty, &mut buf);
        with_router_lock(&self.inner, |rtr| {
            dispatch_log_slice::<u8>(rtr, ty, timestamp_ms, queue, &buf)
        })
    }

    #[pyo3(signature = (ty, values, timestamp_ms=None, queue=false))]
    fn log_f32(
        &self,
        _py: Python<'_>,
        ty: u32,
        values: &Bound<'_, PyAny>,
        timestamp_ms: Option<u64>,
        queue: bool,
    ) -> PyResult<()> {
        let ty = dtype_from_u32(ty).map_err(py_err_from)?;
        let mut vals: Vec<f32> = values.extract()?;

        normalize_f32_payload_for_type(ty, &mut vals)?;
        with_router_lock(&self.inner, |rtr| {
            dispatch_log_slice::<f32>(rtr, ty, timestamp_ms, queue, &vals)
        })
    }

    #[allow(clippy::too_many_arguments)] // PyO3 method: `self` and `py` are not Python-visible
    #[pyo3(signature = (ty, data, elem_size, elem_kind, timestamp_ms=None, queue=false))]
    fn log(
        &self,
        py: Python<'_>,
        ty: u32,
        data: &Bound<'_, PyAny>,
        elem_size: usize,
        elem_kind: u32,
        timestamp_ms: Option<u64>,
        queue: bool,
    ) -> PyResult<()> {
        if !(elem_size == 1 || elem_size == 2 || elem_size == 4 || elem_size == 8) {
            return Err(PyValueError::new_err("elem_size must be 1,2,4,8"));
        }
        let ty = dtype_from_u32(ty).map_err(py_err_from)?;

        let mut bytes: Vec<u8> = if let Ok(b) = data.extract::<&[u8]>() {
            b.to_vec()
        } else if let Ok(py_str) = data.cast::<pyo3::types::PyString>() {
            py_str.to_str()?.as_bytes().to_vec()
        } else {
            let builtins = PyModule::import(py, "builtins")?;
            match builtins.call_method1("bytes", (data.clone(),)) {
                Ok(pybytes) => pybytes.extract::<Vec<u8>>()?,
                Err(_) => {
                    let mv = builtins.getattr("memoryview")?.call1((data.clone(),))?;
                    let itemsize: usize = mv.getattr("itemsize")?.extract()?;
                    let mv_bytes = if itemsize != 1 {
                        mv.call_method1("cast", ("B",))?
                    } else {
                        mv
                    };
                    let pybytes = mv_bytes.call_method0("tobytes")?;
                    pybytes.extract::<Vec<u8>>()?
                }
            }
        };

        normalize_u8_payload_for_type(ty, &mut bytes);

        let ts = timestamp_ms;

        if elem_size == 1 && elem_kind == EK_UNSIGNED {
            return with_router_lock(&self.inner, |rtr| {
                dispatch_log_slice::<u8>(rtr, ty, ts, queue, &bytes)
            });
        }

        macro_rules! finish_with {
            ($T:ty) => {{
                let cnt = bytes.len() / elem_size;
                if cnt == 0 || bytes.len() % elem_size != 0 {
                    return Err(PyValueError::new_err(
                        "buffer length not divisible by elem_size",
                    ));
                }
                let mut v: Vec<$T> = Vec::with_capacity(cnt);
                vectorize_data::<$T>(bytes.as_ptr(), cnt, elem_size, &mut v)
                    .map_err(|_| PyValueError::new_err("vectorize failed"))?;
                with_router_lock(&self.inner, |rtr| {
                    dispatch_log_slice::<$T>(rtr, ty, ts, queue, &v)
                })
            }};
        }

        match (elem_kind, elem_size) {
            (EK_UNSIGNED, 2) => finish_with!(u16),
            (EK_UNSIGNED, 4) => finish_with!(u32),
            (EK_UNSIGNED, 8) => finish_with!(u64),

            (EK_SIGNED, 1) => finish_with!(i8),
            (EK_SIGNED, 2) => finish_with!(i16),
            (EK_SIGNED, 4) => finish_with!(i32),
            (EK_SIGNED, 8) => finish_with!(i64),

            (EK_FLOAT, 4) => finish_with!(f32),
            (EK_FLOAT, 8) => finish_with!(f64),

            _ => Err(PyValueError::new_err(
                "unsupported elem_kind/elem_size combination",
            )),
        }
    }

    #[cfg(feature = "timesync")]
    fn poll_timesync(&self) -> PyResult<bool> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.poll_timesync().map_err(py_err_from)
    }

    #[cfg(feature = "discovery")]
    fn announce_discovery(&self) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.announce_discovery().map_err(py_err_from)
    }

    #[cfg(feature = "discovery")]
    fn poll_discovery(&self) -> PyResult<bool> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.poll_discovery().map_err(py_err_from)
    }

    #[cfg(feature = "discovery")]
    fn export_topology(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        topology_snapshot_to_pydict(py, rtr.export_topology())
    }

    #[cfg(feature = "discovery")]
    fn export_runtime_stats(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        runtime_stats_snapshot_to_pydict(py, rtr.export_runtime_stats())
    }

    #[cfg(feature = "timesync")]
    fn network_time_ms(&self) -> PyResult<Option<u64>> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        Ok(rtr.network_time_ms())
    }

    #[cfg(feature = "timesync")]
    fn network_time(&self, py: Python<'_>) -> PyResult<Option<Py<PyDict>>> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        let Some(reading) = rtr.network_time() else {
            return Ok(None);
        };

        let out = PyDict::new(py);
        if let Some(v) = reading.unix_time_ms {
            out.set_item("unix_time_ms", v)?;
        }
        if let Some(v) = reading.time.year {
            out.set_item("year", v)?;
        }
        if let Some(v) = reading.time.month {
            out.set_item("month", v)?;
        }
        if let Some(v) = reading.time.day {
            out.set_item("day", v)?;
        }
        if let Some(v) = reading.time.hour {
            out.set_item("hour", v)?;
        }
        if let Some(v) = reading.time.minute {
            out.set_item("minute", v)?;
        }
        if let Some(v) = reading.time.second {
            out.set_item("second", v)?;
        }
        if let Some(v) = reading.time.nanosecond {
            out.set_item("nanosecond", v)?;
        }
        Ok(Some(out.unbind()))
    }

    #[cfg(feature = "timesync")]
    #[pyo3(signature = (
        year=None,
        month=None,
        day=None,
        hour=None,
        minute=None,
        second=None,
        nanosecond=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn set_local_network_time(
        &self,
        year: Option<i32>,
        month: Option<u8>,
        day: Option<u8>,
        hour: Option<u8>,
        minute: Option<u8>,
        second: Option<u8>,
        nanosecond: Option<u32>,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_local_network_time(crate::timesync::PartialNetworkTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
            nanosecond,
        });
        Ok(())
    }

    #[cfg(feature = "timesync")]
    fn set_local_network_date(&self, year: i32, month: u8, day: u8) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_local_network_date(year, month, day);
        Ok(())
    }

    #[cfg(feature = "timesync")]
    fn set_local_network_time_hm(&self, hour: u8, minute: u8) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_local_network_time_hm(hour, minute);
        Ok(())
    }

    #[cfg(feature = "timesync")]
    fn set_local_network_time_hms(&self, hour: u8, minute: u8, second: u8) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_local_network_time_hms(hour, minute, second);
        Ok(())
    }

    #[cfg(feature = "timesync")]
    fn set_local_network_time_hms_millis(
        &self,
        hour: u8,
        minute: u8,
        second: u8,
        millisecond: u16,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_local_network_time_hms_millis(hour, minute, second, millisecond);
        Ok(())
    }

    #[cfg(feature = "timesync")]
    fn set_local_network_time_hms_nanos(
        &self,
        hour: u8,
        minute: u8,
        second: u8,
        nanosecond: u32,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_local_network_time_hms_nanos(hour, minute, second, nanosecond);
        Ok(())
    }

    #[cfg(feature = "timesync")]
    fn set_local_network_datetime(
        &self,
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_local_network_datetime(year, month, day, hour, minute, second);
        Ok(())
    }

    #[cfg(feature = "timesync")]
    #[allow(clippy::too_many_arguments)]
    fn set_local_network_datetime_millis(
        &self,
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        millisecond: u16,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_local_network_datetime_millis(year, month, day, hour, minute, second, millisecond);
        Ok(())
    }

    #[cfg(feature = "timesync")]
    #[allow(clippy::too_many_arguments)]
    fn set_local_network_datetime_nanos(
        &self,
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        nanosecond: u32,
    ) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.set_local_network_datetime_nanos(year, month, day, hour, minute, second, nanosecond);
        Ok(())
    }

    // ------------------------------------------------------------------------
    //  Queue processing / clearing
    // ------------------------------------------------------------------------

    fn process_send_queue(&self) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_tx_queue().map_err(py_err_from)
    }

    fn process_received_queue(&self) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_rx_queue().map_err(py_err_from)
    }

    fn process_all_queues(&self) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_all_queues().map_err(py_err_from)
    }

    fn clear_rx_queue(&self) {
        if let Ok(r) = self.inner.lock() {
            r.clear_rx_queue();
        }
    }

    fn clear_tx_queue(&self) {
        if let Ok(r) = self.inner.lock() {
            r.clear_tx_queue();
        }
    }

    fn clear_queues(&self) {
        if let Ok(r) = self.inner.lock() {
            r.clear_queues();
        }
    }

    fn process_tx_queue_with_timeout(&self, timeout_ms: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_tx_queue_with_timeout(timeout_ms)
            .map_err(py_err_from)
    }

    fn process_rx_queue_with_timeout(&self, timeout_ms: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_rx_queue_with_timeout(timeout_ms)
            .map_err(py_err_from)
    }

    fn process_all_queues_with_timeout(&self, timeout_ms: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.process_all_queues_with_timeout(timeout_ms)
            .map_err(py_err_from)
    }

    fn periodic(&self, timeout_ms: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.periodic(timeout_ms).map_err(py_err_from)
    }

    fn periodic_no_timesync(&self, timeout_ms: u32) -> PyResult<()> {
        let rtr = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("router poisoned"))?;
        rtr.periodic_no_timesync(timeout_ms).map_err(py_err_from)
    }
}

// ============================================================================
//  Relay (PyRelay)
// ============================================================================

/// Python-visible relay wrapper.
///
/// The Relay:
///   - fans out serialized packets from one side to all others
///   - has RX/TX queues and time-budgeted processing
///   - uses the same Clock abstraction as Router.
#[pyclass(name = "Relay")]
pub struct PyRelay {
    inner: SArc<Relay>,
    _now_cb: Option<Py<PyAny>>,
    _tx_cbs: Vec<Option<Py<PyAny>>>,
}

#[pymethods]
impl PyRelay {
    #[new]
    #[pyo3(signature = (now_ms=None))]
    fn new(py: Python<'_>, now_ms: Option<Py<PyAny>>) -> PyResult<Self> {
        let now_keep = now_ms.as_ref().map(|p| p.clone_ref(py));

        let clock = PyClock {
            cb: now_keep.as_ref().map(|p| p.clone_ref(py)),
        };

        let relay = Relay::new(Box::new(clock));

        Ok(PyRelay {
            inner: SArc::new(relay),
            _now_cb: now_keep,
            _tx_cbs: Vec::new(),
        })
    }

    #[pyo3(signature = (name, tx, reliable_enabled=false))]
    fn add_side_serialized(
        &mut self,
        py: Python<'_>,
        name: &str,
        tx: Py<PyAny>,
        reliable_enabled: bool,
    ) -> PyResult<u32> {
        let cb_keep = tx.clone_ref(py);
        let cb_for_closure = cb_keep.clone_ref(py);

        let name_static: &'static str = Box::leak(name.to_owned().into_boxed_str());

        let opts = RelaySideOptions {
            reliable_enabled,
            link_local_enabled: false,
            ..RelaySideOptions::default()
        };

        let id = self.inner.add_side_serialized_with_options(
            name_static,
            move |bytes| {
                Python::attach(|py| {
                    let arg = PyBytes::new(py, bytes);
                    match cb_for_closure.call1(py, (&arg,)) {
                        Ok(_) => Ok(()),
                        Err(err) => {
                            err.restore(py);
                            Err(TelemetryError::Io("relay tx error"))
                        }
                    }
                })
            },
            opts,
        );

        if self._tx_cbs.len() <= id {
            self._tx_cbs.resize_with(id + 1, || None);
        }
        self._tx_cbs[id] = Some(cb_keep);

        Ok(id as u32)
    }

    #[pyo3(signature = (name, tx, reliable_enabled=false))]
    fn add_side_packet(
        &mut self,
        py: Python<'_>,
        name: &str,
        tx: Py<PyAny>,
        reliable_enabled: bool,
    ) -> PyResult<u32> {
        let cb_keep = tx.clone_ref(py);
        let cb_for_closure = cb_keep.clone_ref(py);

        let name_static: &'static str = Box::leak(name.to_owned().into_boxed_str());

        let opts = RelaySideOptions {
            reliable_enabled,
            link_local_enabled: false,
            ..RelaySideOptions::default()
        };

        let id = self.inner.add_side_packet_with_options(
            name_static,
            move |pkt: &Packet| {
                Python::attach(|py| {
                    let py_pkt = PyPacket { inner: pkt.clone() };
                    let any = Py::new(py, py_pkt)
                        .map_err(|_| TelemetryError::Io("relay packet wrapper"))?;
                    match cb_for_closure.call1(py, (&any,)) {
                        Ok(_) => Ok(()),
                        Err(err) => {
                            err.restore(py);
                            Err(TelemetryError::Io("relay packet tx error"))
                        }
                    }
                })
            },
            opts,
        );

        if self._tx_cbs.len() <= id {
            self._tx_cbs.resize_with(id + 1, || None);
        }
        self._tx_cbs[id] = Some(cb_keep);

        Ok(id as u32)
    }

    fn remove_side(&mut self, side_id: u32) -> PyResult<()> {
        self.inner
            .remove_side(side_id as usize)
            .map_err(py_err_from)?;
        if let Some(slot) = self._tx_cbs.get_mut(side_id as usize) {
            *slot = None;
        }
        Ok(())
    }

    fn set_side_ingress_enabled(&self, side_id: u32, enabled: bool) -> PyResult<()> {
        self.inner
            .set_side_ingress_enabled(side_id as usize, enabled)
            .map_err(py_err_from)
    }

    fn set_side_egress_enabled(&self, side_id: u32, enabled: bool) -> PyResult<()> {
        self.inner
            .set_side_egress_enabled(side_id as usize, enabled)
            .map_err(py_err_from)
    }

    fn set_route(&self, src_side_id: Option<u32>, dst_side_id: u32, enabled: bool) -> PyResult<()> {
        self.inner
            .set_route(
                src_side_id.map(|id| id as usize),
                dst_side_id as usize,
                enabled,
            )
            .map_err(py_err_from)
    }

    fn clear_route(&self, src_side_id: Option<u32>, dst_side_id: u32) -> PyResult<()> {
        self.inner
            .clear_route(src_side_id.map(|id| id as usize), dst_side_id as usize)
            .map_err(py_err_from)
    }

    fn set_typed_route(
        &self,
        src_side_id: Option<u32>,
        ty: u32,
        dst_side_id: u32,
        enabled: bool,
    ) -> PyResult<()> {
        self.inner
            .set_typed_route(
                src_side_id.map(|id| id as usize),
                dtype_from_u32(ty).map_err(py_err_from)?,
                dst_side_id as usize,
                enabled,
            )
            .map_err(py_err_from)
    }

    fn clear_typed_route(
        &self,
        src_side_id: Option<u32>,
        ty: u32,
        dst_side_id: u32,
    ) -> PyResult<()> {
        self.inner
            .clear_typed_route(
                src_side_id.map(|id| id as usize),
                dtype_from_u32(ty).map_err(py_err_from)?,
                dst_side_id as usize,
            )
            .map_err(py_err_from)
    }

    fn set_source_route_mode(&self, src_side_id: Option<u32>, mode: i32) -> PyResult<()> {
        let mode = match mode {
            0 => RouteSelectionMode::Fanout,
            1 => RouteSelectionMode::Weighted,
            2 => RouteSelectionMode::Failover,
            _ => return Err(PyValueError::new_err("invalid route selection mode")),
        };
        self.inner
            .set_source_route_mode(src_side_id.map(|id| id as usize), mode)
            .map_err(py_err_from)
    }

    fn clear_source_route_mode(&self, src_side_id: Option<u32>) -> PyResult<()> {
        self.inner
            .clear_source_route_mode(src_side_id.map(|id| id as usize))
            .map_err(py_err_from)
    }

    fn set_route_weight(
        &self,
        src_side_id: Option<u32>,
        dst_side_id: u32,
        weight: u32,
    ) -> PyResult<()> {
        self.inner
            .set_route_weight(
                src_side_id.map(|id| id as usize),
                dst_side_id as usize,
                weight,
            )
            .map_err(py_err_from)
    }

    fn clear_route_weight(&self, src_side_id: Option<u32>, dst_side_id: u32) -> PyResult<()> {
        self.inner
            .clear_route_weight(src_side_id.map(|id| id as usize), dst_side_id as usize)
            .map_err(py_err_from)
    }

    fn set_route_priority(
        &self,
        src_side_id: Option<u32>,
        dst_side_id: u32,
        priority: u32,
    ) -> PyResult<()> {
        self.inner
            .set_route_priority(
                src_side_id.map(|id| id as usize),
                dst_side_id as usize,
                priority,
            )
            .map_err(py_err_from)
    }

    fn clear_route_priority(&self, src_side_id: Option<u32>, dst_side_id: u32) -> PyResult<()> {
        self.inner
            .clear_route_priority(src_side_id.map(|id| id as usize), dst_side_id as usize)
            .map_err(py_err_from)
    }

    fn rx_serialized_from_side(
        &self,
        _py: Python<'_>,
        side_id: u32,
        data: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let bytes: &[u8] = data.extract()?;
        self.inner
            .rx_serialized_from_side(side_id as usize, bytes)
            .map_err(py_err_from)
    }

    fn rx_packet_from_side(
        &self,
        _py: Python<'_>,
        side_id: u32,
        packet: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let pkt_ref: PyRef<PyPacket> = packet.extract()?;
        self.inner
            .rx_from_side(side_id as usize, pkt_ref.inner.clone())
            .map_err(py_err_from)
    }

    fn clear_queues(&self) {
        self.inner.clear_queues();
    }

    fn clear_rx_queue(&self) {
        self.inner.clear_rx_queue();
    }

    fn clear_tx_queue(&self) {
        self.inner.clear_tx_queue();
    }

    fn process_rx_queue(&self) -> PyResult<()> {
        self.inner.process_rx_queue().map_err(py_err_from)
    }

    fn process_tx_queue(&self) -> PyResult<()> {
        self.inner.process_tx_queue().map_err(py_err_from)
    }

    fn process_all_queues(&self) -> PyResult<()> {
        self.inner.process_all_queues().map_err(py_err_from)
    }

    fn process_rx_queue_with_timeout(&self, timeout_ms: u32) -> PyResult<()> {
        self.inner
            .process_rx_queue_with_timeout(timeout_ms)
            .map_err(py_err_from)
    }

    fn process_tx_queue_with_timeout(&self, timeout_ms: u32) -> PyResult<()> {
        self.inner
            .process_tx_queue_with_timeout(timeout_ms)
            .map_err(py_err_from)
    }

    fn process_all_queues_with_timeout(&self, timeout_ms: u32) -> PyResult<()> {
        self.inner
            .process_all_queues_with_timeout(timeout_ms)
            .map_err(py_err_from)
    }

    fn periodic(&self, timeout_ms: u32) -> PyResult<()> {
        self.inner.periodic(timeout_ms).map_err(py_err_from)
    }

    #[cfg(feature = "discovery")]
    fn announce_discovery(&self) -> PyResult<()> {
        self.inner.announce_discovery().map_err(py_err_from)
    }

    #[cfg(feature = "discovery")]
    fn poll_discovery(&self) -> PyResult<bool> {
        self.inner.poll_discovery().map_err(py_err_from)
    }

    #[cfg(feature = "discovery")]
    fn export_topology(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        topology_snapshot_to_pydict(py, self.inner.export_topology())
    }

    #[cfg(feature = "discovery")]
    fn export_runtime_stats(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        runtime_stats_snapshot_to_pydict(py, self.inner.export_runtime_stats())
    }
}

// ============================================================================
//  Top-level helper functions
// ============================================================================

#[pyfunction]
/// Deserializes wire bytes into a `PyPacket` instance.
pub fn deserialize_packet_py(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let bytes: &[u8] = data.extract()?;
    let pkt = deserialize_packet(bytes).map_err(py_err_from)?;
    if let Err(e) = pkt.validate() {
        return Err(py_err_from(e));
    }
    Ok(Py::new(py, PyPacket { inner: pkt })?.into_any())
}

#[pyfunction]
/// Peeks the packet envelope from wire bytes and returns a Python dictionary summary.
pub fn peek_header_py(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let bytes: &[u8] = data.extract()?;
    let env = peek_envelope(bytes).map_err(py_err_from)?;

    let out = PyDict::new(py);
    out.set_item("ty", env.ty.as_u32())?;
    out.set_item("sender", env.sender.as_ref())?;
    out.set_item(
        "endpoints",
        env.endpoints
            .iter()
            .map(|e| e.as_u32())
            .collect::<Vec<u32>>(),
    )?;
    out.set_item("timestamp_ms", env.timestamp_ms)?;
    Ok(out.unbind().into_any())
}

#[pyfunction]
#[pyo3(signature = (ty, sender, endpoints, timestamp_ms, payload))]
/// Builds a `PyPacket` from explicit packet fields supplied by Python.
pub fn make_packet(
    py: Python<'_>,
    ty: u32,
    sender: &str,
    endpoints: Vec<u32>,
    timestamp_ms: u64,
    payload: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let ty = dtype_from_u32(ty).map_err(py_err_from)?;

    let eps: Vec<DataEndpoint> = endpoints
        .into_iter()
        .map(|e| endpoint_from_u32(e).map_err(py_err_from))
        .collect::<Result<_, _>>()?;

    let mut buf: Vec<u8> = payload.extract()?;

    normalize_u8_payload_for_type(ty, &mut buf);

    let payload_arc = AArc::<[u8]>::from(buf);

    let pkt = Packet::new(ty, &eps, sender, timestamp_ms, payload_arc).map_err(py_err_from)?;

    Ok(Py::new(py, PyPacket { inner: pkt })?.into_any())
}

#[pyfunction(name = "endpoint_exists")]
pub fn endpoint_exists_py(endpoint: u32) -> bool {
    endpoint_exists(DataEndpoint(endpoint))
}

#[pyfunction(name = "data_type_exists")]
pub fn data_type_exists_py(ty: u32) -> bool {
    data_type_exists(DataType(ty))
}

#[pyfunction(name = "register_endpoint")]
#[pyo3(signature = (endpoint, name, link_local_only=false, description=""))]
pub fn register_endpoint_py(
    endpoint: u32,
    name: &str,
    link_local_only: bool,
    description: &str,
) -> PyResult<u32> {
    register_endpoint_id_with_description(
        DataEndpoint(endpoint),
        name,
        description,
        link_local_only,
    )
    .map(|ep| ep.as_u32())
    .map_err(py_err_from)
}

#[pyfunction(name = "register_data_type")]
#[pyo3(signature = (ty, name, is_static, element_count, message_data_type, message_class, endpoints, reliable=0, priority=0, description="")
)]
#[allow(clippy::too_many_arguments)]
pub fn register_data_type_py(
    ty: u32,
    name: &str,
    is_static: bool,
    element_count: usize,
    message_data_type: u8,
    message_class: u8,
    endpoints: Vec<u32>,
    reliable: u8,
    priority: u8,
    description: &str,
) -> PyResult<u32> {
    let data_type = message_data_type_from_code(message_data_type)
        .ok_or_else(|| PyValueError::new_err("bad message_data_type"))?;
    let class = message_class_from_code(message_class)
        .ok_or_else(|| PyValueError::new_err("bad message_class"))?;
    let reliable =
        reliable_from_code(reliable).ok_or_else(|| PyValueError::new_err("bad reliable"))?;
    let element = if is_static {
        MessageElement::Static(element_count, data_type, class)
    } else {
        MessageElement::Dynamic(data_type, class)
    };
    let eps = endpoints.into_iter().map(DataEndpoint).collect::<Vec<_>>();
    register_data_type_id_with_description(
        DataType(ty),
        name,
        description,
        element,
        &eps,
        reliable,
        priority,
    )
    .map(|dt| dt.as_u32())
    .map_err(py_err_from)
}

#[pyfunction(name = "endpoint_info")]
pub fn endpoint_info_py(py: Python<'_>, endpoint: u32) -> PyResult<Py<PyAny>> {
    let out = PyDict::new(py);
    if let Some(def) = endpoint_definition(DataEndpoint(endpoint)) {
        out.set_item("exists", true)?;
        out.set_item("id", def.id.as_u32())?;
        out.set_item("name", def.name)?;
        out.set_item("description", def.description)?;
        out.set_item("link_local_only", def.link_local_only)?;
    } else {
        out.set_item("exists", false)?;
        out.set_item("id", endpoint)?;
    }
    Ok(out.unbind().into_any())
}

#[pyfunction(name = "data_type_info")]
pub fn data_type_info_py(py: Python<'_>, ty: u32) -> PyResult<Py<PyAny>> {
    let out = PyDict::new(py);
    if let Some(def) = data_type_definition(DataType(ty)) {
        let (is_static, count, data_type, class) = match def.element {
            MessageElement::Static(count, data_type, class) => (true, count, data_type, class),
            MessageElement::Dynamic(data_type, class) => (false, 0, data_type, class),
        };
        out.set_item("exists", true)?;
        out.set_item("id", def.id.as_u32())?;
        out.set_item("name", def.name)?;
        out.set_item("description", def.description)?;
        out.set_item("is_static", is_static)?;
        out.set_item("element_count", count)?;
        out.set_item("message_data_type", message_data_type_code(data_type))?;
        out.set_item("message_class", message_class_code(class))?;
        out.set_item("reliable", reliable_code(def.reliable))?;
        out.set_item("priority", def.priority)?;
        out.set_item("fixed_size", get_needed_message_size(def.id))?;
        out.set_item(
            "endpoints",
            def.endpoints
                .iter()
                .map(|ep| ep.as_u32())
                .collect::<Vec<_>>(),
        )?;
    } else {
        out.set_item("exists", false)?;
        out.set_item("id", ty)?;
    }
    Ok(out.unbind().into_any())
}

#[pyfunction(name = "endpoint_info_by_name")]
pub fn endpoint_info_by_name_py(py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
    let out = PyDict::new(py);
    if let Some(def) = endpoint_definition_by_name(name) {
        out.set_item("exists", true)?;
        out.set_item("id", def.id.as_u32())?;
        out.set_item("name", def.name)?;
        out.set_item("description", def.description)?;
        out.set_item("link_local_only", def.link_local_only)?;
    } else {
        out.set_item("exists", false)?;
        out.set_item("name", name)?;
    }
    Ok(out.unbind().into_any())
}

#[pyfunction(name = "data_type_info_by_name")]
pub fn data_type_info_by_name_py(py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
    let out = PyDict::new(py);
    if let Some(def) = data_type_definition_by_name(name) {
        let (is_static, count, data_type, class) = match def.element {
            MessageElement::Static(count, data_type, class) => (true, count, data_type, class),
            MessageElement::Dynamic(data_type, class) => (false, 0, data_type, class),
        };
        out.set_item("exists", true)?;
        out.set_item("id", def.id.as_u32())?;
        out.set_item("name", def.name)?;
        out.set_item("description", def.description)?;
        out.set_item("is_static", is_static)?;
        out.set_item("element_count", count)?;
        out.set_item("message_data_type", message_data_type_code(data_type))?;
        out.set_item("message_class", message_class_code(class))?;
        out.set_item("reliable", reliable_code(def.reliable))?;
        out.set_item("priority", def.priority)?;
        out.set_item("fixed_size", get_needed_message_size(def.id))?;
        out.set_item(
            "endpoints",
            def.endpoints
                .iter()
                .map(|ep| ep.as_u32())
                .collect::<Vec<_>>(),
        )?;
    } else {
        out.set_item("exists", false)?;
        out.set_item("name", name)?;
    }
    Ok(out.unbind().into_any())
}

#[pyfunction(name = "remove_endpoint")]
pub fn remove_endpoint_py(endpoint: u32) -> PyResult<bool> {
    remove_endpoint(DataEndpoint(endpoint)).map_err(py_err_from)
}

#[pyfunction(name = "remove_endpoint_by_name")]
pub fn remove_endpoint_by_name_py(name: &str) -> PyResult<bool> {
    remove_endpoint_by_name(name).map_err(py_err_from)
}

#[pyfunction(name = "remove_data_type")]
pub fn remove_data_type_py(ty: u32) -> PyResult<bool> {
    remove_data_type(DataType(ty)).map_err(py_err_from)
}

#[pyfunction(name = "remove_data_type_by_name")]
pub fn remove_data_type_by_name_py(name: &str) -> PyResult<bool> {
    remove_data_type_by_name(name).map_err(py_err_from)
}

#[pyfunction(name = "register_schema_json_bytes")]
pub fn register_schema_json_bytes_py(data: &Bound<'_, PyAny>) -> PyResult<()> {
    let bytes: &[u8] = data.extract()?;
    register_schema_json_bytes(bytes).map_err(py_err_from)
}

#[pyfunction(name = "register_schema_json_file")]
pub fn register_schema_json_file_py(path: &str) -> PyResult<()> {
    #[cfg(feature = "std")]
    {
        crate::config::register_schema_json_path(path).map_err(py_err_from)
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = path;
        Err(PyRuntimeError::new_err(
            "schema JSON file loading requires std",
        ))
    }
}

// ============================================================================
//  Module init: classes, functions, dynamic enums
// ============================================================================

#[pymodule]
/// Initializes the `sedsprintf_rs` Python extension module.
pub fn sedsprintf_rs(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyRouter>()?;
    m.add_class::<PyPacket>()?;
    m.add_class::<PyRelay>()?;

    m.add_function(wrap_pyfunction!(deserialize_packet_py, m)?)?;
    m.add_function(wrap_pyfunction!(peek_header_py, m)?)?;
    m.add_function(wrap_pyfunction!(make_packet, m)?)?;
    m.add_function(wrap_pyfunction!(endpoint_exists_py, m)?)?;
    m.add_function(wrap_pyfunction!(data_type_exists_py, m)?)?;
    m.add_function(wrap_pyfunction!(register_endpoint_py, m)?)?;
    m.add_function(wrap_pyfunction!(register_data_type_py, m)?)?;
    m.add_function(wrap_pyfunction!(register_schema_json_bytes_py, m)?)?;
    m.add_function(wrap_pyfunction!(register_schema_json_file_py, m)?)?;
    m.add_function(wrap_pyfunction!(endpoint_info_py, m)?)?;
    m.add_function(wrap_pyfunction!(data_type_info_py, m)?)?;
    m.add_function(wrap_pyfunction!(endpoint_info_by_name_py, m)?)?;
    m.add_function(wrap_pyfunction!(data_type_info_by_name_py, m)?)?;
    m.add_function(wrap_pyfunction!(remove_endpoint_py, m)?)?;
    m.add_function(wrap_pyfunction!(remove_endpoint_by_name_py, m)?)?;
    m.add_function(wrap_pyfunction!(remove_data_type_py, m)?)?;
    m.add_function(wrap_pyfunction!(remove_data_type_by_name_py, m)?)?;

    let enum_mod = PyModule::import(py, "enum")?;
    let int_enum = enum_mod.getattr("IntEnum")?;

    let mod_name: String = m.getattr("__name__")?.extract()?;

    // ------------------ DataType ------------------
    {
        let dt_dict = PyDict::new(py);
        dt_dict.set_item("__module__", &mod_name)?;

        for v in 0..=MAX_VALUE_DATA_TYPE {
            if let Some(e) = try_enum_from_u32::<DataType>(v) {
                let name = get_message_name(e);
                dt_dict.set_item(name, v)?;
                m.add(name, v)?;
            }
        }
        let dt_enum = int_enum.call1(("DataType", dt_dict))?;
        m.add("DataType", dt_enum)?;
    }

    // ------------------ DataEndpoint ------------------
    {
        let ep_dict = PyDict::new(py);
        ep_dict.set_item("__module__", &mod_name)?;
        for v in 0..=MAX_VALUE_DATA_ENDPOINT {
            if let Some(e) = try_enum_from_u32::<DataEndpoint>(v) {
                let name = e.as_str();
                ep_dict.set_item(name, v)?;
                m.add(name, v)?;
            }
        }
        let ep_enum = int_enum.call1(("DataEndpoint", ep_dict))?;
        m.add("DataEndpoint", ep_enum)?;
    }

    // ------------------ ElemKind ------------------
    {
        let ek_dict = PyDict::new(py);
        ek_dict.set_item("__module__", &mod_name)?;
        ek_dict.set_item("UNSIGNED", EK_UNSIGNED)?;
        ek_dict.set_item("SIGNED", EK_SIGNED)?;
        ek_dict.set_item("FLOAT", EK_FLOAT)?;
        let ek_enum = int_enum.call1(("ElemKind", ek_dict))?;
        m.add("ElemKind", ek_enum)?;
    }

    // ------------------ RouteSelectionMode ------------------
    {
        let rsm_dict = PyDict::new(py);
        rsm_dict.set_item("__module__", &mod_name)?;
        for v in 0..=MAX_VALUE_ROUTE_SELECTION_MODE {
            if let Some(e) = try_enum_from_i32::<RouteSelectionMode>(v) {
                let name = match e {
                    RouteSelectionMode::Fanout => "Fanout",
                    RouteSelectionMode::Weighted => "Weighted",
                    RouteSelectionMode::Failover => "Failover",
                };
                rsm_dict.set_item(name, v)?;
                m.add(name, v)?;
            }
        }
        let rsm_enum = int_enum.call1(("RouteSelectionMode", rsm_dict))?;
        m.add("RouteSelectionMode", rsm_enum)?;
    }

    Ok(())
}
