//! Telemetry Router
//!
//! Router with internal named sides (like Relay), plus local processing.
//!
//! Design:
//! - Sides are registered with per-side TX handlers (serialized or packet).
//! - RX is tagged by source side; route rules decide whether it is forwarded.
//! - Local endpoint handlers process packets as before (no side parameter).
//! - De-duplication remains packet-id based and side-agnostic.

use crate::config::{
    MAX_QUEUE_BUDGET, MAX_RECENT_RX_IDS, QUEUE_GROW_STEP, RECENT_RX_QUEUE_BYTES,
    STARTING_QUEUE_SIZE,
};
#[cfg(feature = "discovery")]
use crate::discovery::{
    self, DiscoveryCadenceState, TopologyAnnouncerRoute, TopologyBoardNode, TopologySideRoute,
    TopologySnapshot, DISCOVERY_ROUTE_TTL_MS,
};
use crate::packet::hash_bytes_u64;
use crate::queue::{BoundedDeque, ByteCost};
#[cfg(all(not(feature = "std"), target_os = "none"))]
use crate::seds_error_msg;
#[cfg(feature = "timesync")]
use crate::timesync::{
    advance_network_time, compute_network_time_sample, decode_timesync_announce,
    decode_timesync_request, decode_timesync_response, NetworkClock,
    NetworkTimeReading, PartialNetworkTime, SlewedNetworkClock, TimeSyncConfig, TimeSyncLeader,
    TimeSyncTracker, INTERNAL_TIMESYNC_SOURCE_ID, LOCAL_TIMESYNC_DATE_SOURCE_ID, LOCAL_TIMESYNC_FULL_SOURCE_ID,
    LOCAL_TIMESYNC_SUBSEC_SOURCE_ID, LOCAL_TIMESYNC_TOD_SOURCE_ID,
};
use crate::{
    config::{
        DataEndpoint, DataType, DEVICE_IDENTIFIER, MAX_HANDLER_RETRIES,
        RELIABLE_MAX_END_TO_END_PENDING, RELIABLE_MAX_PENDING, RELIABLE_MAX_RETRIES,
        RELIABLE_MAX_RETURN_ROUTES, RELIABLE_RETRANSMIT_MS,
    }, get_needed_message_size, impl_letype_num, is_reliable_type,
    lock::{ReentryGate, RouterMutex},
    message_meta, message_priority, packet::Packet,
    reliable_mode,
    serialize, MessageElement,
    RouteSelectionMode,
    TelemetryError, TelemetryResult,
};
use alloc::string::{String, ToString};
use alloc::{
    borrow::ToOwned,
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    format,
    sync::Arc,
    vec,
    vec::Vec,
};
use core::cell::UnsafeCell;
use core::fmt;
use core::fmt::{Debug, Formatter};
use core::mem::size_of;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "std")]
use std::time::Instant;

/// Logical side index (CAN, UART, RADIO, etc.)
pub type RouterSideId = usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouterItem {
    Packet(Packet),
    Serialized(Arc<[u8]>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RouterRxItem {
    src: Option<RouterSideId>,
    data: RouterItem,
    priority: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RouterTxItem {
    Broadcast(RouterItem),
    EndToEndReplay {
        packet_id: u64,
    },
    ToSide {
        src: Option<RouterSideId>,
        dst: RouterSideId,
        data: RouterItem,
    },
    ReliableReplay {
        dst: RouterSideId,
        bytes: Arc<[u8]>,
    },
}

impl ByteCost for RouterRxItem {
    #[inline]
    fn byte_cost(&self) -> usize {
        match &self.data {
            RouterItem::Packet(pkt) => pkt.byte_cost(),
            RouterItem::Serialized(bytes) => size_of::<Arc<[u8]>>() + bytes.len(),
        }
    }
}

impl ByteCost for RouterTxItem {
    #[inline]
    fn byte_cost(&self) -> usize {
        match self {
            RouterTxItem::Broadcast(data) => match data {
                RouterItem::Packet(pkt) => pkt.byte_cost(),
                RouterItem::Serialized(bytes) => size_of::<Arc<[u8]>>() + bytes.len(),
            },
            RouterTxItem::EndToEndReplay { .. } => size_of::<u64>(),
            RouterTxItem::ToSide { data, .. } => match data {
                RouterItem::Packet(pkt) => pkt.byte_cost(),
                RouterItem::Serialized(bytes) => size_of::<Arc<[u8]>>() + bytes.len(),
            },
            RouterTxItem::ReliableReplay { bytes, .. } => size_of::<Arc<[u8]>>() + bytes.len(),
        }
    }
}

/// Transmit queue item with flags.
/// Holds a RouterTxItem and a flag to ignore local dispatch.
/// Used internally by the Router transmit queue.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TxQueued {
    item: RouterTxItem,
    ignore_local: bool,
    priority: u8,
}

/// ByteCost implementation for TxQueued.
impl ByteCost for TxQueued {
    /// Byte cost is the cost of the inner item plus one bool.
    #[inline]
    fn byte_cost(&self) -> usize {
        self.item.byte_cost() + size_of::<bool>() + size_of::<u8>()
    }
}

/// ByteCost implementation for `u64` (used by `recent_rx`).
impl ByteCost for u64 {
    /// Byte cost is size of u64.
    #[inline]
    fn byte_cost(&self) -> usize {
        size_of::<u64>()
    }
}

// -------------------- Reliable delivery state --------------------

#[derive(Debug, Clone)]
struct ReliableTxState {
    next_seq: u32,
    sent_order: VecDeque<u32>,
    sent: BTreeMap<u32, ReliableSent>,
}

#[derive(Debug, Clone)]
struct ReliableSent {
    bytes: Arc<[u8]>,
    last_send_ms: u64,
    retries: u32,
    queued: bool,
    partial_acked: bool,
}

#[derive(Debug, Clone)]
struct EndToEndReliableSent {
    data: RouterItem,
    pending_destinations: BTreeMap<u64, RouterSideId>,
    tracked_destinations: bool,
    last_send_ms: u64,
    retries: u32,
    queued: bool,
}

#[derive(Debug, Clone)]
struct ReliableRxState {
    expected_seq: u32,
    buffered: BTreeMap<u32, Arc<[u8]>>,
}

#[derive(Debug, Clone)]
struct ReliableReturnRouteState {
    side: RouterSideId,
}

#[cfg(feature = "discovery")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DiscoverySenderState {
    reachable: Vec<DataEndpoint>,
    reachable_timesync_sources: Vec<String>,
    topology_boards: Vec<TopologyBoardNode>,
    last_seen_ms: u64,
}

#[cfg(feature = "discovery")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DiscoverySideState {
    reachable: Vec<DataEndpoint>,
    reachable_timesync_sources: Vec<String>,
    last_seen_ms: u64,
    announcers: BTreeMap<String, DiscoverySenderState>,
}

#[derive(Debug, Clone, Default)]
struct AdaptiveRouteStats {
    estimated_bandwidth_bps: u64,
    last_observed_ms: u64,
}

impl AdaptiveRouteStats {
    #[inline]
    fn observe(&mut self, sample_bps: u64, now_ms: u64) {
        self.estimated_bandwidth_bps = if self.estimated_bandwidth_bps == 0 {
            sample_bps
        } else if sample_bps >= self.estimated_bandwidth_bps {
            self.estimated_bandwidth_bps
                .saturating_mul(3)
                .saturating_add(sample_bps.saturating_mul(5))
                / 8
        } else {
            self.estimated_bandwidth_bps
                .saturating_mul(7)
                .saturating_add(sample_bps)
                / 8
        };
        self.last_observed_ms = now_ms;
    }

    #[inline]
    fn weight(&self) -> u64 {
        self.estimated_bandwidth_bps.max(1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteSelectionOrigin {
    Flood,
    Discovered,
}

// -------------------- endpoint + board config --------------------
/// Packet Handler function type
type PacketHandlerFn = dyn Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static;

/// Serialized Handler function type
type SerializedHandlerFn = dyn Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static;

// Make handlers usable across tasks
/// Endpoint handler function enum.
/// Holds either a `Packet` handler or a serialized byte-slice handler.
/// /// - Packet handler signature: `Fn(&Packet) -> TelemetryResult<()>`
/// /// - Serialized handler signature: `Fn(&[u8]) -> TelemetryResult<()>`
#[derive(Clone)]
pub enum EndpointHandlerFn {
    Packet(Arc<PacketHandlerFn>),
    Serialized(Arc<SerializedHandlerFn>),
}

/// Endpoint handler for a specific data endpoint.
pub struct EndpointHandler {
    endpoint: DataEndpoint,
    handler: EndpointHandlerFn,
}

/// Debug implementation for EndpointHandlerFn.
impl Debug for EndpointHandlerFn {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            EndpointHandlerFn::Packet(_) => f.write_str("EndpointHandlerFn::Packet(<handler>)"),
            EndpointHandlerFn::Serialized(_) => {
                f.write_str("EndpointHandlerFn::Serialized(<handler>)")
            }
        }
    }
}

/// TX handler for a router side: either serialized or packet-based.
#[derive(Clone)]
pub enum RouterTxHandlerFn {
    Serialized(Arc<SerializedHandlerFn>),
    Packet(Arc<PacketHandlerFn>),
}

impl Debug for RouterTxHandlerFn {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RouterTxHandlerFn::Serialized(_) => {
                f.debug_tuple("Serialized").field(&"<handler>").finish()
            }
            RouterTxHandlerFn::Packet(_) => f.debug_tuple("Packet").field(&"<handler>").finish(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RouterSideOptions {
    /// Enables the router's per-link reliable transport layer on this side.
    ///
    /// When `true` and the side uses a serialized TX handler, reliable schema types gain
    /// sequence numbers, ACKs, packet requests, and retransmits on this specific hop.
    /// When `false`, the router strips the reliable framing for that side and sends only the
    /// application packet payload once.
    ///
    /// This setting only affects the hop between this router and the side's TX callback. It does
    /// not change whether a schema `DataType` is defined as reliable, and it does not disable the
    /// router's end-to-end reliable tracking for packets originated elsewhere in the network.
    pub reliable_enabled: bool,
    /// Marks the side as eligible for link-local-only endpoints and discovery routes.
    pub link_local_enabled: bool,
    /// Allows packets received from this side to enter router processing.
    pub ingress_enabled: bool,
    /// Allows the router to transmit packets toward this side.
    pub egress_enabled: bool,
}

impl Default for RouterSideOptions {
    fn default() -> Self {
        Self {
            reliable_enabled: false,
            link_local_enabled: false,
            ingress_enabled: true,
            egress_enabled: true,
        }
    }
}

/// One side of the router – a name + TX handler.
#[derive(Clone, Debug)]
pub struct RouterSide {
    pub name: &'static str,
    pub tx_handler: RouterTxHandlerFn,
    pub opts: RouterSideOptions,
}

/// Debug implementation for EndpointHandler.
impl Debug for EndpointHandler {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointHandler")
            .field("endpoint", &self.endpoint)
            .field("handler", &self.handler)
            .finish()
    }
}

#[inline]
pub(crate) const fn endpoint_is_router_internal(endpoint: DataEndpoint) -> bool {
    #[cfg(feature = "timesync")]
    if matches!(endpoint, DataEndpoint::TimeSync) {
        return true;
    }
    discovery::is_discovery_endpoint(endpoint)
}

impl EndpointHandler {
    /// Create a new endpoint handler for `Packet` callbacks.
    ///
    /// Handler signature is `Fn(&Packet) -> TelemetryResult<()>`.
    #[inline]
    pub fn new_packet_handler<F>(endpoint: DataEndpoint, f: F) -> Self
    where
        F: Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        assert!(
            !endpoint_is_router_internal(endpoint),
            "reserved internal endpoint handlers must not be user-registered"
        );
        #[cfg(feature = "std")]
        crate::config::ensure_endpoint_id(endpoint, false)
            .expect("endpoint handler endpoint registration failed");
        Self {
            endpoint,
            handler: EndpointHandlerFn::Packet(Arc::new(f)),
        }
    }

    /// Create a new packet handler from a runtime endpoint definition.
    #[inline]
    pub fn new_packet_handler_for<F>(endpoint: crate::config::EndpointDefinition, f: F) -> Self
    where
        F: Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        Self::new_packet_handler(endpoint.id, f)
    }

    /// Create a new packet handler by endpoint name.
    #[cfg(feature = "std")]
    #[inline]
    pub fn new_packet_handler_by_name<F>(endpoint_name: &str, f: F) -> TelemetryResult<Self>
    where
        F: Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        let endpoint = crate::config::endpoint_definition_by_name(endpoint_name)
            .ok_or(TelemetryError::BadArg)?;
        Ok(Self::new_packet_handler(endpoint.id, f))
    }

    /// Create a new endpoint handler for serialized byte-slice callbacks.
    ///
    /// Handler signature is `Fn(&[u8]) -> TelemetryResult<()>`.
    #[inline]
    pub fn new_serialized_handler<F>(endpoint: DataEndpoint, f: F) -> Self
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        assert!(
            !endpoint_is_router_internal(endpoint),
            "reserved internal endpoint handlers must not be user-registered"
        );
        #[cfg(feature = "std")]
        crate::config::ensure_endpoint_id(endpoint, false)
            .expect("endpoint handler endpoint registration failed");
        Self {
            endpoint,
            handler: EndpointHandlerFn::Serialized(Arc::new(f)),
        }
    }

    /// Create a new serialized handler from a runtime endpoint definition.
    #[inline]
    pub fn new_serialized_handler_for<F>(endpoint: crate::config::EndpointDefinition, f: F) -> Self
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        Self::new_serialized_handler(endpoint.id, f)
    }

    /// Create a new serialized handler by endpoint name.
    #[cfg(feature = "std")]
    #[inline]
    pub fn new_serialized_handler_by_name<F>(endpoint_name: &str, f: F) -> TelemetryResult<Self>
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        let endpoint = crate::config::endpoint_definition_by_name(endpoint_name)
            .ok_or(TelemetryError::BadArg)?;
        Ok(Self::new_serialized_handler(endpoint.id, f))
    }

    /// Return the endpoint that the handler is registered for.
    #[inline]
    pub fn get_endpoint(&self) -> DataEndpoint {
        self.endpoint
    }

    /// Return a reference to the handler function.
    #[inline]
    pub fn get_handler(&self) -> &EndpointHandlerFn {
        &self.handler
    }
}

pub trait Clock {
    /// Return the current time in milliseconds.
    fn now_ms(&self) -> u64;

    /// Return the current time in nanoseconds.
    ///
    /// The default implementation derives this from [`Clock::now_ms`].
    fn now_ns(&self) -> u64 {
        self.now_ms().saturating_mul(1_000_000)
    }
}

impl<T: Fn() -> u64> Clock for T {
    #[inline]
    fn now_ms(&self) -> u64 {
        self()
    }
}

#[cfg(feature = "std")]
#[derive(Debug)]
struct StdMonotonicClock {
    start: Instant,
}

#[cfg(feature = "std")]
impl Default for StdMonotonicClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

#[cfg(feature = "std")]
impl Clock for StdMonotonicClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn now_ns(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Handlers for local endpoints.
    handlers: Arc<[EndpointHandler]>,
    /// Whether to enable reliable ordering/ACKs for reliable data types.
    reliable_enabled: bool,
    /// Optional per-router sender override.
    sender: Option<Arc<str>>,
    #[cfg(feature = "timesync")]
    timesync: Option<TimeSyncConfig>,
}

impl RouterConfig {
    /// Create a new router configuration with the specified local endpoint handlers.
    pub fn new<H>(handlers: H) -> Self
    where
        H: Into<Arc<[EndpointHandler]>>,
    {
        let handlers: Arc<[EndpointHandler]> = handlers.into();
        assert!(
            handlers
                .iter()
                .all(|handler| !endpoint_is_router_internal(handler.endpoint)),
            "reserved internal endpoint handlers must not be user-registered"
        );
        Self {
            handlers,
            reliable_enabled: true,
            sender: None,
            #[cfg(feature = "timesync")]
            timesync: None,
        }
    }

    /// Enable or disable reliable delivery for this router instance.
    pub fn with_reliable_enabled(mut self, enabled: bool) -> Self {
        self.reliable_enabled = enabled;
        self
    }

    /// Override the sender identifier for this router instance.
    pub fn with_sender<S: AsRef<str>>(mut self, sender: S) -> Self {
        self.sender = Some(Arc::from(sender.as_ref()));
        self
    }

    #[cfg(feature = "timesync")]
    /// Enables and configures built-in time synchronization for this router.
    pub fn with_timesync(mut self, cfg: TimeSyncConfig) -> Self {
        self.timesync = Some(cfg);
        self
    }

    #[inline]
    /// Check if the specified endpoint is local to this router.
    fn is_local_endpoint(&self, ep: DataEndpoint) -> bool {
        if endpoint_is_router_internal(ep) {
            return false;
        }
        self.handlers.iter().any(|h| h.endpoint == ep)
    }

    #[inline]
    fn reliable_enabled(&self) -> bool {
        self.reliable_enabled
    }

    #[inline]
    fn sender(&self) -> &str {
        self.sender.as_deref().unwrap_or(DEVICE_IDENTIFIER)
    }

    #[cfg(feature = "timesync")]
    #[inline]
    fn timesync_config(&self) -> Option<TimeSyncConfig> {
        self.timesync
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            handlers: Arc::from([]),
            reliable_enabled: true,
            sender: None,
            #[cfg(feature = "timesync")]
            timesync: None,
        }
    }
}

// -------------------- generic little-endian serialization --------------------

pub trait LeBytes: Copy + Sized {
    const WIDTH: usize;
    fn write_le(self, out: &mut [u8]);
    fn from_le_slice(bytes: &[u8]) -> Self;
}

impl_letype_num!(u8, 1);
impl_letype_num!(u16, 2);
impl_letype_num!(u32, 4);
impl_letype_num!(u64, 8);
impl_letype_num!(u128, 16);
impl_letype_num!(i8, 1);
impl_letype_num!(i16, 2);
impl_letype_num!(i32, 4);
impl_letype_num!(i64, 8);
impl_letype_num!(i128, 16);
impl_letype_num!(f32, 4);
impl_letype_num!(f64, 8);

/// Encode a slice of `LeBytes` into a contiguous little-endian byte array.
pub(crate) fn encode_slice_le<T: LeBytes>(data: &[T]) -> Arc<[u8]> {
    let total = data.len() * T::WIDTH;
    let mut buf = vec![0u8; total];

    for (i, v) in data.iter().copied().enumerate() {
        let start = i * T::WIDTH;
        v.write_le(&mut buf[start..start + T::WIDTH]);
    }

    Arc::from(buf)
}

/// Build an error payload for `TelemetryError` packets, respecting the
/// static/dynamic sizing rules from `message_meta`.
fn make_error_payload(msg: &str) -> Arc<[u8]> {
    let meta = message_meta(DataType::TelemetryError);
    match meta.element {
        MessageElement::Static(_, _, _) => {
            let max = get_needed_message_size(DataType::TelemetryError);
            let bytes = msg.as_bytes();
            let n = core::cmp::min(max, bytes.len());
            let mut buf = vec![0u8; max];
            if n > 0 {
                buf[..n].copy_from_slice(&bytes[..n]);
            }
            Arc::from(buf)
        }
        MessageElement::Dynamic(_, _) => Arc::from(msg.as_bytes()),
    }
}

/// Generic raw logger function used by Router::log and Router::log_queue.
/// Builds a Packet from the provided data slice and passes it to the
/// provided transmission function.
fn log_raw<T, F>(
    sender: &'static str,
    ty: DataType,
    data: &[T],
    timestamp: u64,
    mut tx_function: F,
) -> TelemetryResult<()>
where
    T: LeBytes,
    F: FnMut(Packet) -> TelemetryResult<()>,
{
    let meta = message_meta(ty);
    let got = data.len() * T::WIDTH;

    match meta.element {
        MessageElement::Static(_, _, _) => {
            if got != get_needed_message_size(ty) {
                return Err(TelemetryError::SizeMismatch {
                    expected: get_needed_message_size(ty),
                    got,
                });
            }
        }
        MessageElement::Dynamic(_, _) => {
            // For dynamic numeric payloads, require total byte length to be a multiple of element width.
            if !got.is_multiple_of(T::WIDTH) {
                return Err(TelemetryError::SizeMismatch {
                    expected: T::WIDTH,
                    got,
                });
            }
        }
    }

    let payload = encode_slice_le(data);
    let pkt = Packet::new(ty, meta.endpoints, sender, timestamp, payload)?;
    tx_function(pkt)
}

/// Fallback printing for error messages when no local endpoints exist.
/// - With `std`: prints to stderr.
/// - Without `std`: forwards to `seds_error_msg` (platform-provided).
fn fallback_stdout(msg: &str) {
    #[cfg(feature = "std")]
    {
        eprintln!("{}", msg);
    }
    #[cfg(all(not(feature = "std"), target_os = "none"))]
    {
        let message = format!("{}\n", msg);
        unsafe {
            seds_error_msg(message.as_ptr(), message.len());
        }
    }
}

// -------------------- Router --------------------

/// Internal mutable state of the Router, protected by `RouterMutex`.
/// Holds the RX/TX queues and the recent-RX de-duplication set.
#[derive(Debug, Clone)]
struct RouterInner {
    sides: Vec<Option<RouterSide>>,
    route_overrides: BTreeMap<(Option<RouterSideId>, RouterSideId), bool>,
    typed_route_overrides: BTreeMap<(Option<RouterSideId>, u32, RouterSideId), bool>,
    route_weights: BTreeMap<(Option<RouterSideId>, RouterSideId), u32>,
    route_priorities: BTreeMap<(Option<RouterSideId>, RouterSideId), u32>,
    source_route_modes: BTreeMap<Option<RouterSideId>, RouteSelectionMode>,
    route_selection_cursors: BTreeMap<Option<RouterSideId>, u64>,
    adaptive_route_stats: BTreeMap<RouterSideId, AdaptiveRouteStats>,
    received_queue: BoundedDeque<RouterRxItem>,
    transmit_queue: BoundedDeque<TxQueued>,
    recent_rx: BoundedDeque<u64>,
    reliable_tx: BTreeMap<(RouterSideId, u32), ReliableTxState>,
    reliable_rx: BTreeMap<(RouterSideId, u32), ReliableRxState>,
    reliable_return_routes: BTreeMap<u64, ReliableReturnRouteState>,
    reliable_return_route_order: VecDeque<u64>,
    end_to_end_reliable_tx: BTreeMap<u64, EndToEndReliableSent>,
    end_to_end_reliable_tx_order: VecDeque<u64>,
    #[cfg(feature = "discovery")]
    discovery_routes: BTreeMap<RouterSideId, DiscoverySideState>,
    #[cfg(feature = "discovery")]
    discovery_cadence: DiscoveryCadenceState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouterQueueKind {
    Received,
    Transmit,
    Recent,
    ReliableRxBuffer,
    #[cfg(feature = "discovery")]
    Discovery,
}

impl RouterInner {
    #[cfg(feature = "discovery")]
    fn topology_board_byte_cost(board: &TopologyBoardNode) -> usize {
        board
            .sender_id
            .len()
            .saturating_add(board.reachable_endpoints.len() * size_of::<DataEndpoint>())
            .saturating_add(
                board
                    .reachable_timesync_sources
                    .iter()
                    .map(|s| s.len())
                    .sum::<usize>(),
            )
            .saturating_add(board.connections.iter().map(|s| s.len()).sum::<usize>())
    }

