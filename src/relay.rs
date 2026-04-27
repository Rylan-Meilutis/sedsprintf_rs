use crate::config::{
    MAX_QUEUE_BUDGET, MAX_RECENT_RX_IDS, QUEUE_GROW_STEP, RECENT_RX_QUEUE_BYTES,
    RELIABLE_MAX_END_TO_END_ACK_CACHE, RELIABLE_MAX_END_TO_END_PENDING, RELIABLE_MAX_PENDING,
    RELIABLE_MAX_RETRIES, RELIABLE_MAX_RETURN_ROUTES, RELIABLE_RETRANSMIT_MS, STARTING_QUEUE_SIZE,
};
use crate::diagnostics::{
    AdaptiveLinkStats, DiscoveryRuntimeStats, QueueRuntimeStats, ReliableRuntimeStats,
    RouteModeStats, RouteOverrideStats, RoutePriorityStats, RouteWeightStats, RuntimeSideStats,
    RuntimeStatsSnapshot, RuntimeTypeStats, TypedRouteOverrideStats,
};
#[cfg(feature = "discovery")]
use crate::discovery::{
    self, DiscoveryCadenceState, TopologyAnnouncerRoute, TopologyBoardNode, TopologySideRoute,
    TopologySnapshot, DISCOVERY_ROUTE_TTL_MS,
};
use crate::packet::{hash_bytes_u64, Packet};
use crate::queue::{BoundedDeque, ByteCost};
use crate::serialize;
use crate::{is_reliable_type, message_meta, message_priority, reliable_mode};
use crate::{
    router::Clock,
    {
        lock::{ReentryGate, ReentryGuard, RouterMutex}, RouteSelectionMode, TelemetryError,
        TelemetryResult,
    },
};
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::{String, ToString};
use alloc::{sync::Arc, vec, vec::Vec};
use core::mem::size_of;

/// Logical side index (CAN, UART, RADIO, etc.)
pub type RelaySideId = usize;
/// Packet Handler function type
type PacketHandlerFn = dyn Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static;

/// Serialized Handler function type
type SerializedHandlerFn = dyn Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static;

/// TX handler for a relay side: either serialized or packet-based.
#[derive(Clone)]
pub enum RelayTxHandlerFn {
    Serialized(Arc<SerializedHandlerFn>),
    Packet(Arc<PacketHandlerFn>),
}

#[derive(Clone, Copy, Debug)]
pub struct RelaySideOptions {
    /// Enables the relay's per-link reliable transport layer on this side.
    ///
    /// When `true` and the side uses a serialized TX handler, reliable schema traffic on this hop
    /// gains relay-managed sequence numbers, ACKs, packet requests, and retransmits.
    /// Packet-output sides still receive decoded packets rather than serialized reliable framing.
    pub reliable_enabled: bool,
    /// Marks the side as eligible for link-local-only endpoints and discovery routes.
    pub link_local_enabled: bool,
    /// Allows packets received from this side to enter relay processing.
    pub ingress_enabled: bool,
    /// Allows the relay to transmit packets toward this side.
    pub egress_enabled: bool,
}

impl Default for RelaySideOptions {
    fn default() -> Self {
        Self {
            reliable_enabled: false,
            link_local_enabled: false,
            ingress_enabled: true,
            egress_enabled: true,
        }
    }
}

/// One side of the relay – a name + TX handler.
#[derive(Clone)]
pub struct RelaySide {
    pub name: &'static str,
    pub tx_handler: RelayTxHandlerFn,
    pub opts: RelaySideOptions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayItem {
    Serialized(Arc<[u8]>),
    Packet(Arc<Packet>),
}

/// Item that was received by the relay from some side.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RelayRxItem {
    src: RelaySideId,
    data: RelayItem,
    priority: u8,
}

impl ByteCost for RelayRxItem {
    fn byte_cost(&self) -> usize {
        match &self.data {
            RelayItem::Serialized(bytes) => bytes.len(),
            RelayItem::Packet(pkt) => pkt.byte_cost(),
        }
    }
}

/// Item that is ready to be transmitted out a destination side.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RelayTxItem {
    src: Option<RelaySideId>,
    dst: RelaySideId,
    data: RelayItem,
    priority: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RelayReplayItem {
    dst: RelaySideId,
    bytes: Arc<[u8]>,
    priority: u8,
}

impl ByteCost for RelayTxItem {
    fn byte_cost(&self) -> usize {
        match &self.data {
            RelayItem::Serialized(bytes) => bytes.len(),
            RelayItem::Packet(pkt) => pkt.byte_cost(),
        }
    }
}

impl ByteCost for RelayReplayItem {
    fn byte_cost(&self) -> usize {
        self.bytes.len()
    }
}

// -------------------- Reliable delivery state (relay) --------------------

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
struct ReliableRxState {
    expected_seq: u32,
    buffered: BTreeMap<u32, Arc<[u8]>>,
}

#[derive(Debug, Clone)]
struct ReliableReturnRouteState {
    side: RelaySideId,
}

#[cfg(feature = "discovery")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DiscoverySenderState {
    reachable: Vec<crate::DataEndpoint>,
    reachable_timesync_sources: Vec<String>,
    topology_boards: Vec<TopologyBoardNode>,
    last_seen_ms: u64,
}

