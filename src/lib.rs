// std on host/tests; no_std when the `std` feature is OFF

// Dear programmer:
// When I wrote this code, only god and I knew how it worked.
// Now, only god knows it!
// Therefore, if you are trying to optimize
// this routine, and it fails (it most surely will),
// please increase this counter as a warning for the next person:
// total hours wasted on this project = 1582

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(unused_doc_comments)]
//! Crate root: common telemetry types, error codes, and feature-gated glue.
//!
//! This module:
//! - Sets up `no_std` vs `std` behavior.
//! - Provides an embedded allocator / panic handler when targeting bare metal.
//! - Re-exports core telemetry configuration and metadata helpers
//!   (`MessageElementCount`, `MessageMeta`, `TelemetryError`, etc.).
//! - Wires in the C and Python FFI layers (`c_api`, `python_api`).
//!
//! Most user-facing APIs live in:
//! - [`config`]: schema + data type/endpoint configuration.
//! - [`router`]: the core Router abstraction.
//! - [`packet`]: `Packet` and friends.
//! - [`serialize`]: wire serialization helpers.
//!
//! Version 4.0.0 highlights:
//! - Telemetry endpoints and data types are runtime IDs with process-local registry metadata.
//! - The build no longer reads `telemetry_config.json` or generates schema-specific Rust enums.
//! - A JSON schema can still seed the runtime registry with `SEDSPRINTF_RS_STATIC_SCHEMA_PATH`.
//! - Nodes can export known endpoints/types and register new ones over time.

extern crate alloc;
extern crate core;
#[cfg(feature = "std")]
extern crate std;
#[cfg(feature = "std")]
use std::io::Error;

use crate::config::{
    DataEndpoint, DataType, STATIC_HEX_LENGTH, STATIC_STRING_LENGTH, get_endpoint_meta,
    get_message_meta, max_data_type_id, max_endpoint_id,
};
use crate::macros::{ReprI32Enum, ReprU32Enum};
use alloc::string::ToString;
use alloc::sync::Arc;
use core::fmt::Formatter;
use core::mem::size_of;
use core::ops::Mul;

// ============================================================================
//  Test / Python FFI modules (std-only)
// ============================================================================

#[cfg(all(test, feature = "std"))]
mod tests;

#[cfg(feature = "python")]
#[cfg(feature = "std")]
mod python_api;

// ============================================================================
//  Allocator & panic handlers (embedded no_std)
// ============================================================================
//
// For EMBEDDED builds (no_std + bare-metal target), provide Telemetry allocator
// + panic handler. Host builds rely on the system allocator / default panic.

#[cfg(all(not(feature = "std"), target_os = "none"))]
unsafe extern "C" {
    fn seds_error_msg(msg: *const u8, len: usize);
}

#[cfg(all(not(feature = "std"), target_os = "none"))]
mod embedded_alloc {
    use core::alloc::{GlobalAlloc, Layout};
    use core::mem::size_of;

    unsafe extern "C" {
        fn telemetryMalloc(size: usize) -> *mut core::ffi::c_void;
        fn telemetryFree(ptr: *mut core::ffi::c_void);
        fn telemetry_panic_hook(msg: *const u8, len: usize);
    }

    /// Global allocator that forwards to `telemetryMalloc` / `telemetryFree`
    /// provided by the host environment.
    struct TelemetryAlloc;

    #[inline]
    fn align_up(addr: usize, align: usize) -> usize {
        (addr + (align - 1)) & !(align - 1)
    }

    unsafe impl GlobalAlloc for TelemetryAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let align = layout.align().max(size_of::<usize>());
            let header = size_of::<usize>();
            let total = match layout
                .size()
                .checked_add(align)
                .and_then(|v| v.checked_add(header))
            {
                Some(v) => v,
                None => return core::ptr::null_mut(),
            };

            let raw = unsafe { telemetryMalloc(total) as *mut u8 };
            if raw.is_null() {
                return core::ptr::null_mut();
            }

            let base = raw as usize + header;
            let aligned = align_up(base, align) as *mut u8;

            // Store the original pointer just before the aligned pointer.
            unsafe {
                let slot = (aligned as *mut usize).offset(-1);
                *slot = raw as usize;
            }