    #[cfg(feature = "discovery")]
    fn discovery_sender_byte_cost(sender: &str, state: &DiscoverySenderState) -> usize {
        sender
            .len()
            .saturating_add(state.reachable.len() * size_of::<DataEndpoint>())
            .saturating_add(
                state
                    .reachable_timesync_sources
                    .iter()
                    .map(|s| s.len())
                    .sum::<usize>(),
            )
            .saturating_add(
                state
                    .topology_boards
                    .iter()
                    .map(Self::topology_board_byte_cost)
                    .sum::<usize>(),
            )
            .saturating_add(size_of::<DiscoverySenderState>())
    }

    #[cfg(feature = "discovery")]
    fn discovery_route_byte_cost(side: RouterSideId, route: &DiscoverySideState) -> usize {
        size_of::<RouterSideId>()
            .saturating_add(size_of::<DiscoverySideState>())
            .saturating_add(route.reachable.len() * size_of::<DataEndpoint>())
            .saturating_add(
                route
                    .reachable_timesync_sources
                    .iter()
                    .map(|s| s.len())
                    .sum::<usize>(),
            )
            .saturating_add(
                route
                    .announcers
                    .iter()
                    .map(|(sender, state)| Self::discovery_sender_byte_cost(sender, state))
                    .sum::<usize>(),
            )
            .saturating_add(side.saturating_sub(side))
    }

    #[cfg(feature = "discovery")]
    fn discovery_bytes_used(&self) -> usize {
        self.discovery_routes
            .iter()
            .map(|(side, route)| Self::discovery_route_byte_cost(*side, route))
            .sum()
    }

    #[inline]
    fn reliable_rx_buffered_bytes(&self) -> usize {
        self.reliable_rx
            .values()
            .flat_map(|state| state.buffered.values())
            .map(|bytes| size_of::<Arc<[u8]>>() + bytes.len())
            .sum()
    }

    #[inline]
    fn shared_queue_bytes_used(&self) -> usize {
        self.received_queue
            .bytes_used()
            .saturating_add(self.transmit_queue.bytes_used())
            .saturating_add(self.recent_rx.max_bytes())
            .saturating_add(self.reliable_rx_buffered_bytes())
            .saturating_add(crate::config::schema_bytes_used())
            .saturating_add({
                #[cfg(feature = "discovery")]
                {
                    self.discovery_bytes_used()
                }
                #[cfg(not(feature = "discovery"))]
                {
                    0
                }
            })
    }

    fn reliable_rx_buffer_len(&self) -> usize {
        self.reliable_rx
            .values()
            .map(|state| state.buffered.len())
            .sum()
    }

    fn pop_reliable_rx_buffered(&mut self) -> Option<Arc<[u8]>> {
        let key = self
            .reliable_rx
            .iter()
            .find_map(|(key, state)| (!state.buffered.is_empty()).then_some(*key))?;
        self.reliable_rx
            .get_mut(&key)?
            .buffered
            .pop_first()
            .map(|(_, v)| v)
    }

    fn pop_shared_queue_item(&mut self, preferred: RouterQueueKind) -> bool {
        match preferred {
            RouterQueueKind::Received => self.received_queue.pop_front().is_some(),
            RouterQueueKind::Transmit => self.transmit_queue.pop_front().is_some(),
            RouterQueueKind::Recent => self.recent_rx.pop_front().is_some(),
            RouterQueueKind::ReliableRxBuffer => self.pop_reliable_rx_buffered().is_some(),
            #[cfg(feature = "discovery")]
            RouterQueueKind::Discovery => self.pop_discovery_route(),
        }
    }

    #[cfg(feature = "discovery")]
    fn pop_discovery_route(&mut self) -> bool {
        let Some((&side, _)) = self
            .discovery_routes
            .iter()
            .min_by_key(|(_, route)| route.last_seen_ms)
        else {
            return false;
        };
        self.discovery_routes.remove(&side);
        Self::queue_budget_warning("topology route evicted because MAX_QUEUE_BUDGET is full");
        true
    }

    fn largest_shared_queue(&self) -> Option<RouterQueueKind> {
        let candidates = [
            (
                RouterQueueKind::Received,
                self.received_queue.bytes_used(),
                self.received_queue.len(),
            ),
            (
                RouterQueueKind::Transmit,
                self.transmit_queue.bytes_used(),
                self.transmit_queue.len(),
            ),
            (RouterQueueKind::Recent, 0, 0),
            (
                RouterQueueKind::ReliableRxBuffer,
                self.reliable_rx_buffered_bytes(),
                self.reliable_rx_buffer_len(),
            ),
            #[cfg(feature = "discovery")]
            (
                RouterQueueKind::Discovery,
                self.discovery_bytes_used(),
                self.discovery_routes.len(),
            ),
        ];
        candidates
            .into_iter()
            .filter(|(_, bytes, len)| *bytes > 0 && *len > 0)
            .max_by_key(|(kind, bytes, _)| {
                (
                    *bytes,
                    if *kind == RouterQueueKind::ReliableRxBuffer {
                        0
                    } else {
                        1
                    },
                )
            })
            .map(|(kind, _, _)| kind)
    }

    fn make_shared_queue_room(
        &mut self,
        incoming_cost: usize,
        preferred: RouterQueueKind,
    ) -> TelemetryResult<()> {
        if incoming_cost > MAX_QUEUE_BUDGET {
            return Err(TelemetryError::PacketTooLarge(
                "Item exceeds maximum shared queue budget",
            ));
        }

        while self.shared_queue_bytes_used().saturating_add(incoming_cost) > MAX_QUEUE_BUDGET {
            let victim = self.largest_shared_queue().unwrap_or(preferred);
            if victim == RouterQueueKind::Discovery {
                Self::queue_budget_warning("topology data is using the largest queue budget share");
            }
            if !self.pop_shared_queue_item(victim) && !self.pop_shared_queue_item(preferred) {
                return Err(TelemetryError::PacketTooLarge(
                    "Item exceeds maximum shared queue budget",
                ));
            }
        }

        Ok(())
    }

    #[inline]
    fn queue_budget_warning(msg: &str) {
        #[cfg(feature = "std")]
        eprintln!("sedsprintf_rs queue budget warning: {msg}");
        let _ = msg;
    }

    #[cfg(feature = "discovery")]
    fn fit_discovery_budget(&mut self) {
        while self.shared_queue_bytes_used() > MAX_QUEUE_BUDGET {
            if !self.pop_discovery_route() {
                break;
            }
        }
    }

    fn push_received(&mut self, item: RouterRxItem) -> TelemetryResult<()> {
        self.make_shared_queue_room(item.byte_cost(), RouterQueueKind::Received)?;
        self.received_queue
            .push_back_prioritized(item, |queued| queued.priority)
    }

    fn push_transmit(&mut self, item: TxQueued) -> TelemetryResult<()> {
        self.make_shared_queue_room(item.byte_cost(), RouterQueueKind::Transmit)?;
        self.transmit_queue
            .push_back_prioritized(item, |queued| queued.priority)
    }

    fn push_recent_rx(&mut self, id: u64) -> TelemetryResult<()> {
        while self.recent_rx.len() >= MAX_RECENT_RX_IDS {
            let _ = self.recent_rx.pop_front();
        }
        self.make_shared_queue_room(0, RouterQueueKind::Recent)?;
        self.recent_rx.push_back(id)
    }

    fn buffer_reliable_rx(
        &mut self,
        side: RouterSideId,
        ty: DataType,
        seq: u32,
        bytes: Arc<[u8]>,
    ) -> TelemetryResult<()> {
        let key = Router::reliable_key(side, ty);
        if self
            .reliable_rx
            .get(&key)
            .is_some_and(|state| state.buffered.contains_key(&seq))
        {
            return Ok(());
        }
        let cost = size_of::<Arc<[u8]>>() + bytes.len();
        self.make_shared_queue_room(cost, RouterQueueKind::ReliableRxBuffer)?;
        let rx_state = self
            .reliable_rx
            .entry(key)
            .or_insert_with(|| ReliableRxState {
                expected_seq: 1,
                buffered: BTreeMap::new(),
            });
        if rx_state.buffered.len() >= RELIABLE_MAX_PENDING {
            let _ = rx_state.buffered.pop_first();
        }
        rx_state.buffered.insert(seq, bytes);
        Ok(())
    }
}

/// Non-blocking RX queue used by ISR-safe `rx_queue*` APIs.
///
/// Uses a tiny atomic try-lock so enqueue never blocks. If contended, push/pop
/// operations return `TelemetryError::Io("rx queue busy")`.
struct IsrRxQueue {
    busy: AtomicBool,
    q: UnsafeCell<BoundedDeque<RouterRxItem>>,
}

unsafe impl Send for IsrRxQueue {}
unsafe impl Sync for IsrRxQueue {}

struct IsrRxQueueGuard<'a> {
    owner: &'a IsrRxQueue,
}

impl Deref for IsrRxQueueGuard<'_> {
    type Target = BoundedDeque<RouterRxItem>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.owner.q.get() }
    }
}

impl DerefMut for IsrRxQueueGuard<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.owner.q.get() }
    }
}

impl Drop for IsrRxQueueGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.owner.busy.store(false, Ordering::Release);
    }
}

impl IsrRxQueue {
    #[inline]
    fn new(max_bytes: usize, starting_bytes: usize, grow_mult: f64) -> Self {
        Self {
            busy: AtomicBool::new(false),
            q: UnsafeCell::new(BoundedDeque::new(max_bytes, starting_bytes, grow_mult)),
        }
    }

    #[inline]
    fn try_lock(&self) -> TelemetryResult<IsrRxQueueGuard<'_>> {
        match self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => Ok(IsrRxQueueGuard { owner: self }),
            Err(_) => Err(TelemetryError::Io("rx queue busy")),
        }
    }

    #[allow(dead_code)]
    #[inline]
    fn push_back(&self, item: RouterRxItem) -> TelemetryResult<()> {
        let mut g = self.try_lock()?;
        g.push_back(item)
    }

    #[inline]
    fn push_back_prioritized(&self, item: RouterRxItem) -> TelemetryResult<()> {
        let mut g = self.try_lock()?;
        g.push_back_prioritized(item, |queued| queued.priority)
    }

    #[inline]
    fn pop_front(&self) -> TelemetryResult<Option<RouterRxItem>> {
        let mut g = self.try_lock()?;
        Ok(g.pop_front())
    }

    #[inline]
    fn clear(&self) -> TelemetryResult<()> {
        let mut g = self.try_lock()?;
        g.clear();
        Ok(())
    }
}

/// Telemetry Router for handling incoming and outgoing telemetry packets.
/// Supports queuing, processing, and dispatching to local endpoint handlers.
/// Thread-safe via internal locking.
pub struct Router {
    sender: &'static str,
    cfg: RouterConfig,
    state: RouterMutex<RouterInner>,
    isr_rx_queue: IsrRxQueue,
    side_tx_gate: ReentryGate,
    clock: Box<dyn Clock + Send + Sync>,
    #[cfg(feature = "timesync")]
    timesync: RouterMutex<TimeSyncRuntime>,
}

#[cfg(feature = "timesync")]
#[derive(Debug, Clone)]
struct PendingTimeSyncRequest {
    seq: u64,
    t1_mono_ms: u64,
    source: String,
}

#[cfg(feature = "timesync")]
#[derive(Debug, Clone)]
struct RemoteTimeSyncSource {
    priority: u64,
    last_sample_mono_ms: u64,
    sample_unix_ms: u64,
}

#[cfg(feature = "timesync")]
#[derive(Debug, Clone)]
struct TimeSyncRuntime {
    cfg: Option<TimeSyncConfig>,
    tracker: Option<TimeSyncTracker>,
    clock: NetworkClock,
    disciplined_clock: SlewedNetworkClock,
    remote_sources: BTreeMap<String, RemoteTimeSyncSource>,
    next_seq: u64,
    next_announce_mono_ms: u64,
    next_request_mono_ms: u64,
    pending_request: Option<PendingTimeSyncRequest>,
}

#[cfg(feature = "timesync")]
impl TimeSyncRuntime {
    fn new(cfg: Option<TimeSyncConfig>) -> Self {
        Self {
            tracker: cfg.map(TimeSyncTracker::new),
            cfg,
            clock: NetworkClock::default(),
            disciplined_clock: SlewedNetworkClock::new(
                cfg.map(|c| c.max_slew_ppm)
                    .unwrap_or(TimeSyncConfig::default().max_slew_ppm),
            ),
            remote_sources: BTreeMap::new(),
            next_seq: 1,
            next_announce_mono_ms: 0,
            next_request_mono_ms: 0,
            pending_request: None,
        }
    }
}

enum RemoteSidePlan {
    Target(Vec<RouterSideId>),
}

impl Debug for Router {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router")
            .field("sender", &self.sender)
            .field("cfg", &self.cfg)
            .field("state", &"<mutex>")
            .field("clock", &"Clock")
            .finish()
    }
}

/// Check if any of the provided endpoints require remote forwarding when
/// discovery is unavailable and routing falls back to local-vs-remote schema.
#[inline]
fn has_nonlocal_endpoint(eps: &[DataEndpoint], cfg: &RouterConfig) -> bool {
    eps.iter()
        .copied()
        .any(|ep| !cfg.is_local_endpoint(ep) && !ep.is_link_local_only())
}

#[inline]
fn is_internal_control_type(ty: DataType) -> bool {
    if matches!(
        ty,
        DataType::ReliableAck | DataType::ReliablePartialAck | DataType::ReliablePacketRequest
    ) {
        return true;
    }

    #[cfg(feature = "timesync")]
    if matches!(
        ty,
        DataType::TimeSyncAnnounce | DataType::TimeSyncRequest | DataType::TimeSyncResponse
    ) {
        return true;
    }

    #[cfg(feature = "discovery")]
    if discovery::is_discovery_type(ty) {
        return true;
    }

    let _ = ty;
    false
}

/// Helper function to call a handler with retries and error handling.
fn with_retries<F>(
    this: &Router,
    dest: DataEndpoint,
    data: &RouterItem,
    pkt_for_ctx: Option<&Packet>,
    env_for_ctx: Option<&serialize::TelemetryEnvelope>,
    run: F,
) -> TelemetryResult<()>
where
    F: Fn() -> TelemetryResult<()>,
{
    this.retry(MAX_HANDLER_RETRIES, run).map_err(|e| {
        // If handler fails, remove from dedupe so it can be retried later if resent.
        this.remove_pkt_id(data);

        // Emit error packet (to local endpoints).
        if let Some(pkt) = pkt_for_ctx {
            let _ = this.handle_callback_error(pkt, Some(dest), e);
        } else if let Some(env) = env_for_ctx {
            let _ = this.handle_callback_error_from_env(env, Some(dest), e);
        }

        TelemetryError::HandlerError("local handler failed")
    })
}
/// Router implementation
impl Router {
    const END_TO_END_ACK_SENDER: &'static str = "E2EACK";
    const END_TO_END_ACK_PREFIX: &'static str = "E2EACK:";

    #[inline]
    fn side_ref(st: &RouterInner, side: RouterSideId) -> TelemetryResult<&RouterSide> {
        st.sides
            .get(side)
            .and_then(|side| side.as_ref())
            .ok_or(TelemetryError::HandlerError("router: invalid side id"))
    }

    #[inline]
    fn ensure_side_ingress_enabled(&self, side: RouterSideId) -> TelemetryResult<()> {
        let st = self.state.lock();
        let side_ref = Self::side_ref(&st, side)?;
        if side_ref.opts.ingress_enabled {
            Ok(())
        } else {
            Err(TelemetryError::HandlerError(
                "router: ingress disabled for side id",
            ))
        }
    }

    #[inline]
    fn default_route_enabled(&self, src: Option<RouterSideId>, dst: RouterSideId) -> bool {
        src != Some(dst)
    }

    #[inline]
    fn route_allowed_locked(
        &self,
        st: &RouterInner,
        src: Option<RouterSideId>,
        ty: Option<DataType>,
        dst: RouterSideId,
    ) -> bool {
        let Some(dst_side) = st.sides.get(dst).and_then(|side| side.as_ref()) else {
            return false;
        };
        if !dst_side.opts.egress_enabled {
            return false;
        }
        if let Some(src_id) = src {
            let Some(src_side) = st.sides.get(src_id).and_then(|side| side.as_ref()) else {
                return false;
            };
            if !src_side.opts.ingress_enabled {
                return false;
            }
        }
        let base_allowed = st
            .route_overrides
            .get(&(src, dst))
            .copied()
            .unwrap_or_else(|| self.default_route_enabled(src, dst));
        if !base_allowed {
            return false;
        }

        let Some(ty) = ty else {
            return true;
        };
        if st
            .typed_route_overrides
            .keys()
            .any(|(typed_src, typed_ty, _)| *typed_src == src && *typed_ty == ty.as_u32())
        {
            return st
                .typed_route_overrides
                .get(&(src, ty.as_u32(), dst))
                .copied()
                .unwrap_or(false);
        }
        true
    }

    fn eligible_side_ids_locked(
        &self,
        st: &RouterInner,
        src: Option<RouterSideId>,
        ty: Option<DataType>,
        restrict_link_local: bool,
    ) -> Vec<RouterSideId> {
        st.sides
            .iter()
            .enumerate()
            .filter_map(|(side_id, side)| {
                let side = side.as_ref()?;
                if restrict_link_local && !side.opts.link_local_enabled {
                    return None;
                }
                if self.route_allowed_locked(st, src, ty, side_id) {
                    Some(side_id)
                } else {
                    None
                }
            })
            .collect()
    }

    fn apply_route_selection_locked(
        &self,
        st: &mut RouterInner,
        src: Option<RouterSideId>,
        mut sides: Vec<RouterSideId>,
        origin: RouteSelectionOrigin,
    ) -> Vec<RouterSideId> {
        if sides.len() <= 1 {
            return sides;
        }

        let selection_mode = st.source_route_modes.get(&src).copied();
        if selection_mode.is_none() && origin == RouteSelectionOrigin::Discovered {
            return self.apply_adaptive_discovery_selection_locked(st, src, sides);
        }

        match selection_mode.unwrap_or(RouteSelectionMode::Fanout) {
            RouteSelectionMode::Fanout => sides,
            RouteSelectionMode::Weighted => {
                sides.sort_unstable();
                let total_weight = sides.iter().fold(0_u64, |acc, side| {
                    acc + u64::from(st.route_weights.get(&(src, *side)).copied().unwrap_or(1))
                });
                if total_weight == 0 {
                    return Vec::new();
                }
                let cursor = st.route_selection_cursors.entry(src).or_insert(0);
                let pick = *cursor % total_weight;
                *cursor = cursor.wrapping_add(1);
                let mut remaining = pick;
                for side in sides {
                    let weight =
                        u64::from(st.route_weights.get(&(src, side)).copied().unwrap_or(1));
                    if remaining < weight {
                        return vec![side];
                    }
                    remaining -= weight;
                }
                Vec::new()
            }
            RouteSelectionMode::Failover => {
                sides.sort_by_key(|side| {
                    (
                        st.route_priorities.get(&(src, *side)).copied().unwrap_or(0),
                        *side,
                    )
                });
                sides.truncate(1);
                sides
            }
        }
    }

    fn apply_adaptive_discovery_selection_locked(
        &self,
        st: &mut RouterInner,
        src: Option<RouterSideId>,
        mut sides: Vec<RouterSideId>,
    ) -> Vec<RouterSideId> {
        sides.sort_unstable();
        let mut unmeasured: Vec<_> = sides
            .iter()
            .copied()
            .filter(|side| !st.adaptive_route_stats.contains_key(side))
            .collect();
        if !unmeasured.is_empty() {
            let cursor = st.route_selection_cursors.entry(src).or_insert(0);
            let pick = (*cursor as usize) % unmeasured.len();
            *cursor = cursor.wrapping_add(1);
            return vec![unmeasured.swap_remove(pick)];
        }

        let total_weight = sides.iter().fold(0_u64, |acc, side| {
            acc + st
                .adaptive_route_stats
                .get(side)
                .map(AdaptiveRouteStats::weight)
                .unwrap_or(1)
        });
        if total_weight == 0 {
            sides.truncate(1);
            return sides;
        }

        let cursor = st.route_selection_cursors.entry(src).or_insert(0);
        let pick = *cursor % total_weight;
        *cursor = cursor.wrapping_add(1);
        let mut remaining = pick;
        for side in sides {
            let weight = st
                .adaptive_route_stats
                .get(&side)
                .map(AdaptiveRouteStats::weight)
                .unwrap_or(1);
            if remaining < weight {
                return vec![side];
            }
            remaining -= weight;
        }
        Vec::new()
    }

    fn record_side_tx_sample(
        &self,
        side: RouterSideId,
        bytes: usize,
        started_ms: u64,
        ended_ms: u64,
    ) {
        let sample_ms = ended_ms.saturating_sub(started_ms).max(1);
        let sample_bps = ((bytes as u128).saturating_mul(1000) / u128::from(sample_ms))
            .min(u128::from(u64::MAX)) as u64;
        let mut st = self.state.lock();
        st.adaptive_route_stats
            .entry(side)
            .or_default()
            .observe(sample_bps, ended_ms);
    }

    fn router_item_wire_len(data: &RouterItem) -> TelemetryResult<usize> {
        match data {
            RouterItem::Packet(pkt) => Ok(serialize::serialize_packet(pkt).len()),
            RouterItem::Serialized(bytes) => Ok(bytes.len()),
        }
    }

    #[inline]
    fn reliable_control_target_packet_id(data: &RouterItem) -> TelemetryResult<Option<u64>> {
        match data {
            RouterItem::Packet(pkt) => {
                if pkt.data_type() != DataType::ReliableAck
                    || !Self::is_end_to_end_ack_sender(pkt.sender())
                {
                    return Ok(None);
                }
                Self::decode_end_to_end_reliable_ack(pkt.payload()).map(Some)
            }
            RouterItem::Serialized(bytes) => {
                let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                if pkt.data_type() != DataType::ReliableAck
                    || !Self::is_end_to_end_ack_sender(pkt.sender())
                {
                    return Ok(None);
                }
                Self::decode_end_to_end_reliable_ack(pkt.payload()).map(Some)
            }
        }
    }

    fn decode_end_to_end_reliable_ack(payload: &[u8]) -> TelemetryResult<u64> {
        if payload.len() != 8 {
            return Err(TelemetryError::Deserialize("bad reliable e2e ack payload"));
        }
        Ok(u64::from_le_bytes(payload[0..8].try_into().unwrap()))
    }

    #[inline]
    fn sender_hash(sender: &str) -> u64 {
        hash_bytes_u64(0x517C_C1B7_2722_0A95, sender.as_bytes())
    }

    #[inline]
    fn is_end_to_end_ack_sender(sender: &str) -> bool {
        sender == Self::END_TO_END_ACK_SENDER || sender.starts_with(Self::END_TO_END_ACK_PREFIX)
    }

    fn decode_end_to_end_ack_sender_hash(sender: &str) -> Option<u64> {
        if let Some(ack_sender) = sender.strip_prefix(Self::END_TO_END_ACK_PREFIX)
            && !ack_sender.is_empty()
        {
            return Some(Self::sender_hash(ack_sender));
        }
        None
    }

    fn encode_end_to_end_ack_sender(&self) -> String {
        format!("{}{}", Self::END_TO_END_ACK_PREFIX, self.sender)
    }

