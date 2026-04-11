//! Telemetry Router
//!
//! Router with internal named sides (like Relay), plus local processing.
//!
//! Design:
//! - Sides are registered with per-side TX handlers (serialized or packet).
//! - RX is tagged by source side; relay mode rebroadcasts to other sides.
//! - Local endpoint handlers process packets as before (no side parameter).
//! - De-duplication remains packet-id based and side-agnostic.

use crate::config::{
    MAX_QUEUE_SIZE, MAX_RECENT_RX_IDS, QUEUE_GROW_STEP, STARTING_QUEUE_SIZE, STARTING_RECENT_RX_IDS,
};
#[cfg(feature = "discovery")]
use crate::discovery::{
    self, DiscoveryCadenceState, TopologySideRoute, TopologySnapshot, DISCOVERY_ROUTE_TTL_MS,
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
        DataEndpoint, DataType, DEVICE_IDENTIFIER, MAX_HANDLER_RETRIES, RELIABLE_MAX_PENDING,
        RELIABLE_MAX_RETRIES, RELIABLE_RETRANSMIT_MS,
    }, get_needed_message_size, impl_letype_num, is_reliable_type,
    lock::RouterMutex,
    message_meta, message_priority, packet::Packet,
    reliable_mode,
    serialize, MessageElement,
    RouteSelectionMode,
    TelemetryError, TelemetryResult,
};
use alloc::string::String;
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

/// The mode the router is operating in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum RouterMode {
    /// When a packet is received, the router calls any applicable *local* endpoint handlers.
    /// If an endpoint in the packet has no matching local handler (i.e. it is "unsatisfied"),
    /// and the packet has at least one endpoint eligible for remote forwarding, the router will
    /// rebroadcast the packet via its TX function (once per received item).
    ///
    /// This relies on packet de-duplication to prevent infinite loops between routers.
    Relay,

    /// Default behavior: on receive, only applicable local handlers are called.
    /// No rebroadcasting is performed.
    Sink,
}

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
    ToSide {
        src: Option<RouterSideId>,
        dst: RouterSideId,
        data: RouterItem,
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
            RouterTxItem::ToSide { data, .. } => match data {
                RouterItem::Packet(pkt) => pkt.byte_cost(),
                RouterItem::Serialized(bytes) => size_of::<Arc<[u8]>>() + bytes.len(),
            },
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
    inflight: Option<ReliableInflight>,
    pending: VecDeque<PendingReliable>,
}

#[derive(Debug, Clone)]
struct ReliableInflight {
    seq: u32,
    bytes: Arc<[u8]>,
    last_send_ms: u64,
    retries: u32,
}

#[derive(Debug, Clone)]
struct PendingReliable {
    data: RouterItem,
    side: RouterSideId,
}

#[derive(Debug, Clone)]
struct ReliableRxState {
    expected_seq: u32,
    last_ack: u32,
}