            aligned
        }

        unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
            if ptr.is_null() {
                return;
            }

            let raw = unsafe {
                let slot = (ptr as *mut usize).offset(-1);
                *slot as *mut core::ffi::c_void
            };
            unsafe { telemetryFree(raw) };
        }
    }

    #[global_allocator]
    static A: TelemetryAlloc = TelemetryAlloc;

    // Panic handler for embedded
    use core::panic::PanicInfo;

    #[panic_handler]
    fn panic(_info: &PanicInfo) -> ! {
        let msg = b"rust panic";
        unsafe {
            telemetry_panic_hook(msg.as_ptr(), msg.len());
        }

        // Halt forever after panic.
        loop {}
    }

    // ensure cortex-m only compiles on embedded
    // use cortex_m as _;
}

// For HOST builds (std is ON), the system allocator is used automatically.
// No custom panic handler needed.

// ============================================================================
//  Portable core logic: modules
// ============================================================================

mod c_api;
pub mod config;
pub mod diagnostics;
#[cfg(feature = "discovery")]
pub mod discovery;
mod lock;
mod macros;
pub mod packet;
mod queue;
pub mod relay;
pub mod router;
pub mod serialize;
mod small_payload;
#[cfg(feature = "timesync")]
pub mod timesync;
// ============================================================================
//  Schema-derived global constants
// ============================================================================

/// Maximum enum value for `DataEndpoint` (inclusive), derived from the schema.
pub const MAX_VALUE_DATA_ENDPOINT: u32 = 255;

/// Maximum enum value for `DataType` (inclusive), derived from the schema.
pub const MAX_VALUE_DATA_TYPE: u32 = 4095;

/// Maximum enum value for `RouteSelectionMode` (inclusive).
pub const MAX_VALUE_ROUTE_SELECTION_MODE: i32 = 2;

impl crate::macros::ReprU32Enum for DataType {
    const MAX: u32 = MAX_VALUE_DATA_TYPE;

    #[inline]
    fn from_u32(x: u32) -> Option<Self> {
        DataType::try_from_u32(x)
    }
}

impl crate::macros::ReprU32Enum for DataEndpoint {
    const MAX: u32 = MAX_VALUE_DATA_ENDPOINT;

    #[inline]
    fn from_u32(x: u32) -> Option<Self> {
        DataEndpoint::try_from_u32(x)
    }
}

#[inline]
pub fn current_max_endpoint_id() -> u32 {
    max_endpoint_id()
}

#[inline]
pub fn current_max_data_type_id() -> u32 {
    max_data_type_id()
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum RouteSelectionMode {
    Fanout = 0,
    Weighted = 1,
    Failover = 2,
}

impl_repr_i32_enum!(
    RouteSelectionMode,
    RouteSelectionMode::Fanout as i32,
    RouteSelectionMode::Failover as i32
);

#[inline]
const fn parse_usize(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut val = 0;

    while i < bytes.len() {
        let c = bytes[i];
        if c < b'0' || c > b'9' {
            panic!("Invalid digit");
        }
        val = val * 10 + (c - b'0') as usize;
        i += 1;
    }
    val
}

#[inline]
pub const fn parse_f64(s: &str) -> f64 {
    let bytes = s.as_bytes();
    let mut i = 0;

    if bytes.is_empty() {
        panic!("empty string");
    }

    // sign
    let mut sign = 1.0;
    if bytes[i] == b'-' {
        sign = -1.0;
        i += 1;
    } else if bytes[i] == b'+' {
        i += 1;
    }

    let mut int_part: f64 = 0.0;
    let mut has_digits = false;

    while i < bytes.len() && bytes[i] >= b'0' && bytes[i] <= b'9' {
        int_part = int_part * 10.0 + (bytes[i] - b'0') as f64;
        i += 1;
        has_digits = true;
    }

    let mut frac_part: f64 = 0.0;
    let mut scale: f64 = 1.0;

    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;

        while i < bytes.len() && bytes[i] >= b'0' && bytes[i] <= b'9' {
            scale *= 10.0;
            frac_part += (bytes[i] - b'0') as f64 / scale;
            i += 1;
            has_digits = true;
        }
    }

    if !has_digits || i != bytes.len() {
        panic!("invalid f64 literal");
    }

    sign * (int_part + frac_part)
}

#[inline(always)]
const fn parse_strings(s: &str) -> &str {
    s
}

#[inline]
pub const fn parse_u8(s: &str) -> u8 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut val: u16 = 0;

    if bytes.is_empty() {
        panic!("empty string");
    }

    while i < bytes.len() {
        let c = bytes[i];
        if c < b'0' || c > b'9' {
            panic!("invalid digit in u8");
        }

        val = val * 10 + (c - b'0') as u16;
        if val > 255 {
            panic!("u8 overflow");
        }

        i += 1;
    }

    val as u8
}