#[inline]
fn is_internal_control_type(ty: crate::DataType) -> bool {
    if matches!(
        ty,
        crate::DataType::ReliableAck
            | crate::DataType::ReliablePartialAck
            | crate::DataType::ReliablePacketRequest
    ) {
        return true;
    }

    #[cfg(feature = "timesync")]
    if matches!(
        ty,
        crate::DataType::TimeSyncAnnounce
            | crate::DataType::TimeSyncRequest
            | crate::DataType::TimeSyncResponse
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

#[cfg(feature = "discovery")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DiscoverySideState {
    reachable: Vec<crate::DataEndpoint>,
    reachable_timesync_sources: Vec<String>,
    last_seen_ms: u64,
    announcers: BTreeMap<String, DiscoverySenderState>,
}

#[derive(Debug, Clone, Default)]
struct AdaptiveRouteStats {
    estimated_bandwidth_bps: u64,
    peak_bandwidth_bps: u64,
    last_observed_ms: u64,
    sample_count: u64,
    window_started_ms: u64,
    window_bytes: u64,
    peak_usage_bps: u64,
}

impl AdaptiveRouteStats {
    #[inline]
    fn observe(&mut self, bytes: usize, sample_bps: u64, now_ms: u64) {
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
        self.peak_bandwidth_bps = self.peak_bandwidth_bps.max(sample_bps);
        self.last_observed_ms = now_ms;
        self.sample_count = self.sample_count.saturating_add(1);
        if self.window_started_ms == 0 || now_ms.saturating_sub(self.window_started_ms) > 1_000 {
            self.window_started_ms = now_ms;
            self.window_bytes = 0;
        }
        self.window_bytes = self.window_bytes.saturating_add(bytes as u64);
        self.peak_usage_bps = self.peak_usage_bps.max(self.current_usage_bps(now_ms));
    }

    #[inline]
    fn current_usage_bps(&self, now_ms: u64) -> u64 {
        if self.window_started_ms == 0 {
            return 0;
        }
        let elapsed_ms = now_ms.saturating_sub(self.window_started_ms).max(1);
        ((u128::from(self.window_bytes)).saturating_mul(1000) / u128::from(elapsed_ms))
            .min(u128::from(u64::MAX)) as u64
    }

    #[inline]
    fn available_headroom_bps(&self, now_ms: u64) -> u64 {
        let capacity = self
            .estimated_bandwidth_bps
            .max(self.peak_bandwidth_bps)
            .max(1);
        capacity.saturating_sub(self.current_usage_bps(now_ms))
    }

    #[inline]
    fn weight(&self, now_ms: u64) -> u64 {
        self.available_headroom_bps(now_ms).max(1)
    }

    #[inline]
    fn snapshot(&self, now_ms: u64, auto_balancing_enabled: bool) -> AdaptiveLinkStats {
        let current_usage_bps = self.current_usage_bps(now_ms);
        let estimated_capacity_bps = self.estimated_bandwidth_bps.max(1);
        let peak_capacity_bps = self.peak_bandwidth_bps.max(estimated_capacity_bps);
        let available_headroom_bps = peak_capacity_bps.saturating_sub(current_usage_bps);
        AdaptiveLinkStats {
            auto_balancing_enabled,
            estimated_capacity_bps,
            peak_capacity_bps,
            current_usage_bps,
            peak_usage_bps: self.peak_usage_bps.max(current_usage_bps),
            available_headroom_bps,
            effective_weight: available_headroom_bps.max(1),
            last_observed_ms: self.last_observed_ms,
            sample_count: self.sample_count,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct TypeRuntimeStatsInner {
    tx_packets: u64,
    tx_bytes: u64,
    rx_packets: u64,
    rx_bytes: u64,
    relayed_tx_packets: u64,
    relayed_tx_bytes: u64,
    relayed_rx_packets: u64,
    relayed_rx_bytes: u64,
    tx_retries: u64,
    handler_failures: u64,
}

#[derive(Debug, Clone, Default)]
struct SideRuntimeStatsInner {
    tx_packets: u64,
    tx_bytes: u64,
    rx_packets: u64,
    rx_bytes: u64,
    relayed_tx_packets: u64,
    relayed_tx_bytes: u64,
    relayed_rx_packets: u64,
    relayed_rx_bytes: u64,
    tx_retries: u64,
    tx_handler_failures: u64,
    total_handler_retries: u64,
    data_types: BTreeMap<u32, TypeRuntimeStatsInner>,
}

impl SideRuntimeStatsInner {
    fn type_stats_mut(&mut self, ty: crate::DataType) -> &mut TypeRuntimeStatsInner {
        self.data_types.entry(ty.as_u32()).or_default()
    }

    fn note_tx(&mut self, ty: crate::DataType, bytes: usize, retries: usize) {
        self.tx_packets = self.tx_packets.saturating_add(1);
        self.tx_bytes = self.tx_bytes.saturating_add(bytes as u64);
        self.relayed_tx_packets = self.relayed_tx_packets.saturating_add(1);
        self.relayed_tx_bytes = self.relayed_tx_bytes.saturating_add(bytes as u64);
        self.tx_retries = self.tx_retries.saturating_add(retries as u64);
        self.total_handler_retries = self.total_handler_retries.saturating_add(retries as u64);
        let stats = self.type_stats_mut(ty);
        stats.tx_packets = stats.tx_packets.saturating_add(1);
        stats.tx_bytes = stats.tx_bytes.saturating_add(bytes as u64);
        stats.relayed_tx_packets = stats.relayed_tx_packets.saturating_add(1);
        stats.relayed_tx_bytes = stats.relayed_tx_bytes.saturating_add(bytes as u64);
        stats.tx_retries = stats.tx_retries.saturating_add(retries as u64);
    }

    fn note_rx(&mut self, ty: crate::DataType, bytes: usize) {
        self.rx_packets = self.rx_packets.saturating_add(1);
        self.rx_bytes = self.rx_bytes.saturating_add(bytes as u64);
        self.relayed_rx_packets = self.relayed_rx_packets.saturating_add(1);
        self.relayed_rx_bytes = self.relayed_rx_bytes.saturating_add(bytes as u64);
        let stats = self.type_stats_mut(ty);
        stats.rx_packets = stats.rx_packets.saturating_add(1);
        stats.rx_bytes = stats.rx_bytes.saturating_add(bytes as u64);
        stats.relayed_rx_packets = stats.relayed_rx_packets.saturating_add(1);
        stats.relayed_rx_bytes = stats.relayed_rx_bytes.saturating_add(bytes as u64);
    }

    fn note_tx_failure(&mut self, ty: crate::DataType, retries: usize) {
        self.tx_handler_failures = self.tx_handler_failures.saturating_add(1);
        self.tx_retries = self.tx_retries.saturating_add(retries as u64);
        self.total_handler_retries = self.total_handler_retries.saturating_add(retries as u64);
        let stats = self.type_stats_mut(ty);
        stats.handler_failures = stats.handler_failures.saturating_add(1);
        stats.tx_retries = stats.tx_retries.saturating_add(retries as u64);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteSelectionOrigin {
    Flood,
    Discovered,
}

/// Internal state, protected by RouterMutex so all public methods can take &self.
struct RelayInner {
    sides: Vec<Option<RelaySide>>,
    route_overrides: BTreeMap<(Option<RelaySideId>, RelaySideId), bool>,
    typed_route_overrides: BTreeMap<(Option<RelaySideId>, u32, RelaySideId), bool>,
    route_weights: BTreeMap<(Option<RelaySideId>, RelaySideId), u32>,
    route_priorities: BTreeMap<(Option<RelaySideId>, RelaySideId), u32>,
    source_route_modes: BTreeMap<Option<RelaySideId>, RouteSelectionMode>,
    route_selection_cursors: BTreeMap<Option<RelaySideId>, u64>,
    adaptive_route_stats: BTreeMap<RelaySideId, AdaptiveRouteStats>,
    side_runtime_stats: BTreeMap<RelaySideId, SideRuntimeStatsInner>,
    rx_queue: BoundedDeque<RelayRxItem>,
    tx_queue: BoundedDeque<RelayTxItem>,
    replay_queue: BoundedDeque<RelayReplayItem>,
    recent_rx: BoundedDeque<u64>,
    reliable_tx: BTreeMap<(RelaySideId, u32), ReliableTxState>,
    reliable_rx: BTreeMap<(RelaySideId, u32), ReliableRxState>,
    reliable_return_routes: BTreeMap<u64, ReliableReturnRouteState>,
    reliable_return_route_order: VecDeque<u64>,
    end_to_end_acked_destinations: BTreeMap<u64, BTreeSet<u64>>,
    end_to_end_acked_destination_order: VecDeque<u64>,
    total_handler_failures: u64,
    total_handler_retries: u64,
    #[cfg(feature = "discovery")]
    discovery_routes: BTreeMap<RelaySideId, DiscoverySideState>,
    #[cfg(feature = "discovery")]
    discovery_cadence: DiscoveryCadenceState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelayQueueKind {
    Rx,
    Tx,
    Replay,
    Recent,
    ReliableRxBuffer,
    #[cfg(feature = "discovery")]
    Discovery,
}

impl RelayInner {
    #[cfg(feature = "discovery")]
    fn topology_board_byte_cost(board: &TopologyBoardNode) -> usize {
        board
            .sender_id
            .len()
            .saturating_add(board.reachable_endpoints.len() * size_of::<crate::DataEndpoint>())
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
            .saturating_add(state.reachable.len() * size_of::<crate::DataEndpoint>())
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
    fn discovery_route_byte_cost(route: &DiscoverySideState) -> usize {
        size_of::<DiscoverySideState>()
            .saturating_add(route.reachable.len() * size_of::<crate::DataEndpoint>())
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
    }

    #[cfg(feature = "discovery")]
    fn discovery_bytes_used(&self) -> usize {
        self.discovery_routes
            .values()
            .map(Self::discovery_route_byte_cost)
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
        self.rx_queue
            .bytes_used()
            .saturating_add(self.tx_queue.bytes_used())
            .saturating_add(self.replay_queue.bytes_used())
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

    fn pop_shared_queue_item(&mut self, preferred: RelayQueueKind) -> bool {
        match preferred {
            RelayQueueKind::Rx => self.rx_queue.pop_front().is_some(),
            RelayQueueKind::Tx => self.tx_queue.pop_front().is_some(),
            RelayQueueKind::Replay => self.replay_queue.pop_front().is_some(),
            RelayQueueKind::Recent => self.recent_rx.pop_front().is_some(),
            RelayQueueKind::ReliableRxBuffer => self.pop_reliable_rx_buffered().is_some(),
            #[cfg(feature = "discovery")]
            RelayQueueKind::Discovery => self.pop_discovery_route(),
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

    fn largest_shared_queue(&self) -> Option<RelayQueueKind> {
        let candidates = [
            (
                RelayQueueKind::Rx,
                self.rx_queue.bytes_used(),
                self.rx_queue.len(),
            ),
            (
                RelayQueueKind::Tx,
                self.tx_queue.bytes_used(),
                self.tx_queue.len(),
            ),
            (
                RelayQueueKind::Replay,
                self.replay_queue.bytes_used(),
                self.replay_queue.len(),
            ),
            (RelayQueueKind::Recent, 0, 0),
            (
                RelayQueueKind::ReliableRxBuffer,
                self.reliable_rx_buffered_bytes(),
                self.reliable_rx_buffer_len(),
            ),
            #[cfg(feature = "discovery")]
            (
                RelayQueueKind::Discovery,
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
                    if *kind == RelayQueueKind::ReliableRxBuffer {
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
        preferred: RelayQueueKind,
    ) -> TelemetryResult<()> {
        if incoming_cost > MAX_QUEUE_BUDGET {
            return Err(TelemetryError::PacketTooLarge(
                "Item exceeds maximum shared queue budget",
            ));
        }

        while self.shared_queue_bytes_used().saturating_add(incoming_cost) > MAX_QUEUE_BUDGET {
            let victim = self.largest_shared_queue().unwrap_or(preferred);
            if victim == RelayQueueKind::Discovery {
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

    fn push_rx(&mut self, item: RelayRxItem) -> TelemetryResult<()> {
        self.make_shared_queue_room(item.byte_cost(), RelayQueueKind::Rx)?;
        self.rx_queue
            .push_back_prioritized(item, |queued| queued.priority)
    }

    fn push_tx(&mut self, item: RelayTxItem) -> TelemetryResult<()> {
        self.make_shared_queue_room(item.byte_cost(), RelayQueueKind::Tx)?;
        self.tx_queue
            .push_back_prioritized(item, |queued| queued.priority)
    }

    fn push_replay(&mut self, item: RelayReplayItem) -> TelemetryResult<()> {
        self.make_shared_queue_room(item.byte_cost(), RelayQueueKind::Replay)?;
        self.replay_queue
            .push_back_prioritized(item, |queued| queued.priority)
    }

    fn push_recent_rx(&mut self, id: u64) -> TelemetryResult<()> {
        while self.recent_rx.len() >= MAX_RECENT_RX_IDS {
            let _ = self.recent_rx.pop_front();
        }
        self.make_shared_queue_room(0, RelayQueueKind::Recent)?;
        self.recent_rx.push_back(id)
    }

    fn buffer_reliable_rx(
        &mut self,
        side: RelaySideId,
        ty: crate::DataType,
        seq: u32,
        bytes: Arc<[u8]>,
    ) -> TelemetryResult<()> {
        let key = Relay::reliable_key(side, ty);
        if self
            .reliable_rx
            .get(&key)
            .is_some_and(|state| state.buffered.contains_key(&seq))
        {
            return Ok(());
        }
        let cost = size_of::<Arc<[u8]>>() + bytes.len();
        self.make_shared_queue_room(cost, RelayQueueKind::ReliableRxBuffer)?;
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

/// Relay that fans out packets from one side to all others.
/// - Supports both serialized bytes and full Packet.
/// - Has RX & TX queues, like Router.
/// - Uses a Clock for the *_with_timeout APIs, same style as Router.
pub struct Relay {
    state: RouterMutex<RelayInner>,
    side_tx_gate: ReentryGate,
    clock: Box<dyn Clock + Send + Sync>,
}

enum RemoteSidePlan {
    Target(Vec<RelaySideId>),
}

impl Relay {
    const END_TO_END_ACK_SENDER: &'static str = "E2EACK";
    const END_TO_END_ACK_PREFIX: &'static str = "E2EACK:";

    fn relay_item_priority(data: &RelayItem) -> TelemetryResult<u8> {
        let ty = match data {
            RelayItem::Packet(pkt) => pkt.data_type(),
            RelayItem::Serialized(bytes) => serialize::peek_envelope(bytes.as_ref())?.ty,
        };
        Ok(message_priority(ty))
    }

    #[inline]
    fn is_side_tx_busy(err: &TelemetryError) -> bool {
        matches!(err, TelemetryError::Io("side tx busy"))
    }

    fn process_replay_queue_item(&self) -> TelemetryResult<bool> {
        let Some(item) = ({
            let mut st = self.state.lock();
            st.replay_queue.pop_front()
        }) else {
            return Ok(false);
        };
        let frame = serialize::peek_frame_info(item.bytes.as_ref())?;
        let ty = frame.envelope.ty;
        let Some(hdr) = frame.reliable else {
            return Ok(false);
        };
        {
            let mut st = self.state.lock();
            let tx_state = self.reliable_tx_state_mut(&mut st, item.dst, ty);
            if !tx_state.sent.contains_key(&hdr.seq) {
                return Ok(false);
            }
        }
        if let Err(e) = self.send_reliable_raw_to_side(item.dst, item.bytes.clone()) {
            if Self::is_side_tx_busy(&e) {
                let mut st = self.state.lock();
                st.push_replay(item)?;
                return Ok(false);
            }
            return Err(e);
        }
        let mut st = self.state.lock();
        let tx_state = self.reliable_tx_state_mut(&mut st, item.dst, ty);
        if let Some(sent) = tx_state.sent.get_mut(&hdr.seq) {
            sent.last_send_ms = self.clock.now_ms();
            sent.queued = false;
        }
        Ok(true)
    }

    fn pop_ready_tx_item(
        &self,
    ) -> Option<(
        Option<RelaySideId>,
        RelaySideId,
        RelayTxHandlerFn,
        RelaySideOptions,
        RelayItem,
    )> {
        let mut st = self.state.lock();
        if let Some(item) = st.tx_queue.pop_front() {
            let side = st.sides.get(item.dst).and_then(|side| side.clone());
            side.map(|s| (item.src, item.dst, s.tx_handler, s.opts, item.data))
        } else {
            None
        }
    }

    fn send_tx_item(
        &self,
        src: Option<RelaySideId>,
        dst: RelaySideId,
        handler: RelayTxHandlerFn,
        opts: RelaySideOptions,
        data: RelayItem,
    ) -> TelemetryResult<bool> {
        let allowed = {
            let st = self.state.lock();
            let ty = match &data {
                RelayItem::Packet(pkt) => Some(pkt.data_type()),
                RelayItem::Serialized(bytes) => Some(serialize::peek_envelope(bytes.as_ref())?.ty),
            };
            self.route_allowed_locked(&st, src, ty, dst)
        };
        if !allowed {
            return Ok(false);
        }
        if opts.reliable_enabled && matches!(handler, RelayTxHandlerFn::Serialized(_)) {
            self.send_reliable_to_side(dst, data)?;
            Ok(true)
        } else if let Some(adjusted) = self.adjust_reliable_for_side(opts, data)? {
            self.call_tx_handler(dst, &handler, &adjusted)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Create a new relay with the given clock.
    pub fn new(clock: Box<dyn Clock + Send + Sync>) -> Self {
        Self {
            state: RouterMutex::new(RelayInner {
                sides: Vec::new(),
                route_overrides: BTreeMap::new(),
                typed_route_overrides: BTreeMap::new(),
                route_weights: BTreeMap::new(),
                route_priorities: BTreeMap::new(),
                source_route_modes: BTreeMap::new(),
                route_selection_cursors: BTreeMap::new(),
                adaptive_route_stats: BTreeMap::new(),
                side_runtime_stats: BTreeMap::new(),
                rx_queue: BoundedDeque::new(MAX_QUEUE_BUDGET, STARTING_QUEUE_SIZE, QUEUE_GROW_STEP),
                tx_queue: BoundedDeque::new(MAX_QUEUE_BUDGET, STARTING_QUEUE_SIZE, QUEUE_GROW_STEP),
                replay_queue: BoundedDeque::new(
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
                end_to_end_acked_destinations: BTreeMap::new(),
                end_to_end_acked_destination_order: VecDeque::new(),
                total_handler_failures: 0,
                total_handler_retries: 0,
                #[cfg(feature = "discovery")]
                discovery_routes: BTreeMap::new(),
                #[cfg(feature = "discovery")]
                discovery_cadence: DiscoveryCadenceState::default(),
            }),
            side_tx_gate: ReentryGate::new(),
            clock,
        }
    }

    #[inline]
    fn try_enter_side_tx(&self) -> Option<ReentryGuard<'_>> {
        self.side_tx_gate.try_enter()
    }

    #[inline]
    fn side_tx_active(&self) -> bool {
        self.side_tx_gate.is_active()
    }

    #[inline]
    fn side_ref(st: &RelayInner, side: RelaySideId) -> TelemetryResult<&RelaySide> {
        st.sides
            .get(side)
            .and_then(|side| side.as_ref())
            .ok_or(TelemetryError::HandlerError("relay: invalid side id"))
    }

    fn note_side_tx_success(
        &self,
        side: RelaySideId,
        ty: crate::DataType,
        bytes: usize,
        attempts: usize,
    ) {
        let mut st = self.state.lock();
        let entry = st.side_runtime_stats.entry(side).or_default();
        entry.note_tx(ty, bytes, attempts.saturating_sub(1));
    }

    fn note_side_tx_failure(&self, side: RelaySideId, ty: crate::DataType, attempts: usize) {
        let mut st = self.state.lock();
        st.total_handler_failures = st.total_handler_failures.saturating_add(1);
        st.total_handler_retries = st.total_handler_retries.saturating_add(attempts as u64);
        let entry = st.side_runtime_stats.entry(side).or_default();
        entry.note_tx_failure(ty, attempts);
    }

    fn note_side_rx(&self, side: RelaySideId, ty: crate::DataType, bytes: usize) {
        let mut st = self.state.lock();
        let entry = st.side_runtime_stats.entry(side).or_default();
        entry.note_rx(ty, bytes);
    }

    #[inline]
    fn ensure_side_ingress_enabled(&self, side: RelaySideId) -> TelemetryResult<()> {
        let st = self.state.lock();
        let side_ref = Self::side_ref(&st, side)?;
        if side_ref.opts.ingress_enabled {
            Ok(())
        } else {
            Err(TelemetryError::HandlerError(
                "relay: ingress disabled for side id",
            ))
        }
    }

    #[inline]
    fn route_allowed_locked(
        &self,
        st: &RelayInner,
        src: Option<RelaySideId>,
        ty: Option<crate::DataType>,
        dst: RelaySideId,
    ) -> bool {
        let Ok(dst_side) = Self::side_ref(st, dst) else {
            return false;
        };
        if !dst_side.opts.egress_enabled {
            return false;
        }
        if let Some(src_id) = src {
            let Ok(src_side) = Self::side_ref(st, src_id) else {
                return false;
            };
            if !src_side.opts.ingress_enabled || src_id == dst {
                return false;
            }
        }
        let base_allowed = st.route_overrides.get(&(src, dst)).copied().unwrap_or(true);
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
        st: &RelayInner,
        src: Option<RelaySideId>,
        ty: Option<crate::DataType>,
        restrict_link_local: bool,
    ) -> Vec<RelaySideId> {
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
        st: &mut RelayInner,
        src: Option<RelaySideId>,
        mut sides: Vec<RelaySideId>,
        origin: RouteSelectionOrigin,
    ) -> Vec<RelaySideId> {
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
        st: &mut RelayInner,
        src: Option<RelaySideId>,
        mut sides: Vec<RelaySideId>,
    ) -> Vec<RelaySideId> {
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

        let now_ms = self.clock.now_ms();
        let total_weight = sides.iter().fold(0_u64, |acc, side| {
            acc + st
                .adaptive_route_stats
                .get(side)
                .map(|stats| stats.weight(now_ms))
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
                .map(|stats| stats.weight(now_ms))
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
        side: RelaySideId,
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
            .observe(bytes, sample_bps, ended_ms);
    }

    fn relay_item_wire_len(data: &RelayItem) -> TelemetryResult<usize> {
        match data {
            RelayItem::Packet(pkt) => Ok(serialize::serialize_packet(pkt).len()),
            RelayItem::Serialized(bytes) => Ok(bytes.len()),
        }
    }

    #[inline]
    fn decode_end_to_end_reliable_ack(payload: &[u8]) -> TelemetryResult<u64> {
        if payload.len() != 8 {
            return Err(TelemetryError::Deserialize("bad reliable e2e ack payload"));
        }
        Ok(u64::from_le_bytes(payload[0..8].try_into().unwrap()))
    }

    #[inline]
    fn is_end_to_end_ack_sender(sender: &str) -> bool {
        sender == Self::END_TO_END_ACK_SENDER || sender.starts_with(Self::END_TO_END_ACK_PREFIX)
    }

    #[inline]
    fn sender_hash(sender: &str) -> u64 {
        hash_bytes_u64(0x517C_C1B7_2722_0A95, sender.as_bytes())
    }

    fn decode_end_to_end_ack_sender_hash(sender: &str) -> Option<u64> {
        sender
            .strip_prefix(Self::END_TO_END_ACK_PREFIX)
            .filter(|sender| !sender.is_empty())
            .map(Self::sender_hash)
    }

    #[cfg(feature = "discovery")]
    fn is_end_to_end_destination_sender(sender: &str) -> bool {
        sender != "RELAY" && !Self::is_end_to_end_ack_sender(sender)
    }

    /// Extract the logical packet ID targeted by an end-to-end reliable ACK item.
    ///
    /// Relay queues can hold either decoded packets or serialized frames. This
    /// helper normalizes both forms so relay ACK-routing logic can treat them
    /// uniformly.
    ///
    /// Only relay-visible end-to-end `ReliableAck` packets qualify here.
    /// Unrelated traffic returns `Ok(None)`.
    fn reliable_control_target_packet_id(data: &RelayItem) -> TelemetryResult<Option<u64>> {
        match data {
            RelayItem::Packet(pkt) => {
                if pkt.data_type() != crate::DataType::ReliableAck
                    || !Self::is_end_to_end_ack_sender(pkt.sender())
                {
                    return Ok(None);
                }
                Self::decode_end_to_end_reliable_ack(pkt.payload()).map(Some)
            }
            RelayItem::Serialized(bytes) => {
                let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                if pkt.data_type() != crate::DataType::ReliableAck
                    || !Self::is_end_to_end_ack_sender(pkt.sender())
                {
                    return Ok(None);
                }
                Self::decode_end_to_end_reliable_ack(pkt.payload()).map(Some)
            }
        }
    }

    fn note_reliable_return_route(&self, side: RelaySideId, packet_id: u64) {
        let mut st = self.state.lock();
        Self::remember_reliable_return_route_locked(&mut st, packet_id);
        st.reliable_return_routes
            .insert(packet_id, ReliableReturnRouteState { side });
    }

    /// Refresh or insert `packet_id` in the bounded reliable return-route cache.
    ///
    /// The relay uses this cache to route end-to-end acknowledgements back
    /// toward the source side that most recently forwarded the corresponding
    /// reliable data packet.
    fn remember_reliable_return_route_locked(st: &mut RelayInner, packet_id: u64) {
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

    fn note_end_to_end_acked_destination_locked(
        st: &mut RelayInner,
        packet_id: u64,
        sender_hash: u64,
    ) {
        let entry_cap = RELIABLE_MAX_END_TO_END_ACK_CACHE.max(1);
        st.end_to_end_acked_destination_order
            .retain(|id| st.end_to_end_acked_destinations.contains_key(id) && *id != packet_id);
        while st.end_to_end_acked_destination_order.len() >= entry_cap {
            if let Some(oldest) = st.end_to_end_acked_destination_order.pop_front() {
                st.end_to_end_acked_destinations.remove(&oldest);
            } else {
                break;
            }
        }
        st.end_to_end_acked_destination_order.push_back(packet_id);

        let acked = st
            .end_to_end_acked_destinations
            .entry(packet_id)
            .or_default();
        let sender_cap = RELIABLE_MAX_END_TO_END_PENDING.max(1);
        if acked.len() < sender_cap || acked.contains(&sender_hash) {
            acked.insert(sender_hash);
        }
    }

    #[inline]
    fn reliable_key(side: RelaySideId, ty: crate::DataType) -> (RelaySideId, u32) {
        (side, ty.as_u32())
    }

    fn reliable_tx_state_mut<'a>(
        &'a self,
        st: &'a mut RelayInner,
        side: RelaySideId,
        ty: crate::DataType,
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
        st: &'a mut RelayInner,
        side: RelaySideId,
        ty: crate::DataType,
    ) -> &'a mut ReliableRxState {
        let key = Self::reliable_key(side, ty);
        st.reliable_rx
            .entry(key)
            .or_insert_with(|| ReliableRxState {
                expected_seq: 1,
                buffered: BTreeMap::new(),
            })
    }

    fn handle_reliable_ack(&self, side: RelaySideId, ty: crate::DataType, ack: u32) {
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

    fn handle_reliable_partial_ack(&self, side: RelaySideId, ty: crate::DataType, seq: u32) {
        let mut st = self.state.lock();
        let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
        if let Some(sent) = tx_state.sent.get_mut(&seq) {
            sent.partial_acked = true;
        }
    }

    fn reliable_control_packet(
        &self,
        control_ty: crate::DataType,
        ty: crate::DataType,
        seq: u32,
    ) -> TelemetryResult<Packet> {
        Packet::new(
            control_ty,
            message_meta(control_ty).endpoints,
            "RELAY",
            self.clock.now_ms(),
            crate::router::encode_slice_le(&[ty.as_u32(), seq]),
        )
    }

    fn queue_reliable_ack(
        &self,
        side: RelaySideId,
        ty: crate::DataType,
        seq: u32,
    ) -> TelemetryResult<()> {
        let pkt = self.reliable_control_packet(crate::DataType::ReliableAck, ty, seq)?;
        let data = RelayItem::Packet(Arc::new(pkt));
        let priority = Self::relay_item_priority(&data)?;
        let mut st = self.state.lock();
        st.push_tx(RelayTxItem {
            src: None,
            dst: side,
            data,
            priority,
        })?;
        Ok(())
    }

    fn queue_reliable_packet_request(
        &self,
        side: RelaySideId,
        ty: crate::DataType,
        seq: u32,
    ) -> TelemetryResult<()> {
        let pkt = self.reliable_control_packet(crate::DataType::ReliablePacketRequest, ty, seq)?;
        let data = RelayItem::Packet(Arc::new(pkt));
        let priority = Self::relay_item_priority(&data)?;
        let mut st = self.state.lock();
        st.push_tx(RelayTxItem {
            src: None,
            dst: side,
            data,
            priority,
        })?;
        Ok(())
    }

    fn queue_reliable_partial_ack(
        &self,
        side: RelaySideId,
        ty: crate::DataType,
        seq: u32,
    ) -> TelemetryResult<()> {
        let pkt = self.reliable_control_packet(crate::DataType::ReliablePartialAck, ty, seq)?;
        let data = RelayItem::Packet(Arc::new(pkt));
        let priority = Self::relay_item_priority(&data)?;
        let mut st = self.state.lock();
        st.push_tx(RelayTxItem {
            src: None,
            dst: side,
            data,
            priority,
        })?;
        Ok(())
    }

    fn queue_reliable_retransmit(
        &self,
        side: RelaySideId,
        ty: crate::DataType,
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
            let mut st = self.state.lock();
            st.push_replay(RelayReplayItem {
                dst: side,
                bytes,
                priority: message_priority(ty).saturating_add(16),
            })?;
        }
        Ok(())
    }

    fn send_reliable_raw_to_side(
        &self,
        side: RelaySideId,
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
        let ty = serialize::peek_envelope(bytes.as_ref())
            .map(|env| env.ty)
            .unwrap_or(crate::DataType::ReliableAck);
        let result = match handler {
            RelayTxHandlerFn::Serialized(f) => f(bytes.as_ref()),
            RelayTxHandlerFn::Packet(f) => {
                let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                f(&pkt)
            }
        };
        if result.is_ok() {
            self.record_side_tx_sample(side, bytes.len(), started_ms, self.clock.now_ms());
            self.note_side_tx_success(side, ty, bytes.len(), 1);
        } else {
            self.note_side_tx_failure(side, ty, 1);
        }
        result
    }

    fn send_reliable_to_side(&self, side: RelaySideId, data: RelayItem) -> TelemetryResult<()> {
        let (handler, opts, hop_reliable_enabled) = {
            let st = self.state.lock();
            let side_ref = Self::side_ref(&st, side)?;
            let opts = side_ref.opts;
            let hop_reliable_enabled = opts.reliable_enabled
                && !self.side_has_multiple_announcers_locked(&st, side, self.clock.now_ms());
            (side_ref.tx_handler.clone(), opts, hop_reliable_enabled)
        };

        let RelayTxHandlerFn::Serialized(f) = &handler else {
            return self.call_tx_handler(side, &handler, &data);
        };

        if !hop_reliable_enabled {
            let mut adjusted_opts = opts;
            adjusted_opts.reliable_enabled = false;
            if let Some(adjusted) = self.adjust_reliable_for_side(adjusted_opts, data)? {
                return self.call_tx_handler(side, &handler, &adjusted);
            }
            return Ok(());
        }

        let ty = match &data {
            RelayItem::Packet(pkt) => pkt.data_type(),
            RelayItem::Serialized(bytes) => {
                let Ok(frame) = serialize::peek_frame_info(bytes.as_ref()) else {
                    return self.call_tx_handler(side, &handler, &data);
                };
                frame.envelope.ty
            }
        };

        if !is_reliable_type(ty) {
            if let Some(adjusted) = self.adjust_reliable_for_side(opts, data)? {
                self.call_tx_handler(side, &handler, &adjusted)?;
            }
            return Ok(());
        }

        let (seq, flags) = {
            let mut st = self.state.lock();
            let tx_state = self.reliable_tx_state_mut(&mut st, side, ty);
            if tx_state.sent.len() >= RELIABLE_MAX_PENDING {
                return Err(TelemetryError::PacketTooLarge(
                    "relay reliable history full",
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
            RelayItem::Packet(pkt) => serialize::serialize_packet_with_reliable(
                &pkt,
                serialize::ReliableHeader { flags, seq, ack: 0 },
            ),
            RelayItem::Serialized(bytes) => {
                let mut v = bytes.to_vec();
                if !serialize::rewrite_reliable_header(&mut v, flags, seq, 0)? {
                    let Some(_side_tx_guard) = self.try_enter_side_tx() else {
                        return Err(TelemetryError::Io("side tx busy"));
                    };
                    let started_ms = self.clock.now_ms();
                    f(bytes.as_ref())?;
                    self.record_side_tx_sample(side, bytes.len(), started_ms, self.clock.now_ms());
                    self.note_side_tx_success(side, ty, bytes.len(), 1);
                    return Ok(());
                }
                Arc::from(v)
            }
        };

        let Some(_side_tx_guard) = self.try_enter_side_tx() else {
            return Err(TelemetryError::Io("side tx busy"));
        };
        let started_ms = self.clock.now_ms();
        f(bytes.as_ref())?;
        self.record_side_tx_sample(side, bytes.len(), started_ms, self.clock.now_ms());
        self.note_side_tx_success(side, ty, bytes.len(), 1);

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

    fn item_route_info(
        &self,
        data: &RelayItem,
    ) -> TelemetryResult<(Vec<crate::DataEndpoint>, crate::DataType)> {
        match data {
            RelayItem::Packet(pkt) => {
                let mut eps = pkt.endpoints().to_vec();
                eps.sort_unstable();
                eps.dedup();
                Ok((eps, pkt.data_type()))
            }
            RelayItem::Serialized(bytes) => {
                let env = serialize::peek_envelope(bytes.as_ref())?;
                let mut eps: Vec<crate::DataEndpoint> = env.endpoints.iter().copied().collect();
                eps.sort_unstable();
                eps.dedup();
                Ok((eps, env.ty))
            }
        }
    }

    fn endpoints_are_link_local_only(eps: &[crate::DataEndpoint]) -> bool {
        !eps.is_empty() && eps.iter().all(|ep| ep.is_link_local_only())
    }

    fn item_target_senders(&self, data: &RelayItem) -> TelemetryResult<Arc<[u64]>> {
        match data {
            RelayItem::Packet(pkt) => Ok(Arc::from(pkt.wire_target_senders())),
            RelayItem::Serialized(bytes) => {
                Ok(serialize::peek_envelope(bytes.as_ref())?.target_senders)
            }
        }
    }

    #[cfg(feature = "discovery")]
    fn side_matches_target_senders_locked(
        st: &RelayInner,
        side: RelaySideId,
        target_senders: &[u64],
        now_ms: u64,
    ) -> bool {
        st.discovery_routes
            .get(&side)
            .map(|route| {
                if now_ms.saturating_sub(route.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                    return false;
                }
                route.announcers.values().any(|sender_state| {
                    if now_ms.saturating_sub(sender_state.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                        return false;
                    }
                    sender_state
                        .topology_boards
                        .iter()
                        .any(|board| target_senders.contains(&Self::sender_hash(&board.sender_id)))
                })
            })
            .unwrap_or(false)
    }

    fn remote_side_plan(
        &self,
        data: &RelayItem,
        exclude: RelaySideId,
    ) -> TelemetryResult<RemoteSidePlan> {
        #[cfg(feature = "discovery")]
        {
            let (eps, ty) = self.item_route_info(data)?;
            let target_senders = self.item_target_senders(data)?;
            let preferred_packet_id = Self::reliable_control_target_packet_id(data)?;
            if discovery::is_discovery_type(ty) {
                let mut st = self.state.lock();
                let sides = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), false);
                return Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    sides,
                    RouteSelectionOrigin::Flood,
                )));
            }

            #[cfg(feature = "timesync")]
            let preferred_timesync_source = self.preferred_timesync_route_source(data, ty)?;
            #[cfg(not(feature = "timesync"))]
            let preferred_timesync_source: Option<String> = None;
            let mut st = self.state.lock();
            if let Some(packet_id) = preferred_packet_id {
                let target_side = self.allowed_target_side_locked(
                    &st,
                    exclude,
                    ty,
                    st.reliable_return_routes
                        .get(&packet_id)
                        .map(|route| route.side),
                );
                if let Some(side) = target_side {
                    return Ok(RemoteSidePlan::Target(vec![side]));
                }
                return Ok(RemoteSidePlan::Target(Vec::new()));
            }
            let restrict_link_local = Self::endpoints_are_link_local_only(&eps);
            let discovered_origin = if is_reliable_type(ty) {
                RouteSelectionOrigin::Flood
            } else {
                RouteSelectionOrigin::Discovered
            };
            if st.discovery_routes.is_empty() {
                if restrict_link_local {
                    let targets = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), true);
                    return Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                        &mut st,
                        Some(exclude),
                        targets,
                        RouteSelectionOrigin::Flood,
                    )));
                }
                let targets = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), false);
                return Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    targets,
                    RouteSelectionOrigin::Flood,
                )));
            }
            let now_ms = self.clock.now_ms();
            let mut had_exact = false;
            let mut exact_targets = Vec::new();
            let mut had_known = false;
            let mut generic_targets = Vec::new();

            for (&side, route) in st.discovery_routes.iter() {
                if side == exclude
                    || now_ms.saturating_sub(route.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS
                {
                    continue;
                }
                if restrict_link_local
                    && st
                        .sides
                        .get(side)
                        .and_then(|side| side.as_ref())
                        .map(|s| !s.opts.link_local_enabled)
                        .unwrap_or(true)
                {
                    continue;
                }
                if !self.route_allowed_locked(&st, Some(exclude), Some(ty), side) {
                    continue;
                }
                if !target_senders.is_empty()
                    && !Self::side_matches_target_senders_locked(&st, side, &target_senders, now_ms)
                {
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
                let targets = self.filter_end_to_end_satisfied_sides_locked(
                    &st,
                    data,
                    exact_targets,
                    &eps,
                    ty,
                )?;
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    targets,
                    discovered_origin,
                )))
            } else if had_known {
                let targets = self.filter_end_to_end_satisfied_sides_locked(
                    &st,
                    data,
                    generic_targets,
                    &eps,
                    ty,
                )?;
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    targets,
                    discovered_origin,
                )))
            } else if !target_senders.is_empty() {
                let targets = self.eligible_side_ids_locked(
                    &st,
                    Some(exclude),
                    Some(ty),
                    restrict_link_local,
                );
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    targets,
                    RouteSelectionOrigin::Flood,
                )))
            } else if restrict_link_local {
                let targets = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), true);
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    targets,
                    RouteSelectionOrigin::Flood,
                )))
            } else {
                let targets = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), false);
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    targets,
                    RouteSelectionOrigin::Flood,
                )))
            }
        }
        #[cfg(not(feature = "discovery"))]
        {
            let (_, ty) = self.item_route_info(data)?;
            let mut st = self.state.lock();
            if let Some(packet_id) = Self::reliable_control_target_packet_id(data)? {
                let target_side = self.allowed_target_side_locked(
                    &st,
                    exclude,
                    ty,
                    st.reliable_return_routes
                        .get(&packet_id)
                        .map(|route| route.side),
                );
                if let Some(side) = target_side {
                    return Ok(RemoteSidePlan::Target(vec![side]));
                }
                return Ok(RemoteSidePlan::Target(Vec::new()));
            }
            let sides = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), false);
            Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                &mut st,
                Some(exclude),
                sides,
                RouteSelectionOrigin::Flood,
            )))
        }
    }

    #[inline]
    fn allowed_target_side_locked(
        &self,
        st: &RelayInner,
        exclude: RelaySideId,
        ty: crate::DataType,
        target_side: Option<RelaySideId>,
    ) -> Option<RelaySideId> {
        target_side.filter(|side| self.route_allowed_locked(st, Some(exclude), Some(ty), *side))
    }

    fn filter_end_to_end_satisfied_sides_locked(
        &self,
        st: &RelayInner,
        data: &RelayItem,
        sides: Vec<RelaySideId>,
        eps: &[crate::DataEndpoint],
        ty: crate::DataType,
    ) -> TelemetryResult<Vec<RelaySideId>> {
        if !is_reliable_type(ty) || Self::reliable_control_target_packet_id(data)?.is_some() {
            return Ok(sides);
        }
        let packet_id = match data {
            RelayItem::Packet(pkt) => pkt.packet_id(),
            RelayItem::Serialized(bytes) => serialize::packet_id_from_wire(bytes.as_ref())?,
        };
        let Some(acked) = st.end_to_end_acked_destinations.get(&packet_id) else {
            return Ok(sides);
        };
        let now_ms = self.clock.now_ms();
        let mut filtered = Vec::new();
        for side in sides {
            let Some(route) = st.discovery_routes.get(&side) else {
                filtered.push(side);
                continue;
            };
            let mut still_pending = false;
            let mut had_destination_board = false;
            for sender_state in route.announcers.values() {
                if now_ms.saturating_sub(sender_state.last_seen_ms) > DISCOVERY_ROUTE_TTL_MS {
                    continue;
                }
                for board in sender_state.topology_boards.iter() {
                    if !Self::is_end_to_end_destination_sender(&board.sender_id) {
                        continue;
                    }
                    had_destination_board = true;
                    let sender_hash = Self::sender_hash(&board.sender_id);
                    if acked.contains(&sender_hash) {
                        continue;
                    }
                    if eps
                        .iter()
                        .copied()
                        .any(|ep| board.reachable_endpoints.contains(&ep))
                    {
                        still_pending = true;
                        break;
                    }
                    // Keep forwarding while any discovered destination sender for this packet
                    // remains unacked, even if topology/schema metadata changed for new packets.
                    still_pending = true;
                    break;
                }
                if still_pending {
                    break;
                }
            }
            if still_pending || !had_destination_board {
                filtered.push(side);
            }
        }
        Ok(filtered)
    }

    #[cfg(feature = "discovery")]
    fn side_has_multiple_announcers_locked(
        &self,
        st: &RelayInner,
        side: RelaySideId,
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
        _st: &RelayInner,
        _side: RelaySideId,
        _now_ms: u64,
    ) -> bool {
        false
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
    fn local_discovery_topology_board(&self, st: &RelayInner, now_ms: u64) -> TopologyBoardNode {
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
            sender_id: "RELAY".to_string(),
            reachable_endpoints: Vec::new(),
            reachable_timesync_sources: Vec::new(),
            connections,
        }
    }

    #[cfg(feature = "discovery")]
    fn advertised_discovery_topology_for_link_locked(
        &self,
        st: &RelayInner,
        now_ms: u64,
        link_local_enabled: bool,
    ) -> Vec<TopologyBoardNode> {
        let mut boards = vec![self.local_discovery_topology_board(st, now_ms)];
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
                        connections: vec!["RELAY".to_string()],
                    });
                } else if let Some(board) = sender_boards
                    .iter_mut()
                    .find(|board| board.sender_id == *announcer)
                {
                    board.connections.push("RELAY".to_string());
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
    fn note_discovery_topology_change_locked(st: &mut RelayInner, now_ms: u64) {
        st.discovery_cadence.on_topology_change(now_ms);
    }

    #[cfg(feature = "discovery")]
    fn prune_discovery_routes_locked(st: &mut RelayInner, now_ms: u64) -> bool {
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
    fn reconcile_end_to_end_acked_destinations_locked(st: &mut RelayInner) {
        let mut active_senders = BTreeSet::new();
        for route in st.discovery_routes.values() {
            for sender_state in route.announcers.values() {
                for board in sender_state.topology_boards.iter() {
                    if Self::is_end_to_end_destination_sender(&board.sender_id) {
                        active_senders.insert(Self::sender_hash(&board.sender_id));
                    }
                }
            }
        }
        st.end_to_end_acked_destinations.retain(|_, acked| {
            acked.retain(|sender_hash| active_senders.contains(sender_hash));
            !acked.is_empty()
        });
    }

    #[cfg(feature = "discovery")]
    fn advertised_discovery_endpoints_for_link_locked(
        &self,
        st: &RelayInner,
        now_ms: u64,
        link_local_enabled: bool,
    ) -> Vec<crate::DataEndpoint> {
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
        st: &RelayInner,
        now_ms: u64,
    ) -> Vec<String> {
        let (_, sources) = discovery::summarize_topology_boards(
            &self.advertised_discovery_topology_for_link_locked(st, now_ms, true),
        );
        sources
    }

    #[cfg(feature = "discovery")]
    #[cfg(feature = "timesync")]
    fn preferred_timesync_route_source(
        &self,
        data: &RelayItem,
        ty: crate::DataType,
    ) -> TelemetryResult<Option<String>> {
        if !matches!(
            ty,
            crate::DataType::TimeSyncAnnounce | crate::DataType::TimeSyncResponse
        ) {
            return Ok(None);
        }

        let sender = match data {
            RelayItem::Packet(pkt) => pkt.sender().to_owned(),
            RelayItem::Serialized(bytes) => serialize::deserialize_packet(bytes.as_ref())?
                .sender()
                .to_owned(),
        };
        Ok(Some(sender))
    }

    #[cfg(feature = "discovery")]
    fn queue_discovery_announce(&self) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let per_side = {
            let mut st = self.state.lock();
            if Self::prune_discovery_routes_locked(&mut st, now_ms) {
                Self::reconcile_end_to_end_acked_destinations_locked(&mut st);
                Self::note_discovery_topology_change_locked(&mut st, now_ms);
            }
            st.fit_discovery_budget();
            if !st.sides.iter().any(|side| side.is_some()) {
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
                        Some(crate::DataType::DiscoveryAnnounce),
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
        let mut st = self.state.lock();
        for (dst, endpoints, timesync_sources, topology) in per_side {
            let pkt = discovery::build_discovery_schema("RELAY", now_ms)?;
            let data = RelayItem::Packet(Arc::new(pkt));
            let priority = Self::relay_item_priority(&data)?;
            st.push_tx(RelayTxItem {
                src: None,
                dst,
                data,
                priority,
            })?;
            if !endpoints.is_empty() {
                let pkt =
                    discovery::build_discovery_announce("RELAY", now_ms, endpoints.as_slice())?;
                let data = RelayItem::Packet(Arc::new(pkt));
                let priority = Self::relay_item_priority(&data)?;
                st.push_tx(RelayTxItem {
                    src: None,
                    dst,
                    data,
                    priority,
                })?;
            }
            if !timesync_sources.is_empty() {
                let pkt = discovery::build_discovery_timesync_sources(
                    "RELAY",
                    now_ms,
                    timesync_sources.as_slice(),
                )?;
                let data = RelayItem::Packet(Arc::new(pkt));
                let priority = Self::relay_item_priority(&data)?;
                st.push_tx(RelayTxItem {
                    src: None,
                    dst,
                    data,
                    priority,
                })?;
            }
            if !topology.is_empty() {
                let pkt = discovery::build_discovery_topology("RELAY", now_ms, &topology)?;
                let data = RelayItem::Packet(Arc::new(pkt));
                let priority = Self::relay_item_priority(&data)?;
                st.push_tx(RelayTxItem {
                    src: None,
                    dst,
                    data,
                    priority,
                })?;
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
                Self::reconcile_end_to_end_acked_destinations_locked(&mut st);
                Self::note_discovery_topology_change_locked(&mut st, now_ms);
            }
            st.fit_discovery_budget();
            let has_any = st.sides.iter().enumerate().any(|(side_id, side)| {
                let Some(side) = side.as_ref() else {
                    return false;
                };
                if !self.route_allowed_locked(
                    &st,
                    None,
                    Some(crate::DataType::DiscoveryAnnounce),
                    side_id,
                ) {
                    return false;
                }
                let _ = side;
                true
            });
            if !st.sides.iter().any(|side| side.is_some()) || !has_any {
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
    fn learn_discovery_item(&self, src: RelaySideId, data: &RelayItem) -> TelemetryResult<()> {
        let pkt = match data {
            RelayItem::Packet(pkt) => {
                if !discovery::is_discovery_type(pkt.data_type()) {
                    return Ok(());
                }
                pkt.as_ref().clone()
            }
            RelayItem::Serialized(bytes) => {
                let env = serialize::peek_envelope(bytes.as_ref())?;
                if !discovery::is_discovery_type(env.ty) {
                    return Ok(());
                }
                serialize::deserialize_packet(bytes.as_ref())?
            }
        };

        let now_ms = self.clock.now_ms();
        if pkt.data_type() == crate::DataType::DiscoverySchema {
            let snapshot = discovery::decode_discovery_schema(&pkt)?;
            let incoming_cost = crate::config::owned_schema_byte_cost(&snapshot);
            let mut st = self.state.lock();
            st.make_shared_queue_room(incoming_cost, RelayQueueKind::Discovery)?;
            drop(st);
            let report =
                crate::config::merge_owned_schema_snapshot_with_budget(snapshot, MAX_QUEUE_BUDGET)?;
            if report.changed() {
                let mut st = self.state.lock();
                st.fit_discovery_budget();
                Self::note_discovery_topology_change_locked(&mut st, now_ms);
            }
            return Ok(());
        }
        let mut st = self.state.lock();
        let mut route = st.discovery_routes.get(&src).cloned().unwrap_or_default();
        let side_link_local_enabled = st
            .sides
            .get(src)
            .and_then(|entry| entry.as_ref())
            .map(|side_ref| side_ref.opts.link_local_enabled)
            .unwrap_or(false);
        let mut sender_state = route
            .announcers
            .get(pkt.sender())
            .cloned()
            .unwrap_or_default();
        let changed = match pkt.data_type() {
            crate::DataType::DiscoveryAnnounce => {
                let mut reachable = discovery::decode_discovery_announce(&pkt)?;
                if !side_link_local_enabled {
                    reachable.retain(|ep| !ep.is_link_local_only());
                }
                let board = Self::sender_topology_board_mut(&mut sender_state, pkt.sender());
                let changed = board.reachable_endpoints != reachable;
                board.reachable_endpoints = reachable;
                Self::refresh_sender_topology_state(&mut sender_state);
                changed
            }
            crate::DataType::DiscoveryTimeSyncSources => {
                let sources = discovery::decode_discovery_timesync_sources(&pkt)?;
                let board = Self::sender_topology_board_mut(&mut sender_state, pkt.sender());
                let changed = board.reachable_timesync_sources != sources;
                board.reachable_timesync_sources = sources;
                Self::refresh_sender_topology_state(&mut sender_state);
                changed
            }
            crate::DataType::DiscoveryTopology => {
                let mut boards = discovery::decode_discovery_topology(&pkt)?;
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
            crate::DataType::DiscoverySchema => false,
            _ => false,
        };
        sender_state.last_seen_ms = now_ms;
        route
            .announcers
            .insert(pkt.sender().to_string(), sender_state);
        Self::recompute_discovery_side_state(&mut route);
        st.discovery_routes.insert(src, route);
        st.fit_discovery_budget();
        if changed {
            Self::note_discovery_topology_change_locked(&mut st, now_ms);
        }
        let _ = Self::prune_discovery_routes_locked(&mut st, now_ms);
        Self::reconcile_end_to_end_acked_destinations_locked(&mut st);
        Ok(())
    }

    #[cfg(not(feature = "discovery"))]
    fn learn_discovery_item(&self, _src: RelaySideId, _data: &RelayItem) -> TelemetryResult<()> {
        Ok(())
    }

    #[cfg(not(feature = "discovery"))]
    fn queue_discovery_announce(&self) -> TelemetryResult<()> {
        Ok(())
    }

    #[cfg(not(feature = "discovery"))]
    fn poll_discovery_announce(&self) -> TelemetryResult<bool> {
        Ok(false)
    }

    fn process_reliable_timeouts(&self) -> TelemetryResult<()> {
        let now = self.clock.now_ms();
        let mut requeue: Vec<(RelaySideId, crate::DataType, u32)> = Vec::new();

        {
            let mut st = self.state.lock();
            if st.reliable_tx.is_empty() {
                return Ok(());
            }

            for ((side, ty_u32), tx_state) in st.reliable_tx.iter_mut() {
                let Some(ty) = crate::DataType::try_from_u32(*ty_u32) else {
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

    /// Compute a de-dupe hash for a QueueItem.
    /// Uses packet ID for Packet items, and attempts to extract packet ID from
    /// serialized bytes. If extraction fails, hashes raw bytes as a fallback.
    fn get_hash(item: &RelayRxItem) -> u64 {
        match &item.data {
            RelayItem::Packet(pkt) => pkt.packet_id(),
            RelayItem::Serialized(bytes) => {
                let reliable_seq = serialize::peek_frame_info(bytes.as_ref())
                    .ok()
                    .and_then(|frame| frame.reliable)
                    .and_then(|hdr| {
                        if (hdr.flags & serialize::RELIABLE_FLAG_ACK_ONLY) != 0 {
                            None
                        } else {
                            Some(hdr.seq)
                        }
                    });

                match serialize::packet_id_from_wire(bytes.as_ref()) {
                    Ok(id) => {
                        if let Some(seq) = reliable_seq {
                            hash_bytes_u64(id, &seq.to_le_bytes())
                        } else {
                            id
                        }
                    }
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

    /// Compute a dedupe ID for an incoming RelayRxItem.
    /// Note: we intentionally do *not* include `src` so that the same
    /// packet coming from multiple sides is only processed once.
    fn is_duplicate_pkt(&self, item: &RelayRxItem) -> TelemetryResult<bool> {
        let id = Self::get_hash(item);

        let mut st = self.state.lock();
        if st.recent_rx.contains(&id) {
            Ok(true)
        } else {
            st.push_recent_rx(id)?;
            Ok(false)
        }
    }

    fn should_forward_duplicate_reliable_item(&self, item: &RelayRxItem) -> TelemetryResult<bool> {
        let (_, ty) = self.item_route_info(&item.data)?;
        if !is_reliable_type(ty)
            || matches!(
                ty,
                crate::DataType::ReliableAck
                    | crate::DataType::ReliablePartialAck
                    | crate::DataType::ReliablePacketRequest
            )
        {
            return Ok(false);
        }

        let RemoteSidePlan::Target(sides) = self.remote_side_plan(&item.data, item.src)?;
        let st = self.state.lock();
        let now_ms = self.clock.now_ms();
        Ok(sides
            .into_iter()
            .any(|side| self.side_has_multiple_announcers_locked(&st, side, now_ms)))
    }

    /// Register a side whose TX callback consumes serialized packet bytes.
    ///
    /// Returns the side id later used for ingress APIs such as `rx_serialized_from_side`.
    /// The default options disable the relay's per-link reliable framing on this side.
    pub fn add_side_serialized<F>(&self, name: &'static str, tx: F) -> RelaySideId
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        self.add_side_serialized_with_options(name, tx, RelaySideOptions::default())
    }

    /// Register a serialized-output side with explicit side options.
    ///
    /// `opts.reliable_enabled` enables relay-managed per-hop ACK/retransmit behavior on this side.
    /// `opts.link_local_enabled` gates link-local-only forwarding and discovery use of this side.
    /// `ingress_enabled` and `egress_enabled` set the initial directional policy.
    pub fn add_side_serialized_with_options<F>(
        &self,
        name: &'static str,
        tx: F,
        opts: RelaySideOptions,
    ) -> RelaySideId
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        let mut st = self.state.lock();
        let id = st.sides.len();
        st.sides.push(Some(RelaySide {
            name,
            tx_handler: RelayTxHandlerFn::Serialized(Arc::new(tx)),
            opts,
        }));
        st.side_runtime_stats
            .insert(id, SideRuntimeStatsInner::default());
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, self.clock.now_ms());
        id
    }

    /// Register a side whose TX callback receives decoded [`Packet`] values.
    ///
    /// Packet-output sides do not preserve the relay's serialized reliable hop framing, so use a
    /// serialized side when this hop should participate in relay-managed per-link reliability.
    pub fn add_side_packet<F>(&self, name: &'static str, tx: F) -> RelaySideId
    where
        F: Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        self.add_side_packet_with_options(name, tx, RelaySideOptions::default())
    }

    /// Register a packet-output side with explicit side options.
    pub fn add_side_packet_with_options<F>(
        &self,
        name: &'static str,
        tx: F,
        opts: RelaySideOptions,
    ) -> RelaySideId
    where
        F: Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        let mut st = self.state.lock();
        let id = st.sides.len();
        st.sides.push(Some(RelaySide {
            name,
            tx_handler: RelayTxHandlerFn::Packet(Arc::new(tx)),
            opts,
        }));
        st.side_runtime_stats
            .insert(id, SideRuntimeStatsInner::default());
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, self.clock.now_ms());
        id
    }

    /// Remove a side while keeping existing side IDs stable.
    ///
    /// `side` must be an id returned by one of the `add_side_*` calls. Remaining side ids are not
    /// renumbered.
    pub fn remove_side(&self, side: RelaySideId) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
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
        st.side_runtime_stats.remove(&side);
        st.reliable_return_routes
            .retain(|_, route| route.side != side);
        st.rx_queue.retain(|queued| queued.src != side);
        st.tx_queue
            .retain(|queued| queued.dst != side && queued.src != Some(side));
        st.replay_queue.retain(|queued| queued.dst != side);
        st.reliable_tx.retain(|(side_id, _), _| *side_id != side);
        st.reliable_rx.retain(|(side_id, _), _| *side_id != side);
        #[cfg(feature = "discovery")]
        {
            st.discovery_routes.remove(&side);
            Self::note_discovery_topology_change_locked(&mut st, now_ms);
        }
        Ok(())
    }

    /// Enable or disable ingress processing for a registered side.
    pub fn set_side_ingress_enabled(
        &self,
        side: RelaySideId,
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
    pub fn set_side_egress_enabled(&self, side: RelaySideId, enabled: bool) -> TelemetryResult<()> {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        let side_ref = st
            .sides
            .get_mut(side)
            .and_then(|side| side.as_mut())
            .ok_or(TelemetryError::BadArg)?;
        side_ref.opts.egress_enabled = enabled;
        if !enabled {
            st.tx_queue.retain(|queued| queued.dst != side);
            st.replay_queue.retain(|queued| queued.dst != side);
        }
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

    /// Set the route-selection policy for traffic originating from `src`.
    ///
    /// `src == None` targets locally-originated relay traffic such as discovery output.
    pub fn set_source_route_mode(
        &self,
        src: Option<RelaySideId>,
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

    /// Clear a source-specific route-selection override.
    pub fn clear_source_route_mode(&self, src: Option<RelaySideId>) -> TelemetryResult<()> {
        self.set_source_route_mode(src, RouteSelectionMode::Fanout)
    }

    /// Set the weighted-routing weight from `src` toward `dst`.
    pub fn set_route_weight(
        &self,
        src: Option<RelaySideId>,
        dst: RelaySideId,
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
        src: Option<RelaySideId>,
        dst: RelaySideId,
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
    pub fn set_route_priority(
        &self,
        src: Option<RelaySideId>,
        dst: RelaySideId,
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
        src: Option<RelaySideId>,
        dst: RelaySideId,
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
    pub fn set_route(
        &self,
        src: Option<RelaySideId>,
        dst: RelaySideId,
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
    pub fn set_typed_route(
        &self,
        src: Option<RelaySideId>,
        ty: crate::DataType,
        dst: RelaySideId,
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
        src: Option<RelaySideId>,
        ty: crate::DataType,
        dst: RelaySideId,
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

    /// Clear a non-typed route override so the relay falls back to default behavior.
    pub fn clear_route(&self, src: Option<RelaySideId>, dst: RelaySideId) -> TelemetryResult<()> {
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

    #[cfg(feature = "discovery")]
    /// Queues an immediate discovery announcement for this relay.
    pub fn announce_discovery(&self) -> TelemetryResult<()> {
        self.queue_discovery_announce()
    }

    #[cfg(feature = "discovery")]
    /// Polls discovery state and queues an announce if the cadence says one is due.
    pub fn poll_discovery(&self) -> TelemetryResult<bool> {
        self.poll_discovery_announce()
    }

    #[cfg(feature = "discovery")]
    /// Exports the relay's current discovered topology snapshot.
    pub fn export_topology(&self) -> TopologySnapshot {
        let now_ms = self.clock.now_ms();
        let mut st = self.state.lock();
        if Self::prune_discovery_routes_locked(&mut st, now_ms) {
            Self::reconcile_end_to_end_acked_destinations_locked(&mut st);
            Self::note_discovery_topology_change_locked(&mut st, now_ms);
        }
        let routes = st
            .discovery_routes
            .iter()
            .filter_map(|(&side_id, route)| {
                let side = st.sides.get(side_id).and_then(|side| side.as_ref())?;
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

    pub fn export_runtime_stats(&self) -> RuntimeStatsSnapshot {
        let now_ms = self.clock.now_ms();
        let st = self.state.lock();

        let mut sides = Vec::new();
        for (side_id, side) in st.sides.iter().enumerate() {
            let Some(side) = side.as_ref() else { continue };
            let stats = st
                .side_runtime_stats
                .get(&side_id)
                .cloned()
                .unwrap_or_default();
            let adaptive = st
                .adaptive_route_stats
                .get(&side_id)
                .cloned()
                .unwrap_or_default()
                .snapshot(now_ms, true);
            let mut data_types: Vec<RuntimeTypeStats> = stats
                .data_types
                .into_iter()
                .map(|(ty, item)| RuntimeTypeStats {
                    data_type: crate::DataType(ty),
                    tx_packets: item.tx_packets,
                    tx_bytes: item.tx_bytes,
                    rx_packets: item.rx_packets,
                    rx_bytes: item.rx_bytes,
                    relayed_tx_packets: item.relayed_tx_packets,
                    relayed_tx_bytes: item.relayed_tx_bytes,
                    relayed_rx_packets: item.relayed_rx_packets,
                    relayed_rx_bytes: item.relayed_rx_bytes,
                    tx_retries: item.tx_retries,
                    handler_failures: item.handler_failures,
                })
                .collect();
            data_types.sort_unstable_by_key(|item| item.data_type.as_u32());
            sides.push(RuntimeSideStats {
                side_id,
                side_name: side.name,
                reliable_enabled: side.opts.reliable_enabled,
                link_local_enabled: side.opts.link_local_enabled,
                ingress_enabled: side.opts.ingress_enabled,
                egress_enabled: side.opts.egress_enabled,
                tx_packets: stats.tx_packets,
                tx_bytes: stats.tx_bytes,
                rx_packets: stats.rx_packets,
                rx_bytes: stats.rx_bytes,
                relayed_tx_packets: stats.relayed_tx_packets,
                relayed_tx_bytes: stats.relayed_tx_bytes,
                relayed_rx_packets: stats.relayed_rx_packets,
                relayed_rx_bytes: stats.relayed_rx_bytes,
                local_delivery_packets: 0,
                tx_retries: stats.tx_retries,
                tx_handler_failures: stats.tx_handler_failures,
                local_handler_failures: 0,
                total_handler_retries: stats.total_handler_retries,
                adaptive,
                data_types,
            });
        }

        let mut route_modes: Vec<RouteModeStats> = st
            .route_selection_cursors
            .iter()
            .map(|(src, cursor)| RouteModeStats {
                src_side_id: *src,
                selection_mode: st.source_route_modes.get(src).copied(),
                cursor: *cursor,
            })
            .collect();
        for src in st.source_route_modes.keys() {
            if !route_modes.iter().any(|mode| mode.src_side_id == *src) {
                route_modes.push(RouteModeStats {
                    src_side_id: *src,
                    selection_mode: st.source_route_modes.get(src).copied(),
                    cursor: 0,
                });
            }
        }
        route_modes.sort_unstable_by_key(|mode| mode.src_side_id.unwrap_or(usize::MAX));

        let mut route_overrides: Vec<RouteOverrideStats> = st
            .route_overrides
            .iter()
            .map(|((src, dst), enabled)| RouteOverrideStats {
                src_side_id: *src,
                dst_side_id: *dst,
                enabled: *enabled,
            })
            .collect();
        route_overrides.sort_unstable_by_key(|item| {
            (item.src_side_id.unwrap_or(usize::MAX), item.dst_side_id)
        });

        let mut typed_route_overrides: Vec<TypedRouteOverrideStats> = st
            .typed_route_overrides
            .iter()
            .map(|((src, ty, dst), enabled)| TypedRouteOverrideStats {
                src_side_id: *src,
                data_type: crate::DataType(*ty),
                dst_side_id: *dst,
                enabled: *enabled,
            })
            .collect();
        typed_route_overrides.sort_unstable_by_key(|item| {
            (
                item.src_side_id.unwrap_or(usize::MAX),
                item.data_type.as_u32(),
                item.dst_side_id,
            )
        });

        let mut route_weights: Vec<RouteWeightStats> = st
            .route_weights
            .iter()
            .map(|((src, dst), weight)| RouteWeightStats {
                src_side_id: *src,
                dst_side_id: *dst,
                weight: *weight,
            })
            .collect();
        route_weights.sort_unstable_by_key(|item| {
            (item.src_side_id.unwrap_or(usize::MAX), item.dst_side_id)
        });

        let mut route_priorities: Vec<RoutePriorityStats> = st
            .route_priorities
            .iter()
            .map(|((src, dst), priority)| RoutePriorityStats {
                src_side_id: *src,
                dst_side_id: *dst,
                priority: *priority,
            })
            .collect();
        route_priorities.sort_unstable_by_key(|item| {
            (item.src_side_id.unwrap_or(usize::MAX), item.dst_side_id)
        });

        #[cfg(feature = "discovery")]
        let discovery = DiscoveryRuntimeStats {
            route_count: st.discovery_routes.len(),
            announcer_count: st
                .discovery_routes
                .values()
                .map(|route| route.announcers.len())
                .sum(),
            current_announce_interval_ms: Some(st.discovery_cadence.current_interval_ms),
            next_announce_ms: Some(st.discovery_cadence.next_announce_ms),
        };
        #[cfg(not(feature = "discovery"))]
        let discovery = DiscoveryRuntimeStats {
            route_count: 0,
            announcer_count: 0,
            current_announce_interval_ms: None,
            next_announce_ms: None,
        };

        RuntimeStatsSnapshot {
            sides,
            route_modes,
            route_overrides,
            typed_route_overrides,
            route_weights,
            route_priorities,
            queues: QueueRuntimeStats {
                rx_len: st.rx_queue.len(),
                rx_bytes: st.rx_queue.bytes_used(),
                tx_len: st.tx_queue.len(),
                tx_bytes: st.tx_queue.bytes_used(),
                replay_len: st.replay_queue.len(),
                replay_bytes: st.replay_queue.bytes_used(),
                recent_rx_len: st.recent_rx.len(),
                recent_rx_bytes: st.recent_rx.bytes_used(),
                reliable_rx_buffered_len: st.reliable_rx_buffer_len(),
                reliable_rx_buffered_bytes: st.reliable_rx_buffered_bytes(),
                shared_queue_bytes_used: st.shared_queue_bytes_used(),
            },
            reliable: ReliableRuntimeStats {
                reliable_return_route_count: st.reliable_return_routes.len(),
                end_to_end_pending_count: 0,
                end_to_end_pending_destination_count: 0,
                end_to_end_acked_cache_count: st.end_to_end_acked_destinations.len(),
            },
            discovery,
            total_handler_failures: st.total_handler_failures,
            total_handler_retries: st.total_handler_retries,
        }
    }

    #[cfg(test)]
    pub(crate) fn debug_end_to_end_acked_destination_count(&self, packet_id: u64) -> Option<usize> {
        let st = self.state.lock();
        st.end_to_end_acked_destinations
            .get(&packet_id)
            .map(BTreeSet::len)
    }

    #[cfg(test)]
    pub(crate) fn debug_end_to_end_acked_packet_count(&self) -> usize {
        let st = self.state.lock();
        st.end_to_end_acked_destinations.len()
    }

    #[cfg(test)]
    pub(crate) fn debug_reliable_return_route_count(&self) -> usize {
        let st = self.state.lock();
        st.reliable_return_routes.len()
    }

    /// Enqueue serialized bytes that originated from `src` into the relay RX queue.
    ///
    /// Note: `Arc::from(bytes)` allocates and copies `len` bytes into a new `Arc<[u8]>`.
    /// This is still “fast enough” for many cases, but it is not allocation-free / ISR-safe.
    pub fn rx_serialized_from_side(&self, src: RelaySideId, bytes: &[u8]) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(src)?;
        let mut st = self.state.lock();

        let data = RelayItem::Serialized(Arc::from(bytes));
        let priority = Self::relay_item_priority(&data)?;
        st.push_rx(RelayRxItem {
            src,
            data,
            priority,
        })
    }

    /// Enqueue a full packet that originated from `src` into the relay RX queue.
    ///
    /// The packet is wrapped in `Arc<Packet>` so fanout can clone the pointer cheaply.
    pub fn rx_from_side(&self, src: RelaySideId, packet: Packet) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(src)?;
        let mut st = self.state.lock();

        let data = RelayItem::Packet(Arc::new(packet));
        let priority = Self::relay_item_priority(&data)?;
        st.push_rx(RelayRxItem {
            src,
            data,
            priority,
        })
    }

    /// Clear both RX and TX queues.
    pub fn clear_queues(&self) {
        let mut st = self.state.lock();
        st.rx_queue.clear();
        st.tx_queue.clear();
    }

    /// Clear only RX queue.
    pub fn clear_rx_queue(&self) {
        let mut st = self.state.lock();
        st.rx_queue.clear();
    }

    /// Clear only TX queue.
    pub fn clear_tx_queue(&self) {
        let mut st = self.state.lock();
        st.tx_queue.clear();
        st.replay_queue.clear();
    }

    /// Internal: expand one RX item into TX items for all other sides.
    ///
    /// Fanout is cheap: the `RelayItem` is cloned (Arc bump) and reused across all destinations.
    fn process_rx_queue_item(&self, item: RelayRxItem) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(item.src)?;
        match &item.data {
            RelayItem::Packet(pkt) => {
                let bytes = serialize::serialize_packet(pkt).len();
                self.note_side_rx(item.src, pkt.data_type(), bytes);
            }
            RelayItem::Serialized(bytes) => {
                if let Ok(env) = serialize::peek_envelope(bytes.as_ref()) {
                    self.note_side_rx(item.src, env.ty, bytes.len());
                }
            }
        }
        match &item.data {
            RelayItem::Packet(pkt) => {
                if is_reliable_type(pkt.data_type()) && !is_internal_control_type(pkt.data_type()) {
                    self.note_reliable_return_route(item.src, pkt.packet_id());
                }
            }
            RelayItem::Serialized(bytes) => {
                if let Ok(env) = serialize::peek_envelope(bytes.as_ref())
                    && is_reliable_type(env.ty)
                    && !is_internal_control_type(env.ty)
                    && let Ok(packet_id) = serialize::packet_id_from_wire(bytes.as_ref())
                {
                    self.note_reliable_return_route(item.src, packet_id);
                }
            }
        }
        let mut released_buffered: Vec<Arc<[u8]>> = Vec::new();
        if let RelayItem::Serialized(bytes) = &item.data {
            let (_opts, handler_is_serialized, hop_reliable_enabled) = {
                let st = self.state.lock();
                let side_ref = Self::side_ref(&st, item.src)?;
                let opts = side_ref.opts;
                (
                    opts,
                    matches!(side_ref.tx_handler, RelayTxHandlerFn::Serialized(_)),
                    opts.reliable_enabled
                        && !self.side_has_multiple_announcers_locked(
                            &st,
                            item.src,
                            self.clock.now_ms(),
                        ),
                )
            };

            let frame = match serialize::peek_frame_info(bytes.as_ref()) {
                Ok(frame) => frame,
                Err(e) => {
                    if matches!(e, TelemetryError::Deserialize(msg) if msg == "crc32 mismatch")
                        && hop_reliable_enabled
                        && handler_is_serialized
                        && let Ok(frame) = serialize::peek_frame_info_unchecked(bytes.as_ref())
                    {
                        if is_reliable_type(frame.envelope.ty)
                            && let Some(hdr) = frame.reliable
                        {
                            let unordered = (hdr.flags & serialize::RELIABLE_FLAG_UNORDERED) != 0;
                            let unsequenced =
                                (hdr.flags & serialize::RELIABLE_FLAG_UNSEQUENCED) != 0;

                            if !unsequenced {
                                let requested = if unordered {
                                    hdr.seq
                                } else {
                                    let mut st = self.state.lock();
                                    let rx_state = self.reliable_rx_state_mut(
                                        &mut st,
                                        item.src,
                                        frame.envelope.ty,
                                    );
                                    rx_state.expected_seq.min(hdr.seq)
                                };
                                self.queue_reliable_packet_request(
                                    item.src,
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

            if hop_reliable_enabled
                && handler_is_serialized
                && is_reliable_type(frame.envelope.ty)
                && let Some(hdr) = frame.reliable
            {
                let unordered = (hdr.flags & serialize::RELIABLE_FLAG_UNORDERED) != 0;
                let unsequenced = (hdr.flags & serialize::RELIABLE_FLAG_UNSEQUENCED) != 0;

                if !unsequenced {
                    if unordered {
                        self.queue_reliable_ack(item.src, frame.envelope.ty, hdr.seq)?;
                    } else {
                        let mut release: Vec<Arc<[u8]>> = Vec::new();
                        let mut last_delivered = None;
                        let mut ack_old = None;
                        let mut request_missing = None;
                        let mut partial_ack = None;
                        {
                            let mut st = self.state.lock();
                            let rx_state =
                                self.reliable_rx_state_mut(&mut st, item.src, frame.envelope.ty);
                            let expected_seq = rx_state.expected_seq;
                            if hdr.seq < expected_seq {
                                ack_old = Some(expected_seq.saturating_sub(1));
                            } else if hdr.seq > expected_seq {
                                request_missing = Some(expected_seq);
                                partial_ack = Some(hdr.seq);
                                st.buffer_reliable_rx(
                                    item.src,
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
                            self.queue_reliable_ack(item.src, frame.envelope.ty, ack_seq)?;
                            return Ok(());
                        }
                        if let Some(request_seq) = request_missing {
                            if let Some(partial_seq) = partial_ack {
                                self.queue_reliable_partial_ack(
                                    item.src,
                                    frame.envelope.ty,
                                    partial_seq,
                                )?;
                            }
                            self.queue_reliable_packet_request(
                                item.src,
                                frame.envelope.ty,
                                request_seq,
                            )?;
                            return Ok(());
                        }
                        if let Some(ack_seq) = last_delivered {
                            self.queue_reliable_ack(item.src, frame.envelope.ty, ack_seq)?;
                        }
                        released_buffered.extend(release.into_iter().skip(1));
                    }
                }
            }
        }

        if self.is_duplicate_pkt(&item)? && !self.should_forward_duplicate_reliable_item(&item)? {
            // Already fanned out this packet recently; skip.
            return Ok(());
        }

        self.dispatch_relay_rx_item(&item)?;

        for release_bytes in released_buffered {
            let release_item = RelayRxItem {
                src: item.src,
                priority: Self::relay_item_priority(&RelayItem::Serialized(release_bytes.clone()))?,
                data: RelayItem::Serialized(release_bytes),
            };
            if self.is_duplicate_pkt(&release_item)?
                && !self.should_forward_duplicate_reliable_item(&release_item)?
            {
                continue;
            }
            self.dispatch_relay_rx_item(&release_item)?;
        }
        Ok(())
    }

    fn dispatch_relay_rx_item(&self, item: &RelayRxItem) -> TelemetryResult<()> {
        match &item.data {
            RelayItem::Packet(pkt) => {
                if matches!(
                    pkt.data_type(),
                    crate::DataType::ReliableAck
                        | crate::DataType::ReliablePartialAck
                        | crate::DataType::ReliablePacketRequest
                ) {
                    if pkt.data_type() == crate::DataType::ReliableAck
                        && Self::is_end_to_end_ack_sender(pkt.sender())
                        && Self::decode_end_to_end_reliable_ack(pkt.payload()).is_ok()
                    {
                        if let Ok(packet_id) = Self::decode_end_to_end_reliable_ack(pkt.payload())
                            && let Some(sender_hash) =
                                Self::decode_end_to_end_ack_sender_hash(pkt.sender())
                        {
                            let mut st = self.state.lock();
                            Self::note_end_to_end_acked_destination_locked(
                                &mut st,
                                packet_id,
                                sender_hash,
                            );
                        }
                    } else {
                        let vals = pkt.data_as_u32()?;
                        if vals.len() != 2 {
                            return Err(TelemetryError::Deserialize(
                                "bad reliable control payload",
                            ));
                        }
                        let ty = crate::DataType::try_from_u32(vals[0])
                            .ok_or(TelemetryError::InvalidType)?;
                        let seq = vals[1];
                        match pkt.data_type() {
                            crate::DataType::ReliableAck => {
                                self.handle_reliable_ack(item.src, ty, seq)
                            }
                            crate::DataType::ReliablePartialAck => {
                                self.handle_reliable_partial_ack(item.src, ty, seq)
                            }
                            crate::DataType::ReliablePacketRequest => {
                                self.queue_reliable_retransmit(item.src, ty, seq)?
                            }
                            _ => {}
                        }
                        return Ok(());
                    }
                }
            }
            RelayItem::Serialized(bytes) => {
                let env = serialize::peek_envelope(bytes.as_ref())?;
                if matches!(
                    env.ty,
                    crate::DataType::ReliableAck
                        | crate::DataType::ReliablePacketRequest
                        | crate::DataType::ReliablePartialAck
                ) {
                    let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                    return self.dispatch_relay_rx_item(&RelayRxItem {
                        src: item.src,
                        data: RelayItem::Packet(Arc::new(pkt)),
                        priority: item.priority,
                    });
                }
            }
        }

        let src = item.src;
        let data = item.data.clone();
        self.learn_discovery_item(src, &data)?;

        let plan = self.remote_side_plan(&data, src)?;
        let mut st = self.state.lock();
        let RemoteSidePlan::Target(sides) = plan;
        for dst in sides {
            let priority = Self::relay_item_priority(&data)?;
            st.push_tx(RelayTxItem {
                src: Some(src),
                dst,
                data: data.clone(),
                priority,
            })?;
        }
        Ok(())
    }

    /// Helper: call a TX handler with the best representation we have.
    /// - Packet handler + Packet item: direct.
    /// - Serialized handler + Serialized item: direct.
    /// - Packet handler + Serialized item: deserialize for this call.
    /// - Serialized handler + Packet item: serialize for this call.
    fn call_tx_handler(
        &self,
        side: RelaySideId,
        handler: &RelayTxHandlerFn,
        data: &RelayItem,
    ) -> TelemetryResult<()> {
        let Some(_side_tx_guard) = self.try_enter_side_tx() else {
            return Err(TelemetryError::Io("side tx busy"));
        };
        let started_ms = self.clock.now_ms();
        let ty = match data {
            RelayItem::Packet(pkt) => pkt.data_type(),
            RelayItem::Serialized(bytes) => serialize::peek_envelope(bytes.as_ref())?.ty,
        };
        let result = match (handler, data) {
            // Fast paths
            (RelayTxHandlerFn::Serialized(f), RelayItem::Serialized(bytes)) => f(bytes.as_ref()),
            (RelayTxHandlerFn::Packet(f), RelayItem::Packet(pkt)) => f(pkt),

            // Conversion paths
            (RelayTxHandlerFn::Serialized(f), RelayItem::Packet(pkt)) => {
                let owned = serialize::serialize_packet(pkt);
                f(&owned)
            }
            (RelayTxHandlerFn::Packet(f), RelayItem::Serialized(bytes)) => {
                let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                f(&pkt)
            }
        };
        if result.is_ok()
            && let Ok(bytes) = Self::relay_item_wire_len(data)
        {
            self.record_side_tx_sample(side, bytes, started_ms, self.clock.now_ms());
            self.note_side_tx_success(side, ty, bytes, 1);
        } else if result.is_err() {
            self.note_side_tx_failure(side, ty, 1);
        }
        result
    }

    fn adjust_reliable_for_side(
        &self,
        opts: RelaySideOptions,
        data: RelayItem,
    ) -> TelemetryResult<Option<RelayItem>> {
        if opts.reliable_enabled {
            return Ok(Some(data));
        }

        match data {
            RelayItem::Serialized(bytes) => {
                let frame = match serialize::peek_frame_info(bytes.as_ref()) {
                    Ok(frame) => frame,
                    Err(_) => return Ok(Some(RelayItem::Serialized(bytes))),
                };
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
                        return Ok(Some(RelayItem::Serialized(Arc::from(v))));
                    }
                }
                Ok(Some(RelayItem::Serialized(bytes)))
            }
            RelayItem::Packet(pkt) => {
                if matches!(
                    pkt.data_type(),
                    crate::DataType::ReliableAck
                        | crate::DataType::ReliablePartialAck
                        | crate::DataType::ReliablePacketRequest
                ) {
                    return Ok(None);
                }
                Ok(Some(RelayItem::Packet(pkt)))
            }
        }
    }

    /// Drain the RX queue fully, expanding to TX items.
    #[inline]
    pub fn process_rx_queue(&self) -> TelemetryResult<()> {
        self.process_rx_queue_with_timeout(0)
    }

    /// Drain the TX queue fully, invoking per-side TX handlers.
    ///
    /// If called from inside a side TX callback, this becomes a no-op so relay TX handlers cannot
    /// recurse into nested queue drains on the same stack.
    #[inline]
    pub fn process_tx_queue(&self) -> TelemetryResult<()> {
        self.process_tx_queue_with_timeout(0)
    }

    /// Drain RX then TX queues fully (one pass).
    #[inline]
    pub fn process_all_queues(&self) -> TelemetryResult<()> {
        self.process_all_queues_with_timeout(0)
    }

    /// Process the TX queue for up to `timeout_ms` milliseconds.
    ///
    /// `timeout_ms == 0` drains fully. If called from inside a side TX callback, this becomes a
    /// no-op so relay TX handlers cannot recurse into nested queue drains on the same stack.
    pub fn process_tx_queue_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return Ok(());
        }
        let start = self.clock.now_ms();
        loop {
            self.process_reliable_timeouts()?;
            if self.process_replay_queue_item()? {
                if timeout_ms != 0 && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                    break;
                }
                continue;
            }
            let Some((src, dst, handler, opts, data)) = self.pop_ready_tx_item() else {
                break;
            };
            match self.send_tx_item(src, dst, handler, opts, data.clone()) {
                Ok(sent) => {
                    if sent
                        && timeout_ms != 0
                        && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64
                    {
                        break;
                    }
                }
                Err(e) if Self::is_side_tx_busy(&e) => {
                    let priority = Self::relay_item_priority(&data)?;
                    let mut st = self.state.lock();
                    st.push_tx(RelayTxItem {
                        src,
                        dst,
                        data,
                        priority,
                    })?;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Process RX queue with timeout.
    pub fn process_rx_queue_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
        let start = self.clock.now_ms();
        loop {
            let item_opt = {
                let mut st = self.state.lock();
                st.rx_queue.pop_front()
            };
            let Some(item) = item_opt else { break };
            self.process_rx_queue_item(item)?;

            if timeout_ms != 0 && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                break;
            }
        }
        Ok(())
    }

    /// Process RX and TX queues interleaved for up to `timeout_ms` milliseconds.
    ///
    /// `timeout_ms == 0` drains fully. If called from inside a side TX callback, this becomes a
    /// no-op so relay TX handlers cannot recurse into nested queue drains on the same stack.
    pub fn process_all_queues_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
        if self.side_tx_active() {
            return Ok(());
        }
        let drain_fully = timeout_ms == 0;
        let start = if drain_fully { 0 } else { self.clock.now_ms() };

        loop {
            let mut did_any = false;
            self.process_reliable_timeouts()?;

            // First move RX → TX
            if let Some(item) = {
                let mut st = self.state.lock();
                st.rx_queue.pop_front()
            } {
                self.process_rx_queue_item(item)?;
                did_any = true;
            }

            if !drain_fully && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                break;
            }

            if self.process_replay_queue_item()? {
                did_any = true;
            }

            // Then send out TX
            let sent_one = if let Some((src, dst, handler, opts, data)) = self.pop_ready_tx_item() {
                self.send_tx_item(src, dst, handler, opts, data)?
            } else {
                false
            };

            if sent_one {
                did_any = true;
            }

            if !drain_fully && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64 {
                break;
            }

            if !did_any {
                break;
            }
        }

        Ok(())
    }

    /// Runs one application-loop maintenance cycle.
    ///
    /// This polls built-in discovery when that feature is compiled in, then drains queued RX/TX
    /// work for up to `timeout_ms` milliseconds.
    pub fn periodic(&self, timeout_ms: u32) -> TelemetryResult<()> {
        #[cfg(feature = "discovery")]
        {
            let _ = self.poll_discovery()?;
        }

        self.process_all_queues_with_timeout(timeout_ms)
    }
}
