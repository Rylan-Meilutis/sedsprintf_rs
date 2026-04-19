//! Telemetry configuration and schema description.
//!
//! This module defines:
//! - Device-/build-time constants (identifiers, limits, retries).
//! - The `DataEndpoint` and `DataType` enums.
//! - Functions that describe per-type schema metadata:
//!   - [`get_endpoint_meta`]
//!   - [`get_message_meta`]

use crate::{
    parse_f64, parse_strings, parse_usize, EndpointMeta, MessageClass, MessageDataType,
    MessageElement, MessageMeta,
};
use core::mem::size_of;
use sedsprintf_macros::{define_stack_payload, define_telemetry_schema};
use strum_macros::EnumCount;
// -----------------------------------------------------------------------------
// User-editable configuration
// -----------------------------------------------------------------------------

/// Device identifier string.
///
/// This string is attached to every telemetry packet and is used to identify
/// the platform in downstream tools. It should be **unique per platform**.
pub const DEVICE_IDENTIFIER: &str = match option_env!("DEVICE_IDENTIFIER") {
    Some(val) => parse_strings(val),
    None => "TEST_PLATFORM",
};

/// Maximum number of recent received packet IDs to track for duplicate
/// detection.
///
/// Higher values increase memory usage but improve duplicate detection for
/// high-throughput links or bursty traffic patterns.
// This value should be a power of two for optimal performance.
pub const MAX_RECENT_RX_IDS: usize = match option_env!("MAX_RECENT_RX_IDS") {
    Some(val) => parse_usize(val),
    None => 128,
};

/// Starting size of the internal router and relay queues in bytes.
pub const STARTING_QUEUE_SIZE: usize = match option_env!("STARTING_QUEUE_SIZE") {
    Some(val) => parse_usize(val),
    None => 128,
};

/// Shared memory budget for router and relay queue-owned state in bytes.
///
/// Higher values increase memory usage but may help prevent packet drops,
/// reliable buffer eviction, and discovery topology eviction under high load.
/// `MAX_QUEUE_SIZE` is accepted as a legacy environment alias.
pub const MAX_QUEUE_BUDGET: usize = match option_env!("MAX_QUEUE_BUDGET") {
    Some(val) => parse_usize(val),
    None => match option_env!("MAX_QUEUE_SIZE") {
        Some(val) => parse_usize(val),
        None => 1024 * 100, // 100 KiB
    },
};

/// Bytes reserved up front for the recent-RX ID cache.
///
/// The recent-ID cache is expected to fill to `MAX_RECENT_RX_IDS` during normal
/// operation, so it preallocates its final storage and counts that reserved
/// storage against the shared queue budget from construction.
pub const RECENT_RX_QUEUE_BYTES: usize = {
    let requested = MAX_RECENT_RX_IDS.saturating_mul(size_of::<u64>());
    if requested < MAX_QUEUE_BUDGET {
        requested
    } else {
        MAX_QUEUE_BUDGET
    }
};

/// Grow amount of the internal router and relay queues.
/// Higher values increase memory usage but may help with performance.
/// the value is a multiplier of the current queue size.
/// This value should be greater than 1.0.
pub const QUEUE_GROW_STEP: f64 = match option_env!("QUEUE_GROW_STEP") {
    Some(val) => parse_f64(val),
    None => 3.2,
};

/// Minimum payload size (in bytes) before we consider compression.
pub const PAYLOAD_COMPRESS_THRESHOLD: usize = match option_env!("PAYLOAD_COMPRESS_THRESHOLD") {
    Some(val) => parse_usize(val),
    None => 128,
};

/// Maximum length, in bytes, of any **static** UTF-8 string payload.
///
/// Dynamic string messages may be longer, but many tests and error paths
/// assume this bound when generating placeholder data.
pub const STATIC_STRING_LENGTH: usize = match option_env!("STATIC_STRING_LENGTH") {
    Some(val) => parse_usize(val),
    None => 1024,
};

/// Maximum length, in bytes, of any **static** hex payload.
pub const STATIC_HEX_LENGTH: usize = match option_env!("STATIC_HEX_LENGTH") {
    Some(val) => parse_usize(val),
    None => 1024,
};

/// Maximum number of fractional digits when converting floating-point values
/// to strings (e.g., for human-readable error payloads).
///
/// Higher values increase both payload size and formatting cost.
pub const STRING_PRECISION: usize = match option_env!("STRING_PRECISION") {
    Some(val) => parse_usize(val),
    None => 8, // 12 is expensive; tune as needed
};

/// Maximum payload size (in bytes) that is allowed to be allocated on the
/// stack before the implementation switches to heap allocation.
define_stack_payload!(env = "MAX_STACK_PAYLOAD", default = 64);

/// Maximum number of times a handler is retried before giving up and
/// surfacing a [`TelemetryError::HandlerError`](crate::TelemetryError::HandlerError).
pub const MAX_HANDLER_RETRIES: usize = match option_env!("MAX_HANDLER_RETRIES") {
    Some(val) => parse_usize(val),
    None => 3,
};

/// Reliable retransmit timeout in milliseconds.
pub const RELIABLE_RETRANSMIT_MS: u64 = match option_env!("RELIABLE_RETRANSMIT_MS") {
    Some(val) => parse_usize(val) as u64,
    None => 200,
};

/// Maximum number of retransmit attempts before giving up.
pub const RELIABLE_MAX_RETRIES: u32 = match option_env!("RELIABLE_MAX_RETRIES") {
    Some(val) => parse_usize(val) as u32,
    None => 8,
};

/// Maximum pending reliable packets per (link, data type) stream.
pub const RELIABLE_MAX_PENDING: usize = match option_env!("RELIABLE_MAX_PENDING") {
    Some(val) => parse_usize(val),
    None => 16,
};

/// Maximum cached packet-id return routes for directed reliable ACK traffic.
pub const RELIABLE_MAX_RETURN_ROUTES: usize = match option_env!("RELIABLE_MAX_RETURN_ROUTES") {
    Some(val) => parse_usize(val),
    None => MAX_RECENT_RX_IDS,
};

/// Maximum source-originated packets tracked for end-to-end reliable ACKs.
pub const RELIABLE_MAX_END_TO_END_PENDING: usize =
    match option_env!("RELIABLE_MAX_END_TO_END_PENDING") {
        Some(val) => parse_usize(val),
        None => RELIABLE_MAX_PENDING,
    };

/// Maximum relay-side packet-id ACK cache entries for end-to-end reliable forwarding.
pub const RELIABLE_MAX_END_TO_END_ACK_CACHE: usize =
    match option_env!("RELIABLE_MAX_END_TO_END_ACK_CACHE") {
        Some(val) => parse_usize(val),
        None => MAX_RECENT_RX_IDS,
    };

// Schema path can be overridden at build time via SEDSPRINTF_RS_SCHEMA_PATH.
// Board-local IPC/link-local additions can be merged via SEDSPRINTF_RS_IPC_SCHEMA_PATH.
#[cfg(all(feature = "timesync", feature = "discovery"))]
define_telemetry_schema!(
    path = "telemetry_config.json",
    timesync = true,
    discovery = true
);
#[cfg(all(feature = "timesync", not(feature = "discovery")))]
define_telemetry_schema!(path = "telemetry_config.json", timesync = true);
#[cfg(all(not(feature = "timesync"), feature = "discovery"))]
define_telemetry_schema!(path = "telemetry_config.json", discovery = true);
#[cfg(all(not(feature = "timesync"), not(feature = "discovery")))]
define_telemetry_schema!(path = "telemetry_config.json");