#[inline]
pub const fn parse_u128(s: &str) -> u128 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut val: u128 = 0;

    if bytes.is_empty() {
        panic!("empty string");
    }

    while i < bytes.len() {
        let c = bytes[i];
        if c < b'0' || c > b'9' {
            panic!("invalid digit in u128");
        }

        let digit = (c - b'0') as u128;

        // Overflow check: val*10 + digit <= u128::MAX
        // i.e. val <= (u128::MAX - digit) / 10
        if val > (u128::MAX - digit) / 10 {
            panic!("u128 overflow");
        }

        val = val * 10 + digit;
        i += 1;
    }

    val
}

// ============================================================================
//  Message metadata (element counts, data types, sizes)
// ============================================================================
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct EndpointMeta {
    /// Static name of the endpoint
    name: &'static str,
    /// Human-readable description used by schema lookup APIs.
    description: &'static str,
    /// Restrict remote forwarding to link-local/software-bus sides only.
    link_local_only: bool,
}

impl EndpointMeta {
    /// Return a stable string representation used in logs and in
    /// `Packet::to_string()` output.
    ///
    /// This should remain stable over time for compatibility with tests and
    /// external tooling.
    pub fn as_str(&self) -> &'static str {
        self.name
    }

    /// Return the human-readable endpoint description.
    #[inline]
    pub fn description(&self) -> &'static str {
        self.description
    }

    /// Return whether this endpoint is restricted to link-local/software-bus sides.
    #[inline]
    pub fn is_link_local_only(&self) -> bool {
        self.link_local_only
    }
}

impl DataEndpoint {
    /// Return a stable string representation used in logs and in
    /// `Packet::to_string()` output.
    ///
    /// This should remain stable over time for compatibility with tests and
    /// external tooling.
    pub fn as_str(&self) -> &'static str {
        get_endpoint_meta(*self).name
    }

    /// Return the human-readable endpoint description.
    pub fn description(&self) -> &'static str {
        get_endpoint_meta(*self).description
    }

    /// Return whether this endpoint is restricted to link-local/software-bus sides.
    #[inline]
    pub fn is_link_local_only(&self) -> bool {
        get_endpoint_meta(*self).link_local_only
    }
}

/// Describes how many elements are present for a given message type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum MessageElement {
    /// Fixed number of elements.
    ///
    /// (count, MessageDataType, MessageClass)
    Static(usize, MessageDataType, MessageClass),
    /// Variable number of elements (payload size can vary).
    ///
    /// (MessageDataType, MessageClass)
    Dynamic(MessageDataType, MessageClass),
}

impl MessageElement {
    /// Get the `MessageDataType` for this element count.
    #[inline]
    pub const fn data_type(&self) -> MessageDataType {
        match self {
            MessageElement::Static(_, dt, _) => *dt,
            MessageElement::Dynamic(dt, _) => *dt,
        }
    }

    /// Get the `MessageType` for this element count.
    #[inline]
    pub const fn message_type(&self) -> MessageClass {
        match self {
            MessageElement::Static(_, _, mt) => *mt,
            MessageElement::Dynamic(_, mt) => *mt,
        }
    }
}

/// Reliable delivery mode for a data type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum ReliableMode {
    /// No reliable delivery/acknowledgement on the wire.
    None,
    /// Reliable delivery with strict ordering.
    Ordered,
    /// Reliable delivery without ordering guarantees.
    Unordered,
}

/// Static metadata for a message type: element count and valid endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct MessageMeta {
    name: &'static str,
    /// Human-readable description used by schema lookup APIs.
    description: &'static str,
    /// How many elements are present (fixed vs dynamic).
    element: MessageElement,
    /// Allowed endpoints for this message type.
    endpoints: &'static [DataEndpoint],
    /// Reliable delivery mode for this type.
    reliable: ReliableMode,
    /// Queue priority for this type. Higher values are serviced first.
    priority: u8,
}

impl DataType {
    /// Get the string representation of the DataType
    pub fn as_str(&self) -> &'static str {
        get_message_meta(*self).name
    }

    /// Return the human-readable data type description.
    pub fn description(&self) -> &'static str {
        get_message_meta(*self).description
    }
}
/// Lookup `MessageMeta` for a given [`DataType`] using the generated config.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - `MessageMeta` struct with element count and allowed endpoints.
#[inline]
pub fn message_meta(ty: DataType) -> MessageMeta {
    get_message_meta(ty)
}

