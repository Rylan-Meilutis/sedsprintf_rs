use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::router::encode_slice_le;
use crate::{
    packet::Packet, try_enum_from_u32, DataEndpoint, DataType, TelemetryError, TelemetryResult,
};

pub const DISCOVERY_ROUTE_TTL_MS: u64 = 30_000;
pub const DISCOVERY_FAST_INTERVAL_MS: u64 = 250;
pub const DISCOVERY_SLOW_INTERVAL_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryCadenceState {
    pub current_interval_ms: u64,
    pub next_announce_ms: u64,
}

impl Default for DiscoveryCadenceState {
    fn default() -> Self {
        Self {
            current_interval_ms: DISCOVERY_FAST_INTERVAL_MS,
            next_announce_ms: 0,
        }
    }
}

impl DiscoveryCadenceState {
    /// Switches discovery back to fast cadence and schedules an immediate announce.
    pub fn on_topology_change(&mut self, now_ms: u64) {
        self.current_interval_ms = DISCOVERY_FAST_INTERVAL_MS;
        self.next_announce_ms = now_ms;
    }

    /// Advances the cadence after sending an announce, backing off toward the slow interval.
    pub fn on_announce_sent(&mut self, now_ms: u64) {
        self.next_announce_ms = now_ms.saturating_add(self.current_interval_ms);
        self.current_interval_ms = core::cmp::min(
            self.current_interval_ms.saturating_mul(2),
            DISCOVERY_SLOW_INTERVAL_MS,
        );
    }

