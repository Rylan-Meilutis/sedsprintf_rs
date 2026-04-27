use alloc::vec::Vec;

use crate::{DataType, RouteSelectionMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTypeStats {
    pub data_type: DataType,
    pub tx_packets: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub relayed_tx_packets: u64,
    pub relayed_tx_bytes: u64,
    pub relayed_rx_packets: u64,
    pub relayed_rx_bytes: u64,
    pub tx_retries: u64,
    pub handler_failures: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveLinkStats {
    pub auto_balancing_enabled: bool,
    pub estimated_capacity_bps: u64,
    pub peak_capacity_bps: u64,
    pub current_usage_bps: u64,
    pub peak_usage_bps: u64,
    pub available_headroom_bps: u64,
    pub effective_weight: u64,
    pub last_observed_ms: u64,
    pub sample_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSideStats {
    pub side_id: usize,
    pub side_name: &'static str,
    pub reliable_enabled: bool,
    pub link_local_enabled: bool,
    pub ingress_enabled: bool,
    pub egress_enabled: bool,
    pub tx_packets: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub relayed_tx_packets: u64,
    pub relayed_tx_bytes: u64,
    pub relayed_rx_packets: u64,
    pub relayed_rx_bytes: u64,
    pub local_delivery_packets: u64,
    pub tx_retries: u64,
    pub tx_handler_failures: u64,
    pub local_handler_failures: u64,
    pub total_handler_retries: u64,
    pub adaptive: AdaptiveLinkStats,
    pub data_types: Vec<RuntimeTypeStats>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteModeStats {
    pub src_side_id: Option<usize>,
    pub selection_mode: Option<RouteSelectionMode>,
    pub cursor: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteOverrideStats {
    pub src_side_id: Option<usize>,
    pub dst_side_id: usize,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedRouteOverrideStats {
    pub src_side_id: Option<usize>,
    pub data_type: DataType,
    pub dst_side_id: usize,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteWeightStats {
    pub src_side_id: Option<usize>,
    pub dst_side_id: usize,
    pub weight: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePriorityStats {
    pub src_side_id: Option<usize>,
    pub dst_side_id: usize,
    pub priority: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRuntimeStats {
    pub rx_len: usize,
    pub rx_bytes: usize,
    pub tx_len: usize,
    pub tx_bytes: usize,
    pub replay_len: usize,
    pub replay_bytes: usize,
    pub recent_rx_len: usize,
    pub recent_rx_bytes: usize,
    pub reliable_rx_buffered_len: usize,
    pub reliable_rx_buffered_bytes: usize,
    pub shared_queue_bytes_used: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReliableRuntimeStats {
    pub reliable_return_route_count: usize,
    pub end_to_end_pending_count: usize,
    pub end_to_end_pending_destination_count: usize,
    pub end_to_end_acked_cache_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryRuntimeStats {
    pub route_count: usize,
    pub announcer_count: usize,
    pub current_announce_interval_ms: Option<u64>,
    pub next_announce_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatsSnapshot {
    pub sides: Vec<RuntimeSideStats>,
    pub route_modes: Vec<RouteModeStats>,
    pub route_overrides: Vec<RouteOverrideStats>,
    pub typed_route_overrides: Vec<TypedRouteOverrideStats>,
    pub route_weights: Vec<RouteWeightStats>,
    pub route_priorities: Vec<RoutePriorityStats>,
    pub queues: QueueRuntimeStats,
    pub reliable: ReliableRuntimeStats,
    pub discovery: DiscoveryRuntimeStats,
    pub total_handler_failures: u64,
    pub total_handler_retries: u64,
}