#[cfg(feature = "discovery")]
#[derive(Debug, Clone, Default)]
struct DiscoverySideState {
    reachable: Vec<DataEndpoint>,
    reachable_timesync_sources: Vec<String>,
    last_seen_ms: u64,
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
    pub reliable_enabled: bool,
    pub link_local_enabled: bool,
    pub ingress_enabled: bool,
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
        Self {
            endpoint,
            handler: EndpointHandlerFn::Packet(Arc::new(f)),
        }
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
        Self {
            endpoint,
            handler: EndpointHandlerFn::Serialized(Arc::new(f)),
        }
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
            #[cfg(feature = "timesync")]
            timesync: None,
        }
    }

    /// Enable or disable reliable delivery for this router instance.
    pub fn with_reliable_enabled(mut self, enabled: bool) -> Self {
        self.reliable_enabled = enabled;
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
    received_queue: BoundedDeque<RouterRxItem>,
    transmit_queue: BoundedDeque<TxQueued>,
    recent_rx: BoundedDeque<u64>,
    reliable_tx: BTreeMap<(RouterSideId, u32), ReliableTxState>,
    reliable_rx: BTreeMap<(RouterSideId, u32), ReliableRxState>,
    #[cfg(feature = "discovery")]
    discovery_routes: BTreeMap<RouterSideId, DiscoverySideState>,
    #[cfg(feature = "discovery")]
    discovery_cadence: DiscoveryCadenceState,
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

    mode: RouterMode,
    cfg: RouterConfig,
    state: RouterMutex<RouterInner>,
    isr_rx_queue: IsrRxQueue,
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
            .field("mode", &self.mode)
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
    #[cfg(feature = "timesync")]
    if matches!(
        ty,
        DataType::TimeSyncAnnounce | DataType::TimeSyncRequest | DataType::TimeSyncResponse
    ) {
        return true;
    }

    #[cfg(feature = "discovery")]
    if matches!(ty, DataType::DiscoveryAnnounce) {
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
        if src == Some(dst) {
            return false;
        }
        match src {
            None => true,
            Some(_) => self.mode == RouterMode::Relay,
        }
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
            .any(|(typed_src, typed_ty, _)| *typed_src == src && *typed_ty == ty as u32)
        {
            return st
                .typed_route_overrides
                .get(&(src, ty as u32, dst))
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
    ) -> Vec<RouterSideId> {
        if sides.len() <= 1 {
            return sides;
        }

        match st
            .source_route_modes
            .get(&src)
            .copied()
            .unwrap_or(RouteSelectionMode::Fanout)
        {
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

    fn router_item_priority(data: &RouterItem) -> TelemetryResult<u8> {
        let ty = match data {
            RouterItem::Packet(pkt) => pkt.data_type(),
            RouterItem::Serialized(bytes) => serialize::peek_envelope(bytes.as_ref())?.ty,
        };
        Ok(message_priority(ty))
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
            st.transmit_queue.push_back_prioritized(
                TxQueued {
                    item: RouterTxItem::ToSide {
                        src: exclude,
                        dst: idx,
                        data: data.clone(),
                    },
                    ignore_local,
                    priority,
                },
                |queued| queued.priority,
            )?;
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
            if discovery::is_discovery_type(ty) {
                let mut st = self.state.lock();
                let sides = self.eligible_side_ids_locked(&st, exclude, Some(ty), false);
                return Ok(RemoteSidePlan::Target(
                    self.apply_route_selection_locked(&mut st, exclude, sides),
                ));
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
            let has_nonlocal = has_nonlocal_endpoint(&eps, &self.cfg);
            let restrict_link_local = Self::endpoints_are_link_local_only(&eps);
            if st.discovery_routes.is_empty() {
                if restrict_link_local {
                    let targets = self.eligible_side_ids_locked(&st, exclude, Some(ty), true);
                    return Ok(RemoteSidePlan::Target(
                        self.apply_route_selection_locked(&mut st, exclude, targets),
                    ));
                }
                return if has_nonlocal || bootstrap_timesync {
                    let targets = self.eligible_side_ids_locked(&st, exclude, Some(ty), false);
                    Ok(RemoteSidePlan::Target(
                        self.apply_route_selection_locked(&mut st, exclude, targets),
                    ))
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
                )))
            } else if had_known {
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    exclude,
                    generic_targets,
                )))
            } else if restrict_link_local {
                let targets = self.eligible_side_ids_locked(&st, exclude, Some(ty), true);
                Ok(RemoteSidePlan::Target(
                    self.apply_route_selection_locked(&mut st, exclude, targets),
                ))
            } else if has_nonlocal || bootstrap_timesync {
                let targets = self.eligible_side_ids_locked(&st, exclude, Some(ty), false);
                Ok(RemoteSidePlan::Target(
                    self.apply_route_selection_locked(&mut st, exclude, targets),
                ))
            } else {
                Ok(RemoteSidePlan::Target(Vec::new()))
            }
        }
        #[cfg(not(feature = "discovery"))]
        {
            let (_, ty) = self.item_route_info(data)?;
            let mut st = self.state.lock();
            let sides = self.eligible_side_ids_locked(&st, exclude, Some(ty), false);
            Ok(RemoteSidePlan::Target(
                self.apply_route_selection_locked(&mut st, exclude, sides),
            ))
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
    fn prune_discovery_routes_locked(st: &mut RouterInner, now_ms: u64) -> bool {
        let before = st.discovery_routes.len();
        st.discovery_routes
            .retain(|_, route| now_ms.saturating_sub(route.last_seen_ms) <= DISCOVERY_ROUTE_TTL_MS);
        before != st.discovery_routes.len()
    }

    #[cfg(feature = "discovery")]
    fn advertised_discovery_endpoints_for_link_locked(
        &self,
        st: &RouterInner,
        now_ms: u64,
        link_local_enabled: bool,
    ) -> Vec<DataEndpoint> {
        let mut eps = self.local_discovery_endpoints();
        if !link_local_enabled {
            eps.retain(|ep| !ep.is_link_local_only());
        }
        for route in st.discovery_routes.values() {
            if now_ms.saturating_sub(route.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                continue;
            }
            eps.extend(route.reachable.iter().copied());
        }
        eps.retain(|ep| {
            !discovery::is_discovery_endpoint(*ep)
                && (link_local_enabled || !ep.is_link_local_only())
        });
        eps.sort_unstable();
        eps.dedup();
        eps
    }

    #[cfg(feature = "discovery")]
    fn advertised_discovery_timesync_sources_for_link_locked(
        &self,
        st: &RouterInner,
        now_ms: u64,
    ) -> Vec<String> {
        let mut sources = self.local_discovery_timesync_sources(now_ms);
        for route in st.discovery_routes.values() {
            if now_ms.saturating_sub(route.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                continue;
            }
            sources.extend(route.reachable_timesync_sources.iter().cloned());
        }
        sources.sort_unstable();
        sources.dedup();
        sources
    }

    #[cfg(feature = "discovery")]
    fn queue_discovery_announce(&self) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let per_side = {
            let mut st = self.state.lock();
            if Self::prune_discovery_routes_locked(&mut st, now_ms) {
                Self::note_discovery_topology_change_locked(&mut st, now_ms);
            }
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
                    Some((side_id, endpoints, timesync_sources))
                })
                .collect::<Vec<_>>()
        };
        for (side_id, endpoints, timesync_sources) in per_side {
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
                Self::note_discovery_topology_change_locked(&mut st, now_ms);
            }
            let has_any = st.sides.iter().enumerate().any(|(side_id, side)| {
                let Some(side) = side.as_ref() else {
                    return false;
                };
                if !self.route_allowed_locked(&st, None, Some(DataType::DiscoveryAnnounce), side_id)
                {
                    return false;
                }
                !self
                    .advertised_discovery_endpoints_for_link_locked(
                        &st,
                        now_ms,
                        side.opts.link_local_enabled,
                    )
                    .is_empty()
                    || !self
                        .advertised_discovery_timesync_sources_for_link_locked(&st, now_ms)
                        .is_empty()
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
        let mut st = self.state.lock();
        let now_ms = self.clock.now_ms();
        let mut route = st.discovery_routes.get(&side).cloned().unwrap_or_default();
        let changed = match pkt.data_type() {
            DataType::DiscoveryAnnounce => {
                let reachable = discovery::decode_discovery_announce(pkt)?;
                let changed = route.reachable != reachable;
                route.reachable = reachable;
                changed
            }
            DataType::DiscoveryTimeSyncSources => {
                let sources = discovery::decode_discovery_timesync_sources(pkt)?;
                let changed = route.reachable_timesync_sources != sources;
                route.reachable_timesync_sources = sources;
                changed
            }
            _ => false,
        };
        route.last_seen_ms = now_ms;
        st.discovery_routes.insert(side, route);
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
        (side, ty as u32)
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
                inflight: None,
                pending: VecDeque::new(),
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
                last_ack: 0,
            })
    }

    fn reliable_ack_to_send(&self, st: &mut RouterInner, side: RouterSideId, ty: DataType) -> u32 {
        let rx = self.reliable_rx_state_mut(st, side, ty);
        rx.last_ack
    }

    fn send_reliable_ack(&self, side: RouterSideId, ty: DataType, ack: u32) -> TelemetryResult<()> {
        let (handler, opts) = {
            let st = self.state.lock();
            let side_ref = Self::side_ref(&st, side)?;
            (side_ref.tx_handler.clone(), side_ref.opts)
        };

        if !opts.reliable_enabled {
            return Ok(());
        }

        let RouterTxHandlerFn::Serialized(f) = handler else {
            return Ok(());
        };

        let bytes =
            serialize::serialize_reliable_ack(self.sender, ty, self.packet_timestamp_ms(), ack);
        self.retry(MAX_HANDLER_RETRIES, || f(bytes.as_ref()))
            .map_err(|_| TelemetryError::Io("reliable ack send failed"))?;
        Ok(())
    }

    fn handle_reliable_ack(&self, side: RouterSideId, ty: DataType, ack: u32) {
        let pending = {
            let mut st = self.state.lock();
            let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
            if let Some(inflight) = tx_state.inflight.as_mut() {
                if ack >= tx_state.next_seq {
                    let next = ack.wrapping_add(1);
                    tx_state.next_seq = if next == 0 { 1 } else { next };
                }
                if ack >= inflight.seq {
                    tx_state.inflight = None;
                    self.take_next_reliable(&mut st, side, ty)
                } else if ack + 1 == inflight.seq {
                    // NACK-like signal: requester is still waiting on inflight seq.
                    inflight.last_send_ms = 0;
                    None
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(p) = pending {
            let _ = self.send_reliable_to_side(p.side, p.data);
        }
    }

    fn take_next_reliable(
        &self,
        st: &mut RouterInner,
        side: RouterSideId,
        ty: DataType,
    ) -> Option<PendingReliable> {
        let tx_state = self.reliable_tx_state_mut(st, side, ty);
        if tx_state.inflight.is_some() {
            return None;
        }
        tx_state.pending.pop_front()
    }

    fn send_reliable_to_side(&self, side: RouterSideId, data: RouterItem) -> TelemetryResult<()> {
        let (handler, opts) = {
            let st = self.state.lock();
            let side_ref = Self::side_ref(&st, side)?;
            (side_ref.tx_handler.clone(), side_ref.opts)
        };

        let RouterTxHandlerFn::Serialized(f) = &handler else {
            return self.call_side_tx_handler(&handler, &data);
        };

        if !opts.reliable_enabled || !self.cfg.reliable_enabled() {
            if let Some(adjusted) = self.adjust_reliable_for_side(opts, data)? {
                return self.call_side_tx_handler(&handler, &adjusted);
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
                self.call_side_tx_handler(&handler, &adjusted)?;
            }
            return Ok(());
        }

        let (seq, ack, flags) = {
            let mut st = self.state.lock();
            let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
            if tx_state.inflight.is_some() {
                if tx_state.pending.len() >= RELIABLE_MAX_PENDING {
                    return Err(TelemetryError::PacketTooLarge(
                        "router reliable pending full",
                    ));
                }
                tx_state.pending.push_back(PendingReliable { data, side });
                return Ok(());
            }
            let seq = tx_state.next_seq;
            let next = tx_state.next_seq.wrapping_add(1);
            tx_state.next_seq = if next == 0 { 1 } else { next };
            let ack = self.reliable_ack_to_send(&mut st, side, ty);
            let flags = match reliable_mode(ty) {
                crate::ReliableMode::Unordered => serialize::RELIABLE_FLAG_UNORDERED,
                _ => 0,
            };
            (seq, ack, flags)
        };

        let bytes: Arc<[u8]> = match data {
            RouterItem::Packet(pkt) => serialize::serialize_packet_with_reliable(
                &pkt,
                serialize::ReliableHeader { flags, seq, ack },
            ),
            RouterItem::Serialized(bytes) => {
                let mut v = bytes.to_vec();
                if !serialize::rewrite_reliable_header(&mut v, flags, seq, ack)? {
                    return f(bytes.as_ref());
                }
                Arc::from(v)
            }
        };

        f(bytes.as_ref())?;

        {
            let mut st = self.state.lock();
            let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
            if tx_state.inflight.is_none() {
                tx_state.inflight = Some(ReliableInflight {
                    seq,
                    bytes: bytes.clone(),
                    last_send_ms: self.clock.now_ms(),
                    retries: 0,
                });
            }
        }

        Ok(())
    }

    fn call_side_tx_handler(
        &self,
        handler: &RouterTxHandlerFn,
        data: &RouterItem,
    ) -> TelemetryResult<()> {
        match (handler, data) {
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
        }
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
                            0,
                            0,
                        )?;
                        return Ok(Some(RouterItem::Serialized(Arc::from(v))));
                    }
                }
                Ok(Some(RouterItem::Serialized(bytes)))
            }
            other => Ok(Some(other)),
        }
    }

    fn process_reliable_timeouts(&self) -> TelemetryResult<()> {
        {
            let st = self.state.lock();
            if st.reliable_tx.is_empty() {
                return Ok(());
            }
        }

        let now = self.clock.now_ms();
        let mut resend: Vec<(RouterTxHandlerFn, Arc<[u8]>)> = Vec::new();
        let mut to_send: Vec<PendingReliable> = Vec::new();
        let mut cleared: Vec<(RouterSideId, u32)> = Vec::new();

        {
            let mut st = self.state.lock();

            // Snapshot handlers so we don't borrow st immutably while iterating reliable_tx mutably.
            let handlers: Vec<Option<RouterTxHandlerFn>> = st
                .sides
                .iter()
                .map(|side_ref| side_ref.as_ref().map(|side| side.tx_handler.clone()))
                .collect();

            for ((side, ty_u32), tx_state) in st.reliable_tx.iter_mut() {
                if let Some(inflight) = tx_state.inflight.as_mut()
                    && now.wrapping_sub(inflight.last_send_ms) >= RELIABLE_RETRANSMIT_MS
                {
                    if inflight.retries >= RELIABLE_MAX_RETRIES {
                        tx_state.inflight = None;
                        cleared.push((*side, *ty_u32));
                        continue;
                    }

                    inflight.retries += 1;
                    inflight.last_send_ms = now;

                    if let Some(Some(handler)) = handlers.get(*side) {
                        resend.push((handler.clone(), inflight.bytes.clone()));
                    }
                }
            }

            for (side, ty_u32) in cleared.iter().copied() {
                if let Some(ty) = DataType::try_from_u32(ty_u32)
                    && let Some(p) = self.take_next_reliable(&mut st, side, ty)
                {
                    to_send.push(p);
                }
            }
        }

        for (handler, bytes) in resend {
            if let RouterTxHandlerFn::Serialized(f) = handler {
                self.retry(MAX_HANDLER_RETRIES, || f(bytes.as_ref()))
                    .map_err(|_| TelemetryError::Io("reliable retransmit failed"))?;
            }
        }

        for p in to_send {
            let _ = self.send_reliable_to_side(p.side, p.data);
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
            if !(src.is_none() && self.mode == RouterMode::Sink)
                && self.should_route_remote(&RouterItem::Packet(pkt.clone()), src)?
            {
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

        if !(src.is_none() && self.mode == RouterMode::Sink)
            && self.should_route_remote(&RouterItem::Packet(pkt.clone()), src)?
        {
            self.relay_send(RouterItem::Packet(pkt.clone()), src, called_from_queue)?;
        }

        Ok(true)
    }

    /// Create a new Router with an internal monotonic clock.
    #[cfg(feature = "std")]
    pub fn new(mode: RouterMode, cfg: RouterConfig) -> Self {
        Self::new_with_clock(mode, cfg, Box::new(StdMonotonicClock::default()))
    }

    /// Create a new Router with the specified mode, router configuration, and clock.
    pub fn new_with_clock(
        mode: RouterMode,
        cfg: RouterConfig,
        clock: Box<dyn Clock + Send + Sync>,
    ) -> Self {
        #[cfg(feature = "timesync")]
        let timesync_cfg = cfg.timesync_config();
        Self {
            sender: DEVICE_IDENTIFIER,
            mode,
            cfg,
            state: RouterMutex::new(RouterInner {
                sides: Vec::new(),
                route_overrides: BTreeMap::new(),
                typed_route_overrides: BTreeMap::new(),
                route_weights: BTreeMap::new(),
                route_priorities: BTreeMap::new(),
                source_route_modes: BTreeMap::new(),
                route_selection_cursors: BTreeMap::new(),
                received_queue: BoundedDeque::new(
                    MAX_QUEUE_SIZE,
                    STARTING_QUEUE_SIZE,
                    QUEUE_GROW_STEP,
                ),
                transmit_queue: BoundedDeque::new(
                    MAX_QUEUE_SIZE,
                    STARTING_QUEUE_SIZE,
                    QUEUE_GROW_STEP,
                ),
                recent_rx: BoundedDeque::new(
                    MAX_RECENT_RX_IDS * size_of::<u64>(),
                    STARTING_RECENT_RX_IDS * size_of::<u64>(),
                    QUEUE_GROW_STEP,
                ),
                reliable_tx: BTreeMap::new(),
                reliable_rx: BTreeMap::new(),
                #[cfg(feature = "discovery")]
                discovery_routes: BTreeMap::new(),
                #[cfg(feature = "discovery")]
                discovery_cadence: DiscoveryCadenceState::default(),
            }),
            isr_rx_queue: IsrRxQueue::new(MAX_QUEUE_SIZE, STARTING_QUEUE_SIZE, QUEUE_GROW_STEP),
            clock,
            #[cfg(feature = "timesync")]
            timesync: RouterMutex::new(TimeSyncRuntime::new(timesync_cfg)),
        }
    }

    /// Add a new side with a **serialized handler**.
    pub fn add_side_serialized<F>(&self, name: &'static str, tx: F) -> RouterSideId
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        self.add_side_serialized_with_options(name, tx, RouterSideOptions::default())
    }

    /// Adds a serialized-output side with explicit reliability and link-local options.
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

    /// Add a new side with a **packet handler**.
    pub fn add_side_packet<F>(&self, name: &'static str, tx: F) -> RouterSideId
    where
        F: Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        self.add_side_packet_with_options(name, tx, RouterSideOptions::default())
    }

    /// Adds a packet-output side with explicit reliability and link-local options.
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
            st.transmit_queue.retain(
                |queued| !matches!(&queued.item, RouterTxItem::ToSide { dst, .. } if *dst == side),
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
                |queued| !matches!(&queued.item, RouterTxItem::ToSide { dst, .. } if *dst == side),
            );
        }
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

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

    pub fn clear_source_route_mode(&self, src: Option<RouterSideId>) -> TelemetryResult<()> {
        self.set_source_route_mode(src, RouteSelectionMode::Fanout)
    }

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
            .insert((src, ty as u32, dst), enabled);
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

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
        st.typed_route_overrides.remove(&(src, ty as u32, dst));
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

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
                Some(TopologySideRoute {
                    side_id,
                    side_name: side.name,
                    reachable_endpoints: route.reachable.clone(),
                    reachable_timesync_sources: route.reachable_timesync_sources.clone(),
                    last_seen_ms: route.last_seen_ms,
                    age_ms: now_ms.saturating_sub(route.last_seen_ms),
                })
            })
            .collect();
        let advertised_endpoints =
            self.advertised_discovery_endpoints_for_link_locked(&st, now_ms, true);
        let advertised_timesync_sources =
            self.advertised_discovery_timesync_sources_for_link_locked(&st, now_ms);
        TopologySnapshot {
            advertised_endpoints,
            advertised_timesync_sources,
            routes,
            current_announce_interval_ms: st.discovery_cadence.current_interval_ms,
            next_announce_ms: st.discovery_cadence.next_announce_ms,
        }
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
            st.recent_rx.push_back(id)?;
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
        let mut st = self.state.lock();
        let priority = match &item {
            RouterTxItem::Broadcast(data) => Self::router_item_priority(data)?,
            RouterTxItem::ToSide { data, .. } => Self::router_item_priority(data)?,
        };
        st.transmit_queue.push_back_prioritized(
            TxQueued {
                item,
                ignore_local,
                priority,
            },
            |queued| queued.priority,
        )?;
        Ok(())
    }

    /// Enqueue an item for later transmission (default: local dispatch enabled).
    #[inline]
    fn tx_queue_item(&self, item: RouterTxItem) -> TelemetryResult<()> {
        self.tx_queue_item_with_flags(item, false)
    }

    // ---------- PUBLIC API: RX queue ----------

    /// Drain the receive queue fully.
    #[inline]
    pub fn process_rx_queue(&self) -> TelemetryResult<()> {
        self.process_rx_queue_with_timeout(0)
    }

    /// Enqueue serialized bytes for RX processing (local source).
    #[inline]
    pub fn rx_serialized_queue(&self, bytes: &[u8]) -> TelemetryResult<()> {
        let data = RouterItem::Serialized(Arc::from(bytes));
        let priority = Self::router_item_priority(&data)?;
        let mut st = self.state.lock();
        st.received_queue.push_back_prioritized(
            RouterRxItem {
                src: None,
                data,
                priority,
            },
            |queued| queued.priority,
        )?;
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

    /// Enqueue a packet for RX processing (local source).
    #[inline]
    pub fn rx_queue(&self, pkt: Packet) -> TelemetryResult<()> {
        pkt.validate()?;
        let data = RouterItem::Packet(pkt);
        let priority = Self::router_item_priority(&data)?;
        let mut st = self.state.lock();
        st.received_queue.push_back_prioritized(
            RouterRxItem {
                src: None,
                data,
                priority,
            },
            |queued| queued.priority,
        )?;
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

    /// Enqueue a packet for RX processing with explicit source side.
    #[inline]
    pub fn rx_queue_from_side(&self, pkt: Packet, side: RouterSideId) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(side)?;
        pkt.validate()?;
        let data = RouterItem::Packet(pkt);
        let priority = Self::router_item_priority(&data)?;
        let mut st = self.state.lock();
        st.received_queue.push_back_prioritized(
            RouterRxItem {
                src: Some(side),
                data,
                priority,
            },
            |queued| queued.priority,
        )?;
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

    /// Enqueue serialized bytes for RX processing with explicit source side.
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
        st.received_queue.push_back_prioritized(
            RouterRxItem {
                src: Some(side),
                data,
                priority,
            },
            |queued| queued.priority,
        )?;
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

    /// Core receive function handling both Packet and Serialized items.
    ///
    /// Relay mode: if a destination endpoint has no matching local handler and the packet has
    /// any remotely-forwardable endpoints, the router will rebroadcast the packet ONCE, excluding
    /// the ingress side.
    fn rx_item(&self, item: &RouterRxItem, called_from_queue: bool) -> TelemetryResult<()> {
        if let Some(src) = item.src {
            self.ensure_side_ingress_enabled(src)?;
        }
        let mut skip_dedupe = false;

        if let (Some(src), RouterItem::Serialized(bytes)) = (item.src, &item.data) {
            let (opts, handler_is_serialized) = {
                let st = self.state.lock();
                let side_ref = Self::side_ref(&st, src)?;
                (
                    side_ref.opts,
                    matches!(side_ref.tx_handler, RouterTxHandlerFn::Serialized(_)),
                )
            };

            if opts.reliable_enabled && handler_is_serialized && self.cfg.reliable_enabled() {
                let frame = match serialize::peek_frame_info(bytes.as_ref()) {
                    Ok(frame) => frame,
                    Err(e) => {
                        if matches!(e, TelemetryError::Deserialize(msg) if msg == "crc32 mismatch")
                        {
                            if let Ok(frame) = serialize::peek_frame_info_unchecked(bytes.as_ref())
                                && is_reliable_type(frame.envelope.ty)
                                && let Some(hdr) = frame.reliable
                            {
                                if (hdr.flags & serialize::RELIABLE_FLAG_ACK_ONLY) != 0 {
                                    return Ok(());
                                }

                                let unordered =
                                    (hdr.flags & serialize::RELIABLE_FLAG_UNORDERED) != 0;
                                let unsequenced =
                                    (hdr.flags & serialize::RELIABLE_FLAG_UNSEQUENCED) != 0;

                                if !unsequenced {
                                    if unordered {
                                        let ack = {
                                            let mut st = self.state.lock();
                                            let rx_state = self.reliable_rx_state_mut(
                                                &mut st,
                                                src,
                                                frame.envelope.ty,
                                            );
                                            rx_state.last_ack
                                        };
                                        let _ = self.send_reliable_ack(src, frame.envelope.ty, ack);
                                    } else {
                                        let expected = {
                                            let mut st = self.state.lock();
                                            let rx_state = self.reliable_rx_state_mut(
                                                &mut st,
                                                src,
                                                frame.envelope.ty,
                                            );
                                            rx_state.expected_seq
                                        };
                                        let ack = expected.saturating_sub(1);
                                        let _ = self.send_reliable_ack(src, frame.envelope.ty, ack);
                                    }
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
                    if (hdr.flags & serialize::RELIABLE_FLAG_ACK_ONLY) != 0 {
                        self.handle_reliable_ack(src, frame.envelope.ty, hdr.ack);
                        return Ok(());
                    }
                    self.handle_reliable_ack(src, frame.envelope.ty, hdr.ack);

                    let unordered = (hdr.flags & serialize::RELIABLE_FLAG_UNORDERED) != 0;
                    let unsequenced = (hdr.flags & serialize::RELIABLE_FLAG_UNSEQUENCED) != 0;

                    if !unsequenced {
                        if unordered {
                            {
                                let mut st = self.state.lock();
                                let rx_state =
                                    self.reliable_rx_state_mut(&mut st, src, frame.envelope.ty);
                                rx_state.last_ack = hdr.seq;
                            }
                            let _ = self.send_reliable_ack(src, frame.envelope.ty, hdr.seq);
                        } else {
                            let expected = {
                                let mut st = self.state.lock();
                                let rx_state =
                                    self.reliable_rx_state_mut(&mut st, src, frame.envelope.ty);
                                rx_state.expected_seq
                            };

                            if hdr.seq != expected {
                                let ack = expected.saturating_sub(1);
                                let _ = self.send_reliable_ack(src, frame.envelope.ty, ack);
                                return Ok(());
                            }

                            {
                                let mut st = self.state.lock();
                                let rx_state =
                                    self.reliable_rx_state_mut(&mut st, src, frame.envelope.ty);
                                let next = rx_state.expected_seq.wrapping_add(1);
                                rx_state.expected_seq = if next == 0 { 1 } else { next };
                                rx_state.last_ack = expected;
                            }

                            let ack = expected;
                            let _ = self.send_reliable_ack(src, frame.envelope.ty, ack);
                            skip_dedupe = true;
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

        if !skip_dedupe && self.is_duplicate_pkt(&item.data)? {
            return Ok(());
        }

        match &item.data {
            RouterItem::Packet(pkt) => {
                pkt.validate()?;

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
                    if !(item.src.is_none() && self.mode == RouterMode::Sink)
                        && self.should_route_remote(&item.data, item.src)?
                    {
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

                let has_remote = if item.src.is_none() && self.mode == RouterMode::Sink {
                    false
                } else {
                    self.should_route_remote(&item.data, item.src)?
                };

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
                    let mut any_matched = false;

                    for h in self.cfg.handlers.iter().filter(|h| h.endpoint == dest) {
                        any_matched = true;
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

                    let _ = any_matched;
                }

                if has_remote {
                    let relay_item = RouterItem::Packet(pkt.to_owned());
                    self.relay_send(relay_item, item.src, called_from_queue)?;
                }

                Ok(())
            }

            RouterItem::Serialized(bytes) => {
                let env = serialize::peek_envelope(bytes.as_ref())?;

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
                    if !(item.src.is_none() && self.mode == RouterMode::Sink)
                        && self.should_route_remote(&item.data, item.src)?
                    {
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

                let has_remote = if item.src.is_none() && self.mode == RouterMode::Sink {
                    false
                } else {
                    self.should_route_remote(&item.data, item.src)?
                };

                for dest in eps {
                    let mut any_matched = false;

                    for h in self.cfg.handlers.iter().filter(|h| h.endpoint == dest) {
                        any_matched = true;
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

                    let _ = any_matched;
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
                let RemoteSidePlan::Target(sides) = self.remote_side_plan(&data, None)?;
                for side in sides {
                    if let Err(e) = self.send_reliable_to_side(side, data.clone()) {
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

        Ok(())
    }

    /// Transmit a telemetry item immediately (remote + local).
    #[inline]
    fn tx_item(&self, item: RouterTxItem) -> TelemetryResult<()> {
        self.tx_item_impl(item, false)
    }

    // ---------- PUBLIC API: RX immediate ----------

    /// Receive serialized bytes (local source).
    #[inline]
    pub fn rx_serialized(&self, bytes: &[u8]) -> TelemetryResult<()> {
        let data = RouterItem::Serialized(Arc::from(bytes));
        let item = RouterRxItem {
            src: None,
            priority: Self::router_item_priority(&data)?,
            data,
        };
        self.rx_item(&item, false)
    }

    /// Receive a packet (local source).
    #[inline]
    pub fn rx(&self, pkt: &Packet) -> TelemetryResult<()> {
        let data = RouterItem::Packet(pkt.clone());
        let item = RouterRxItem {
            src: None,
            priority: Self::router_item_priority(&data)?,
            data,
        };
        self.rx_item(&item, false)
    }

    /// Receive a packet with explicit ingress side.
    #[inline]
    pub fn rx_from_side(&self, pkt: &Packet, side: RouterSideId) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(side)?;
        let data = RouterItem::Packet(pkt.clone());
        let item = RouterRxItem {
            src: Some(side),
            priority: Self::router_item_priority(&data)?,
            data,
        };
        self.rx_item(&item, false)
    }

    /// Receive serialized bytes with explicit ingress side.
    #[inline]
    pub fn rx_serialized_from_side(&self, bytes: &[u8], side: RouterSideId) -> TelemetryResult<()> {
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

    /// Transmit a packet immediately (broadcast to all sides).
    #[inline]
    pub fn tx(&self, pkt: Packet) -> TelemetryResult<()> {
        self.tx_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
    }

    /// Transmit serialized bytes immediately (broadcast to all sides).
    #[inline]
    pub fn tx_serialized(&self, pkt: Arc<[u8]>) -> TelemetryResult<()> {
        self.tx_item(RouterTxItem::Broadcast(RouterItem::Serialized(pkt)))
    }

    // ---------- PUBLIC API: TX queue ----------

    /// Queue a packet for later TX (broadcast to all sides).
    #[inline]
    pub fn tx_queue(&self, pkt: Packet) -> TelemetryResult<()> {
        self.tx_queue_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
    }

    /// Queue serialized bytes for later TX (broadcast to all sides).
    #[inline]
    pub fn tx_serialized_queue(&self, data: Arc<[u8]>) -> TelemetryResult<()> {
        self.tx_queue_item(RouterTxItem::Broadcast(RouterItem::Serialized(data)))
    }

    // ---------- PUBLIC API: logging ----------

    /// Build a packet then send immediately.
    #[inline]
    pub fn log<T: LeBytes>(&self, ty: DataType, data: &[T]) -> TelemetryResult<()> {
        log_raw(self.sender, ty, data, self.packet_timestamp_ms(), |pkt| {
            self.tx_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
        })
    }

    /// Build a packet and queue it for later TX.
    #[inline]
    pub fn log_queue<T: LeBytes>(&self, ty: DataType, data: &[T]) -> TelemetryResult<()> {
        log_raw(self.sender, ty, data, self.packet_timestamp_ms(), |pkt| {
            self.tx_queue_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
        })
    }

    /// Build a packet with a specific timestamp then send immediately.
    #[inline]
    pub fn log_ts<T: LeBytes>(
        &self,
        ty: DataType,
        timestamp: u64,
        data: &[T],
    ) -> TelemetryResult<()> {
        log_raw(self.sender, ty, data, timestamp, |pkt| {
            self.tx_item(RouterTxItem::Broadcast(RouterItem::Packet(pkt)))
        })
    }

    /// Build a packet with a specific timestamp and queue it for later TX.
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
