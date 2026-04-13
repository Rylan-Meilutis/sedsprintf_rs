use crate::config::{
    MAX_QUEUE_SIZE, MAX_RECENT_RX_IDS, QUEUE_GROW_STEP, RELIABLE_MAX_PENDING, RELIABLE_MAX_RETRIES,
    RELIABLE_RETRANSMIT_MS, STARTING_QUEUE_SIZE, STARTING_RECENT_RX_IDS,
};
#[cfg(feature = "discovery")]
use crate::discovery::{
    self, DiscoveryCadenceState, TopologySideRoute, TopologySnapshot, DISCOVERY_ROUTE_TTL_MS,
};
use crate::packet::{hash_bytes_u64, Packet};
use crate::queue::{BoundedDeque, ByteCost};
use crate::serialize;
use crate::{is_reliable_type, message_meta, message_priority, reliable_mode};
use crate::{
    router::Clock,
    {lock::RouterMutex, RouteSelectionMode, TelemetryError, TelemetryResult},
};
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::{sync::Arc, vec, vec::Vec};

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
    pub reliable_enabled: bool,
    pub link_local_enabled: bool,
    pub ingress_enabled: bool,
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
}

#[derive(Debug, Clone)]
struct ReliableRxState {
    expected_seq: u32,
    buffered: BTreeMap<u32, Arc<[u8]>>,
}

#[cfg(feature = "discovery")]
#[derive(Debug, Clone, Default)]
struct DiscoverySideState {
    reachable: Vec<crate::DataEndpoint>,
    reachable_timesync_sources: Vec<String>,
    last_seen_ms: u64,
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
    rx_queue: BoundedDeque<RelayRxItem>,
    tx_queue: BoundedDeque<RelayTxItem>,
    replay_queue: BoundedDeque<RelayReplayItem>,
    recent_rx: BoundedDeque<u64>,
    reliable_tx: BTreeMap<(RelaySideId, u32), ReliableTxState>,
    reliable_rx: BTreeMap<(RelaySideId, u32), ReliableRxState>,
    #[cfg(feature = "discovery")]
    discovery_routes: BTreeMap<RelaySideId, DiscoverySideState>,
    #[cfg(feature = "discovery")]
    discovery_cadence: DiscoveryCadenceState,
}

/// Relay that fans out packets from one side to all others.
/// - Supports both serialized bytes and full Packet.
/// - Has RX & TX queues, like Router.
/// - Uses a Clock for the *_with_timeout APIs, same style as Router.
pub struct Relay {
    state: RouterMutex<RelayInner>,
    clock: Box<dyn Clock + Send + Sync>,
}

enum RemoteSidePlan {
    Target(Vec<RelaySideId>),
}

impl Relay {
    fn relay_item_priority(data: &RelayItem) -> TelemetryResult<u8> {
        let ty = match data {
            RelayItem::Packet(pkt) => pkt.data_type(),
            RelayItem::Serialized(bytes) => serialize::peek_envelope(bytes.as_ref())?.ty,
        };
        Ok(message_priority(ty))
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
        self.send_reliable_raw_to_side(item.dst, item.bytes.clone())?;
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
            self.call_tx_handler(&handler, &adjusted)?;
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
                rx_queue: BoundedDeque::new(MAX_QUEUE_SIZE, STARTING_QUEUE_SIZE, QUEUE_GROW_STEP),
                tx_queue: BoundedDeque::new(MAX_QUEUE_SIZE, STARTING_QUEUE_SIZE, QUEUE_GROW_STEP),
                replay_queue: BoundedDeque::new(
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
            clock,
        }
    }