    #[cfg(feature = "discovery")]
    fn is_end_to_end_destination_sender(sender: &str) -> bool {
        sender != "RELAY" && !Self::is_end_to_end_ack_sender(sender)
    }

    fn encode_end_to_end_reliable_ack(packet_id: u64) -> Arc<[u8]> {
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&packet_id.to_le_bytes());
        Arc::from(payload)
    }

    fn note_reliable_return_route(&self, side: RouterSideId, packet_id: u64) {
        let mut st = self.state.lock();
        Self::remember_reliable_return_route_locked(&mut st, packet_id);
        st.reliable_return_routes
            .insert(packet_id, ReliableReturnRouteState { side });
    }

    fn remember_reliable_return_route_locked(st: &mut RouterInner, packet_id: u64) {
        let cap = RELIABLE_MAX_RETURN_ROUTES.max(1);
        st.reliable_return_route_order
            .retain(|id| st.reliable_return_routes.contains_key(id) && *id != packet_id);
        while st.reliable_return_route_order.len() >= cap {
            if let Some(oldest) = st.reliable_return_route_order.pop_front() {
                st.reliable_return_routes.remove(&oldest);
            } else {
                break;
            }
        }
        st.reliable_return_route_order.push_back(packet_id);
    }

    fn remember_end_to_end_reliable_tx_locked(st: &mut RouterInner, packet_id: u64) {
        let cap = RELIABLE_MAX_END_TO_END_PENDING.max(1);
        st.end_to_end_reliable_tx_order
            .retain(|id| st.end_to_end_reliable_tx.contains_key(id) && *id != packet_id);
        while st.end_to_end_reliable_tx_order.len() >= cap {
            if let Some(oldest) = st.end_to_end_reliable_tx_order.pop_front() {
                st.end_to_end_reliable_tx.remove(&oldest);
            } else {
                break;
            }
        }
        st.end_to_end_reliable_tx_order.push_back(packet_id);
    }

    #[cfg(feature = "discovery")]
    fn expected_end_to_end_destinations_locked(
        &self,
        st: &RouterInner,
        data: &RouterItem,
    ) -> TelemetryResult<BTreeMap<u64, RouterSideId>> {
        let (eps, ty) = self.item_route_info(data)?;
        let now_ms = self.clock.now_ms();
        let restrict_link_local = Self::endpoints_are_link_local_only(&eps);
        let mut out = BTreeMap::new();
        for (&side, route) in st.discovery_routes.iter() {
            if now_ms.saturating_sub(route.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                continue;
            }
            let Some(side_ref) = st.sides.get(side).and_then(Option::as_ref) else {
                continue;
            };
            if restrict_link_local && !side_ref.opts.link_local_enabled {
                continue;
            }
            if !self.route_allowed_locked(st, None, Some(ty), side) {
                continue;
            }
            for sender_state in route.announcers.values() {
                if now_ms.saturating_sub(sender_state.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                    continue;
                }
                for board in sender_state.topology_boards.iter() {
                    if !Self::is_end_to_end_destination_sender(&board.sender_id) {
                        continue;
                    }
                    if eps
                        .iter()
                        .copied()
                        .any(|ep| board.reachable_endpoints.contains(&ep))
                    {
                        out.insert(Self::sender_hash(&board.sender_id), side);
                        if out.len() >= RELIABLE_MAX_END_TO_END_PENDING.max(1) {
                            return Ok(out);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn register_end_to_end_reliable_tx(&self, data: &RouterItem) -> TelemetryResult<()> {
        let packet_id = Self::get_hash(data);
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        #[cfg(feature = "discovery")]
        let pending_destinations = self.expected_end_to_end_destinations_locked(&st, data)?;
        #[cfg(not(feature = "discovery"))]
        let pending_destinations = BTreeMap::new();
        let tracked_destinations = !pending_destinations.is_empty();
        Self::remember_end_to_end_reliable_tx_locked(&mut st, packet_id);
        st.end_to_end_reliable_tx.insert(
            packet_id,
            EndToEndReliableSent {
                data: data.clone(),
                pending_destinations,
                tracked_destinations,
                last_send_ms: now_ms,
                retries: 0,
                queued: false,
            },
        );
        Ok(())
    }

    #[cfg(feature = "discovery")]
    fn reconcile_end_to_end_reliable_destinations_locked(
        &self,
        st: &mut RouterInner,
    ) -> TelemetryResult<()> {
        let packet_ids: Vec<u64> = st.end_to_end_reliable_tx.keys().copied().collect();
        let mut completed = Vec::new();

        for packet_id in packet_ids {
            let Some(data) = st
                .end_to_end_reliable_tx
                .get(&packet_id)
                .map(|sent| sent.data.clone())
            else {
                continue;
            };
            let expected = self.expected_end_to_end_destinations_locked(st, &data)?;
            let Some(sent) = st.end_to_end_reliable_tx.get_mut(&packet_id) else {
                continue;
            };
            if !sent.tracked_destinations {
                continue;
            }
            sent.pending_destinations
                .retain(|sender_hash, side| match expected.get(sender_hash) {
                    Some(next_side) => {
                        *side = *next_side;
                        true
                    }
                    None => false,
                });
            if sent.pending_destinations.is_empty() {
                completed.push(packet_id);
            }
        }

        for packet_id in completed {
            st.end_to_end_reliable_tx.remove(&packet_id);
        }

        Ok(())
    }

    fn side_supports_end_to_end_reliable_locked(st: &RouterInner, side: RouterSideId) -> bool {
        let Some(side_ref) = st.sides.get(side).and_then(Option::as_ref) else {
            return false;
        };
        side_ref.opts.reliable_enabled
            && matches!(side_ref.tx_handler, RouterTxHandlerFn::Serialized(_))
    }

    #[cfg(feature = "discovery")]
    fn side_has_multiple_announcers_locked(
        &self,
        st: &RouterInner,
        side: RouterSideId,
        now_ms: u64,
    ) -> bool {
        st.discovery_routes
            .get(&side)
            .map(|route| {
                route
                    .announcers
                    .values()
                    .filter(|sender| {
                        now_ms.saturating_sub(sender.last_seen_ms) <= DISCOVERY_ROUTE_TTL_MS
                    })
                    .take(2)
                    .count()
                    > 1
            })
            .unwrap_or(false)
    }

    #[cfg(not(feature = "discovery"))]
    fn side_has_multiple_announcers_locked(
        &self,
        _st: &RouterInner,
        _side: RouterSideId,
        _now_ms: u64,
    ) -> bool {
        false
    }

    fn queue_end_to_end_reliable_ack(&self, pkt: &Packet) -> TelemetryResult<()> {
        let ack_sender = self.encode_end_to_end_ack_sender();
        let ack = Packet::new(
            DataType::ReliableAck,
            message_meta(DataType::ReliableAck).endpoints,
            ack_sender.as_str(),
            self.packet_timestamp_ms(),
            Self::encode_end_to_end_reliable_ack(pkt.packet_id()),
        )?;
        self.tx_queue_item_with_flags(RouterTxItem::Broadcast(RouterItem::Packet(ack)), true)
    }

    fn queue_end_to_end_reliable_retransmit(&self, packet_id: u64) -> TelemetryResult<()> {
        {
            let mut st = self.state.lock();
            let Some(sent) = st.end_to_end_reliable_tx.get_mut(&packet_id) else {
                return Ok(());
            };
            if sent.queued {
                return Ok(());
            }
            sent.queued = true;
        }
        self.tx_queue_item_with_priority(
            RouterTxItem::EndToEndReplay { packet_id },
            true,
            Self::router_item_priority_bumped(DataType::ReliableAck),
        )
    }

    fn end_to_end_retransmit_sides(
        &self,
        packet_id: u64,
    ) -> Option<(RouterItem, Vec<RouterSideId>)> {
        let mut st = self.state.lock();
        let (data, tracked_destinations, mut sides) = {
            let sent = st.end_to_end_reliable_tx.get_mut(&packet_id)?;
            sent.queued = false;
            sent.last_send_ms = self.clock.now_ms();
            let data = sent.data.clone();
            let tracked_destinations = sent.tracked_destinations;
            let sides: Vec<RouterSideId> = sent.pending_destinations.values().copied().collect();
            (data, tracked_destinations, sides)
        };
        if tracked_destinations && sides.is_empty() {
            st.end_to_end_reliable_tx.remove(&packet_id);
            return None;
        }
        sides.sort_unstable();
        sides.dedup();
        Some((data, sides))
    }

    fn router_item_priority(data: &RouterItem) -> TelemetryResult<u8> {
        let ty = match data {
            RouterItem::Packet(pkt) => pkt.data_type(),
            RouterItem::Serialized(bytes) => serialize::peek_envelope(bytes.as_ref())?.ty,
        };
        Ok(message_priority(ty))
    }

    #[inline]
    fn router_item_priority_bumped(ty: DataType) -> u8 {
        message_priority(ty).saturating_add(16)
    }

    #[inline]
    fn is_side_tx_busy(err: &TelemetryError) -> bool {
        matches!(err, TelemetryError::Io("side tx busy"))
    }

    #[cfg(feature = "timesync")]
    fn timesync_has_usable_time_locked(st: &TimeSyncRuntime, now_mono_ns: u64) -> bool {
        st.disciplined_clock.read_unix_ms(now_mono_ns).is_some()
            || st
            .clock
            .current_time(now_mono_ns)
            .and_then(|reading| reading.unix_time_ms)
            .is_some()
    }

    #[cfg(feature = "timesync")]
    fn reconcile_pending_timesync_request_locked(
        st: &mut TimeSyncRuntime,
        leader: &Option<TimeSyncLeader>,
        now_ms: u64,
    ) {
        let active_remote = match leader {
            Some(TimeSyncLeader::Remote(remote)) => Some(remote.sender.as_str()),
            _ => None,
        };
        let should_clear = st
            .pending_request
            .as_ref()
            .is_some_and(|pending| Some(pending.source.as_str()) != active_remote);
        if should_clear {
            st.pending_request = None;
            st.next_request_mono_ms = now_ms;
        }
    }

    ///Helper function for relay_send
    #[inline]
    fn enqueue_to_sides(
        &self,
        data: RouterItem,
        exclude: Option<RouterSideId>,
        ignore_local: bool,
    ) -> TelemetryResult<()> {
        let plan = self.remote_side_plan(&data, exclude)?;
        let mut st = self.state.lock();
        let priority = Self::router_item_priority(&data)?;

        let RemoteSidePlan::Target(sides) = plan;
        for idx in sides {
            st.push_transmit(TxQueued {
                item: RouterTxItem::ToSide {
                    src: exclude,
                    dst: idx,
                    data: data.clone(),
                },
                ignore_local,
                priority,
            })?;
        }

        Ok(())
    }

    fn relay_send(
        &self,
        data: RouterItem,
        src: Option<RouterSideId>,
        called_from_queue: bool,
    ) -> TelemetryResult<()> {
        if called_from_queue {
            return self.enqueue_to_sides(data, src, true);
        }

        let RemoteSidePlan::Target(sides) = self.remote_side_plan(&data, src)?;
        for side in sides {
            self.tx_item_impl(
                RouterTxItem::ToSide {
                    src,
                    dst: side,
                    data: data.clone(),
                },
                true,
            )?;
        }

        Ok(())
    }

    fn item_route_info(&self, data: &RouterItem) -> TelemetryResult<(Vec<DataEndpoint>, DataType)> {
        match data {
            RouterItem::Packet(pkt) => {
                pkt.validate()?;
                let mut eps = pkt.endpoints().to_vec();
                eps.sort_unstable();
                eps.dedup();
                Ok((eps, pkt.data_type()))
            }
            RouterItem::Serialized(bytes) => {
                let env = serialize::peek_envelope(bytes.as_ref())?;
                let mut eps: Vec<DataEndpoint> = env.endpoints.iter().copied().collect();
                eps.sort_unstable();
                eps.dedup();
                Ok((eps, env.ty))
            }
        }
    }

    fn endpoints_are_link_local_only(eps: &[DataEndpoint]) -> bool {
        !eps.is_empty() && eps.iter().all(|ep| ep.is_link_local_only())
    }

    fn should_route_remote(
        &self,
        data: &RouterItem,
        exclude: Option<RouterSideId>,
    ) -> TelemetryResult<bool> {
        #[cfg(feature = "discovery")]
        {
            let RemoteSidePlan::Target(sides) = self.remote_side_plan(data, exclude)?;
            Ok(!sides.is_empty())
        }

        #[cfg(not(feature = "discovery"))]
        {
            let (eps, ty) = self.item_route_info(data)?;
            if !(has_nonlocal_endpoint(&eps, &self.cfg) || force_remote_for_type(ty)) {
                return Ok(false);
            }
            let st = self.state.lock();
            Ok(!self
                .eligible_side_ids_locked(
                    &st,
                    exclude,
                    Some(ty),
                    Self::endpoints_are_link_local_only(&eps),
                )
                .is_empty())
        }
    }

    fn remote_side_plan(
        &self,
        data: &RouterItem,
        exclude: Option<RouterSideId>,
    ) -> TelemetryResult<RemoteSidePlan> {
        #[cfg(feature = "discovery")]
        {
            let (eps, ty) = self.item_route_info(data)?;
            let preferred_packet_id = Self::reliable_control_target_packet_id(data)?;
            if discovery::is_discovery_type(ty) {
                let mut st = self.state.lock();
                let sides = self.eligible_side_ids_locked(&st, exclude, Some(ty), false);
                return Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    exclude,
                    sides,
                    RouteSelectionOrigin::Flood,
                )));
            }

            #[cfg(feature = "timesync")]
            let bootstrap_timesync = matches!(
                ty,
                DataType::TimeSyncAnnounce | DataType::TimeSyncRequest | DataType::TimeSyncResponse
            );
            #[cfg(not(feature = "timesync"))]
            let bootstrap_timesync = false;

            #[cfg(feature = "timesync")]
            let preferred_timesync_source = self.preferred_timesync_route_source(data, ty)?;
            #[cfg(not(feature = "timesync"))]
            let preferred_timesync_source: Option<String> = None;

            let mut st = self.state.lock();
            if let Some(packet_id) = preferred_packet_id {
                let target_side = st
                    .reliable_return_routes
                    .get(&packet_id)
                    .map(|route| route.side);
                if let Some(side) = target_side
                    .filter(|side| self.route_allowed_locked(&st, exclude, Some(ty), *side))
                {
                    return Ok(RemoteSidePlan::Target(vec![side]));
                }
                return Ok(RemoteSidePlan::Target(Vec::new()));
            }
            let has_nonlocal = has_nonlocal_endpoint(&eps, &self.cfg);
            let restrict_link_local = Self::endpoints_are_link_local_only(&eps);
            let discovered_origin = if is_reliable_type(ty) {
                RouteSelectionOrigin::Flood
            } else {
                RouteSelectionOrigin::Discovered
            };
            if st.discovery_routes.is_empty() {
                if restrict_link_local {
                    let targets = self.eligible_side_ids_locked(&st, exclude, Some(ty), true);
                    return Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                        &mut st,
                        exclude,
                        targets,
                        RouteSelectionOrigin::Flood,
                    )));
                }
                return if has_nonlocal || bootstrap_timesync {
                    let targets = self.eligible_side_ids_locked(&st, exclude, Some(ty), false);
                    Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                        &mut st,
                        exclude,
                        targets,
                        RouteSelectionOrigin::Flood,
                    )))
                } else {
                    Ok(RemoteSidePlan::Target(Vec::new()))
                };
            }
            let now_ms = self.clock.now_ms();
            let mut had_exact = false;
            let mut exact_targets = Vec::new();
            let mut had_known = false;
            let mut generic_targets = Vec::new();

            for (&side, route) in st.discovery_routes.iter() {
                if exclude == Some(side)
                    || now_ms.saturating_sub(route.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS
                {
                    continue;
                }
                if restrict_link_local
                    && st
                    .sides
                    .get(side)
                    .and_then(|s| s.as_ref())
                    .map(|s| !s.opts.link_local_enabled)
                    .unwrap_or(true)
                {
                    continue;
                }
                if !self.route_allowed_locked(&st, exclude, Some(ty), side) {
                    continue;
                }
                if preferred_timesync_source.as_deref().is_some_and(|source| {
                    route.reachable_timesync_sources.iter().any(|s| s == source)
                }) {
                    had_exact = true;
                    exact_targets.push(side);
                    continue;
                }
                if eps.iter().copied().any(|ep| route.reachable.contains(&ep)) {
                    had_known = true;
                    generic_targets.push(side);
                }
            }

            if had_exact {
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    exclude,
                    exact_targets,
                    discovered_origin,
                )))
            } else if had_known {
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    exclude,
                    generic_targets,
                    discovered_origin,
                )))
            } else if restrict_link_local {
                let targets = self.eligible_side_ids_locked(&st, exclude, Some(ty), true);
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    exclude,
                    targets,
                    RouteSelectionOrigin::Flood,
                )))
            } else if has_nonlocal || bootstrap_timesync {
                let targets = self.eligible_side_ids_locked(&st, exclude, Some(ty), false);
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    exclude,
                    targets,
                    RouteSelectionOrigin::Flood,
                )))
            } else {
                Ok(RemoteSidePlan::Target(Vec::new()))
            }
        }
        #[cfg(not(feature = "discovery"))]
        {
            let (_, ty) = self.item_route_info(data)?;
            let mut st = self.state.lock();
            if let Some(packet_id) = Self::reliable_control_target_packet_id(data)? {
                let target_side = st
                    .reliable_return_routes
                    .get(&packet_id)
                    .map(|route| route.side);
                if let Some(side) = target_side
                    .filter(|side| self.route_allowed_locked(&st, exclude, Some(ty), *side))
                {
                    return Ok(RemoteSidePlan::Target(vec![side]));
                }
                return Ok(RemoteSidePlan::Target(Vec::new()));
            }
            let sides = self.eligible_side_ids_locked(&st, exclude, Some(ty), false);
            Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                &mut st,
                exclude,
                sides,
                RouteSelectionOrigin::Flood,
            )))
        }
    }

    #[cfg(feature = "discovery")]
    fn local_discovery_endpoints(&self) -> Vec<DataEndpoint> {
        let mut eps: Vec<DataEndpoint> = self.cfg.handlers.iter().map(|h| h.endpoint).collect();
        #[cfg(feature = "timesync")]
        if self.cfg.timesync_config().is_some() {
            eps.push(DataEndpoint::TimeSync);
        }
        eps.retain(|ep| !discovery::is_discovery_endpoint(*ep));
        eps.sort_unstable();
        eps.dedup();
        eps
    }

    #[cfg(feature = "discovery")]
    fn local_discovery_timesync_sources(&self, now_ms: u64) -> Vec<String> {
        #[cfg(feature = "timesync")]
        {
            let st = self.timesync.lock();
            if let Some(tracker) = st.tracker.as_ref()
                && tracker.should_serve(
                now_ms,
                Self::timesync_has_usable_time_locked(&st, self.monotonic_now_ns()),
            )
            {
                return vec![self.sender.to_owned()];
            }
        }
        Vec::new()
    }

    #[cfg(all(feature = "discovery", feature = "timesync"))]
    fn preferred_timesync_route_source(
        &self,
        data: &RouterItem,
        ty: DataType,
    ) -> TelemetryResult<Option<String>> {
        if !matches!(
            ty,
            DataType::TimeSyncAnnounce | DataType::TimeSyncRequest | DataType::TimeSyncResponse
        ) {
            return Ok(None);
        }

        match data {
            RouterItem::Packet(pkt) => match ty {
                DataType::TimeSyncRequest if pkt.sender() == self.sender => Ok(self
                    .timesync
                    .lock()
                    .tracker
                    .as_ref()
                    .and_then(|tracker| tracker.current_source().map(|src| src.sender.clone()))),
                DataType::TimeSyncAnnounce | DataType::TimeSyncResponse => {
                    Ok(Some(pkt.sender().to_owned()))
                }
                _ => Ok(None),
            },
            RouterItem::Serialized(bytes) => {
                let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                self.preferred_timesync_route_source(&RouterItem::Packet(pkt), ty)
            }
        }
    }

    #[cfg(feature = "discovery")]
    fn note_discovery_topology_change_locked(st: &mut RouterInner, now_ms: u64) {
        st.discovery_cadence.on_topology_change(now_ms);
    }

    #[cfg(feature = "discovery")]
    fn sender_topology_board_mut<'a>(
        sender_state: &'a mut DiscoverySenderState,
        sender_id: &str,
    ) -> &'a mut TopologyBoardNode {
        if let Some(idx) = sender_state
            .topology_boards
            .iter()
            .position(|board| board.sender_id == sender_id)
        {
            return &mut sender_state.topology_boards[idx];
        }
        sender_state.topology_boards.push(TopologyBoardNode {
            sender_id: sender_id.to_string(),
            reachable_endpoints: Vec::new(),
            reachable_timesync_sources: Vec::new(),
            connections: Vec::new(),
        });
        sender_state
            .topology_boards
            .last_mut()
            .expect("board inserted above")
    }

    #[cfg(feature = "discovery")]
    fn refresh_sender_topology_state(sender_state: &mut DiscoverySenderState) {
        discovery::normalize_topology_boards(&mut sender_state.topology_boards);
        let (reachable, reachable_timesync_sources) =
            discovery::summarize_topology_boards(&sender_state.topology_boards);
        sender_state.reachable = reachable;
        sender_state.reachable_timesync_sources = reachable_timesync_sources;
    }

    #[cfg(feature = "discovery")]
    fn recompute_discovery_side_state(route: &mut DiscoverySideState) {
        let mut reachable = Vec::new();
        let mut reachable_timesync_sources = Vec::new();
        let mut last_seen_ms = 0u64;
        for sender in route.announcers.values() {
            reachable.extend(sender.reachable.iter().copied());
            reachable_timesync_sources.extend(sender.reachable_timesync_sources.iter().cloned());
            last_seen_ms = last_seen_ms.max(sender.last_seen_ms);
        }
        reachable.sort_unstable();
        reachable.dedup();
        reachable_timesync_sources.sort_unstable();
        reachable_timesync_sources.dedup();
        route.reachable = reachable;
        route.reachable_timesync_sources = reachable_timesync_sources;
        route.last_seen_ms = last_seen_ms;
    }

    #[cfg(feature = "discovery")]
    fn local_discovery_topology_board(
        &self,
        st: &RouterInner,
        now_ms: u64,
        link_local_enabled: bool,
    ) -> TopologyBoardNode {
        let mut reachable_endpoints = self.local_discovery_endpoints();
        if !link_local_enabled {
            reachable_endpoints.retain(|ep| !ep.is_link_local_only());
        }
        let mut connections = Vec::new();
        for route in st.discovery_routes.values() {
            if now_ms.saturating_sub(route.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                continue;
            }
            for (sender, sender_state) in route.announcers.iter() {
                if now_ms.saturating_sub(sender_state.last_seen_ms) <= DISCOVERY_ROUTE_TTL_MS {
                    connections.push(sender.clone());
                }
            }
        }
        connections.sort_unstable();
        connections.dedup();
        TopologyBoardNode {
            sender_id: self.sender.to_string(),
            reachable_endpoints,
            reachable_timesync_sources: self.local_discovery_timesync_sources(now_ms),
            connections,
        }
    }

    #[cfg(feature = "discovery")]
    fn advertised_discovery_topology_for_link_locked(
        &self,
        st: &RouterInner,
        now_ms: u64,
        link_local_enabled: bool,
    ) -> Vec<TopologyBoardNode> {
        let mut boards = vec![self.local_discovery_topology_board(st, now_ms, link_local_enabled)];
        for route in st.discovery_routes.values() {
            if now_ms.saturating_sub(route.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                continue;
            }
            for (announcer, sender_state) in route.announcers.iter() {
                if now_ms.saturating_sub(sender_state.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                    continue;
                }
                let mut sender_boards = sender_state.topology_boards.clone();
                if sender_boards.is_empty() {
                    sender_boards.push(TopologyBoardNode {
                        sender_id: announcer.clone(),
                        reachable_endpoints: sender_state.reachable.clone(),
                        reachable_timesync_sources: sender_state.reachable_timesync_sources.clone(),
                        connections: vec![self.sender.to_string()],
                    });
                } else if let Some(board) = sender_boards
                    .iter_mut()
                    .find(|board| board.sender_id == *announcer)
                {
                    board.connections.push(self.sender.to_string());
                }
                if !link_local_enabled {
                    for board in sender_boards.iter_mut() {
                        board
                            .reachable_endpoints
                            .retain(|ep| !ep.is_link_local_only());
                    }
                }
                discovery::merge_topology_boards(&mut boards, &sender_boards);
            }
        }
        discovery::normalize_topology_boards(&mut boards);
        boards
    }

    #[cfg(feature = "discovery")]
    fn prune_discovery_routes_locked(st: &mut RouterInner, now_ms: u64) -> bool {
        let before = st.discovery_routes.clone();
        st.discovery_routes.retain(|_, route| {
            route.announcers.retain(|_, sender| {
                now_ms.saturating_sub(sender.last_seen_ms) <= DISCOVERY_ROUTE_TTL_MS
            });
            Self::recompute_discovery_side_state(route);
            !route.announcers.is_empty()
        });
        st.discovery_routes != before
    }

    #[cfg(feature = "discovery")]
    fn advertised_discovery_endpoints_for_link_locked(
        &self,
        st: &RouterInner,
        now_ms: u64,
        link_local_enabled: bool,
    ) -> Vec<DataEndpoint> {
        let (reachable_endpoints, _) = discovery::summarize_topology_boards(
            &self.advertised_discovery_topology_for_link_locked(st, now_ms, link_local_enabled),
        );
        reachable_endpoints
            .into_iter()
            .filter(|ep| {
                !discovery::is_discovery_endpoint(*ep)
                    && (link_local_enabled || !ep.is_link_local_only())
            })
            .collect()
    }

    #[cfg(feature = "discovery")]
    fn advertised_discovery_timesync_sources_for_link_locked(
        &self,
        st: &RouterInner,
        now_ms: u64,
    ) -> Vec<String> {
        let (_, sources) = discovery::summarize_topology_boards(
            &self.advertised_discovery_topology_for_link_locked(st, now_ms, true),
        );
        sources
    }

    #[cfg(feature = "discovery")]
    fn queue_discovery_announce(&self) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let per_side = {
            let mut st = self.state.lock();
            if Self::prune_discovery_routes_locked(&mut st, now_ms) {
                self.reconcile_end_to_end_reliable_destinations_locked(&mut st)?;
                Self::note_discovery_topology_change_locked(&mut st, now_ms);
            }
            st.fit_discovery_budget();
            if st.sides.iter().all(|side| side.is_none()) {
                return Ok(());
            }
            st.discovery_cadence.on_announce_sent(now_ms);
            st.sides
                .iter()
                .enumerate()
                .filter_map(|(side_id, side)| {
                    let side = side.as_ref()?;
                    if !self.route_allowed_locked(
                        &st,
                        None,
                        Some(DataType::DiscoveryAnnounce),
                        side_id,
                    ) {
                        return None;
                    }
                    let endpoints = self.advertised_discovery_endpoints_for_link_locked(
                        &st,
                        now_ms,
                        side.opts.link_local_enabled,
                    );
                    let timesync_sources =
                        self.advertised_discovery_timesync_sources_for_link_locked(&st, now_ms);
                    let topology = self.advertised_discovery_topology_for_link_locked(
                        &st,
                        now_ms,
                        side.opts.link_local_enabled,
                    );
                    Some((side_id, endpoints, timesync_sources, topology))
                })
                .collect::<Vec<_>>()
        };
        for (side_id, endpoints, timesync_sources, topology) in per_side {
            let pkt = discovery::build_discovery_schema(self.sender, now_ms)?;
            self.tx_queue_item_with_flags(
                RouterTxItem::ToSide {
                    src: None,
                    dst: side_id,
                    data: RouterItem::Packet(pkt),
                },
                true,
            )?;
            if !endpoints.is_empty() {
                let pkt =
                    discovery::build_discovery_announce(self.sender, now_ms, endpoints.as_slice())?;
                self.tx_queue_item_with_flags(
                    RouterTxItem::ToSide {
                        src: None,
                        dst: side_id,
                        data: RouterItem::Packet(pkt),
                    },
                    true,
                )?;
            }
            if !timesync_sources.is_empty() {
                let pkt = discovery::build_discovery_timesync_sources(
                    self.sender,
                    now_ms,
                    timesync_sources.as_slice(),
                )?;
                self.tx_queue_item_with_flags(
                    RouterTxItem::ToSide {
                        src: None,
                        dst: side_id,
                        data: RouterItem::Packet(pkt),
                    },
                    true,
                )?;
            }
            if !topology.is_empty() {
                let pkt = discovery::build_discovery_topology(self.sender, now_ms, &topology)?;
                self.tx_queue_item_with_flags(
                    RouterTxItem::ToSide {
                        src: None,
                        dst: side_id,
                        data: RouterItem::Packet(pkt),
                    },
                    true,
                )?;
            }
        }
        Ok(())
    }

    #[cfg(feature = "discovery")]
    fn poll_discovery_announce(&self) -> TelemetryResult<bool> {
        let now_ms = self.clock.now_ms();
        let due = {
            let mut st = self.state.lock();
            let removed = Self::prune_discovery_routes_locked(&mut st, now_ms);
            if removed {
                self.reconcile_end_to_end_reliable_destinations_locked(&mut st)?;
                Self::note_discovery_topology_change_locked(&mut st, now_ms);
            }
            st.fit_discovery_budget();
            let has_any = st.sides.iter().enumerate().any(|(side_id, side)| {
                let Some(side) = side.as_ref() else {
                    return false;
                };
                if !self.route_allowed_locked(&st, None, Some(DataType::DiscoveryAnnounce), side_id)
                {
                    return false;
                }
                let _ = side;
                true
            });
            if st.sides.is_empty() || !has_any {
                return Ok(false);
            }
            st.discovery_cadence.due(now_ms)
        };
        if !due {
            return Ok(false);
        }
        self.queue_discovery_announce()?;
        Ok(true)
    }

    #[cfg(feature = "discovery")]
    fn learn_discovery_packet(
        &self,
        pkt: &Packet,
        src: Option<RouterSideId>,
    ) -> TelemetryResult<bool> {
        if !discovery::is_discovery_type(pkt.data_type()) {
            return Ok(false);
        }
        let Some(side) = src else {
            return Ok(true);
        };
        if pkt.sender() == self.sender {
            return Ok(true);
        }
        if pkt.data_type() == DataType::DiscoverySchema {
            let snapshot = discovery::decode_discovery_schema(pkt)?;
            let incoming_cost = crate::config::owned_schema_byte_cost(&snapshot);
            let mut st = self.state.lock();
            st.make_shared_queue_room(incoming_cost, RouterQueueKind::Discovery)?;
            drop(st);
            let report =
                crate::config::merge_owned_schema_snapshot_with_budget(snapshot, MAX_QUEUE_BUDGET)?;
            if report.changed() {
                let mut st = self.state.lock();
                st.fit_discovery_budget();
                Self::note_discovery_topology_change_locked(&mut st, self.clock.now_ms());
            }
            return Ok(true);
        }
        let mut st = self.state.lock();
        let now_ms = self.clock.now_ms();
        let mut route = st.discovery_routes.get(&side).cloned().unwrap_or_default();
        let side_link_local_enabled = st
            .sides
            .get(side)
            .and_then(|entry| entry.as_ref())
            .map(|side_ref| side_ref.opts.link_local_enabled)
            .unwrap_or(false);
        let mut sender_state = route
            .announcers
            .get(pkt.sender())
            .cloned()
            .unwrap_or_default();
        let changed = match pkt.data_type() {
            DataType::DiscoveryAnnounce => {
                let mut reachable = discovery::decode_discovery_announce(pkt)?;
                if !side_link_local_enabled {
                    reachable.retain(|ep| !ep.is_link_local_only());
                }
                let board = Self::sender_topology_board_mut(&mut sender_state, pkt.sender());
                let changed = board.reachable_endpoints != reachable;
                board.reachable_endpoints = reachable;
                Self::refresh_sender_topology_state(&mut sender_state);
                changed
            }
            DataType::DiscoveryTimeSyncSources => {
                let sources = discovery::decode_discovery_timesync_sources(pkt)?;
                let board = Self::sender_topology_board_mut(&mut sender_state, pkt.sender());
                let changed = board.reachable_timesync_sources != sources;
                board.reachable_timesync_sources = sources;
                Self::refresh_sender_topology_state(&mut sender_state);
                changed
            }
            DataType::DiscoveryTopology => {
                let mut boards = discovery::decode_discovery_topology(pkt)?;
                if !side_link_local_enabled {
                    for board in boards.iter_mut() {
                        board
                            .reachable_endpoints
                            .retain(|ep| !ep.is_link_local_only());
                    }
                }
                let changed = sender_state.topology_boards != boards;
                sender_state.topology_boards = boards;
                Self::refresh_sender_topology_state(&mut sender_state);
                changed
            }
            DataType::DiscoverySchema => false,
            _ => false,
        };
        sender_state.last_seen_ms = now_ms;
        route
            .announcers
            .insert(pkt.sender().to_string(), sender_state);
        Self::recompute_discovery_side_state(&mut route);
        st.discovery_routes.insert(side, route);
        st.fit_discovery_budget();
        self.reconcile_end_to_end_reliable_destinations_locked(&mut st)?;
        if changed {
            Self::note_discovery_topology_change_locked(&mut st, now_ms);
        }
        Ok(true)
    }

    #[cfg(not(feature = "discovery"))]
    fn queue_discovery_announce(&self) -> TelemetryResult<()> {
        Ok(())
    }

    #[cfg(not(feature = "discovery"))]
    fn poll_discovery_announce(&self) -> TelemetryResult<bool> {
        Ok(false)
    }

    #[cfg(not(feature = "discovery"))]
    fn learn_discovery_packet(
        &self,
        _pkt: &Packet,
        _src: Option<RouterSideId>,
    ) -> TelemetryResult<bool> {
        Ok(false)
    }

    #[inline]
    fn reliable_key(side: RouterSideId, ty: DataType) -> (RouterSideId, u32) {
        (side, ty.as_u32())
    }

    fn reliable_tx_state_mut<'a>(
        &'a self,
        st: &'a mut RouterInner,
        side: RouterSideId,
        ty: DataType,
    ) -> &'a mut ReliableTxState {
        let key = Self::reliable_key(side, ty);
        st.reliable_tx
            .entry(key)
            .or_insert_with(|| ReliableTxState {
                next_seq: 1,
                sent_order: VecDeque::new(),
                sent: BTreeMap::new(),
            })
    }

    fn reliable_rx_state_mut<'a>(
        &'a self,
        st: &'a mut RouterInner,
        side: RouterSideId,
        ty: DataType,
    ) -> &'a mut ReliableRxState {
        let key = Self::reliable_key(side, ty);
        st.reliable_rx
            .entry(key)
            .or_insert_with(|| ReliableRxState {
                expected_seq: 1,
                buffered: BTreeMap::new(),
            })
    }

    fn reliable_control_packet(
        &self,
        control_ty: DataType,
        ty: DataType,
        seq: u32,
    ) -> TelemetryResult<Packet> {
        Packet::new(
            control_ty,
            message_meta(control_ty).endpoints,
            self.sender,
            self.packet_timestamp_ms(),
            encode_slice_le(&[ty.as_u32(), seq]),
        )
    }

    fn queue_reliable_ack(
        &self,
        side: RouterSideId,
        ty: DataType,
        seq: u32,
    ) -> TelemetryResult<()> {
        let pkt = self.reliable_control_packet(DataType::ReliableAck, ty, seq)?;
        self.tx_queue_item_with_priority(
            RouterTxItem::ToSide {
                src: None,
                dst: side,
                data: RouterItem::Packet(pkt),
            },
            true,
            message_priority(DataType::ReliableAck),
        )
    }

    fn queue_reliable_packet_request(
        &self,
        side: RouterSideId,
        ty: DataType,
        seq: u32,
    ) -> TelemetryResult<()> {
        let pkt = self.reliable_control_packet(DataType::ReliablePacketRequest, ty, seq)?;
        self.tx_queue_item_with_priority(
            RouterTxItem::ToSide {
                src: None,
                dst: side,
                data: RouterItem::Packet(pkt),
            },
            true,
            message_priority(DataType::ReliablePacketRequest),
        )
    }

    fn queue_reliable_partial_ack(
        &self,
        side: RouterSideId,
        ty: DataType,
        seq: u32,
    ) -> TelemetryResult<()> {
        let pkt = self.reliable_control_packet(DataType::ReliablePartialAck, ty, seq)?;
        self.tx_queue_item_with_priority(
            RouterTxItem::ToSide {
                src: None,
                dst: side,
                data: RouterItem::Packet(pkt),
            },
            true,
            message_priority(DataType::ReliablePartialAck),
        )
    }

    fn handle_reliable_ack(&self, side: RouterSideId, ty: DataType, ack: u32) {
        let mut st = self.state.lock();
        let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
        if matches!(reliable_mode(ty), crate::ReliableMode::Unordered) {
            tx_state.sent.remove(&ack);
            tx_state.sent_order.retain(|seq| *seq != ack);
            return;
        }

        while let Some(seq) = tx_state.sent_order.front().copied() {
            if seq > ack {
                break;
            }
            tx_state.sent_order.pop_front();
            tx_state.sent.remove(&seq);
        }
    }

    fn handle_reliable_partial_ack(&self, side: RouterSideId, ty: DataType, seq: u32) {
        let mut st = self.state.lock();
        let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
        if let Some(sent) = tx_state.sent.get_mut(&seq) {
            sent.partial_acked = true;
        }
    }

    fn queue_reliable_retransmit(
        &self,
        side: RouterSideId,
        ty: DataType,
        seq: u32,
    ) -> TelemetryResult<()> {
        let mut queued = None;
        {
            let mut st = self.state.lock();
            let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
            if let Some(sent) = tx_state.sent.get_mut(&seq)
                && !sent.queued
            {
                sent.queued = true;
                sent.partial_acked = false;
                queued = Some(sent.bytes.clone());
            }
        }

        if let Some(bytes) = queued {
            self.tx_queue_item_with_priority(
                RouterTxItem::ReliableReplay { dst: side, bytes },
                true,
                Self::router_item_priority_bumped(ty),
            )?;
        }

        Ok(())
    }

    fn send_reliable_raw_to_side(
        &self,
        side: RouterSideId,
        bytes: Arc<[u8]>,
    ) -> TelemetryResult<()> {
        let handler = {
            let st = self.state.lock();
            let side_ref = Self::side_ref(&st, side)?;
            if !side_ref.opts.egress_enabled {
                return Ok(());
            }
            side_ref.tx_handler.clone()
        };

        let Some(_side_tx_guard) = self.try_enter_side_tx() else {
            return Err(TelemetryError::Io("side tx busy"));
        };
        let started_ms = self.clock.now_ms();
        let result = match handler {
            RouterTxHandlerFn::Serialized(f) => self
                .retry(MAX_HANDLER_RETRIES, || f(bytes.as_ref()))
                .map_err(|_| TelemetryError::Io("reliable retransmit failed")),
            RouterTxHandlerFn::Packet(f) => {
                let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                self.retry(MAX_HANDLER_RETRIES, || f(&pkt))
                    .map_err(|_| TelemetryError::Io("reliable retransmit failed"))
            }
        };
        if result.is_ok() {
            self.record_side_tx_sample(side, bytes.len(), started_ms, self.clock.now_ms());
        }
        result
    }

    fn send_reliable_to_side(&self, side: RouterSideId, data: RouterItem) -> TelemetryResult<()> {
        let (handler, opts, hop_reliable_enabled) = {
            let st = self.state.lock();
            let side_ref = Self::side_ref(&st, side)?;
            let opts = side_ref.opts;
            let hop_reliable_enabled = opts.reliable_enabled
                && self.cfg.reliable_enabled()
                && !self.side_has_multiple_announcers_locked(&st, side, self.clock.now_ms());
            (side_ref.tx_handler.clone(), opts, hop_reliable_enabled)
        };

        let RouterTxHandlerFn::Serialized(f) = &handler else {
            return self.call_side_tx_handler(side, &handler, &data);
        };

        if !hop_reliable_enabled {
            let mut adjusted_opts = opts;
            adjusted_opts.reliable_enabled = false;
            if let Some(adjusted) = self.adjust_reliable_for_side(adjusted_opts, data)? {
                return self.call_side_tx_handler(side, &handler, &adjusted);
            }
            return Ok(());
        }

        let ty = match &data {
            RouterItem::Packet(pkt) => pkt.data_type(),
            RouterItem::Serialized(bytes) => {
                serialize::peek_frame_info(bytes.as_ref())?.envelope.ty
            }
        };

        if !is_reliable_type(ty) {
            if let Some(adjusted) = self.adjust_reliable_for_side(opts, data)? {
                self.call_side_tx_handler(side, &handler, &adjusted)?;
            }
            return Ok(());
        }

        let (seq, flags) = {
            let mut st = self.state.lock();
            let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
            if tx_state.sent.len() >= RELIABLE_MAX_PENDING {
                return Err(TelemetryError::PacketTooLarge(
                    "router reliable history full",
                ));
            }
            let seq = tx_state.next_seq;
            let next = tx_state.next_seq.wrapping_add(1);
            tx_state.next_seq = if next == 0 { 1 } else { next };
            let flags = match reliable_mode(ty) {
                crate::ReliableMode::Unordered => serialize::RELIABLE_FLAG_UNORDERED,
                _ => 0,
            };
            (seq, flags)
        };

        let bytes: Arc<[u8]> = match data {
            RouterItem::Packet(pkt) => serialize::serialize_packet_with_reliable(
                &pkt,
                serialize::ReliableHeader { flags, seq, ack: 0 },
            ),
            RouterItem::Serialized(bytes) => {
                let mut v = bytes.to_vec();
                if !serialize::rewrite_reliable_header(&mut v, flags, seq, 0)? {
                    let Some(_side_tx_guard) = self.try_enter_side_tx() else {
                        return Err(TelemetryError::Io("side tx busy"));
                    };
                    let started_ms = self.clock.now_ms();
                    let result = self.retry(MAX_HANDLER_RETRIES, || f(bytes.as_ref()));
                    if result.is_ok() {
                        self.record_side_tx_sample(
                            side,
                            bytes.len(),
                            started_ms,
                            self.clock.now_ms(),
                        );
                    }
                    return result;
                }
                Arc::from(v)
            }
        };

        let Some(_side_tx_guard) = self.try_enter_side_tx() else {
            return Err(TelemetryError::Io("side tx busy"));
        };
        let started_ms = self.clock.now_ms();
        let result = self.retry(MAX_HANDLER_RETRIES, || f(bytes.as_ref()));
        if result.is_ok() {
            self.record_side_tx_sample(side, bytes.len(), started_ms, self.clock.now_ms());
        }
        result?;

        {
            let mut st = self.state.lock();
            let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
            tx_state.sent_order.push_back(seq);
            tx_state.sent.insert(
                seq,
                ReliableSent {
                    bytes: bytes.clone(),
                    last_send_ms: self.clock.now_ms(),
                    retries: 0,
                    queued: false,
                    partial_acked: false,
                },
            );
        }

        Ok(())
    }

    fn call_side_tx_handler(
        &self,
        side: RouterSideId,
        handler: &RouterTxHandlerFn,
        data: &RouterItem,
    ) -> TelemetryResult<()> {
        let Some(_side_tx_guard) = self.try_enter_side_tx() else {
            return Err(TelemetryError::Io("side tx busy"));
        };
        let started_ms = self.clock.now_ms();
        let result = self.retry(MAX_HANDLER_RETRIES, || match (handler, data) {
            (RouterTxHandlerFn::Serialized(f), RouterItem::Serialized(bytes)) => f(bytes.as_ref()),
            (RouterTxHandlerFn::Packet(f), RouterItem::Packet(pkt)) => f(pkt),
            (RouterTxHandlerFn::Serialized(f), RouterItem::Packet(pkt)) => {
                let owned = serialize::serialize_packet(pkt);
                f(owned.as_ref())
            }
            (RouterTxHandlerFn::Packet(f), RouterItem::Serialized(bytes)) => {
                let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                f(&pkt)
            }
        });
        if result.is_ok()
            && let Ok(bytes) = Self::router_item_wire_len(data)
        {
            self.record_side_tx_sample(side, bytes, started_ms, self.clock.now_ms());
        }
        result
    }

    fn adjust_reliable_for_side(
        &self,
        opts: RouterSideOptions,
        data: RouterItem,
    ) -> TelemetryResult<Option<RouterItem>> {
        if opts.reliable_enabled {
            return Ok(Some(data));
        }

        match data {
            RouterItem::Serialized(bytes) => {
                let frame = serialize::peek_frame_info(bytes.as_ref())?;
                if is_reliable_type(frame.envelope.ty)
                    && let Some(hdr) = frame.reliable
                {
                    if (hdr.flags & serialize::RELIABLE_FLAG_ACK_ONLY) != 0 {
                        return Ok(None);
                    }
                    if (hdr.flags & serialize::RELIABLE_FLAG_UNSEQUENCED) == 0 {
                        let mut v = bytes.to_vec();
                        let _ = serialize::rewrite_reliable_header(
                            &mut v,
                            serialize::RELIABLE_FLAG_UNSEQUENCED,
                            hdr.seq,
                            0,
                        )?;
                        return Ok(Some(RouterItem::Serialized(Arc::from(v))));
                    }
                }
                Ok(Some(RouterItem::Serialized(bytes)))
            }
            RouterItem::Packet(pkt) => {
                if matches!(
                    pkt.data_type(),
                    DataType::ReliableAck
                        | DataType::ReliablePartialAck
                        | DataType::ReliablePacketRequest
                ) {
                    return Ok(None);
                }
                Ok(Some(RouterItem::Packet(pkt)))
            }
        }
    }

    fn process_reliable_timeouts(&self) -> TelemetryResult<()> {
        let now = self.clock.now_ms();
        let mut requeue: Vec<(RouterSideId, DataType, u32)> = Vec::new();

        {
            let mut st = self.state.lock();
            if st.reliable_tx.is_empty() {
                return Ok(());
            }

            for ((side, ty_u32), tx_state) in st.reliable_tx.iter_mut() {
                let Some(ty) = DataType::try_from_u32(*ty_u32) else {
                    continue;
                };
                let sent_order: Vec<u32> = tx_state.sent_order.iter().copied().collect();
                for seq in sent_order {
                    let Some(sent) = tx_state.sent.get_mut(&seq) else {
                        continue;
                    };
                    if sent.queued || now.wrapping_sub(sent.last_send_ms) < RELIABLE_RETRANSMIT_MS {
                        continue;
                    }
                    if sent.partial_acked {
                        continue;
                    }
                    if sent.retries >= RELIABLE_MAX_RETRIES {
                        tx_state.sent.remove(&seq);
                        tx_state.sent_order.retain(|existing| *existing != seq);
                        continue;
                    }
                    sent.retries += 1;
                    requeue.push((*side, ty, seq));
                }
            }
        }

        for (side, ty, seq) in requeue {
            self.queue_reliable_retransmit(side, ty, seq)?;
        }

        Ok(())
    }

    fn process_end_to_end_reliable_timeouts(&self) -> TelemetryResult<()> {
        let now = self.clock.now_ms();
        let mut requeue = Vec::new();

        {
            let mut st = self.state.lock();
            #[cfg(feature = "discovery")]
            {
                if Self::prune_discovery_routes_locked(&mut st, now) {
                    Self::note_discovery_topology_change_locked(&mut st, now);
                }
                self.reconcile_end_to_end_reliable_destinations_locked(&mut st)?;
            }
            let packet_ids: Vec<u64> = st.end_to_end_reliable_tx.keys().copied().collect();
            for packet_id in packet_ids {
                let Some(sent) = st.end_to_end_reliable_tx.get_mut(&packet_id) else {
                    continue;
                };
                if sent.queued || now.wrapping_sub(sent.last_send_ms) < RELIABLE_RETRANSMIT_MS {
                    continue;
                }
                if sent.retries >= RELIABLE_MAX_RETRIES {
                    st.end_to_end_reliable_tx.remove(&packet_id);
                    continue;
                }
                sent.retries += 1;
                requeue.push(packet_id);
            }
        }

        for packet_id in requeue {
            self.queue_end_to_end_reliable_retransmit(packet_id)?;
        }

        Ok(())
    }

    #[cfg(feature = "timesync")]
    #[inline]
    fn monotonic_now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    #[cfg(feature = "timesync")]
    #[inline]
    fn monotonic_now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    #[cfg(feature = "timesync")]
    fn refresh_timesync_state(&self, now_mono_ms: u64) {
        let now_mono_ns = self.monotonic_now_ns();
        let mut st = self.timesync.lock();
        st.clock.prune_expired(now_mono_ms);
        let timeout_ms = st.cfg.map(|cfg| cfg.source_timeout_ms).unwrap_or(0);
        st.remote_sources
            .retain(|_, src| now_mono_ms.saturating_sub(src.last_sample_mono_ms) <= timeout_ms);
        let has_usable_time = Self::timesync_has_usable_time_locked(&st, now_mono_ns);
        let leader = if let Some(tracker) = st.tracker.as_mut() {
            let _ = tracker.refresh(now_mono_ms);
            tracker.leader(now_mono_ms, has_usable_time)
        } else {
            None
        };
        Self::reconcile_pending_timesync_request_locked(&mut st, &leader, now_mono_ms);
        if let Some(TimeSyncLeader::Remote(remote)) = leader.as_ref() {
            let target_ms = st
                .remote_sources
                .get(remote.sender.as_str())
                .map(|src| src.sample_unix_ms);
            if let Some(target_ms) = target_ms {
                st.disciplined_clock.steer_unix_ms(now_mono_ns, target_ms);
            }
        }
    }

    #[cfg(feature = "timesync")]
    /// Inserts or updates a named network-time source with an optional expiration TTL.
    pub fn update_network_time_source(
        &self,
        source: &str,
        priority: u64,
        time: PartialNetworkTime,
        ttl_ms: Option<u64>,
    ) {
        let now_ms = self.monotonic_now_ms();
        let now_ns = self.monotonic_now_ns();
        let mut st = self.timesync.lock();
        st.clock
            .update_source(source, priority, time, now_ms, now_ns, ttl_ms);
        if let Some(unix_ms) = time.to_network_time().and_then(|t| t.as_unix_ms()) {
            st.disciplined_clock.steer_unix_ms(now_ns, unix_ms);
        }
    }

    #[cfg(feature = "timesync")]
    fn set_network_time_source_impl(
        &self,
        source: &str,
        priority: u64,
        time: PartialNetworkTime,
        ttl_ms: Option<u64>,
    ) {
        let observed_mono_ms = self.monotonic_now_ms();
        let observed_mono_ns = self.monotonic_now_ns();
        let mut st = self.timesync.lock();
        let commit_mono_ms = self.monotonic_now_ms();
        let commit_mono_ns = self.monotonic_now_ns();
        let adjusted = if let Some(base) = time.to_network_time() {
            let elapsed_ns = commit_mono_ns.saturating_sub(observed_mono_ns);
            advance_network_time(base, elapsed_ns)
                .map(PartialNetworkTime::from)
                .unwrap_or(time)
        } else {
            time
        };
        let adjusted_mono_ms =
            observed_mono_ms.saturating_add(commit_mono_ms.saturating_sub(observed_mono_ms));
        st.clock.update_source(
            source,
            priority,
            adjusted,
            commit_mono_ms.max(adjusted_mono_ms),
            commit_mono_ns,
            ttl_ms,
        );
        if let Some(unix_ms) = adjusted.to_network_time().and_then(|t| t.as_unix_ms()) {
            st.disciplined_clock.steer_unix_ms(commit_mono_ns, unix_ms);
        }
    }

    #[cfg(feature = "timesync")]
    fn local_network_time_priority(&self) -> u64 {
        let st = self.timesync.lock();
        st.cfg.map(|cfg| cfg.priority).unwrap_or(0)
    }

    #[cfg(feature = "timesync")]
    /// Sets the local node's network time using any combination of date, time, and sub-second fields.
    pub fn set_local_network_time(&self, time: PartialNetworkTime) {
        let priority = self.local_network_time_priority();
        if time.is_complete_date() && time.is_complete_time() {
            self.set_network_time_source_impl(LOCAL_TIMESYNC_FULL_SOURCE_ID, priority, time, None);
            let mut st = self.timesync.lock();
            st.clock.remove_source(LOCAL_TIMESYNC_DATE_SOURCE_ID);
            st.clock.remove_source(LOCAL_TIMESYNC_TOD_SOURCE_ID);
            st.clock.remove_source(LOCAL_TIMESYNC_SUBSEC_SOURCE_ID);
            return;
        }

        {
            let mut st = self.timesync.lock();
            st.clock.remove_source(LOCAL_TIMESYNC_FULL_SOURCE_ID);
        }

        if time.year.is_some() || time.month.is_some() || time.day.is_some() {
            self.set_network_time_source_impl(
                LOCAL_TIMESYNC_DATE_SOURCE_ID,
                priority,
                PartialNetworkTime {
                    year: time.year,
                    month: time.month,
                    day: time.day,
                    ..Default::default()
                },
                None,
            );
        }

        if time.hour.is_some() || time.minute.is_some() || time.second.is_some() {
            self.set_network_time_source_impl(
                LOCAL_TIMESYNC_TOD_SOURCE_ID,
                priority,
                PartialNetworkTime {
                    hour: time.hour,
                    minute: time.minute,
                    second: time.second,
                    nanosecond: time.nanosecond,
                    ..Default::default()
                },
                None,
            );
        }

        if time.nanosecond.is_some() {
            self.set_network_time_source_impl(
                LOCAL_TIMESYNC_SUBSEC_SOURCE_ID,
                priority,
                PartialNetworkTime {
                    nanosecond: time.nanosecond,
                    ..Default::default()
                },
                None,
            );
        }
    }

    #[cfg(feature = "timesync")]
    /// Removes all locally supplied network-time fragments from the assembled clock.
    pub fn clear_local_network_time(&self) {
        let mut st = self.timesync.lock();
        st.clock.remove_source(LOCAL_TIMESYNC_FULL_SOURCE_ID);
        st.clock.remove_source(LOCAL_TIMESYNC_DATE_SOURCE_ID);
        st.clock.remove_source(LOCAL_TIMESYNC_TOD_SOURCE_ID);
        st.clock.remove_source(LOCAL_TIMESYNC_SUBSEC_SOURCE_ID);
    }

    #[cfg(feature = "timesync")]
    /// Sets only the local calendar date portion of network time.
    pub fn set_local_network_date(&self, year: i32, month: u8, day: u8) {
        self.set_local_network_time(PartialNetworkTime {
            year: Some(year),
            month: Some(month),
            day: Some(day),
            ..Default::default()
        });
    }

    #[cfg(feature = "timesync")]
    /// Sets the local time of day to hour and minute precision.
    pub fn set_local_network_time_hm(&self, hour: u8, minute: u8) {
        self.set_local_network_time(PartialNetworkTime {
            hour: Some(hour),
            minute: Some(minute),
            ..Default::default()
        });
    }

    #[cfg(feature = "timesync")]
    /// Sets the local time of day to second precision.
    pub fn set_local_network_time_hms(&self, hour: u8, minute: u8, second: u8) {
        self.set_local_network_time(PartialNetworkTime {
            hour: Some(hour),
            minute: Some(minute),
            second: Some(second),
            ..Default::default()
        });
    }

    #[cfg(feature = "timesync")]
    /// Sets the local time of day with millisecond precision.
    pub fn set_local_network_time_hms_millis(
        &self,
        hour: u8,
        minute: u8,
        second: u8,
        millisecond: u16,
    ) {
        self.set_local_network_time(PartialNetworkTime {
            hour: Some(hour),
            minute: Some(minute),
            second: Some(second),
            nanosecond: Some((millisecond as u32).saturating_mul(1_000_000)),
            ..Default::default()
        });
    }

    #[cfg(feature = "timesync")]
    /// Sets the local time of day with nanosecond precision.
    pub fn set_local_network_time_hms_nanos(
        &self,
        hour: u8,
        minute: u8,
        second: u8,
        nanosecond: u32,
    ) {
        self.set_local_network_time(PartialNetworkTime {
            hour: Some(hour),
            minute: Some(minute),
            second: Some(second),
            nanosecond: Some(nanosecond),
            ..Default::default()
        });
    }

    #[cfg(feature = "timesync")]
    /// Sets a complete local date and time with second precision.
    pub fn set_local_network_datetime(
        &self,
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) {
        self.set_local_network_time(PartialNetworkTime {
            year: Some(year),
            month: Some(month),
            day: Some(day),
            hour: Some(hour),
            minute: Some(minute),
            second: Some(second),
            ..Default::default()
        });
    }

    #[cfg(feature = "timesync")]
    #[allow(clippy::too_many_arguments)]
    /// Sets a complete local date and time with millisecond precision.
    pub fn set_local_network_datetime_millis(
        &self,
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        millisecond: u16,
    ) {
        self.set_local_network_time(PartialNetworkTime {
            year: Some(year),
            month: Some(month),
            day: Some(day),
            hour: Some(hour),
            minute: Some(minute),
            second: Some(second),
            nanosecond: Some((millisecond as u32).saturating_mul(1_000_000)),
        });
    }

    #[cfg(feature = "timesync")]
    #[allow(clippy::too_many_arguments)]
    /// Sets a complete local date and time with nanosecond precision.
    pub fn set_local_network_datetime_nanos(
        &self,
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        nanosecond: u32,
    ) {
        self.set_local_network_time(PartialNetworkTime {
            year: Some(year),
            month: Some(month),
            day: Some(day),
            hour: Some(hour),
            minute: Some(minute),
            second: Some(second),
            nanosecond: Some(nanosecond),
        });
    }

    #[cfg(feature = "timesync")]
    /// Removes a previously registered named network-time source.
    pub fn clear_network_time_source(&self, source: &str) {
        let mut st = self.timesync.lock();
        st.clock.remove_source(source);
    }

    #[cfg(feature = "timesync")]
    /// Replaces the active time sync configuration and resets runtime state derived from it.
    pub fn set_timesync_config(&self, cfg: Option<TimeSyncConfig>) {
        let mut st = self.timesync.lock();
        let stale_remote_sources: Vec<String> = st.remote_sources.keys().cloned().collect();
        st.cfg = cfg;
        st.tracker = cfg.map(TimeSyncTracker::new);
        st.disciplined_clock = SlewedNetworkClock::new(
            cfg.map(|c| c.max_slew_ppm)
                .unwrap_or(TimeSyncConfig::default().max_slew_ppm),
        );
        st.remote_sources.clear();
        st.next_seq = 1;
        st.next_announce_mono_ms = 0;
        st.next_request_mono_ms = 0;
        st.pending_request = None;
        st.clock.remove_source(INTERNAL_TIMESYNC_SOURCE_ID);
        for source in stale_remote_sources {
            st.clock.remove_source(&source);
        }
        st.clock.remove_source(LOCAL_TIMESYNC_FULL_SOURCE_ID);
        st.clock.remove_source(LOCAL_TIMESYNC_DATE_SOURCE_ID);
        st.clock.remove_source(LOCAL_TIMESYNC_TOD_SOURCE_ID);
        st.clock.remove_source(LOCAL_TIMESYNC_SUBSEC_SOURCE_ID);
    }

    #[cfg(feature = "timesync")]
    /// Returns the best currently known network-time reading, if any.
    pub fn network_time(&self) -> Option<NetworkTimeReading> {
        let now_ms = self.monotonic_now_ms();
        let now_ns = self.monotonic_now_ns();
        self.refresh_timesync_state(now_ms);
        let st = self.timesync.lock();
        if let Some(unix_ms) = st.disciplined_clock.read_unix_ms(now_ns) {
            return Some(NetworkTimeReading {
                time: PartialNetworkTime::from_unix_ms(unix_ms),
                unix_time_ms: Some(unix_ms),
            });
        }
        st.clock.current_time(now_ns)
    }

    #[cfg(feature = "timesync")]
    /// Returns the current network time as Unix milliseconds when available.
    pub fn network_time_ms(&self) -> Option<u64> {
        self.network_time().and_then(|t| t.unix_time_ms)
    }

    #[cfg(feature = "timesync")]
    fn packet_timestamp_ms(&self) -> u64 {
        self.network_time_ms()
            .unwrap_or_else(|| self.monotonic_now_ms())
    }

    #[cfg(not(feature = "timesync"))]
    fn packet_timestamp_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    #[cfg(feature = "timesync")]
    fn queue_internal_timesync_request(&self, seq: u64, t1_mono_ms: u64) -> TelemetryResult<()> {
        let pkt_ts = self.packet_timestamp_ms();
        self.log_queue_ts(DataType::TimeSyncRequest, pkt_ts, &[seq, t1_mono_ms])
    }

    #[cfg(feature = "timesync")]
    fn queue_internal_timesync_response(
        &self,
        seq: u64,
        t1_mono_ms: u64,
        t2_network_ms: u64,
        t3_network_ms: u64,
        dst: Option<RouterSideId>,
    ) -> TelemetryResult<()> {
        let pkt_ts = self.packet_timestamp_ms();
        let payload = encode_slice_le(&[seq, t1_mono_ms, t2_network_ms, t3_network_ms]);
        let pkt = Packet::new(
            DataType::TimeSyncResponse,
            &[DataEndpoint::TimeSync],
            self.sender,
            pkt_ts,
            payload,
        )?;
        match dst {
            Some(dst) => self.tx_queue_item_with_flags(
                RouterTxItem::ToSide {
                    src: None,
                    dst,
                    data: RouterItem::Packet(pkt),
                },
                true,
            ),
            None => self
                .tx_queue_item_with_flags(RouterTxItem::Broadcast(RouterItem::Packet(pkt)), true),
        }
    }

    #[cfg(feature = "timesync")]
    /// Runs one time sync maintenance cycle and queues any required announce or request packets.
    pub fn poll_timesync(&self) -> TelemetryResult<bool> {
        let now_ms = self.monotonic_now_ms();
        let now_ns = self.monotonic_now_ns();
        let mut queued_any = false;
        let mut announce_priority = None;
        let mut request = None;

        {
            let mut st = self.timesync.lock();
            st.clock.prune_expired(now_ms);
            let timeout_ms = st.cfg.map(|cfg| cfg.source_timeout_ms).unwrap_or(0);
            st.remote_sources
                .retain(|_, src| now_ms.saturating_sub(src.last_sample_mono_ms) <= timeout_ms);
            let Some(cfg) = st.cfg else {
                return Ok(false);
            };

            let has_usable_time = Self::timesync_has_usable_time_locked(&st, now_ns);
            let (leader, announce_prio) = if let Some(tracker) = st.tracker.as_mut() {
                let _ = tracker.refresh(now_ms);
                (
                    tracker.leader(now_ms, has_usable_time),
                    tracker.local_announce_priority(now_ms, has_usable_time),
                )
            } else {
                (None, None)
            };
            Self::reconcile_pending_timesync_request_locked(&mut st, &leader, now_ms);

            if let Some(TimeSyncLeader::Remote(remote)) = leader.as_ref() {
                let target_ms = st
                    .remote_sources
                    .get(&remote.sender)
                    .map(|src| src.sample_unix_ms);
                if let Some(target_ms) = target_ms {
                    st.disciplined_clock.steer_unix_ms(now_ns, target_ms);
                }
            }

            if let Some(priority) = announce_prio
                && now_ms >= st.next_announce_mono_ms
            {
                announce_priority = Some(priority);
                st.next_announce_mono_ms = now_ms.saturating_add(cfg.announce_interval_ms);
            }

            if let Some(TimeSyncLeader::Remote(remote)) = leader
                && now_ms >= st.next_request_mono_ms
                && st.pending_request.is_none()
            {
                let seq = st.next_seq;
                let next = st.next_seq.wrapping_add(1);
                st.next_seq = if next == 0 { 1 } else { next };
                st.next_request_mono_ms = now_ms.saturating_add(cfg.request_interval_ms);
                st.pending_request = Some(PendingTimeSyncRequest {
                    seq,
                    t1_mono_ms: now_ms,
                    source: remote.sender,
                });
                request = Some((seq, now_ms));
            }
        }

        if let Some(priority) = announce_priority {
            let time_ms = self.packet_timestamp_ms();
            self.log_queue_ts(DataType::TimeSyncAnnounce, time_ms, &[priority, time_ms])?;
            queued_any = true;
        }
        if let Some((seq, t1_mono_ms)) = request {
            self.queue_internal_timesync_request(seq, t1_mono_ms)?;
            queued_any = true;
        }

        Ok(queued_any)
    }

    #[cfg(feature = "timesync")]
    fn handle_internal_timesync_packet(
        &self,
        pkt: &Packet,
        src: Option<RouterSideId>,
        called_from_queue: bool,
    ) -> TelemetryResult<bool> {
        let Some(cfg) = self.cfg.timesync_config() else {
            if self.should_route_remote(&RouterItem::Packet(pkt.clone()), src)? {
                self.relay_send(RouterItem::Packet(pkt.clone()), src, called_from_queue)?;
            }
            return Ok(true);
        };

        let now_mono_ms = self.monotonic_now_ms();
        let now_mono_ns = self.monotonic_now_ns();
        let mut response = None;
        let mut poll_after = false;

        {
            let mut st = self.timesync.lock();
            st.clock.prune_expired(now_mono_ms);
            let timeout_ms = st.cfg.map(|cfg| cfg.source_timeout_ms).unwrap_or(0);
            st.remote_sources
                .retain(|_, src| now_mono_ms.saturating_sub(src.last_sample_mono_ms) <= timeout_ms);
            let has_usable_time = Self::timesync_has_usable_time_locked(&st, now_mono_ns);
            if st.tracker.is_none() {
                return Ok(true);
            }

            match pkt.data_type() {
                DataType::TimeSyncAnnounce => {
                    let ann = decode_timesync_announce(pkt)?;
                    let should_steer = {
                        let tracker = st.tracker.as_mut().expect("tracker checked above");
                        let _ = tracker.handle_announce(pkt, now_mono_ms)?;
                        matches!(
                            tracker.leader(now_mono_ms, has_usable_time),
                            Some(TimeSyncLeader::Remote(ref remote)) if remote.sender == pkt.sender()
                        )
                    };
                    st.remote_sources.insert(
                        pkt.sender().to_owned(),
                        RemoteTimeSyncSource {
                            priority: ann.priority,
                            last_sample_mono_ms: now_mono_ms,
                            sample_unix_ms: ann.time_ms,
                        },
                    );
                    st.clock.update_source(
                        pkt.sender(),
                        ann.priority,
                        PartialNetworkTime::from_unix_ms(ann.time_ms),
                        now_mono_ms,
                        now_mono_ns,
                        Some(cfg.source_timeout_ms),
                    );
                    if should_steer {
                        st.disciplined_clock.steer_unix_ms(now_mono_ns, ann.time_ms);
                    }
                    poll_after = true;
                }
                DataType::TimeSyncRequest => {
                    let should_serve = {
                        let tracker = st.tracker.as_ref().expect("tracker checked above");
                        tracker.should_serve(now_mono_ms, has_usable_time)
                    };
                    if should_serve {
                        let req = decode_timesync_request(pkt)?;
                        let network_now = st
                            .disciplined_clock
                            .read_unix_ms(now_mono_ns)
                            .or_else(|| {
                                st.clock
                                    .current_time(now_mono_ns)
                                    .and_then(|t| t.unix_time_ms)
                            })
                            .unwrap_or(now_mono_ms);
                        let t2 = network_now;
                        let t3 = network_now;
                        response = Some((req.seq, req.t1_ms, t2, t3, src));
                    }
                }
                DataType::TimeSyncResponse => {
                    let resp = decode_timesync_response(pkt)?;
                    let pending = st.pending_request.clone();
                    if let Some(pending) = pending
                        && pending.seq == resp.seq
                        && pending.source == pkt.sender()
                    {
                        let source_priority = {
                            let tracker = st.tracker.as_ref().expect("tracker checked above");
                            tracker
                                .best_active_source(now_mono_ms)
                                .map(|s| s.priority)
                                .or_else(|| st.remote_sources.get(pkt.sender()).map(|s| s.priority))
                                .unwrap_or(cfg.priority)
                        };
                        let (estimate_ms, _delay_ms) = compute_network_time_sample(
                            pending.t1_mono_ms,
                            resp.t2_ms,
                            resp.t3_ms,
                            now_mono_ms,
                        );
                        st.remote_sources.insert(
                            pkt.sender().to_owned(),
                            RemoteTimeSyncSource {
                                priority: source_priority,
                                last_sample_mono_ms: now_mono_ms,
                                sample_unix_ms: estimate_ms,
                            },
                        );
                        st.clock.update_source(
                            pkt.sender(),
                            source_priority,
                            PartialNetworkTime::from_unix_ms(estimate_ms),
                            now_mono_ms,
                            now_mono_ns,
                            Some(cfg.source_timeout_ms),
                        );
                        st.disciplined_clock.steer_unix_ms(now_mono_ns, estimate_ms);
                        st.pending_request = None;
                    }
                }
                _ => {}
            }
        }

        if let Some((seq, t1, t2, t3, dst)) = response {
            self.queue_internal_timesync_response(seq, t1, t2, t3, dst)?;
        }
        if poll_after {
            let _ = self.poll_timesync()?;
        }

        if self.should_route_remote(&RouterItem::Packet(pkt.clone()), src)? {
            self.relay_send(RouterItem::Packet(pkt.clone()), src, called_from_queue)?;
        }

        Ok(true)
    }

    /// Create a new Router with an internal monotonic clock.
    #[cfg(feature = "std")]
    pub fn new(cfg: RouterConfig) -> Self {
        Self::new_with_clock(cfg, Box::new(StdMonotonicClock::default()))
    }

    /// Create a new Router with the specified router configuration and clock.
    pub fn new_with_clock(cfg: RouterConfig, clock: Box<dyn Clock + Send + Sync>) -> Self {
        #[cfg(feature = "timesync")]
        let timesync_cfg = cfg.timesync_config();
        Self {
            sender: Box::leak(cfg.sender().to_string().into_boxed_str()),
            cfg,
            state: RouterMutex::new(RouterInner {
                sides: Vec::new(),
                route_overrides: BTreeMap::new(),
                typed_route_overrides: BTreeMap::new(),
                route_weights: BTreeMap::new(),
                route_priorities: BTreeMap::new(),
                source_route_modes: BTreeMap::new(),
                route_selection_cursors: BTreeMap::new(),
                adaptive_route_stats: BTreeMap::new(),
                received_queue: BoundedDeque::new(
                    MAX_QUEUE_BUDGET,
                    STARTING_QUEUE_SIZE,
                    QUEUE_GROW_STEP,
                ),
                transmit_queue: BoundedDeque::new(
                    MAX_QUEUE_BUDGET,
                    STARTING_QUEUE_SIZE,
                    QUEUE_GROW_STEP,
                ),
                recent_rx: BoundedDeque::new(
                    RECENT_RX_QUEUE_BYTES.max(1),
                    RECENT_RX_QUEUE_BYTES.max(1),
                    QUEUE_GROW_STEP,
                ),
                reliable_tx: BTreeMap::new(),
                reliable_rx: BTreeMap::new(),
                reliable_return_routes: BTreeMap::new(),
                reliable_return_route_order: VecDeque::new(),
                end_to_end_reliable_tx: BTreeMap::new(),
                end_to_end_reliable_tx_order: VecDeque::new(),
                #[cfg(feature = "discovery")]
                discovery_routes: BTreeMap::new(),
                #[cfg(feature = "discovery")]
                discovery_cadence: DiscoveryCadenceState::default(),
            }),
            isr_rx_queue: IsrRxQueue::new(MAX_QUEUE_BUDGET, STARTING_QUEUE_SIZE, QUEUE_GROW_STEP),
            side_tx_gate: ReentryGate::new(),
            clock,
            #[cfg(feature = "timesync")]
            timesync: RouterMutex::new(TimeSyncRuntime::new(timesync_cfg)),
        }
    }

    /// Register a side whose TX callback consumes serialized packet bytes.
    ///
    /// `name` is exported in topology/debug views and does not affect routing semantics.
    /// `tx` is called whenever the router decides to send a packet toward this side.
    ///
    /// The default options disable the router's per-link reliable framing on this side. Use
    /// [`Router::add_side_serialized_with_options`] when this hop should participate in router
    /// reliable ACK/retransmit behavior.
    pub fn add_side_serialized<F>(&self, name: &'static str, tx: F) -> RouterSideId
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        self.add_side_serialized_with_options(name, tx, RouterSideOptions::default())
    }

    /// Register a serialized-output side with explicit side options.
    ///
    /// `opts.reliable_enabled` enables the router's per-hop reliable framing on this side only.
    /// That means reliable schema traffic on this side uses router-managed ACKs, packet requests,
    /// and retransmits before the bytes reach `tx`.
    ///
    /// `opts.link_local_enabled` allows link-local-only endpoints and discovery routes to use this
    /// side. `ingress_enabled` and `egress_enabled` set the initial directional policy.
    pub fn add_side_serialized_with_options<F>(
        &self,
        name: &'static str,
        tx: F,
        opts: RouterSideOptions,
    ) -> RouterSideId
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        let mut st = self.state.lock();
        let id = st.sides.len();
        st.sides.push(Some(RouterSide {
            name,
            tx_handler: RouterTxHandlerFn::Serialized(Arc::new(tx)),
            opts,
        }));
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, self.clock.now_ms());
        id
    }

    /// Register a side whose TX callback receives decoded [`Packet`] values.
    ///
    /// Packet-output sides do not preserve the serialized reliable hop framing, so
    /// `RouterSideOptions::reliable_enabled` only has effect for serialized sides.
    pub fn add_side_packet<F>(&self, name: &'static str, tx: F) -> RouterSideId
    where
        F: Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        self.add_side_packet_with_options(name, tx, RouterSideOptions::default())
    }

    /// Register a packet-output side with explicit side options.
    ///
    /// `opts.reliable_enabled` still records the operator's intent for this side, but packet-based
    /// callbacks receive decoded packets rather than the router's serialized reliable hop framing.
    /// For router-managed per-link reliable sequencing and ACKs, use a serialized side instead.
    pub fn add_side_packet_with_options<F>(
        &self,
        name: &'static str,
        tx: F,
        opts: RouterSideOptions,
    ) -> RouterSideId
    where
        F: Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        let mut st = self.state.lock();
        let id = st.sides.len();
        st.sides.push(Some(RouterSide {
            name,
            tx_handler: RouterTxHandlerFn::Packet(Arc::new(tx)),
            opts,
        }));
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, self.clock.now_ms());
        id
    }

    /// Remove a side while keeping existing side IDs stable.
    ///
    /// `side` must be an id returned earlier by one of the `add_side_*` calls. Removed side IDs
    /// are tombstoned rather than renumbered so cached IDs for the remaining sides stay valid.
    pub fn remove_side(&self, side: RouterSideId) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        {
            let mut st = self.state.lock();
            let slot = st.sides.get_mut(side).ok_or(TelemetryError::BadArg)?;
            if slot.is_none() {
                return Err(TelemetryError::BadArg);
            }
            *slot = None;
            st.route_overrides
                .retain(|(src_side, dst_side), _| *src_side != Some(side) && *dst_side != side);
            st.typed_route_overrides
                .retain(|(src_side, _, dst_side), _| *src_side != Some(side) && *dst_side != side);
            st.route_weights
                .retain(|(src_side, dst_side), _| *src_side != Some(side) && *dst_side != side);
            st.route_priorities
                .retain(|(src_side, dst_side), _| *src_side != Some(side) && *dst_side != side);
            st.source_route_modes.remove(&Some(side));
            st.route_selection_cursors.remove(&Some(side));
            st.adaptive_route_stats.remove(&side);
            st.reliable_return_routes
                .retain(|_, route| route.side != side);
            st.transmit_queue.retain(
                |queued| {
                    !matches!(&queued.item, RouterTxItem::ToSide { dst, .. } if *dst == side)
                        && !matches!(&queued.item, RouterTxItem::ReliableReplay { dst, .. } if *dst == side)
                },
            );
            st.received_queue.retain(|queued| queued.src != Some(side));
            st.reliable_tx.retain(|(side_id, _), _| *side_id != side);
            st.reliable_rx.retain(|(side_id, _), _| *side_id != side);
            #[cfg(feature = "discovery")]
            {
                st.discovery_routes.remove(&side);
                Self::note_discovery_topology_change_locked(&mut st, now_ms);
            }
        }
        let mut isr_rx = self.isr_rx_queue.try_lock()?;
        isr_rx.retain(|queued| queued.src != Some(side));
        Ok(())
    }

    /// Enable or disable ingress processing for a registered side.
    ///
    /// When `enabled` is `false`, packets tagged as arriving from `side` are rejected before local
    /// delivery, discovery learning, or forwarding.
    pub fn set_side_ingress_enabled(
        &self,
        side: RouterSideId,
        enabled: bool,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let side_ref = st
            .sides
            .get_mut(side)
            .and_then(|side| side.as_mut())
            .ok_or(TelemetryError::BadArg)?;
        side_ref.opts.ingress_enabled = enabled;
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Enable or disable egress toward a registered side.
    ///
    /// When `enabled` is `false`, the router keeps the side but stops routing packets toward it.
    pub fn set_side_egress_enabled(
        &self,
        side: RouterSideId,
        enabled: bool,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let side_ref = st
            .sides
            .get_mut(side)
            .and_then(|side| side.as_mut())
            .ok_or(TelemetryError::BadArg)?;
        side_ref.opts.egress_enabled = enabled;
        if !enabled {
            st.transmit_queue.retain(
                |queued| {
                    !matches!(&queued.item, RouterTxItem::ToSide { dst, .. } if *dst == side)
                        && !matches!(&queued.item, RouterTxItem::ReliableReplay { dst, .. } if *dst == side)
                },
            );
        }
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Set the route-selection policy for traffic originating from `src`.
    ///
    /// `src == None` targets locally-originated router TX. `src == Some(side)` targets traffic
    /// that was received from a specific ingress side. `mode` only matters when more than one
    /// destination side is currently eligible.
    pub fn set_source_route_mode(
        &self,
        src: Option<RouterSideId>,
        mode: RouteSelectionMode,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        if let Some(src) = src {
            let _ = Self::side_ref(&st, src).map_err(|_| TelemetryError::BadArg)?;
        }
        if mode == RouteSelectionMode::Fanout {
            st.source_route_modes.remove(&src);
        } else {
            st.source_route_modes.insert(src, mode);
        }
        st.route_selection_cursors.remove(&src);
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Clear any source-specific route-selection override for `src`.
    pub fn clear_source_route_mode(&self, src: Option<RouterSideId>) -> TelemetryResult<()> {
        self.set_source_route_mode(src, RouteSelectionMode::Fanout)
    }

    /// Set the weighted-routing weight from `src` toward `dst`.
    ///
    /// Higher `weight` values make `dst` more likely to be chosen when the source route mode is
    /// [`RouteSelectionMode::Weighted`]. `src == None` applies to locally-originated TX.
    pub fn set_route_weight(
        &self,
        src: Option<RouterSideId>,
        dst: RouterSideId,
        weight: u32,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let _ = Self::side_ref(&st, dst).map_err(|_| TelemetryError::BadArg)?;
        if let Some(src) = src {
            let _ = Self::side_ref(&st, src).map_err(|_| TelemetryError::BadArg)?;
        }
        st.route_weights.insert((src, dst), weight);
        st.route_selection_cursors.remove(&src);
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Clear a previously configured weighted-routing weight override.
    pub fn clear_route_weight(
        &self,
        src: Option<RouterSideId>,
        dst: RouterSideId,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let _ = Self::side_ref(&st, dst).map_err(|_| TelemetryError::BadArg)?;
        if let Some(src) = src {
            let _ = Self::side_ref(&st, src).map_err(|_| TelemetryError::BadArg)?;
        }
        st.route_weights.remove(&(src, dst));
        st.route_selection_cursors.remove(&src);
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Set the failover priority from `src` toward `dst`.
    ///
    /// Lower numeric `priority` wins when the source route mode is
    /// [`RouteSelectionMode::Failover`]. `src == None` applies to locally-originated TX.
    pub fn set_route_priority(
        &self,
        src: Option<RouterSideId>,
        dst: RouterSideId,
        priority: u32,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let _ = Self::side_ref(&st, dst).map_err(|_| TelemetryError::BadArg)?;
        if let Some(src) = src {
            let _ = Self::side_ref(&st, src).map_err(|_| TelemetryError::BadArg)?;
        }
        st.route_priorities.insert((src, dst), priority);
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Clear a previously configured failover priority override.
    pub fn clear_route_priority(
        &self,
        src: Option<RouterSideId>,
        dst: RouterSideId,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let _ = Self::side_ref(&st, dst).map_err(|_| TelemetryError::BadArg)?;
        if let Some(src) = src {
            let _ = Self::side_ref(&st, src).map_err(|_| TelemetryError::BadArg)?;
        }
        st.route_priorities.remove(&(src, dst));
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Allow or block routing from `src` toward `dst`.
    ///
    /// `src == None` controls locally-originated router TX. `enabled == false` is the sink-like
    /// building block for disabling specific directions without changing router construction mode.
    pub fn set_route(
        &self,
        src: Option<RouterSideId>,
        dst: RouterSideId,
        enabled: bool,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let _ = Self::side_ref(&st, dst).map_err(|_| TelemetryError::BadArg)?;
        if let Some(src) = src {
            let _ = Self::side_ref(&st, src).map_err(|_| TelemetryError::BadArg)?;
        }
        st.route_overrides.insert((src, dst), enabled);
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Allow or block routing for a specific `DataType` from `src` toward `dst`.
    ///
    /// Typed route rules act as allowlists once any rule exists for the `(src, ty)` pair.
    /// `src == None` targets locally-originated router TX.
    pub fn set_typed_route(
        &self,
        src: Option<RouterSideId>,
        ty: DataType,
        dst: RouterSideId,
        enabled: bool,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let _ = Self::side_ref(&st, dst).map_err(|_| TelemetryError::BadArg)?;
        if let Some(src) = src {
            let _ = Self::side_ref(&st, src).map_err(|_| TelemetryError::BadArg)?;
        }
        st.typed_route_overrides
            .insert((src, ty.as_u32(), dst), enabled);
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Clear a typed route override for the `(src, ty, dst)` triple.
    pub fn clear_typed_route(
        &self,
        src: Option<RouterSideId>,
        ty: DataType,
        dst: RouterSideId,
    ) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let _ = Self::side_ref(&st, dst).map_err(|_| TelemetryError::BadArg)?;
        if let Some(src) = src {
            let _ = Self::side_ref(&st, src).map_err(|_| TelemetryError::BadArg)?;
        }
        st.typed_route_overrides.remove(&(src, ty.as_u32(), dst));
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Clear a non-typed route override so the router falls back to default behavior.
    pub fn clear_route(&self, src: Option<RouterSideId>, dst: RouterSideId) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let _ = Self::side_ref(&st, dst).map_err(|_| TelemetryError::BadArg)?;
        if let Some(src) = src {
            let _ = Self::side_ref(&st, src).map_err(|_| TelemetryError::BadArg)?;
        }
        st.route_overrides.remove(&(src, dst));
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Queue a built-in discovery advertisement describing this router's local endpoints.
    #[cfg(feature = "discovery")]
    pub fn announce_discovery(&self) -> TelemetryResult<()> {
        self.queue_discovery_announce()
    }

    /// Queue a discovery advertisement if the adaptive cadence says one is due.
    #[cfg(feature = "discovery")]
    pub fn poll_discovery(&self) -> TelemetryResult<bool> {
        self.poll_discovery_announce()
    }

    /// Export the current discovery-driven network topology view.
    #[cfg(feature = "discovery")]
    pub fn export_topology(&self) -> TopologySnapshot {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        if Self::prune_discovery_routes_locked(&mut st, now_ms) {
            Self::note_discovery_topology_change_locked(&mut st, now_ms);
        }
        let routes = st
            .discovery_routes
            .iter()
            .filter_map(|(&side_id, route)| {
                let side = st.sides.get(side_id)?.as_ref()?;
                let announcers = route
                    .announcers
                    .iter()
                    .map(|(sender_id, sender_state)| TopologyAnnouncerRoute {
                        sender_id: sender_id.clone(),
                        reachable_endpoints: sender_state.reachable.clone(),
                        reachable_timesync_sources: sender_state.reachable_timesync_sources.clone(),
                        routers: sender_state.topology_boards.clone(),
                        last_seen_ms: sender_state.last_seen_ms,
                        age_ms: now_ms.saturating_sub(sender_state.last_seen_ms),
                    })
                    .collect();
                Some(TopologySideRoute {
                    side_id,
                    side_name: side.name,
                    reachable_endpoints: route.reachable.clone(),
                    reachable_timesync_sources: route.reachable_timesync_sources.clone(),
                    announcers,
                    last_seen_ms: route.last_seen_ms,
                    age_ms: now_ms.saturating_sub(route.last_seen_ms),
                })
            })
            .collect();
        let routers = self.advertised_discovery_topology_for_link_locked(&st, now_ms, true);
        let advertised_endpoints =
            self.advertised_discovery_endpoints_for_link_locked(&st, now_ms, true);
        let advertised_timesync_sources =
            self.advertised_discovery_timesync_sources_for_link_locked(&st, now_ms);
        TopologySnapshot {
            advertised_endpoints,
            advertised_timesync_sources,
            routers,
            routes,
            current_announce_interval_ms: st.discovery_cadence.current_interval_ms,
            next_announce_ms: st.discovery_cadence.next_announce_ms,
        }
    }

    #[cfg(test)]
    pub(crate) fn debug_end_to_end_pending_destination_count(
        &self,
        packet_id: u64,
    ) -> Option<usize> {
        let st = self.state.lock();
        st.end_to_end_reliable_tx
            .get(&packet_id)
            .map(|sent| sent.pending_destinations.len())
    }

    #[cfg(test)]
    pub(crate) fn debug_end_to_end_tracked_count(&self) -> usize {
        let st = self.state.lock();
        st.end_to_end_reliable_tx.len()
    }

    #[cfg(test)]
    pub(crate) fn debug_reliable_return_route_count(&self) -> usize {
        let st = self.state.lock();
        st.reliable_return_routes.len()
    }

    #[cfg(test)]
    pub(crate) fn debug_queue_lengths(&self) -> (usize, usize, usize) {
        let st = self.state.lock();
        (
            st.received_queue.len(),
            st.transmit_queue.len(),
            st.recent_rx.len(),
        )
    }

    #[cfg(test)]
    pub(crate) fn debug_shared_queue_bytes_used(&self) -> usize {
        let st = self.state.lock();
        st.shared_queue_bytes_used()
    }

    #[cfg(test)]
    pub(crate) fn debug_recent_rx_capacity(&self) -> (usize, usize) {
        let st = self.state.lock();
        (st.recent_rx.capacity(), st.recent_rx.max_bytes())
    }

    /// Compute a de-dupe hash for a RouterItem.
    /// Uses packet ID for Packet items, and attempts to extract packet ID from
    /// serialized bytes. If extraction fails, hashes raw bytes as a fallback.
    fn get_hash(item: &RouterItem) -> u64 {
        match item {
            RouterItem::Packet(pkt) => pkt.packet_id(),
            RouterItem::Serialized(bytes) => {
                match serialize::packet_id_from_wire(bytes.as_ref()) {
                    Ok(id) => id,
                    Err(_e) => {
                        // Fallback: if bytes are malformed (or compression feature mismatch),
                        // hash raw bytes so we can still dedupe identical network duplicates.
                        let h: u64 = 0x9E37_79B9_7F4A_7C15;
                        hash_bytes_u64(h, bytes.as_ref())
                    }
                }
            }
        }
    }

    /// Remove a hash from the ring buffer of recent RX IDs.
    fn remove_pkt_id(&self, item: &RouterItem) {
        let hash = Self::get_hash(item);
        let mut st = self.state.lock();
        st.recent_rx.remove_value(&hash);
    }

    /// Compute a de-dupe ID for a RouterItem and record it.
    /// Returns true if this item was seen recently (and should be skipped).
    fn is_duplicate_pkt(&self, item: &RouterItem) -> TelemetryResult<bool> {
        let id = Self::get_hash(item);
        let mut st = self.state.lock();
        if st.recent_rx.contains(&id) {
            Ok(true)
        } else {
            st.push_recent_rx(id)?;
            Ok(false)
        }
    }

    /// Error helper when we have a full Packet.
    ///
    /// Sends a TelemetryError packet to all local endpoints except the failed one (if any).
    /// If no local endpoints remain, falls back to `fallback_stdout`.
    fn handle_callback_error(
        &self,
        pkt: &Packet,
        dest: Option<DataEndpoint>,
        e: TelemetryError,
    ) -> TelemetryResult<()> {
        let error_msg = match dest {
            Some(failed_local) => format!(
                "Handler for endpoint {:?} failed on device {:?}: {:?}",
                failed_local, DEVICE_IDENTIFIER, e
            ),
            None => format!(
                "TX Handler failed on device {:?}: {:?}",
                DEVICE_IDENTIFIER, e
            ),
        };

        let mut recipients: Vec<DataEndpoint> = pkt
            .endpoints()
            .iter()
            .copied()
            .filter(|&ep| self.cfg.is_local_endpoint(ep))
            .collect();
        recipients.sort_unstable();
        recipients.dedup();

        if let Some(failed_local) = dest {
            recipients.retain(|&ep| ep != failed_local);
        }

        // If no local recipient exists, fall back to original packet endpoints
        // so error telemetry can still egress to remote links.
        if recipients.is_empty() {
            recipients = pkt.endpoints().to_vec();
            recipients.sort_unstable();
            recipients.dedup();
            if let Some(failed_local) = dest {
                recipients.retain(|&ep| ep != failed_local);
            }
        }

        if recipients.is_empty() {
            fallback_stdout(&error_msg);
            return Ok(());
        }

        let payload = make_error_payload(&error_msg);

        let error_pkt = Packet::new(
            DataType::TelemetryError,
            &recipients,
            self.sender,
            self.packet_timestamp_ms(),
            payload,
        )?;

        self.tx_item(RouterTxItem::Broadcast(RouterItem::Packet(error_pkt)))
    }

    // ---------- PUBLIC API: queues ----------

    /// Drain the transmit queue fully.
    #[inline]
    pub fn process_tx_queue(&self) -> TelemetryResult<()> {
        self.process_tx_queue_with_timeout(0)
    }

    /// Drain both TX and RX queues fully (same semantics as `*_with_timeout(0)`).
    #[inline]
    pub fn process_all_queues(&self) -> TelemetryResult<()> {
        self.process_all_queues_with_timeout(0)
    }

    /// Clear both the transmit and receive queues without processing.
    #[inline]
    pub fn clear_queues(&self) {
        let mut st = self.state.lock();
        st.transmit_queue.clear();
        st.received_queue.clear();
        drop(st);
        let _ = self.isr_rx_queue.clear();
    }

    /// Clear only the receive queue without processing.
    #[inline]
    pub fn clear_rx_queue(&self) {
        let mut st = self.state.lock();
        st.received_queue.clear();
        drop(st);
        let _ = self.isr_rx_queue.clear();
    }

    /// Clear only the transmit queue without processing.
    #[inline]
    pub fn clear_tx_queue(&self) {
        let mut st = self.state.lock();
        st.transmit_queue.clear();
    }

    /// Process packets in the transmit queue for up to `timeout_ms` milliseconds.
    /// If `timeout_ms == 0`, drains the queue fully.
    fn process_tx_queue_with_timeout_impl(&self, timeout_ms: u32) -> TelemetryResult<()> {
        let start = self.clock.now_ms();
        loop {
            self.process_reliable_timeouts()?;
            self.process_end_to_end_reliable_timeouts()?;
            let pkt_opt = {
                let mut st = self.state.lock();
                st.transmit_queue.pop_front()
            };
            let Some(pkt) = pkt_opt else { break };
            self.tx_item_impl(pkt.item, pkt.ignore_local)?;
            if timeout_ms != 0 && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                break;
            }
        }
        Ok(())
    }

    /// Process packets in the transmit queue for up to `timeout_ms` milliseconds.
    /// If `timeout_ms == 0`, drains the queue fully.
    pub fn process_tx_queue_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
        #[cfg(feature = "timesync")]
        let _ = self.poll_timesync()?;
        self.process_tx_queue_with_timeout_impl(timeout_ms)
    }

    /// Process a single queued receive item.
    #[inline]
    fn process_rx_queue_item(&self, item: RouterRxItem) -> TelemetryResult<()> {
        self.rx_item(&item, true)
    }

    /// Process packets in the receive queue for up to `timeout_ms` milliseconds.
    /// If `timeout_ms == 0`, drains the queue fully.
    fn process_rx_queue_with_timeout_impl(&self, timeout_ms: u32) -> TelemetryResult<()> {
        let start = self.clock.now_ms();
        loop {
            let item_opt = self.isr_rx_queue.pop_front().unwrap_or(None).or_else(|| {
                let mut st = self.state.lock();
                st.received_queue.pop_front()
            });
            let Some(item) = item_opt else { break };
            self.process_rx_queue_item(item)?;
            if timeout_ms != 0 && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                break;
            }
        }
        Ok(())
    }

    /// Process packets in the receive queue for up to `timeout_ms` milliseconds.
    /// If `timeout_ms == 0`, drains the queue fully.
    pub fn process_rx_queue_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
        #[cfg(feature = "timesync")]
        let _ = self.poll_timesync()?;
        self.process_rx_queue_with_timeout_impl(timeout_ms)
    }

    /// Process both transmit and receive queues for up to `timeout_ms` milliseconds.
    /// If `timeout_ms == 0`, drains both queues fully.
    fn process_all_queues_with_timeout_impl(&self, timeout_ms: u32) -> TelemetryResult<()> {
        if timeout_ms == 0 {
            loop {
                let mut did_any = false;
                self.process_reliable_timeouts()?;
                self.process_end_to_end_reliable_timeouts()?;

                if let Some(pkt) = {
                    let mut st = self.state.lock();
                    st.transmit_queue.pop_front()
                } {
                    self.tx_item_impl(pkt.item, pkt.ignore_local)?;
                    did_any = true;
                }

                if let Some(item) = self.isr_rx_queue.pop_front().unwrap_or(None).or_else(|| {
                    let mut st = self.state.lock();
                    st.received_queue.pop_front()
                }) {
                    self.process_rx_queue_item(item)?;
                    did_any = true;
                }

                if !did_any {
                    break;
                }
            }
            return Ok(());
        }

        let tx_budget_ms = u64::from(timeout_ms / 2);
        let rx_budget_ms = u64::from(timeout_ms) - tx_budget_ms;

        let tx_start = self.clock.now_ms();
        loop {
            self.process_reliable_timeouts()?;
            self.process_end_to_end_reliable_timeouts()?;
            let pkt_opt = {
                let mut st = self.state.lock();
                st.transmit_queue.pop_front()
            };
            let Some(pkt) = pkt_opt else { break };
            self.tx_item_impl(pkt.item, pkt.ignore_local)?;
            if self.clock.now_ms().wrapping_sub(tx_start) >= tx_budget_ms {
                break;
            }
        }

        let rx_start = self.clock.now_ms();
        loop {
            let item_opt = self.isr_rx_queue.pop_front().unwrap_or(None).or_else(|| {
                let mut st = self.state.lock();
                st.received_queue.pop_front()
            });
            let Some(item) = item_opt else { break };
            self.process_rx_queue_item(item)?;
            if self.clock.now_ms().wrapping_sub(rx_start) >= rx_budget_ms {
                break;
            }
        }

        Ok(())
    }

    /// Process both transmit and receive queues for up to `timeout_ms` milliseconds.
    /// If `timeout_ms == 0`, drains both queues fully.
    pub fn process_all_queues_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
        #[cfg(feature = "timesync")]
        let _ = self.poll_timesync()?;
        self.process_all_queues_with_timeout_impl(timeout_ms)
    }

    /// Runs one application-loop maintenance cycle.
    ///
    /// This polls built-in time sync and discovery when those features are compiled in, then
    /// drains queued TX/RX work for up to `timeout_ms` milliseconds.
    pub fn periodic(&self, timeout_ms: u32) -> TelemetryResult<()> {
        #[cfg(feature = "timesync")]
        let _ = self.poll_timesync()?;

        #[cfg(feature = "discovery")]
        {
            let _ = self.poll_discovery()?;
        }

        self.process_all_queues_with_timeout_impl(timeout_ms)
    }

    /// Runs one application-loop maintenance cycle without polling built-in time sync.
    ///
    /// Discovery is still polled when that feature is compiled in, then queued TX/RX work is
    /// drained for up to `timeout_ms` milliseconds.
    pub fn periodic_no_timesync(&self, timeout_ms: u32) -> TelemetryResult<()> {
        #[cfg(feature = "discovery")]
        {
            let _ = self.poll_discovery()?;
        }

        self.process_all_queues_with_timeout_impl(timeout_ms)
    }

    /// Enqueue an item for later transmission with flags.
    #[inline]
    fn tx_queue_item_with_flags(
        &self,
        item: RouterTxItem,
        ignore_local: bool,
    ) -> TelemetryResult<()> {
        let priority = match &item {
            RouterTxItem::Broadcast(data) => Self::router_item_priority(data)?,
            RouterTxItem::EndToEndReplay { .. } => {
                Self::router_item_priority_bumped(DataType::ReliableAck)
            }
            RouterTxItem::ToSide { data, .. } => Self::router_item_priority(data)?,
            RouterTxItem::ReliableReplay { bytes, .. } => {
                let ty = serialize::peek_envelope(bytes.as_ref())?.ty;
                Self::router_item_priority_bumped(ty)
            }
        };
        self.tx_queue_item_with_priority(item, ignore_local, priority)
    }

    #[inline]
    fn tx_queue_item_with_priority(
        &self,
        item: RouterTxItem,
        ignore_local: bool,
        priority: u8,
    ) -> TelemetryResult<()> {
        let mut st = self.state.lock();
        st.push_transmit(TxQueued {
            item,
            ignore_local,
            priority,
        })?;
        Ok(())
    }

    /// Enqueue an item for later transmission (default: local dispatch enabled).
    #[inline]
    fn tx_queue_item(&self, item: RouterTxItem) -> TelemetryResult<()> {
        self.tx_queue_item_with_flags(item, false)
    }

    #[inline]
    fn try_enter_side_tx(&self) -> Option<crate::lock::ReentryGuard<'_>> {
        self.side_tx_gate.try_enter()
    }

    #[inline]
    fn side_tx_active(&self) -> bool {
        self.side_tx_gate.is_active()
    }

    // ---------- PUBLIC API: RX queue ----------

    /// Drain the receive queue fully.
    #[inline]
    pub fn process_rx_queue(&self) -> TelemetryResult<()> {
        self.process_rx_queue_with_timeout(0)
    }

    /// Enqueue serialized bytes for RX processing as locally-originated input.
    #[inline]
    pub fn rx_serialized_queue(&self, bytes: &[u8]) -> TelemetryResult<()> {
        let data = RouterItem::Serialized(Arc::from(bytes));
        let priority = Self::router_item_priority(&data)?;
        let mut st = self.state.lock();
        st.push_received(RouterRxItem {
            src: None,
            data,
            priority,
        })?;
        Ok(())
    }

    /// ISR-safe, non-blocking enqueue of serialized bytes for RX processing.
    ///
    /// Returns `TelemetryError::Io("rx queue busy")` if another context is
    /// currently mutating the ISR RX queue.
    #[inline]
    pub fn rx_serialized_queue_isr(&self, bytes: &[u8]) -> TelemetryResult<()> {
        let data = RouterItem::Serialized(Arc::from(bytes));
        let priority = Self::router_item_priority(&data)?;
        self.isr_rx_queue.push_back_prioritized(RouterRxItem {
            src: None,
            data,
            priority,
        })
    }

    /// Enqueue a decoded packet for RX processing as locally-originated input.
    #[inline]
    pub fn rx_queue(&self, pkt: Packet) -> TelemetryResult<()> {
        pkt.validate()?;
        let data = RouterItem::Packet(pkt);
        let priority = Self::router_item_priority(&data)?;
        let mut st = self.state.lock();
        st.push_received(RouterRxItem {
            src: None,
            data,
            priority,
        })?;
        Ok(())
    }

    /// ISR-safe, non-blocking enqueue of a packet for RX processing.
    ///
    /// Returns `TelemetryError::Io("rx queue busy")` if another context is
    /// currently mutating the ISR RX queue.
    #[inline]
    pub fn rx_queue_isr(&self, pkt: Packet) -> TelemetryResult<()> {
        pkt.validate()?;
        let data = RouterItem::Packet(pkt);
        let priority = Self::router_item_priority(&data)?;
        self.isr_rx_queue.push_back_prioritized(RouterRxItem {
            src: None,
            data,
            priority,
        })
    }

    /// Enqueue a decoded packet for RX processing with an explicit ingress side.
    #[inline]
    pub fn rx_queue_from_side(&self, pkt: Packet, side: RouterSideId) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(side)?;
        pkt.validate()?;
        let data = RouterItem::Packet(pkt);
        let priority = Self::router_item_priority(&data)?;
        let mut st = self.state.lock();
        st.push_received(RouterRxItem {
            src: Some(side),
            data,
            priority,
        })?;
        Ok(())
    }

    /// ISR-safe, non-blocking enqueue of a packet with explicit source side.
    ///
    /// Returns `TelemetryError::Io("rx queue busy")` if another context is
    /// currently mutating the ISR RX queue.
    #[inline]
    pub fn rx_queue_from_side_isr(&self, pkt: Packet, side: RouterSideId) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(side)?;
        pkt.validate()?;
        let data = RouterItem::Packet(pkt);
        let priority = Self::router_item_priority(&data)?;
        self.isr_rx_queue.push_back_prioritized(RouterRxItem {
            src: Some(side),
            data,
            priority,
        })
    }

    /// Enqueue serialized bytes for RX processing with an explicit ingress side.
    #[inline]
    pub fn rx_serialized_queue_from_side(
        &self,
        bytes: &[u8],
        side: RouterSideId,
    ) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(side)?;
        let data = RouterItem::Serialized(Arc::from(bytes));
        let priority = Self::router_item_priority(&data)?;
        let mut st = self.state.lock();
        st.push_received(RouterRxItem {
            src: Some(side),
            data,
            priority,
        })?;
        Ok(())
    }

    /// ISR-safe, non-blocking enqueue of serialized bytes with source side.
    ///
    /// Returns `TelemetryError::Io("rx queue busy")` if another context is
    /// currently mutating the ISR RX queue.
    #[inline]
    pub fn rx_serialized_queue_from_side_isr(
        &self,
        bytes: &[u8],
        side: RouterSideId,
    ) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(side)?;
        let data = RouterItem::Serialized(Arc::from(bytes));
        let priority = Self::router_item_priority(&data)?;
        self.isr_rx_queue.push_back_prioritized(RouterRxItem {
            src: Some(side),
            data,
            priority,
        })
    }

    /// Retry helper function to attempt a closure multiple times.
    fn retry<F, T, E>(&self, times: usize, f: F) -> Result<T, E>
    where
        F: Fn() -> Result<T, E>,
    {
        let mut last_err = None;
        for _ in 0..times {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("times > 0"))
    }

    /// Check if the specified endpoint has a packet handler registered.
    #[inline]
    fn endpoint_has_packet_handler(&self, ep: DataEndpoint) -> bool {
        self.cfg
            .handlers
            .iter()
            .any(|h| h.endpoint == ep && matches!(h.handler, EndpointHandlerFn::Packet(_)))
    }

    /// Check if the specified endpoint has a serialized handler registered.
    #[inline]
    fn endpoint_has_serialized_handler(&self, ep: DataEndpoint) -> bool {
        self.cfg
            .handlers
            .iter()
            .any(|h| h.endpoint == ep && matches!(h.handler, EndpointHandlerFn::Serialized(_)))
    }

    fn packet_has_local_handler(&self, pkt: &Packet) -> bool {
        pkt.endpoints().iter().copied().any(|ep| {
            self.endpoint_has_packet_handler(ep) || self.endpoint_has_serialized_handler(ep)
        })
    }

    /// Call the specified endpoint handler with retries on failure.
    ///
    /// - `data` is present when called from RX processing (queue or immediate).
    /// - `pkt_for_ctx` is required for Packet handlers.
    /// - `env_for_ctx` provides header-only context when we haven't deserialized.
    fn call_handler_with_retries(
        &self,
        dest: DataEndpoint,
        handler: &EndpointHandler,
        data: Option<&[u8]>,
        pkt_for_ctx: Option<&Packet>,
        env_for_ctx: Option<&serialize::TelemetryEnvelope>,
    ) -> TelemetryResult<()> {
        let owned_tmp: Option<RouterItem>;

        let item_for_ctx: &RouterItem = match (data, pkt_for_ctx) {
            (Some(d), _) => {
                owned_tmp = Some(RouterItem::Serialized(Arc::from(d)));
                owned_tmp.as_ref().unwrap()
            }
            (None, Some(pkt)) => {
                owned_tmp = Some(RouterItem::Packet(pkt.clone()));
                owned_tmp.as_ref().unwrap()
            }
            (None, None) => {
                debug_assert!(
                    false,
                    "call_handler_with_retries called without data or packet context"
                );
                return Ok(());
            }
        };

        match (&handler.handler, data) {
            (EndpointHandlerFn::Packet(f), _) => {
                let pkt = pkt_for_ctx.expect("Packet handler requires Packet context");
                with_retries(self, dest, item_for_ctx, pkt_for_ctx, env_for_ctx, || {
                    f(pkt)
                })
            }

            (EndpointHandlerFn::Serialized(f), Some(bytes)) => {
                with_retries(self, dest, item_for_ctx, pkt_for_ctx, env_for_ctx, || {
                    f(bytes)
                })
            }

            (EndpointHandlerFn::Serialized(_), None) => Ok(()),
        }
    }

    /// Error helper when we only have an envelope (no full packet).
    ///
    /// Sends a TelemetryError packet to all local endpoints except the failed one (if any).
    /// If no local endpoints remain, falls back to `fallback_stdout`.
    fn handle_callback_error_from_env(
        &self,
        env: &serialize::TelemetryEnvelope,
        dest: Option<DataEndpoint>,
        e: TelemetryError,
    ) -> TelemetryResult<()> {
        let mut recipients: Vec<DataEndpoint> = env
            .endpoints
            .iter()
            .copied()
            .filter(|&ep| self.cfg.is_local_endpoint(ep))
            .collect();
        recipients.sort_unstable();
        recipients.dedup();
        if let Some(failed) = dest {
            recipients.retain(|&ep| ep != failed);
        }

        if recipients.is_empty() {
            recipients = env.endpoints.to_vec();
            recipients.sort_unstable();
            recipients.dedup();
            if let Some(failed) = dest {
                recipients.retain(|&ep| ep != failed);
            }
        }

        let error_msg = format!(
            "Handler for endpoint {:?} failed on device {:?}: {:?}",
            dest, DEVICE_IDENTIFIER, e
        );
        if recipients.is_empty() {
            fallback_stdout(&error_msg);
            return Ok(());
        }

        let payload = make_error_payload(&error_msg);

        let error_pkt = Packet::new(
            DataType::TelemetryError,
            &recipients,
            &env.sender.clone(),
            env.timestamp_ms,
            payload,
        )?;
        self.tx_item(RouterTxItem::Broadcast(RouterItem::Packet(error_pkt)))
    }

    fn handle_internal_reliable_packet(
        &self,
        pkt: &Packet,
        src: Option<RouterSideId>,
    ) -> TelemetryResult<bool> {
        if !matches!(
            pkt.data_type(),
            DataType::ReliableAck | DataType::ReliablePartialAck | DataType::ReliablePacketRequest
        ) {
            return Ok(false);
        }

        let Some(src) = src else {
            return Ok(false);
        };

        if pkt.data_type() == DataType::ReliableAck
            && Self::is_end_to_end_ack_sender(pkt.sender())
            && let Ok(packet_id) = Self::decode_end_to_end_reliable_ack(pkt.payload())
        {
            let mut st = self.state.lock();
            if let Some(sent) = st.end_to_end_reliable_tx.get_mut(&packet_id) {
                if let Some(sender_hash) = Self::decode_end_to_end_ack_sender_hash(pkt.sender()) {
                    sent.pending_destinations.remove(&sender_hash);
                    if sent.pending_destinations.is_empty() {
                        st.end_to_end_reliable_tx.remove(&packet_id);
                    }
                    return Ok(true);
                }
                st.end_to_end_reliable_tx.remove(&packet_id);
                return Ok(true);
            }
            return Ok(false);
        }

        let vals = pkt.data_as_u32()?;
        if vals.len() != 2 {
            return Err(TelemetryError::Deserialize("bad reliable control payload"));
        }
        let ty = DataType::try_from_u32(vals[0]).ok_or(TelemetryError::InvalidType)?;
        let seq = vals[1];

        match pkt.data_type() {
            DataType::ReliableAck => {
                self.handle_reliable_ack(src, ty, seq);
                Ok(true)
            }
            DataType::ReliablePartialAck => {
                self.handle_reliable_partial_ack(src, ty, seq);
                Ok(true)
            }
            DataType::ReliablePacketRequest => {
                self.queue_reliable_retransmit(src, ty, seq)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Core receive function handling both Packet and Serialized items.
    ///
    /// Relay mode: if a destination endpoint has no matching local handler and the packet has
    /// any remotely-forwardable endpoints, the router will rebroadcast the packet ONCE, excluding
    /// the ingress side.
    fn rx_item(&self, item: &RouterRxItem, called_from_queue: bool) -> TelemetryResult<()> {
        if let Some(src) = item.src {
            self.ensure_side_ingress_enabled(src)?;
            match &item.data {
                RouterItem::Packet(pkt) => {
                    if is_reliable_type(pkt.data_type())
                        && !is_internal_control_type(pkt.data_type())
                    {
                        self.note_reliable_return_route(src, pkt.packet_id());
                    }
                }
                RouterItem::Serialized(bytes) => {
                    if let Ok(env) = serialize::peek_envelope(bytes.as_ref())
                        && is_reliable_type(env.ty)
                        && !is_internal_control_type(env.ty)
                        && let Ok(packet_id) = serialize::packet_id_from_wire(bytes.as_ref())
                    {
                        self.note_reliable_return_route(src, packet_id);
                    }
                }
            }
        }
        let mut released_buffered: Vec<Arc<[u8]>> = Vec::new();
        if let (Some(src), RouterItem::Serialized(bytes)) = (item.src, &item.data) {
            let (_opts, handler_is_serialized, hop_reliable_enabled) = {
                let st = self.state.lock();
                let side_ref = Self::side_ref(&st, src)?;
                let opts = side_ref.opts;
                (
                    opts,
                    matches!(side_ref.tx_handler, RouterTxHandlerFn::Serialized(_)),
                    opts.reliable_enabled
                        && self.cfg.reliable_enabled()
                        && !self.side_has_multiple_announcers_locked(&st, src, self.clock.now_ms()),
                )
            };

            if hop_reliable_enabled && handler_is_serialized {
                let frame = match serialize::peek_frame_info(bytes.as_ref()) {
                    Ok(frame) => frame,
                    Err(e) => {
                        if matches!(e, TelemetryError::Deserialize(msg) if msg == "crc32 mismatch")
                        {
                            if let Ok(frame) = serialize::peek_frame_info_unchecked(bytes.as_ref())
                                && is_reliable_type(frame.envelope.ty)
                                && let Some(hdr) = frame.reliable
                            {
                                let unordered =
                                    (hdr.flags & serialize::RELIABLE_FLAG_UNORDERED) != 0;
                                let unsequenced =
                                    (hdr.flags & serialize::RELIABLE_FLAG_UNSEQUENCED) != 0;

                                if !unsequenced {
                                    let requested = if unordered {
                                        hdr.seq
                                    } else {
                                        let mut st = self.state.lock();
                                        let rx_state = self.reliable_rx_state_mut(
                                            &mut st,
                                            src,
                                            frame.envelope.ty,
                                        );
                                        rx_state.expected_seq.min(hdr.seq)
                                    };
                                    self.queue_reliable_packet_request(
                                        src,
                                        frame.envelope.ty,
                                        requested,
                                    )?;
                                }
                            }
                            return Ok(());
                        }
                        return Err(e);
                    }
                };
                if is_reliable_type(frame.envelope.ty)
                    && let Some(hdr) = frame.reliable
                {
                    let unordered = (hdr.flags & serialize::RELIABLE_FLAG_UNORDERED) != 0;
                    let unsequenced = (hdr.flags & serialize::RELIABLE_FLAG_UNSEQUENCED) != 0;

                    if !unsequenced {
                        if unordered {
                            self.queue_reliable_ack(src, frame.envelope.ty, hdr.seq)?;
                        } else {
                            let mut release: Vec<Arc<[u8]>> = Vec::new();
                            let mut last_delivered = None;
                            let mut ack_old = None;
                            let mut request_missing = None;
                            let mut partial_ack = None;
                            {
                                let mut st = self.state.lock();
                                let rx_state =
                                    self.reliable_rx_state_mut(&mut st, src, frame.envelope.ty);
                                let expected_seq = rx_state.expected_seq;
                                if hdr.seq < expected_seq {
                                    ack_old = Some(expected_seq.saturating_sub(1));
                                } else if hdr.seq > expected_seq {
                                    request_missing = Some(expected_seq);
                                    partial_ack = Some(hdr.seq);
                                    st.buffer_reliable_rx(
                                        src,
                                        frame.envelope.ty,
                                        hdr.seq,
                                        bytes.clone(),
                                    )?;
                                } else {
                                    release.push(bytes.clone());
                                    last_delivered = Some(hdr.seq);
                                    let mut next_expected = hdr.seq.wrapping_add(1);
                                    while let Some(buf) = rx_state.buffered.remove(&next_expected) {
                                        release.push(buf);
                                        last_delivered = Some(next_expected);
                                        let next = next_expected.wrapping_add(1);
                                        next_expected = if next == 0 { 1 } else { next };
                                    }
                                    rx_state.expected_seq = next_expected;
                                }
                            }

                            if let Some(ack_seq) = ack_old {
                                self.queue_reliable_ack(src, frame.envelope.ty, ack_seq)?;
                                return Ok(());
                            }
                            if let Some(request_seq) = request_missing {
                                if let Some(partial_seq) = partial_ack {
                                    self.queue_reliable_partial_ack(
                                        src,
                                        frame.envelope.ty,
                                        partial_seq,
                                    )?;
                                }
                                self.queue_reliable_packet_request(
                                    src,
                                    frame.envelope.ty,
                                    request_seq,
                                )?;
                                return Ok(());
                            }

                            if let Some(ack_seq) = last_delivered {
                                self.queue_reliable_ack(src, frame.envelope.ty, ack_seq)?;
                            }

                            released_buffered.extend(release.into_iter().skip(1));
                        }
                    }
                }
            } else {
                match serialize::peek_frame_info(bytes.as_ref()) {
                    Ok(frame) => {
                        if frame.ack_only() {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        if matches!(e, TelemetryError::Deserialize(msg) if msg == "crc32 mismatch")
                        {
                            return Ok(());
                        }
                        return Err(e);
                    }
                }
            }
        }

        if self.is_duplicate_pkt(&item.data)? {
            if item.src.is_some() {
                match &item.data {
                    RouterItem::Packet(pkt)
                    if is_reliable_type(pkt.data_type())
                        && pkt.sender() != self.sender
                        && self.packet_has_local_handler(pkt) =>
                        {
                            self.queue_end_to_end_reliable_ack(pkt)?;
                        }
                    RouterItem::Serialized(bytes) => {
                        if let Ok(pkt) = serialize::deserialize_packet(bytes.as_ref())
                            && is_reliable_type(pkt.data_type())
                            && pkt.sender() != self.sender
                            && self.packet_has_local_handler(&pkt)
                        {
                            self.queue_end_to_end_reliable_ack(&pkt)?;
                        }
                    }
                    _ => {}
                }
            }
            return Ok(());
        }

        self.dispatch_rx_data(item, called_from_queue)?;

        for release_bytes in released_buffered {
            let release_data = RouterItem::Serialized(release_bytes.clone());
            if self.is_duplicate_pkt(&release_data)? {
                continue;
            }
            let release_item = RouterRxItem {
                src: item.src,
                priority: Self::router_item_priority(&release_data)?,
                data: release_data,
            };
            self.dispatch_rx_data(&release_item, called_from_queue)?;
        }

        Ok(())
    }

    fn dispatch_rx_data(
        &self,
        item: &RouterRxItem,
        called_from_queue: bool,
    ) -> TelemetryResult<()> {
        match &item.data {
            RouterItem::Packet(pkt) => {
                pkt.validate()?;

                if self.handle_internal_reliable_packet(pkt, item.src)? {
                    return Ok(());
                }

                #[cfg(feature = "timesync")]
                if matches!(
                    pkt.data_type(),
                    DataType::TimeSyncAnnounce
                        | DataType::TimeSyncRequest
                        | DataType::TimeSyncResponse
                ) {
                    self.handle_internal_timesync_packet(pkt, item.src, called_from_queue)?;
                    return Ok(());
                }

                if self.learn_discovery_packet(pkt, item.src)? {
                    if self.should_route_remote(&item.data, item.src)? {
                        self.relay_send(
                            RouterItem::Packet(pkt.to_owned()),
                            item.src,
                            called_from_queue,
                        )?;
                    }
                    return Ok(());
                }

                let mut eps: Vec<DataEndpoint> = pkt.endpoints().to_vec();
                eps.sort_unstable();
                eps.dedup();
                let had_local_handler = eps.iter().copied().any(|ep| {
                    self.endpoint_has_packet_handler(ep) || self.endpoint_has_serialized_handler(ep)
                });

                let has_remote = self.should_route_remote(&item.data, item.src)?;

                let has_serialized_local = eps
                    .iter()
                    .copied()
                    .any(|ep| self.endpoint_has_serialized_handler(ep));
                let bytes_opt = if has_serialized_local {
                    Some(serialize::serialize_packet(pkt))
                } else {
                    None
                };

                for dest in eps {
                    for h in self.cfg.handlers.iter().filter(|h| h.endpoint == dest) {
                        match (&h.handler, &bytes_opt) {
                            (EndpointHandlerFn::Serialized(_), Some(bytes)) => {
                                let _ = self.call_handler_with_retries(
                                    dest,
                                    h,
                                    Some(bytes.as_ref()),
                                    Some(pkt),
                                    None,
                                );
                            }
                            (EndpointHandlerFn::Serialized(_), None) => {
                                let bytes = serialize::serialize_packet(pkt);
                                let _ = self.call_handler_with_retries(
                                    dest,
                                    h,
                                    Some(bytes.as_ref()),
                                    Some(pkt),
                                    None,
                                );
                            }
                            (EndpointHandlerFn::Packet(_), _) => {
                                let _ =
                                    self.call_handler_with_retries(dest, h, None, Some(pkt), None);
                            }
                        }
                    }
                }

                if item.src.is_some() && had_local_handler && is_reliable_type(pkt.data_type()) {
                    self.queue_end_to_end_reliable_ack(pkt)?;
                }

                if has_remote {
                    let relay_item = RouterItem::Packet(pkt.to_owned());
                    self.relay_send(relay_item, item.src, called_from_queue)?;
                }

                Ok(())
            }
            RouterItem::Serialized(bytes) => {
                let env = serialize::peek_envelope(bytes.as_ref())?;

                if matches!(
                    env.ty,
                    DataType::ReliableAck
                        | DataType::ReliablePartialAck
                        | DataType::ReliablePacketRequest
                ) {
                    let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                    pkt.validate()?;
                    let _ = self.handle_internal_reliable_packet(&pkt, item.src)?;
                    return Ok(());
                }

                #[cfg(feature = "timesync")]
                if matches!(
                    env.ty,
                    DataType::TimeSyncAnnounce
                        | DataType::TimeSyncRequest
                        | DataType::TimeSyncResponse
                ) {
                    let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                    pkt.validate()?;
                    self.handle_internal_timesync_packet(&pkt, item.src, called_from_queue)?;
                    return Ok(());
                }

                #[cfg(feature = "discovery")]
                if discovery::is_discovery_type(env.ty) {
                    let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                    pkt.validate()?;
                    let _ = self.learn_discovery_packet(&pkt, item.src)?;
                    if self.should_route_remote(&item.data, item.src)? {
                        self.relay_send(RouterItem::Packet(pkt), item.src, called_from_queue)?;
                    }
                    return Ok(());
                }

                let any_packet_needed = env
                    .endpoints
                    .iter()
                    .copied()
                    .any(|ep| self.endpoint_has_packet_handler(ep));

                let mut pkt_opt = if any_packet_needed {
                    let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                    pkt.validate()?;
                    Some(pkt)
                } else {
                    None
                };

                let mut eps: Vec<DataEndpoint> = env.endpoints.iter().copied().collect();
                eps.sort_unstable();
                eps.dedup();
                let had_local_handler = eps.iter().copied().any(|ep| {
                    self.endpoint_has_packet_handler(ep) || self.endpoint_has_serialized_handler(ep)
                });

                let has_remote = self.should_route_remote(&item.data, item.src)?;

                for dest in eps {
                    for h in self.cfg.handlers.iter().filter(|h| h.endpoint == dest) {
                        match &h.handler {
                            EndpointHandlerFn::Serialized(_) => {
                                let _ = self.call_handler_with_retries(
                                    dest,
                                    h,
                                    Some(bytes.as_ref()),
                                    pkt_opt.as_ref(),
                                    Some(&env),
                                );
                            }
                            EndpointHandlerFn::Packet(_) => {
                                if pkt_opt.is_none() {
                                    let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                                    pkt.validate()?;
                                    pkt_opt = Some(pkt);
                                }
                                let pkt_ref = pkt_opt.as_ref().expect("just set");
                                let _ = self.call_handler_with_retries(
                                    dest,
                                    h,
                                    None,
                                    Some(pkt_ref),
                                    Some(&env),
                                );
                            }
                        }
                    }
                }

                if item.src.is_some()
                    && had_local_handler
                    && is_reliable_type(env.ty)
                    && let Some(pkt) = pkt_opt.as_ref()
                {
                    self.queue_end_to_end_reliable_ack(pkt)?;
                }

                if has_remote {
                    let relay_item = match pkt_opt {
                        Some(ref p) => RouterItem::Packet(p.clone()),
                        None => RouterItem::Serialized(bytes.clone()),
                    };
                    self.relay_send(relay_item, item.src, called_from_queue)?;
                }

                Ok(())
            }
        }
    }

    fn dispatch_local_for_item(&self, item: &RouterItem) -> TelemetryResult<()> {
        match item {
            RouterItem::Packet(pkt) => {
                pkt.validate()?;
                if is_internal_control_type(pkt.data_type()) {
                    return Ok(());
                }

                let mut eps: Vec<DataEndpoint> = pkt.endpoints().to_vec();
                eps.sort_unstable();
                eps.dedup();

                let has_serialized_local = eps
                    .iter()
                    .copied()
                    .any(|ep| self.endpoint_has_serialized_handler(ep));
                let bytes_opt = if has_serialized_local {
                    Some(serialize::serialize_packet(pkt))
                } else {
                    None
                };

                for dest in eps {
                    for h in self.cfg.handlers.iter().filter(|h| h.endpoint == dest) {
                        match (&h.handler, &bytes_opt) {
                            (EndpointHandlerFn::Serialized(_), Some(bytes)) => {
                                self.call_handler_with_retries(
                                    dest,
                                    h,
                                    Some(bytes.as_ref()),
                                    Some(pkt),
                                    None,
                                )?;
                            }
                            (EndpointHandlerFn::Serialized(_), None) => {
                                let bytes = serialize::serialize_packet(pkt);
                                self.call_handler_with_retries(
                                    dest,
                                    h,
                                    Some(bytes.as_ref()),
                                    Some(pkt),
                                    None,
                                )?;
                            }
                            (EndpointHandlerFn::Packet(_), _) => {
                                self.call_handler_with_retries(dest, h, None, Some(pkt), None)?;
                            }
                        }
                    }
                }
            }
            RouterItem::Serialized(bytes) => {
                let env = serialize::peek_envelope(bytes.as_ref())?;
                if is_internal_control_type(env.ty) {
                    return Ok(());
                }

                let any_packet_needed = env
                    .endpoints
                    .iter()
                    .copied()
                    .any(|ep| self.endpoint_has_packet_handler(ep));

                let mut pkt_opt = if any_packet_needed {
                    let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                    pkt.validate()?;
                    Some(pkt)
                } else {
                    None
                };

                let mut eps: Vec<DataEndpoint> = env.endpoints.iter().copied().collect();
                eps.sort_unstable();
                eps.dedup();

                for dest in eps {
                    for h in self.cfg.handlers.iter().filter(|h| h.endpoint == dest) {
                        match &h.handler {
                            EndpointHandlerFn::Serialized(_) => {
                                self.call_handler_with_retries(
                                    dest,
                                    h,
                                    Some(bytes.as_ref()),
                                    pkt_opt.as_ref(),
                                    Some(&env),
                                )?;
                            }
                            EndpointHandlerFn::Packet(_) => {
                                if pkt_opt.is_none() {
                                    let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                                    pkt.validate()?;
                                    pkt_opt = Some(pkt);
                                }
                                let pkt_ref = pkt_opt.as_ref().expect("just set");
                                self.call_handler_with_retries(
                                    dest,
                                    h,
                                    None,
                                    Some(pkt_ref),
                                    Some(&env),
                                )?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Internal TX implementation used by `tx*()`, `tx_queue*()`, and relay-mode rebroadcast.
    ///
    /// - Broadcast items are sent to all sides when remote forwarding is required.
    /// - ToSide items are sent only to the specified side.
    /// - If `ignore_local` is false, local handlers are invoked once.
    fn tx_item_impl(&self, item: RouterTxItem, ignore_local: bool) -> TelemetryResult<()> {
        match item {
            RouterTxItem::Broadcast(data) => {
                #[cfg(feature = "discovery")]
                let is_discovery = matches!(&data, RouterItem::Packet(pkt) if discovery::is_discovery_type(pkt.data_type()))
                    || matches!(&data, RouterItem::Serialized(bytes)
                        if serialize::peek_envelope(bytes.as_ref())
                            .map(|env| discovery::is_discovery_type(env.ty))
                            .unwrap_or(false));
                if !ignore_local {
                    if self.is_duplicate_pkt(&data)? {
                        return Ok(());
                    }
                    #[cfg(feature = "discovery")]
                    if !is_discovery
                        && !matches!(&data, RouterItem::Packet(pkt) if is_internal_control_type(pkt.data_type()))
                        && !matches!(&data, RouterItem::Serialized(bytes)
                            if serialize::peek_envelope(bytes.as_ref())
                                .map(|env| is_internal_control_type(env.ty))
                                .unwrap_or(false))
                    {
                        self.dispatch_local_for_item(&data)?;
                    }
                    #[cfg(not(feature = "discovery"))]
                    if !matches!(&data, RouterItem::Packet(pkt) if is_internal_control_type(pkt.data_type()))
                        && !matches!(&data, RouterItem::Serialized(bytes)
                            if serialize::peek_envelope(bytes.as_ref())
                                .map(|env| is_internal_control_type(env.ty))
                                .unwrap_or(false))
                    {
                        self.dispatch_local_for_item(&data)?;
                    }
                }

                let send_remote = match &data {
                    RouterItem::Packet(pkt) => {
                        pkt.validate()?;
                        self.should_route_remote(&data, None)?
                    }
                    RouterItem::Serialized(bytes) => {
                        let _ = serialize::peek_envelope(bytes.as_ref())?;
                        self.should_route_remote(&data, None)?
                    }
                };

                if !send_remote {
                    return Ok(());
                }
                let ty = match &data {
                    RouterItem::Packet(pkt) => pkt.data_type(),
                    RouterItem::Serialized(bytes) => serialize::peek_envelope(bytes.as_ref())?.ty,
                };
                let RemoteSidePlan::Target(sides) = self.remote_side_plan(&data, None)?;
                if !ignore_local && is_reliable_type(ty) && !is_internal_control_type(ty) {
                    let should_track_end_to_end = {
                        let st = self.state.lock();
                        sides
                            .iter()
                            .copied()
                            .any(|side| Self::side_supports_end_to_end_reliable_locked(&st, side))
                    };
                    if should_track_end_to_end {
                        self.register_end_to_end_reliable_tx(&data)?;
                    }
                }
                for (idx, side) in sides.iter().copied().enumerate() {
                    if let Err(e) = self.send_reliable_to_side(side, data.clone()) {
                        if Self::is_side_tx_busy(&e) {
                            for retry_side in sides[idx..].iter().copied() {
                                self.tx_queue_item_with_flags(
                                    RouterTxItem::ToSide {
                                        src: None,
                                        dst: retry_side,
                                        data: data.clone(),
                                    },
                                    true,
                                )?;
                            }
                            return Ok(());
                        }
                        match &data {
                            RouterItem::Packet(pkt) => {
                                let _ = self.handle_callback_error(pkt, None, e);
                            }
                            RouterItem::Serialized(bytes) => {
                                if let Ok(env) = serialize::peek_envelope(bytes.as_ref()) {
                                    let _ = self.handle_callback_error_from_env(&env, None, e);
                                }
                            }
                        }
                        return Err(TelemetryError::HandlerError("tx handler failed"));
                    }
                }
            }
            RouterTxItem::ToSide { src, dst, data } => {
                if !ignore_local {
                    if self.is_duplicate_pkt(&data)? {
                        return Ok(());
                    }
                    let suppress_local = matches!(&data, RouterItem::Packet(pkt) if is_internal_control_type(pkt.data_type()))
                        || matches!(&data, RouterItem::Serialized(bytes)
                            if serialize::peek_envelope(bytes.as_ref())
                                .map(|env| is_internal_control_type(env.ty))
                                .unwrap_or(false));
                    if !suppress_local {
                        self.dispatch_local_for_item(&data)?;
                    }
                }
                let allowed = {
                    let st = self.state.lock();
                    let ty = match &data {
                        RouterItem::Packet(pkt) => Some(pkt.data_type()),
                        RouterItem::Serialized(bytes) => {
                            Some(serialize::peek_envelope(bytes.as_ref())?.ty)
                        }
                    };
                    self.route_allowed_locked(&st, src, ty, dst)
                };
                if !allowed {
                    return Ok(());
                }
                if let Err(e) = self.send_reliable_to_side(dst, data.clone()) {
                    if Self::is_side_tx_busy(&e) {
                        self.tx_queue_item_with_flags(
                            RouterTxItem::ToSide { src, dst, data },
                            true,
                        )?;
                        return Ok(());
                    }
                    match &data {
                        RouterItem::Packet(pkt) => {
                            let _ = self.handle_callback_error(pkt, None, e);
                        }
                        RouterItem::Serialized(bytes) => {
                            if let Ok(env) = serialize::peek_envelope(bytes.as_ref()) {
                                let _ = self.handle_callback_error_from_env(&env, None, e);
                            }
                        }
                    }
                    return Err(TelemetryError::HandlerError("tx handler failed"));
                }
            }
            RouterTxItem::EndToEndReplay { packet_id } => {
                let Some((data, mut sides)) = self.end_to_end_retransmit_sides(packet_id) else {
                    return Ok(());
                };
                if sides.is_empty() {
                    let RemoteSidePlan::Target(fallback_sides) =
                        self.remote_side_plan(&data, None)?;
                    sides = fallback_sides;
                }
                for (idx, side) in sides.iter().copied().enumerate() {
                    if let Err(e) = self.send_reliable_to_side(side, data.clone()) {
                        if Self::is_side_tx_busy(&e) {
                            for retry_side in sides[idx..].iter().copied() {
                                self.tx_queue_item_with_flags(
                                    RouterTxItem::ToSide {
                                        src: None,
                                        dst: retry_side,
                                        data: data.clone(),
                                    },
                                    true,
                                )?;
                            }
                            return Ok(());
                        }
                        match &data {
                            RouterItem::Packet(pkt) => {
                                let _ = self.handle_callback_error(pkt, None, e);
                            }
                            RouterItem::Serialized(bytes) => {
                                if let Ok(env) = serialize::peek_envelope(bytes.as_ref()) {
                                    let _ = self.handle_callback_error_from_env(&env, None, e);
                                }
                            }
                        }
                        return Err(TelemetryError::HandlerError("tx handler failed"));
                    }
                }
            }
            RouterTxItem::ReliableReplay { dst, bytes } => {
                let frame = serialize::peek_frame_info(bytes.as_ref())?;
                let ty = frame.envelope.ty;
                let Some(hdr) = frame.reliable else {
                    return Ok(());
                };
                {
                    let mut st = self.state.lock();
                    let tx_state = self.reliable_tx_state_mut(&mut st, dst, ty);
                    if !tx_state.sent.contains_key(&hdr.seq) {
                        return Ok(());
                    }
                }
                if let Err(e) = self.send_reliable_raw_to_side(dst, bytes.clone()) {
                    if Self::is_side_tx_busy(&e) {
                        self.tx_queue_item_with_flags(
                            RouterTxItem::ReliableReplay { dst, bytes },
                            true,
                        )?;
                        return Ok(());
                    }
                    return Err(e);
                }
                let mut st = self.state.lock();
                let tx_state = self.reliable_tx_state_mut(&mut st, dst, ty);
                if let Some(sent) = tx_state.sent.get_mut(&hdr.seq) {
                    sent.last_send_ms = self.clock.now_ms();
                    sent.queued = false;
                }
            }
        }

        Ok(())
    }

    /// Transmit a telemetry item immediately (remote + local).
    #[inline]
    fn tx_item(&self, item: RouterTxItem) -> TelemetryResult<()> {
        self.tx_item_impl(item, false)
    }

    // ---------- PUBLIC API: RX immediate ----------

    /// Process serialized bytes immediately as locally-originated input.
    ///
    /// If this call occurs while a side TX callback is already on the stack, the bytes are queued
    /// instead of being processed re-entrantly.
    #[inline]
    pub fn rx_serialized(&self, bytes: &[u8]) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return self.rx_serialized_queue(bytes);
        }
        let data = RouterItem::Serialized(Arc::from(bytes));
        let item = RouterRxItem {
            src: None,
            priority: Self::router_item_priority(&data)?,
            data,
        };
        self.rx_item(&item, false)
    }

    /// Process a decoded packet immediately as locally-originated input.
    ///
    /// If this call occurs while a side TX callback is already on the stack, the packet is queued
    /// instead of being processed re-entrantly.
    #[inline]
    pub fn rx(&self, pkt: &Packet) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return self.rx_queue(pkt.clone());
        }
        let data = RouterItem::Packet(pkt.clone());
        let item = RouterRxItem {
            src: None,
            priority: Self::router_item_priority(&data)?,
            data,
        };
        self.rx_item(&item, false)
    }

    /// Process a decoded packet immediately with an explicit ingress side id.
    ///
    /// If this call occurs while a side TX callback is already on the stack, the packet is queued
    /// instead of being processed re-entrantly.
    #[inline]
    pub fn rx_from_side(&self, pkt: &Packet, side: RouterSideId) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return self.rx_queue_from_side(pkt.clone(), side);
        }
        self.ensure_side_ingress_enabled(side)?;
        let data = RouterItem::Packet(pkt.clone());
        let item = RouterRxItem {
            src: Some(side),
            priority: Self::router_item_priority(&data)?,
            data,
        };
        self.rx_item(&item, false)
    }

    /// Process serialized bytes immediately with an explicit ingress side id.
    ///
    /// If this call occurs while a side TX callback is already on the stack, the bytes are queued
    /// instead of being processed re-entrantly.
    #[inline]
    pub fn rx_serialized_from_side(&self, bytes: &[u8], side: RouterSideId) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return self.rx_serialized_queue_from_side(bytes, side);
        }
        self.ensure_side_ingress_enabled(side)?;
        let data = RouterItem::Serialized(Arc::from(bytes));
        let item = RouterRxItem {
            src: Some(side),
            priority: Self::router_item_priority(&data)?,
            data,
        };
        self.rx_item(&item, false)
    }

    // ---------- PUBLIC API: TX immediate ----------

    /// Transmit a decoded packet immediately.
    ///
    /// The router delivers locally where appropriate and forwards toward eligible sides. If called
    /// from inside a side TX callback, the packet is queued instead of being sent re-entrantly.
    #[inline]
    pub fn tx(&self, pkt: Packet) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return self.tx_queue_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)));
        }
        self.tx_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
    }

    /// Transmit serialized bytes immediately.
    ///
    /// If called from inside a side TX callback, the bytes are queued instead of being sent
    /// re-entrantly.
    #[inline]
    pub fn tx_serialized(&self, pkt: Arc<[u8]>) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return self.tx_queue_item(RouterTxItem::Broadcast(RouterItem::Serialized(pkt)));
        }
        self.tx_item(RouterTxItem::Broadcast(RouterItem::Serialized(pkt)))
    }

    // ---------- PUBLIC API: TX queue ----------

    /// Queue a decoded packet for later transmission.
    #[inline]
    pub fn tx_queue(&self, pkt: Packet) -> TelemetryResult<()> {
        self.tx_queue_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
    }

    /// Queue serialized bytes for later transmission.
    #[inline]
    pub fn tx_serialized_queue(&self, data: Arc<[u8]>) -> TelemetryResult<()> {
        self.tx_queue_item(RouterTxItem::Broadcast(RouterItem::Serialized(data)))
    }

    // ---------- PUBLIC API: logging ----------

    /// Build a packet from typed elements and send it immediately.
    ///
    /// `ty` selects the schema message type and `data` must match that type's expected element
    /// width and count. If called from inside a side TX callback, the built packet is queued.
    #[inline]
    pub fn log<T: LeBytes>(&self, ty: DataType, data: &[T]) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return self.log_queue(ty, data);
        }
        log_raw(self.sender, ty, data, self.packet_timestamp_ms(), |pkt| {
            self.tx_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
        })
    }

    /// Build a packet from typed elements and queue it for later transmission.
    #[inline]
    pub fn log_queue<T: LeBytes>(&self, ty: DataType, data: &[T]) -> TelemetryResult<()> {
        log_raw(self.sender, ty, data, self.packet_timestamp_ms(), |pkt| {
            self.tx_queue_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
        })
    }

    /// Build a packet with an explicit timestamp and send it immediately.
    #[inline]
    pub fn log_ts<T: LeBytes>(
        &self,
        ty: DataType,
        timestamp: u64,
        data: &[T],
    ) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return self.log_queue_ts(ty, timestamp, data);
        }
        log_raw(self.sender, ty, data, timestamp, |pkt| {
            self.tx_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
        })
    }

    /// Build a packet with an explicit timestamp and queue it for later transmission.
    #[inline]
    pub fn log_queue_ts<T: LeBytes>(
        &self,
        ty: DataType,
        timestamp: u64,
        data: &[T],
    ) -> TelemetryResult<()> {
        log_raw(self.sender, ty, data, timestamp, |pkt| {
            self.tx_queue_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
        })
    }
}