/// Return whether the given [`DataType`] is configured for reliable delivery.
#[inline]
pub fn is_reliable_type(ty: DataType) -> bool {
    !matches!(get_message_meta(ty).reliable, ReliableMode::None)
}

/// Return the reliable delivery mode for the given [`DataType`].
#[inline]
pub fn reliable_mode(ty: DataType) -> ReliableMode {
    get_message_meta(ty).reliable
}

/// Return the queue priority for the given [`DataType`].
#[inline]
pub fn message_priority(ty: DataType) -> u8 {
    get_message_meta(ty).priority
}

// ---- Convenience multiplication helpers ----

impl Mul<MessageElement> for usize {
    type Output = usize;

    #[inline]
    fn mul(self, rhs: MessageElement) -> usize {
        self * rhs.into()
    }
}

impl Mul<usize> for MessageElement {
    type Output = usize;

    #[inline]
    fn mul(self, rhs: usize) -> usize {
        self.into() * rhs
    }
}

impl MessageElement {
    /// Convert the element count to a `usize`.
    ///
    /// - `Static(n)` → `n`
    /// - `Dynamic`   → `0` (caller must handle dynamic sizing separately)
    #[inline]
    fn into(self) -> usize {
        match self {
            MessageElement::Static(a, _, _) => a,
            _ => 0,
        }
    }
}

/// Return the total payload size (in bytes) required for a given `DataType`
/// under the *static* schema.
///
/// This is `element_size * element_count`. For dynamic types, the
/// configuration ensures we only call this where it makes sense.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - Total static payload size in bytes.
#[inline]
pub fn get_needed_message_size(ty: DataType) -> usize {
    data_type_size(get_data_type(ty)) * get_message_meta(ty).element
}

/// Return the logical "info" type (Info/Error) for a given `DataType`.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - `MessageType` enum value.
#[inline]
pub fn get_info_type(ty: DataType) -> MessageClass {
    get_message_meta(ty).element.message_type()
}

/// Return the *element* data type (e.g., `Float32`, `Int16`, `String`) for a
/// given `DataType`.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - `MessageDataType` enum value.
#[inline]
pub fn get_data_type(ty: DataType) -> MessageDataType {
    get_message_meta(ty).element.data_type()
}

/// Return the message name for a given `DataType`.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - Static string name of the message type.
#[inline]
pub fn get_message_name(ty: DataType) -> &'static str {
    get_message_meta(ty).name
}

/// Return the default endpoints for a given `DataType`.
/// # Arguments
/// - `ty`: Logical data type to query.
/// # Returns
/// - Slice of allowed `DataEndpoint` values.
#[inline]
pub fn endpoints_from_datatype(ty: DataType) -> &'static [DataEndpoint] {
    get_message_meta(ty).endpoints
}

/// Primitive element type used by a message.
///
/// This is the underlying "slot" type, not the high-level `DataType`
/// (which is the logical schema type).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum MessageDataType {
    Float64,
    Float32,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    UInt128,
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Bool,
    String,
    Binary,
    NoData,
}

/// Size in bytes of a single element for the given [`MessageDataType`].
///
/// For `String` / `Hex`, this returns the fixed maximum static length
/// configured by the schema (`MAX_STATIC_STRING_LENGTH`, `MAX_STATIC_HEX_LENGTH`).
/// # Arguments
/// - `dt`: Logical data type to query.
/// # Returns
/// - Size in bytes of a single element of that type.
#[inline]
pub const fn data_type_size(dt: MessageDataType) -> usize {
    match dt {
        MessageDataType::Float64 => size_of::<f64>(),
        MessageDataType::Float32 => size_of::<f32>(),
        MessageDataType::UInt8 => size_of::<u8>(),
        MessageDataType::UInt16 => size_of::<u16>(),
        MessageDataType::UInt32 => size_of::<u32>(),
        MessageDataType::UInt64 => size_of::<u64>(),
        MessageDataType::UInt128 => size_of::<u128>(),
        MessageDataType::Int8 => size_of::<i8>(),
        MessageDataType::Int16 => size_of::<i16>(),
        MessageDataType::Int32 => size_of::<i32>(),
        MessageDataType::Int64 => size_of::<i64>(),
        MessageDataType::Int128 => size_of::<i128>(),
        MessageDataType::Bool => size_of::<bool>(),
        MessageDataType::String => STATIC_STRING_LENGTH,
        MessageDataType::Binary => STATIC_HEX_LENGTH,
        MessageDataType::NoData => 0,
    }
}