    #[inline]
    fn side_ref(st: &RelayInner, side: RelaySideId) -> TelemetryResult<&RelaySide> {
        st.sides
            .get(side)
            .and_then(|side| side.as_ref())
            .ok_or(TelemetryError::HandlerError("relay: invalid side id"))
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
    ) -> Vec<RelaySideId> {
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

    #[inline]
    fn reliable_key(side: RelaySideId, ty: crate::DataType) -> (RelaySideId, u32) {
        (side, ty as u32)
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
            crate::router::encode_slice_le(&[ty as u32, seq]),
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
        st.tx_queue.push_back_prioritized(
            RelayTxItem {
                src: None,
                dst: side,
                data,
                priority,
            },
            |queued| queued.priority,
        )?;
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
        st.tx_queue.push_back_prioritized(
            RelayTxItem {
                src: None,
                dst: side,
                data,
                priority,
            },
            |queued| queued.priority,
        )?;
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
                queued = Some(sent.bytes.clone());
            }
        }

        if let Some(bytes) = queued {
            let mut st = self.state.lock();
            st.replay_queue.push_back_prioritized(
                RelayReplayItem {
                    dst: side,
                    bytes,
                    priority: message_priority(ty).saturating_add(16),
                },
                |queued| queued.priority,
            )?;
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

        match handler {
            RelayTxHandlerFn::Serialized(f) => f(bytes.as_ref()),
            RelayTxHandlerFn::Packet(f) => {
                let pkt = serialize::deserialize_packet(bytes.as_ref())?;
                f(&pkt)
            }
        }
    }

    fn send_reliable_to_side(&self, side: RelaySideId, data: RelayItem) -> TelemetryResult<()> {
        let (handler, opts) = {
            let st = self.state.lock();
            let side_ref = Self::side_ref(&st, side)?;
            (side_ref.tx_handler.clone(), side_ref.opts)
        };

        let RelayTxHandlerFn::Serialized(f) = &handler else {
            return self.call_tx_handler(&handler, &data);
        };

        if !opts.reliable_enabled {
            if let Some(adjusted) = self.adjust_reliable_for_side(opts, data)? {
                return self.call_tx_handler(&handler, &adjusted);
            }
            return Ok(());
        }

        let ty = match &data {
            RelayItem::Packet(pkt) => pkt.data_type(),
            RelayItem::Serialized(bytes) => {
                let Ok(frame) = serialize::peek_frame_info(bytes.as_ref()) else {
                    return self.call_tx_handler(&handler, &data);
                };
                frame.envelope.ty
            }
        };

        if !is_reliable_type(ty) {
            if let Some(adjusted) = self.adjust_reliable_for_side(opts, data)? {
                self.call_tx_handler(&handler, &adjusted)?;
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
                    return f(bytes.as_ref());
                }
                Arc::from(v)
            }
        };

        f(bytes.as_ref())?;

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