    /// Returns `true` when discovery should emit another announce at `now_ms`.
    pub fn due(&self, now_ms: u64) -> bool {
        now_ms >= self.next_announce_ms
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyBoardNode {
    pub sender_id: String,
    pub reachable_endpoints: Vec<DataEndpoint>,
    pub reachable_timesync_sources: Vec<String>,
    pub connections: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyAnnouncerRoute {
    pub sender_id: String,
    pub reachable_endpoints: Vec<DataEndpoint>,
    pub reachable_timesync_sources: Vec<String>,
    pub routers: Vec<TopologyBoardNode>,
    pub last_seen_ms: u64,
    pub age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologySideRoute {
    pub side_id: usize,
    pub side_name: &'static str,
    pub reachable_endpoints: Vec<DataEndpoint>,
    pub reachable_timesync_sources: Vec<String>,
    pub announcers: Vec<TopologyAnnouncerRoute>,
    pub last_seen_ms: u64,
    pub age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologySnapshot {
    pub advertised_endpoints: Vec<DataEndpoint>,
    pub advertised_timesync_sources: Vec<String>,
    pub routers: Vec<TopologyBoardNode>,
    pub routes: Vec<TopologySideRoute>,
    pub current_announce_interval_ms: u64,
    pub next_announce_ms: u64,
}

/// Returns `true` when the endpoint is reserved for discovery control traffic.
#[inline]
pub const fn is_discovery_endpoint(ep: DataEndpoint) -> bool {
    matches!(ep, DataEndpoint::Discovery)
}

/// Returns `true` when the data type is a discovery control packet type.
#[inline]
pub const fn is_discovery_type(ty: DataType) -> bool {
    matches!(
        ty,
        DataType::DiscoveryAnnounce
            | DataType::DiscoveryTimeSyncSources
            | DataType::DiscoveryTopology
    )
}

fn sort_dedup_strings(items: &mut Vec<String>) {
    items.sort_unstable();
    items.dedup();
}

/// Normalizes a topology-board list in place so it can be compared, exported, or encoded.
pub fn normalize_topology_boards(boards: &mut Vec<TopologyBoardNode>) {
    for board in boards.iter_mut() {
        board.reachable_endpoints.sort_unstable();
        board.reachable_endpoints.dedup();
        sort_dedup_strings(&mut board.reachable_timesync_sources);
        board.connections.retain(|peer| peer != &board.sender_id);
        sort_dedup_strings(&mut board.connections);
    }
    boards.sort_unstable_by(|a, b| a.sender_id.cmp(&b.sender_id));
    boards.dedup_by(|a, b| a.sender_id == b.sender_id);
}

/// Merges board topology views keyed by sender ID.
pub fn merge_topology_boards(dst: &mut Vec<TopologyBoardNode>, src: &[TopologyBoardNode]) {
    let mut merged: BTreeMap<String, TopologyBoardNode> = dst
        .iter()
        .cloned()
        .map(|board| (board.sender_id.clone(), board))
        .collect();
    for board in src {
        let entry = merged
            .entry(board.sender_id.clone())
            .or_insert_with(|| TopologyBoardNode {
                sender_id: board.sender_id.clone(),
                reachable_endpoints: Vec::new(),
                reachable_timesync_sources: Vec::new(),
                connections: Vec::new(),
            });
        entry
            .reachable_endpoints
            .extend(board.reachable_endpoints.iter().copied());
        entry
            .reachable_timesync_sources
            .extend(board.reachable_timesync_sources.iter().cloned());
        entry.connections.extend(board.connections.iter().cloned());
    }
    let mut out: Vec<TopologyBoardNode> = merged.into_values().collect();
    normalize_topology_boards(&mut out);
    *dst = out;
}

/// Summarizes a board topology list into aggregated endpoint and time-source reachability.
pub fn summarize_topology_boards(boards: &[TopologyBoardNode]) -> (Vec<DataEndpoint>, Vec<String>) {
    let mut reachable_endpoints = Vec::new();
    let mut reachable_timesync_sources = Vec::new();
    for board in boards {
        reachable_endpoints.extend(board.reachable_endpoints.iter().copied());
        reachable_timesync_sources.extend(board.reachable_timesync_sources.iter().cloned());
    }
    reachable_endpoints.sort_unstable();
    reachable_endpoints.dedup();
    sort_dedup_strings(&mut reachable_timesync_sources);
    (reachable_endpoints, reachable_timesync_sources)
}

/// Builds a discovery announce packet advertising reachable non-discovery endpoints.
pub fn build_discovery_announce(
    sender: &'static str,
    timestamp_ms: u64,
    endpoints: &[DataEndpoint],
) -> TelemetryResult<Packet> {
    let payload_words: Vec<u32> = endpoints.iter().copied().map(|ep| ep as u32).collect();
    Packet::new(
        DataType::DiscoveryAnnounce,
        &[DataEndpoint::Discovery],
        sender,
        timestamp_ms,
        encode_slice_le(payload_words.as_slice()),
    )
}

/// Decodes a discovery announce packet into its advertised endpoints.
pub fn decode_discovery_announce(pkt: &Packet) -> TelemetryResult<Vec<DataEndpoint>> {
    if pkt.data_type() != DataType::DiscoveryAnnounce {
        return Err(TelemetryError::InvalidType);
    }
    decode_discovery_payload(pkt.payload())
}

/// Decodes a discovery announce payload into a sorted, deduplicated endpoint list.
pub fn decode_discovery_payload(payload: &[u8]) -> TelemetryResult<Vec<DataEndpoint>> {
    if !payload.len().is_multiple_of(4) {
        return Err(TelemetryError::Deserialize("discovery payload width"));
    }

    let mut endpoints = Vec::with_capacity(payload.len() / 4);
    for chunk in payload.chunks_exact(4) {
        let raw = u32::from_le_bytes(chunk.try_into().expect("4-byte chunk"));
        let ep =
            try_enum_from_u32(raw).ok_or(TelemetryError::Deserialize("bad discovery endpoint"))?;
        if is_discovery_endpoint(ep) {
            continue;
        }
        endpoints.push(ep);
    }
    endpoints.sort_unstable();
    endpoints.dedup();
    Ok(endpoints)
}

/// Builds a discovery packet advertising reachable time sync source identifiers.
pub fn build_discovery_timesync_sources<S: AsRef<str>>(
    sender: &'static str,
    timestamp_ms: u64,
    sources: &[S],
) -> TelemetryResult<Packet> {
    let mut payload = Vec::new();
    let mut deduped: Vec<&str> = sources.iter().map(|s| s.as_ref()).collect();
    deduped.sort_unstable();
    deduped.dedup();

    payload.extend_from_slice(&(deduped.len() as u32).to_le_bytes());
    for source in deduped {
        let bytes = source.as_bytes();
        let len = u32::try_from(bytes.len())
            .map_err(|_| TelemetryError::Serialize("discovery source id too long"))?;
        payload.extend_from_slice(&len.to_le_bytes());
        payload.extend_from_slice(bytes);
    }

    Packet::new(
        DataType::DiscoveryTimeSyncSources,
        &[DataEndpoint::Discovery],
        sender,
        timestamp_ms,
        payload.into(),
    )
}

/// Decodes a discovery time sync source packet into source identifiers.
pub fn decode_discovery_timesync_sources(pkt: &Packet) -> TelemetryResult<Vec<String>> {
    if pkt.data_type() != DataType::DiscoveryTimeSyncSources {
        return Err(TelemetryError::InvalidType);
    }
    decode_discovery_timesync_sources_payload(pkt.payload())
}

/// Decodes a discovery time sync source payload into a sorted, deduplicated source list.
pub fn decode_discovery_timesync_sources_payload(payload: &[u8]) -> TelemetryResult<Vec<String>> {
    if payload.len() < 4 {
        return Err(TelemetryError::Deserialize(
            "discovery timesync source count",
        ));
    }

    let count = u32::from_le_bytes(payload[..4].try_into().expect("4-byte count")) as usize;
    let mut cursor = 4usize;
    let mut out = Vec::with_capacity(count);

    for _ in 0..count {
        if payload.len().saturating_sub(cursor) < 4 {
            return Err(TelemetryError::Deserialize("discovery timesync source len"));
        }
        let len = u32::from_le_bytes(payload[cursor..cursor + 4].try_into().expect("4-byte len"))
            as usize;
        cursor += 4;
        if payload.len().saturating_sub(cursor) < len {
            return Err(TelemetryError::Deserialize(
                "discovery timesync source bytes",
            ));
        }
        let raw = &payload[cursor..cursor + len];
        cursor += len;
        let source = core::str::from_utf8(raw)
            .map_err(|_| TelemetryError::Deserialize("discovery timesync source utf8"))?;
        if !source.is_empty() {
            out.push(source.to_string());
        }
    }

    if cursor != payload.len() {
        return Err(TelemetryError::Deserialize(
            "discovery timesync trailing bytes",
        ));
    }

    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Builds a discovery packet advertising the sender's current board/edge topology graph.
pub fn build_discovery_topology(
    sender: &'static str,
    timestamp_ms: u64,
    boards: &[TopologyBoardNode],
) -> TelemetryResult<Packet> {
    let mut payload = Vec::new();
    let mut normalized = boards.to_vec();
    normalize_topology_boards(&mut normalized);

    payload.extend_from_slice(&(normalized.len() as u32).to_le_bytes());
    for board in normalized {
        let sender_bytes = board.sender_id.as_bytes();
        let sender_len = u32::try_from(sender_bytes.len())
            .map_err(|_| TelemetryError::Serialize("discovery topology sender id too long"))?;
        payload.extend_from_slice(&sender_len.to_le_bytes());
        payload.extend_from_slice(sender_bytes);

        payload.extend_from_slice(&(board.reachable_endpoints.len() as u32).to_le_bytes());
        for ep in board.reachable_endpoints {
            payload.extend_from_slice(&(ep as u32).to_le_bytes());
        }

        payload.extend_from_slice(&(board.reachable_timesync_sources.len() as u32).to_le_bytes());
        for source in board.reachable_timesync_sources {
            let bytes = source.as_bytes();
            let len = u32::try_from(bytes.len())
                .map_err(|_| TelemetryError::Serialize("discovery topology source id too long"))?;
            payload.extend_from_slice(&len.to_le_bytes());
            payload.extend_from_slice(bytes);
        }

        payload.extend_from_slice(&(board.connections.len() as u32).to_le_bytes());
        for peer in board.connections {
            let bytes = peer.as_bytes();
            let len = u32::try_from(bytes.len()).map_err(|_| {
                TelemetryError::Serialize("discovery topology connection id too long")
            })?;
            payload.extend_from_slice(&len.to_le_bytes());
            payload.extend_from_slice(bytes);
        }
    }

    Packet::new(
        DataType::DiscoveryTopology,
        &[DataEndpoint::Discovery],
        sender,
        timestamp_ms,
        payload.into(),
    )
}

fn decode_string(
    payload: &[u8],
    cursor: &mut usize,
    label: &'static str,
) -> TelemetryResult<String> {
    if payload.len().saturating_sub(*cursor) < 4 {
        return Err(TelemetryError::Deserialize(label));
    }
    let len = u32::from_le_bytes(
        payload[*cursor..*cursor + 4]
            .try_into()
            .expect("4-byte len"),
    ) as usize;
    *cursor += 4;
    if payload.len().saturating_sub(*cursor) < len {
        return Err(TelemetryError::Deserialize(label));
    }
    let raw = &payload[*cursor..*cursor + len];
    *cursor += len;
    core::str::from_utf8(raw)
        .map(|s| s.to_string())
        .map_err(|_| TelemetryError::Deserialize(label))
}

/// Decodes a discovery topology packet into board-node records.
pub fn decode_discovery_topology(pkt: &Packet) -> TelemetryResult<Vec<TopologyBoardNode>> {
    if pkt.data_type() != DataType::DiscoveryTopology {
        return Err(TelemetryError::InvalidType);
    }
    decode_discovery_topology_payload(pkt.payload())
}

/// Decodes a discovery topology payload into normalized board-node records.
pub fn decode_discovery_topology_payload(
    payload: &[u8],
) -> TelemetryResult<Vec<TopologyBoardNode>> {
    if payload.len() < 4 {
        return Err(TelemetryError::Deserialize(
            "discovery topology board count",
        ));
    }

    let count = u32::from_le_bytes(payload[..4].try_into().expect("4-byte count")) as usize;
    let mut cursor = 4usize;
    let mut boards = Vec::with_capacity(count);

    for _ in 0..count {
        let sender_id = decode_string(payload, &mut cursor, "discovery topology sender id")?;

        if payload.len().saturating_sub(cursor) < 4 {
            return Err(TelemetryError::Deserialize(
                "discovery topology endpoint count",
            ));
        }
        let endpoint_count = u32::from_le_bytes(
            payload[cursor..cursor + 4]
                .try_into()
                .expect("4-byte count"),
        ) as usize;
        cursor += 4;
        let mut reachable_endpoints = Vec::with_capacity(endpoint_count);
        for _ in 0..endpoint_count {
            if payload.len().saturating_sub(cursor) < 4 {
                return Err(TelemetryError::Deserialize("discovery topology endpoint"));
            }
            let raw =
                u32::from_le_bytes(payload[cursor..cursor + 4].try_into().expect("4-byte ep"));
            cursor += 4;
            let ep = try_enum_from_u32(raw)
                .ok_or(TelemetryError::Deserialize("bad discovery endpoint"))?;
            if !is_discovery_endpoint(ep) {
                reachable_endpoints.push(ep);
            }
        }

        if payload.len().saturating_sub(cursor) < 4 {
            return Err(TelemetryError::Deserialize(
                "discovery topology timesync source count",
            ));
        }
        let source_count = u32::from_le_bytes(
            payload[cursor..cursor + 4]
                .try_into()
                .expect("4-byte count"),
        ) as usize;
        cursor += 4;
        let mut reachable_timesync_sources = Vec::with_capacity(source_count);
        for _ in 0..source_count {
            let source = decode_string(payload, &mut cursor, "discovery topology timesync source")?;
            if !source.is_empty() {
                reachable_timesync_sources.push(source);
            }
        }

        if payload.len().saturating_sub(cursor) < 4 {
            return Err(TelemetryError::Deserialize(
                "discovery topology connection count",
            ));
        }
        let connection_count = u32::from_le_bytes(
            payload[cursor..cursor + 4]
                .try_into()
                .expect("4-byte count"),
        ) as usize;
        cursor += 4;
        let mut connections = Vec::with_capacity(connection_count);
        for _ in 0..connection_count {
            let peer = decode_string(payload, &mut cursor, "discovery topology connection")?;
            if !peer.is_empty() {
                connections.push(peer);
            }
        }

        boards.push(TopologyBoardNode {
            sender_id,
            reachable_endpoints,
            reachable_timesync_sources,
            connections,
        });
    }

    if cursor != payload.len() {
        return Err(TelemetryError::Deserialize(
            "discovery topology trailing bytes",
        ));
    }

    normalize_topology_boards(&mut boards);
    Ok(boards)
}