/// High-level classification of message kind.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum MessageClass {
    /// Informational telemetry.
    Data,
    /// Error / fault telemetry.
    Error,
    /// Warning telemetry.
    Warning,
}

// ============================================================================
//  Error types and error codes
// ============================================================================

/// Rich error type used throughout the telemetry crate.
///
/// Most public APIs expose a `TelemetryResult<T>` alias for
/// `Result<T, TelemetryError>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryError {
    /// Generic / unspecified error.
    GenericError(Option<Arc<str>>),

    /// Logical type ID is not a valid [`DataType`].
    InvalidType,

    /// Payload size doesn't match the schema's expectations.
    SizeMismatch { expected: usize, got: usize },

    /// Legacy / generic size mismatch error (for C/Python parity).
    SizeMismatchError,

    /// No endpoints were supplied where they are required.
    EmptyEndpoints,

    /// Timestamp is invalid (e.g., zero when disallowed).
    TimestampInvalid,

    /// A packet is missing its payload bytes.
    MissingPayload,

    /// A handler (C/Python callback) returned an error.
    HandlerError(&'static str),

    /// Generic invalid argument from caller.
    BadArg,

    /// Serialization error.
    Serialize(&'static str),

    /// Deserialization error.
    Deserialize(&'static str),

    /// IO / transport error.
    Io(&'static str),

    /// UTF-8 decoding failed where string payloads are expected.
    InvalidUtf8,

    /// Payload type size mismatch.
    TypeMismatch { expected: usize, got: usize },

    /// Invalid link ID provided.
    InvalidLinkId(&'static str),

    /// Packet is bigger than the queue size
    PacketTooLarge(&'static str),
}

impl TelemetryError {
    /// Map a rich [`TelemetryError`] to a stable numeric error code
    /// used by the FFI layers.
    pub const fn to_error_code(&self) -> TelemetryErrorCode {
        match self {
            TelemetryError::GenericError(_) => TelemetryErrorCode::GenericError,
            TelemetryError::InvalidType => TelemetryErrorCode::InvalidType,
            TelemetryError::SizeMismatch { .. } => TelemetryErrorCode::SizeMismatch,
            TelemetryError::SizeMismatchError => TelemetryErrorCode::SizeMismatchError,
            TelemetryError::EmptyEndpoints => TelemetryErrorCode::EmptyEndpoints,
            TelemetryError::TimestampInvalid => TelemetryErrorCode::TimestampInvalid,
            TelemetryError::MissingPayload => TelemetryErrorCode::MissingPayload,
            TelemetryError::HandlerError(_) => TelemetryErrorCode::HandlerError,
            TelemetryError::BadArg => TelemetryErrorCode::BadArg,
            TelemetryError::Serialize(_) => TelemetryErrorCode::Serialize,
            TelemetryError::Deserialize(_) => TelemetryErrorCode::Deserialize,
            TelemetryError::Io(_) => TelemetryErrorCode::Io,
            TelemetryError::InvalidUtf8 => TelemetryErrorCode::InvalidUtf8,
            TelemetryError::TypeMismatch { .. } => TelemetryErrorCode::TypeMismatch,
            TelemetryError::InvalidLinkId(_) => TelemetryErrorCode::InvalidLinkId,
            TelemetryError::PacketTooLarge(_) => TelemetryErrorCode::PacketTooLarge,
        }
    }
}

/// Allow conversion of `TelemetryError` to human-readable string.
impl core::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.write_str(&TelemetryError::to_string(self))
    }
}

/// Implement `std::error::Error` for `TelemetryError` when `std` is enabled.
#[cfg(feature = "std")]
impl std::error::Error for TelemetryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

/// Allow the conversion from std error to telemetry error
#[cfg(feature = "std")]
impl From<Error> for TelemetryError {
    fn from(error: Error) -> Self {
        let str = error.to_string();
        let astr: Arc<str> = Arc::from(str.as_str());
        TelemetryError::GenericError(Some(astr))
    }
}

/// Allow the conversion from boxed std error to telemetry error
#[cfg(feature = "std")]
impl From<Box<dyn std::error::Error>> for TelemetryError {
    fn from(err: Box<dyn std::error::Error>) -> Self {
        let str = err.to_string();
        let astr: Arc<str> = Arc::from(str.as_str());
        TelemetryError::GenericError(Some(astr))
    }
}

/// Numeric error codes used on the C/Python FFI boundary.
///
/// Negative values are used to avoid collisions with success codes
/// and other positive return values (e.g. lengths).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum TelemetryErrorCode {
    GenericError = -2,
    InvalidType = -3,
    SizeMismatch = -4,
    SizeMismatchError = -5,
    EmptyEndpoints = -6,
    TimestampInvalid = -7,
    MissingPayload = -8,
    HandlerError = -9,
    BadArg = -10,
    Serialize = -11,
    Deserialize = -12,
    Io = -13,
    InvalidUtf8 = -14,
    TypeMismatch = -15,
    InvalidLinkId = -16,
    PacketTooLarge = -17,
}

// Generate ReprI32Enum helpers for TelemetryErrorCode
impl_repr_i32_enum!(
    TelemetryErrorCode,
    TelemetryErrorCode::MAX,
    TelemetryErrorCode::MIN
);

impl TelemetryErrorCode {
    /// Maximum valid numeric error code value.
    pub const MAX: i32 = TelemetryErrorCode::InvalidType as i32;

    /// Minimum valid numeric error code value.
    pub const MIN: i32 = TelemetryErrorCode::Io as i32;

    /// Human-readable string for logging / debugging.
    /// # Returns
    /// - Static string representation of the error code.
    #[inline]
    pub fn as_str(&self) -> &'static str {
        match self {
            TelemetryErrorCode::GenericError => "GenericError",
            TelemetryErrorCode::InvalidType => "{Invalid Type}",
            TelemetryErrorCode::SizeMismatch => "{Size Mismatch}",
            TelemetryErrorCode::SizeMismatchError => "{Size Mismatch Error}",
            TelemetryErrorCode::EmptyEndpoints => "{Empty Endpoints}",
            TelemetryErrorCode::TimestampInvalid => "{Timestamp Invalid}",
            TelemetryErrorCode::MissingPayload => "{Missing Payload}",
            TelemetryErrorCode::HandlerError => "{Handler Error}",
            TelemetryErrorCode::BadArg => "{Bad Arg}",
            TelemetryErrorCode::Serialize => "{Serialize Error}",
            TelemetryErrorCode::Deserialize => "{Deserialize Error}",
            TelemetryErrorCode::Io => "{IO Error}",
            TelemetryErrorCode::InvalidUtf8 => "{Invalid UTF-8}",
            TelemetryErrorCode::TypeMismatch => "{Type Mismatch}",
            TelemetryErrorCode::InvalidLinkId => "{Invalid Link ID}",
            TelemetryErrorCode::PacketTooLarge => "{Packet Too Large}",
        }
    }

    /// Try to convert a raw i32 error code into a [`TelemetryErrorCode`].
    ///
    /// Returns `None` if the code is out of range or not recognized.
    /// # Arguments
    /// - `x`: Raw i32 error code to convert.
    /// # Returns
    /// - `Some(TelemetryErrorCode)` if valid, `None` if invalid.
    #[inline]
    pub fn try_from_i32(x: i32) -> Option<Self> {
        try_enum_from_i32(x)
    }
}

/// Common result alias for telemetry operations.
pub type TelemetryResult<T> = Result<T, TelemetryError>;

// ============================================================================
//  Generic enum helpers (repr(u32) / repr(i32))
// ============================================================================

/// Try to convert a `u32` into a `#[repr(u32)]` enum `E`.
///
/// Returns `None` if the value is out of range (greater than `E::MAX`).
/// # Arguments
/// - `x`: Raw u32 value to convert.
/// # Returns
/// - `Some(E)` if valid, `None` if invalid.
#[inline]
pub fn try_enum_from_u32<E: ReprU32Enum>(x: u32) -> Option<E> {
    if x > E::MAX {
        return None;
    }
    E::from_u32(x)
}

/// Try to convert an `i32` into a `#[repr(i32)]` enum `E`.
///
/// Returns `None` if the value is outside the `[E::MIN, E::MAX]` range.
/// # Arguments
/// - `x`: Raw i32 value to convert.
/// # Returns
/// - `Some(E)` if valid, `None` if invalid.
#[inline]
pub fn try_enum_from_i32<E: ReprI32Enum>(x: i32) -> Option<E> {
    if x < E::MIN || x > E::MAX {
        return None;
    }

    // SAFETY: `E` is promised to be a fieldless #[repr(i32)] enum (thus 4 bytes, Copy).
    let e = unsafe { (&x as *const i32 as *const E).read() };
    Some(e)
}
