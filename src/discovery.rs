use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::router::encode_slice_le;
use crate::{
    DataEndpoint, DataType, MessageElement, TelemetryError, TelemetryResult,
    config::{
        OwnedDataTypeDefinition, OwnedEndpointDefinition, OwnedRuntimeSchemaSnapshot,
        RuntimeSchemaSnapshot, export_schema, message_class_code, message_class_from_code,
        message_data_type_code, message_data_type_from_code, reliable_code, reliable_from_code,
    },
    packet::Packet,
    try_enum_from_u32,
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
            | DataType::DiscoverySchema
            | DataType::DiscoveryTopologyRequest
            | DataType::DiscoverySchemaRequest
    )
}

#[inline]
pub const fn is_discovery_request_type(ty: DataType) -> bool {
    matches!(
        ty,
        DataType::DiscoveryTopologyRequest | DataType::DiscoverySchemaRequest
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
    sender: &str,
    timestamp_ms: u64,
    endpoints: &[DataEndpoint],
) -> TelemetryResult<Packet> {
    let payload_words: Vec<u32> = endpoints.iter().copied().map(|ep| ep.as_u32()).collect();
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
    sender: &str,
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

pub fn build_discovery_topology_request(
    sender: &str,
    timestamp_ms: u64,
) -> TelemetryResult<Packet> {
    Packet::new(
        DataType::DiscoveryTopologyRequest,
        &[DataEndpoint::Discovery],
        sender,
        timestamp_ms,
        Vec::<u8>::new().into(),
    )
}

pub fn build_discovery_schema_request(sender: &str, timestamp_ms: u64) -> TelemetryResult<Packet> {
    Packet::new(
        DataType::DiscoverySchemaRequest,
        &[DataEndpoint::Discovery],
        sender,
        timestamp_ms,
        Vec::<u8>::new().into(),
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
    sender: &str,
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
            payload.extend_from_slice(&(ep.as_u32()).to_le_bytes());
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

fn encode_string(payload: &mut Vec<u8>, value: &str) -> TelemetryResult<()> {
    let len = u32::try_from(value.len())
        .map_err(|_| TelemetryError::Serialize("discovery schema string too long"))?;
    payload.extend_from_slice(&len.to_le_bytes());
    payload.extend_from_slice(value.as_bytes());
    Ok(())
}

fn read_u8(payload: &[u8], cursor: &mut usize, label: &'static str) -> TelemetryResult<u8> {
    if payload.len().saturating_sub(*cursor) < 1 {
        return Err(TelemetryError::Deserialize(label));
    }
    let out = payload[*cursor];
    *cursor += 1;
    Ok(out)
}

fn read_u32(payload: &[u8], cursor: &mut usize, label: &'static str) -> TelemetryResult<u32> {
    if payload.len().saturating_sub(*cursor) < 4 {
        return Err(TelemetryError::Deserialize(label));
    }
    let out = u32::from_le_bytes(
        payload[*cursor..*cursor + 4]
            .try_into()
            .expect("4-byte u32"),
    );
    *cursor += 4;
    Ok(out)
}

/// Builds a discovery packet containing the complete runtime schema snapshot.
pub fn build_discovery_schema(sender: &str, timestamp_ms: u64) -> TelemetryResult<Packet> {
    build_discovery_schema_from_snapshot(sender, timestamp_ms, export_schema())
}

/// Elects the authoritative discovery/schema master for the current topology view.
///
/// Tie-breaks are deterministic:
/// 1. Fewest unreachable boards.
/// 2. Lowest maximum hop distance to any reachable board.
/// 3. Lowest total hop distance across all reachable boards.
/// 4. Lexicographically smallest sender ID.
pub fn elect_discovery_master(local_sender: &str, boards: &[TopologyBoardNode]) -> String {
    let mut nodes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for board in boards {
        nodes.entry(board.sender_id.clone()).or_default();
        for peer in board.connections.iter() {
            nodes
                .entry(peer.clone())
                .or_default()
                .push(board.sender_id.clone());
            nodes
                .entry(board.sender_id.clone())
                .or_default()
                .push(peer.clone());
        }
    }
    nodes.entry(local_sender.to_string()).or_default();
    for peers in nodes.values_mut() {
        peers.sort_unstable();
        peers.dedup();
    }

    let mut best_sender = local_sender.to_string();
    let mut best_unreachable = usize::MAX;
    let mut best_max_hops = usize::MAX;
    let mut best_total_hops = usize::MAX;

    for sender in nodes.keys() {
        let mut frontier = vec![sender.clone()];
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        seen.insert(sender.clone(), 0);
        let mut idx = 0;
        while idx < frontier.len() {
            let cur = frontier[idx].clone();
            idx += 1;
            let cur_dist = seen[&cur];
            if let Some(peers) = nodes.get(&cur) {
                for peer in peers {
                    if !seen.contains_key(peer) {
                        seen.insert(peer.clone(), cur_dist + 1);
                        frontier.push(peer.clone());
                    }
                }
            }
        }

        let unreachable = nodes.len().saturating_sub(seen.len());
        let max_hops = seen.values().copied().max().unwrap_or(0);
        let total_hops = seen.values().copied().sum();
        let better = unreachable < best_unreachable
            || (unreachable == best_unreachable && max_hops < best_max_hops)
            || (unreachable == best_unreachable
                && max_hops == best_max_hops
                && total_hops < best_total_hops)
            || (unreachable == best_unreachable
                && max_hops == best_max_hops
                && total_hops == best_total_hops
                && sender < &best_sender);
        if better {
            best_sender = sender.clone();
            best_unreachable = unreachable;
            best_max_hops = max_hops;
            best_total_hops = total_hops;
        }
    }

    best_sender
}

/// Builds a discovery schema packet from an explicit snapshot.
pub fn build_discovery_schema_from_snapshot(
    sender: &str,
    timestamp_ms: u64,
    mut schema: RuntimeSchemaSnapshot,
) -> TelemetryResult<Packet> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&2u32.to_le_bytes());

    schema.endpoints.sort_unstable_by_key(|def| def.id.as_u32());
    schema.types.sort_unstable_by_key(|def| def.id.as_u32());

    payload.extend_from_slice(&(schema.endpoints.len() as u32).to_le_bytes());
    for ep in schema.endpoints {
        payload.extend_from_slice(&ep.id.as_u32().to_le_bytes());
        payload.push(ep.link_local_only as u8);
        encode_string(&mut payload, ep.name)?;
        encode_string(&mut payload, ep.description)?;
    }

    payload.extend_from_slice(&(schema.types.len() as u32).to_le_bytes());
    for ty in schema.types {
        payload.extend_from_slice(&ty.id.as_u32().to_le_bytes());
        encode_string(&mut payload, ty.name)?;
        encode_string(&mut payload, ty.description)?;
        match ty.element {
            MessageElement::Static(count, data_type, class) => {
                payload.push(0);
                payload.extend_from_slice(&(count as u32).to_le_bytes());
                payload.push(message_data_type_code(data_type));
                payload.push(message_class_code(class));
            }
            MessageElement::Dynamic(data_type, class) => {
                payload.push(1);
                payload.extend_from_slice(&0u32.to_le_bytes());
                payload.push(message_data_type_code(data_type));
                payload.push(message_class_code(class));
            }
        }
        payload.push(reliable_code(ty.reliable));
        payload.push(ty.priority);
        payload.extend_from_slice(&(ty.endpoints.len() as u32).to_le_bytes());
        for ep in ty.endpoints {
            payload.extend_from_slice(&ep.as_u32().to_le_bytes());
        }
    }

    Packet::new(
        DataType::DiscoverySchema,
        &[DataEndpoint::Discovery],
        sender,
        timestamp_ms,
        payload.into(),
    )
}

/// Decodes a discovery schema packet.
pub fn decode_discovery_schema(pkt: &Packet) -> TelemetryResult<OwnedRuntimeSchemaSnapshot> {
    if pkt.data_type() != DataType::DiscoverySchema {
        return Err(TelemetryError::InvalidType);
    }
    decode_discovery_schema_payload(pkt.payload())
}

/// Decodes a discovery schema payload into runtime definitions.
pub fn decode_discovery_schema_payload(
    payload: &[u8],
) -> TelemetryResult<OwnedRuntimeSchemaSnapshot> {
    let mut cursor = 0usize;
    let version = read_u32(payload, &mut cursor, "discovery schema version")?;
    if version != 1 && version != 2 {
        return Err(TelemetryError::Deserialize("discovery schema version"));
    }

    let endpoint_count =
        read_u32(payload, &mut cursor, "discovery schema endpoint count")? as usize;
    let mut endpoints = Vec::with_capacity(endpoint_count);
    for _ in 0..endpoint_count {
        let id = DataEndpoint(read_u32(
            payload,
            &mut cursor,
            "discovery schema endpoint id",
        )?);
        let link_local_only =
            read_u8(payload, &mut cursor, "discovery schema endpoint flags")? != 0;
        let name = decode_string(payload, &mut cursor, "discovery schema endpoint name")?;
        let description = if version >= 2 {
            decode_string(
                payload,
                &mut cursor,
                "discovery schema endpoint description",
            )?
        } else {
            String::new()
        };
        endpoints.push(OwnedEndpointDefinition {
            id,
            name,
            description,
            link_local_only,
        });
    }

    let type_count = read_u32(payload, &mut cursor, "discovery schema type count")? as usize;
    let mut types = Vec::with_capacity(type_count);
    for _ in 0..type_count {
        let id = DataType(read_u32(payload, &mut cursor, "discovery schema type id")?);
        let name = decode_string(payload, &mut cursor, "discovery schema type name")?;
        let description = if version >= 2 {
            decode_string(payload, &mut cursor, "discovery schema type description")?
        } else {
            String::new()
        };
        let element_kind = read_u8(payload, &mut cursor, "discovery schema element kind")?;
        let count = read_u32(payload, &mut cursor, "discovery schema element count")? as usize;
        let data_type = message_data_type_from_code(read_u8(
            payload,
            &mut cursor,
            "discovery schema data type",
        )?)
        .ok_or(TelemetryError::Deserialize("discovery schema data type"))?;
        let class =
            message_class_from_code(read_u8(payload, &mut cursor, "discovery schema class")?)
                .ok_or(TelemetryError::Deserialize("discovery schema class"))?;
        let element = match element_kind {
            0 => MessageElement::Static(count, data_type, class),
            1 => MessageElement::Dynamic(data_type, class),
            _ => return Err(TelemetryError::Deserialize("discovery schema element kind")),
        };
        let reliable =
            reliable_from_code(read_u8(payload, &mut cursor, "discovery schema reliable")?)
                .ok_or(TelemetryError::Deserialize("discovery schema reliable"))?;
        let priority = read_u8(payload, &mut cursor, "discovery schema priority")?;
        let endpoint_count =
            read_u32(payload, &mut cursor, "discovery schema type endpoint count")? as usize;
        let mut type_endpoints = Vec::with_capacity(endpoint_count);
        for _ in 0..endpoint_count {
            type_endpoints.push(DataEndpoint(read_u32(
                payload,
                &mut cursor,
                "discovery schema type endpoint",
            )?));
        }
        types.push(OwnedDataTypeDefinition {
            id,
            name,
            description,
            element,
            endpoints: type_endpoints,
            reliable,
            priority,
        });
    }

    if cursor != payload.len() {
        return Err(TelemetryError::Deserialize(
            "discovery schema trailing bytes",
        ));
    }
    Ok(OwnedRuntimeSchemaSnapshot { endpoints, types })
}