    fn remote_side_plan(
        &self,
        data: &RelayItem,
        exclude: RelaySideId,
    ) -> TelemetryResult<RemoteSidePlan> {
        #[cfg(feature = "discovery")]
        {
            let (eps, ty) = self.item_route_info(data)?;
            if discovery::is_discovery_type(ty) {
                let mut st = self.state.lock();
                let sides = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), false);
                return Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    sides,
                )));
            }

            #[cfg(feature = "timesync")]
            let preferred_timesync_source = self.preferred_timesync_route_source(data, ty)?;
            #[cfg(not(feature = "timesync"))]
            let preferred_timesync_source: Option<String> = None;
            let mut st = self.state.lock();
            let restrict_link_local = Self::endpoints_are_link_local_only(&eps);
            if st.discovery_routes.is_empty() {
                if restrict_link_local {
                    let targets = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), true);
                    return Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                        &mut st,
                        Some(exclude),
                        targets,
                    )));
                }
                let targets = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), false);
                return Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    targets,
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
                    Some(exclude),
                    exact_targets,
                )))
            } else if had_known {
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    generic_targets,
                )))
            } else if restrict_link_local {
                let targets = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), true);
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    targets,
                )))
            } else {
                let targets = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), false);
                Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                    &mut st,
                    Some(exclude),
                    targets,
                )))
            }
        }
        #[cfg(not(feature = "discovery"))]
        {
            let (_, ty) = self.item_route_info(data)?;
            let mut st = self.state.lock();
            let sides = self.eligible_side_ids_locked(&st, Some(exclude), Some(ty), false);
            Ok(RemoteSidePlan::Target(self.apply_route_selection_locked(
                &mut st,
                Some(exclude),
                sides,
            )))
        }
    }

    #[cfg(feature = "discovery")]
    fn note_discovery_topology_change_locked(st: &mut RelayInner, now_ms: u64) {
        st.discovery_cadence.on_topology_change(now_ms);
    }

    #[cfg(feature = "discovery")]
    fn prune_discovery_routes_locked(st: &mut RelayInner, now_ms: u64) -> bool {
        let before = st.discovery_routes.len();
        st.discovery_routes
            .retain(|_, route| now_ms.saturating_sub(route.last_seen_ms) <= DISCOVERY_ROUTE_TTL_MS);
        before != st.discovery_routes.len()
    }

    #[cfg(feature = "discovery")]
    fn advertised_discovery_endpoints_for_link_locked(
        &self,
        st: &RelayInner,
        now_ms: u64,
        link_local_enabled: bool,
    ) -> Vec<crate::DataEndpoint> {
        let mut eps = Vec::new();
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
        st: &RelayInner,
        now_ms: u64,
    ) -> Vec<String> {
        let mut sources = Vec::new();
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
                Self::note_discovery_topology_change_locked(&mut st, now_ms);
            }
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
                    Some((side_id, endpoints, timesync_sources))
                })
                .collect::<Vec<_>>()
        };
        let mut st = self.state.lock();
        for (dst, endpoints, timesync_sources) in per_side {
            if !endpoints.is_empty() {
                let pkt =
                    discovery::build_discovery_announce("RELAY", now_ms, endpoints.as_slice())?;
                let data = RelayItem::Packet(Arc::new(pkt));
                let priority = Self::relay_item_priority(&data)?;
                st.tx_queue.push_back_prioritized(
                    RelayTxItem {
                        src: None,
                        dst,
                        data,
                        priority,
                    },
                    |queued| queued.priority,
                )?;
            }
            if !timesync_sources.is_empty() {
                let pkt = discovery::build_discovery_timesync_sources(
                    "RELAY",
                    now_ms,
                    timesync_sources.as_slice(),
                )?;
                let data = RelayItem::Packet(Arc::new(pkt));
                let priority = Self::relay_item_priority(&data)?;
                st.tx_queue.push_back_prioritized(
                    RelayTxItem {
                        src: None,
                        dst,
                        data,
                        priority,
                    },
                    |queued| queued.priority,
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
                if !self.route_allowed_locked(
                    &st,
                    None,
                    Some(crate::DataType::DiscoveryAnnounce),
                    side_id,
                ) {
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
        let mut st = self.state.lock();
        let mut route = st.discovery_routes.get(&src).cloned().unwrap_or_default();
        let changed = match pkt.data_type() {
            crate::DataType::DiscoveryAnnounce => {
                let reachable = discovery::decode_discovery_announce(&pkt)?;
                let changed = route.reachable != reachable;
                route.reachable = reachable;
                changed
            }
            crate::DataType::DiscoveryTimeSyncSources => {
                let sources = discovery::decode_discovery_timesync_sources(&pkt)?;
                let changed = route.reachable_timesync_sources != sources;
                route.reachable_timesync_sources = sources;
                changed
            }
            _ => false,
        };
        route.last_seen_ms = now_ms;
        st.discovery_routes.insert(src, route);
        if changed {
            Self::note_discovery_topology_change_locked(&mut st, now_ms);
        }
        let _ = Self::prune_discovery_routes_locked(&mut st, now_ms);
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
            if st.recent_rx.len() >= MAX_RECENT_RX_IDS {
                st.recent_rx.pop_front();
            }
            st.recent_rx.push_back(id)?;
            Ok(false)
        }
    }

    /// Add a new side (e.g. "CAN", "UART", "RADIO") with a **serialized handler**.
    /// Returns the side ID you use when enqueuing from that side.
    pub fn add_side_serialized<F>(&self, name: &'static str, tx: F) -> RelaySideId
    where
        F: Fn(&[u8]) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        self.add_side_serialized_with_options(name, tx, RelaySideOptions::default())
    }

    /// Adds a serialized-output side with explicit reliability and link-local options.
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
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, self.clock.now_ms());
        id
    }

    /// Add a new side with a **packet handler**.
    /// The handler receives a fully decoded Packet.
    pub fn add_side_packet<F>(&self, name: &'static str, tx: F) -> RelaySideId
    where
        F: Fn(&Packet) -> TelemetryResult<()> + Send + Sync + 'static,
    {
        self.add_side_packet_with_options(name, tx, RelaySideOptions::default())
    }

    /// Adds a packet-output side with explicit reliability and link-local options.
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
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, self.clock.now_ms());
        id
    }

    /// Remove a side while keeping existing side IDs stable.
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

    pub fn clear_source_route_mode(&self, src: Option<RelaySideId>) -> TelemetryResult<()> {
        self.set_source_route_mode(src, RouteSelectionMode::Fanout)
    }

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
            .insert((src, ty as u32, dst), enabled);
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

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
        st.typed_route_overrides.remove(&(src, ty as u32, dst));
        #[cfg(feature = "discovery")]
        Self::note_discovery_topology_change_locked(&mut st, now_ms);
        Ok(())
    }

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
            Self::note_discovery_topology_change_locked(&mut st, now_ms);
        }
        let routes = st
            .discovery_routes
            .iter()
            .filter_map(|(&side_id, route)| {
                let side = st.sides.get(side_id).and_then(|side| side.as_ref())?;
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

    /// Enqueue serialized bytes that originated from `src` into the relay RX queue.
    ///
    /// Note: `Arc::from(bytes)` allocates and copies `len` bytes into a new `Arc<[u8]>`.
    /// This is still “fast enough” for many cases, but it is not allocation-free / ISR-safe.
    pub fn rx_serialized_from_side(&self, src: RelaySideId, bytes: &[u8]) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(src)?;
        let mut st = self.state.lock();

        let data = RelayItem::Serialized(Arc::from(bytes));
        let priority = Self::relay_item_priority(&data)?;
        st.rx_queue.push_back_prioritized(
            RelayRxItem {
                src,
                data,
                priority,
            },
            |queued| queued.priority,
        )
    }

    /// Enqueue a full Packet that originated from `src` into the relay RX queue.
    ///
    /// The packet is wrapped in `Arc<Packet>` so fanout can clone the pointer cheaply.
    pub fn rx_from_side(&self, src: RelaySideId, packet: Packet) -> TelemetryResult<()> {
        self.ensure_side_ingress_enabled(src)?;
        let mut st = self.state.lock();

        let data = RelayItem::Packet(Arc::new(packet));
        let priority = Self::relay_item_priority(&data)?;
        st.rx_queue.push_back_prioritized(
            RelayRxItem {
                src,
                data,
                priority,
            },
            |queued| queued.priority,
        )
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
        let mut released_buffered: Vec<Arc<[u8]>> = Vec::new();
        if let RelayItem::Serialized(bytes) = &item.data {
            let (opts, handler_is_serialized) = {
                let st = self.state.lock();
                let side_ref = Self::side_ref(&st, item.src)?;
                (
                    side_ref.opts,
                    matches!(side_ref.tx_handler, RelayTxHandlerFn::Serialized(_)),
                )
            };

            let frame = match serialize::peek_frame_info(bytes.as_ref()) {
                Ok(frame) => frame,
                Err(e) => {
                    if matches!(e, TelemetryError::Deserialize(msg) if msg == "crc32 mismatch")
                        && opts.reliable_enabled
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

            if opts.reliable_enabled
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
                        {
                            let mut st = self.state.lock();
                            let rx_state =
                                self.reliable_rx_state_mut(&mut st, item.src, frame.envelope.ty);
                            if hdr.seq < rx_state.expected_seq {
                                ack_old = Some(rx_state.expected_seq.saturating_sub(1));
                            } else if hdr.seq > rx_state.expected_seq {
                                if rx_state.buffered.len() < RELIABLE_MAX_PENDING {
                                    rx_state
                                        .buffered
                                        .entry(hdr.seq)
                                        .or_insert_with(|| bytes.clone());
                                }
                                request_missing = Some(rx_state.expected_seq);
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

        if self.is_duplicate_pkt(&item)? {
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
            if self.is_duplicate_pkt(&release_item)? {
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
                    crate::DataType::ReliableAck | crate::DataType::ReliablePacketRequest
                ) {
                    let vals = pkt.data_as_u32()?;
                    if vals.len() != 2 {
                        return Err(TelemetryError::Deserialize("bad reliable control payload"));
                    }
                    let ty = crate::DataType::try_from_u32(vals[0])
                        .ok_or(TelemetryError::InvalidType)?;
                    let seq = vals[1];
                    match pkt.data_type() {
                        crate::DataType::ReliableAck => self.handle_reliable_ack(item.src, ty, seq),
                        crate::DataType::ReliablePacketRequest => {
                            self.queue_reliable_retransmit(item.src, ty, seq)?
                        }
                        _ => {}
                    }
                    return Ok(());
                }
            }
            RelayItem::Serialized(bytes) => {
                let env = serialize::peek_envelope(bytes.as_ref())?;
                if matches!(
                    env.ty,
                    crate::DataType::ReliableAck | crate::DataType::ReliablePacketRequest
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
            st.tx_queue.push_back_prioritized(
                RelayTxItem {
                    src: Some(src),
                    dst,
                    data: data.clone(),
                    priority,
                },
                |queued| queued.priority,
            )?;
        }
        Ok(())
    }

    /// Helper: call a TX handler with the best representation we have.
    /// - Packet handler + Packet item: direct.
    /// - Serialized handler + Serialized item: direct.
    /// - Packet handler + Serialized item: deserialize for this call.
    /// - Serialized handler + Packet item: serialize for this call.
    fn call_tx_handler(&self, handler: &RelayTxHandlerFn, data: &RelayItem) -> TelemetryResult<()> {
        match (handler, data) {
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
        }
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
            other => Ok(Some(other)),
        }
    }

    /// Drain the RX queue fully, expanding to TX items.
    #[inline]
    pub fn process_rx_queue(&self) -> TelemetryResult<()> {
        self.process_rx_queue_with_timeout(0)
    }

    /// Drain the TX queue fully, invoking per-side tx_handler.
    #[inline]
    pub fn process_tx_queue(&self) -> TelemetryResult<()> {
        self.process_tx_queue_with_timeout(0)
    }

    /// Drain RX then TX queues fully (one pass).
    #[inline]
    pub fn process_all_queues(&self) -> TelemetryResult<()> {
        self.process_all_queues_with_timeout(0)
    }

    /// Process TX queue with timeout in ms (same style as Router).
    pub fn process_tx_queue_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
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
            if self.send_tx_item(src, dst, handler, opts, data)?
                && timeout_ms != 0
                && self.clock.now_ms().wrapping_sub(start) >= timeout_ms as u64
            {
                break;
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

    /// Process RX and TX queues interleaved, with timeout.
    /// If timeout_ms == 0, drain fully (same semantics as Router).
    pub fn process_all_queues_with_timeout(&self, timeout_ms: u32) -> TelemetryResult<()> {
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
