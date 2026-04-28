//! Telemetry packet core type and formatting helpers.
//!
//! [`Packet`] is the main payload-bearing type. It:
//! - holds sender, endpoints, timestamp and raw payload bytes,
//! - validates payload sizes and encodings against the schema from `message_meta`,
//! - supports pretty printing (header + decoded values) for debugging/logging,
//! - uses [`SmallPayload`] internally to keep small messages on the stack.

use crate::config::{DEVICE_IDENTIFIER, STRING_PRECISION, StandardSmallPayload};
use crate::queue::ByteCost;
use crate::{
    MessageClass, MessageDataType, MessageElement, TelemetryError, TelemetryResult,
    config::{DataEndpoint, DataType},
    data_type_size, get_data_type, get_message_name, message_meta,
    router::LeBytes,
};
use crate::{impl_data_as_prim, impl_from_prim_slices, impl_ledecode_auto};
use alloc::{string::String, string::ToString, sync::Arc, vec, vec::Vec};
use core::any::TypeId;
use core::fmt::{Formatter, Write};
// ============================================================================
// Constants
// ============================================================================

/// Threshold (in ms since boot/epoch) above which timestamps are treated as
/// Unix epoch time rather than an uptime counter.
///
/// Anything smaller is formatted as an uptime-style duration; larger values
/// are formatted as a UTC date-time.
const EPOCH_MS_THRESHOLD: u64 = 1_000_000_000_000; // clearly not an uptime counter

/// Default starting capacity for human-readable strings.
const DEFAULT_STRING_CAPACITY: usize = 96;

// ============================================================================
// Packet
// ============================================================================

/// Payload-bearing packet (safe, validated, shareable).
///
/// This is the primary data structure passed around inside the crate and
/// across FFI boundaries (via views / wrappers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Logical message type (schema selector).
    ty: DataType,

    /// Size of the payload in bytes.
    ///
    /// This is cached and must match `payload.len()`. [`Packet::validate`]
    /// checks the invariant.
    data_size: usize,

    /// Logical sender identifier (e.g. device or subsystem name).
    sender: Arc<str>,

    /// Destination endpoints for this message.
    endpoints: Arc<[DataEndpoint]>,

    /// Timestamp in milliseconds.
    ///
    /// - If `< EPOCH_MS_THRESHOLD`, treated as an uptime counter and formatted
    ///   like `12m 34s 567ms`.
    /// - If `>= EPOCH_MS_THRESHOLD`, treated as Unix epoch ms and formatted
    ///   as `YYYY-MM-DD HH:MM:SS.mmmZ`.
    timestamp: u64,

    /// Raw payload bytes, stored via [`SmallPayload`] for small/large optimization.
    payload: StandardSmallPayload,

    /// Inline wire-format shape used when this packet came from a migration-safe frame.
    wire_shape: Option<MessageElement>,

    /// Explicit destination sender hashes frozen into the wire-format contract.
    wire_target_senders: Arc<[u64]>,
}

// ============================================================================
// Internal helpers for validation / formatting
// ============================================================================

/// Effective element width (in bytes) for the given message data type.
///
/// For numeric/bool types this is the true width of one element.
/// For string/hex we return 1 to treat the payload as a byte stream when
/// checking dynamic length multiples.
#[inline]
const fn element_width(dt: MessageDataType) -> usize {
    match dt {
        MessageDataType::UInt8 | MessageDataType::Int8 | MessageDataType::Bool => 1,
        MessageDataType::UInt16 | MessageDataType::Int16 => 2,
        MessageDataType::UInt32 | MessageDataType::Int32 | MessageDataType::Float32 => 4,
        MessageDataType::UInt64 | MessageDataType::Int64 | MessageDataType::Float64 => 8,
        MessageDataType::UInt128 | MessageDataType::Int128 => 16,
        // For String/Hex we treat width as 1 (byte granularity) when checking dynamic multiples.
        MessageDataType::String | MessageDataType::Binary => 1,
        MessageDataType::NoData => 0,
    }
}

// Simple 64-bit hash used for packet IDs (no_std friendly).
/// Not cryptographic; just a decent mix for deduplication.
/// Takes an initial hash value and a byte slice, returns the updated hash.
#[inline]
pub fn hash_bytes_u64(mut h: u64, bytes: &[u8]) -> u64 {
    // Arbitrary odd constant (not FNV, but good enough for dedupe).
    const PRIME: u64 = 0x9E37_79B1;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
        // a tiny extra mix
        h ^= h >> 27;
    }
    h
}

/// Validate the payload of a dynamic-length message.
///
/// - For `String`: trims trailing NULs for validation and ensures UTF-8 (if non-empty).
/// - For `Hex`: no additional validation.
/// - For numerics/bool: ensures the length is a multiple of the element width.
#[inline]
fn validate_dynamic_len_and_content_for_element(
    element: MessageElement,
    bytes: &[u8],
) -> TelemetryResult<()> {
    match element.data_type() {
        MessageDataType::String => {
            let end = bytes
                .iter()
                .rposition(|&b| b != 0)
                .map(|i| i + 1)
                .unwrap_or(0);
            if end > 0 {
                core::str::from_utf8(&bytes[..end]).map_err(|_| TelemetryError::InvalidUtf8)?;
            }
            Ok(())
        }
        MessageDataType::Binary => Ok(()),
        dt => {
            let w = element_width(dt);
            if w == 0 || !bytes.len().is_multiple_of(w) {
                return Err(TelemetryError::SizeMismatch {
                    expected: w,
                    got: bytes.len(),
                });
            }
            Ok(())
        }
    }
}

// ============================================================================
// Decode bytes trait
// ============================================================================

trait LeDecode: Sized {
    const WIDTH: usize;
    fn from_le(slice: &[u8]) -> Self;
}

impl_ledecode_auto!(f32);
impl_ledecode_auto!(f64);

impl_ledecode_auto!(u16);
impl_ledecode_auto!(u32);
impl_ledecode_auto!(u64);
impl_ledecode_auto!(u128);

impl_ledecode_auto!(i16);
impl_ledecode_auto!(i32);
impl_ledecode_auto!(i64);
impl_ledecode_auto!(i128);

// special 1-byte cases
impl_ledecode_auto!(u8);
impl_ledecode_auto!(i8);

// ============================================================================
// Packet impl
// ============================================================================

impl Packet {
    /// Validate `payload` against the provided schema element.
    ///
    /// This helper is shared by normal packet construction and by
    /// `new_with_wire_contract(...)`. The latter matters because a packet that
    /// was already serialized may need to keep using the element shape it was
    /// written with even if the local runtime registry has since changed.
    ///
    /// # Parameters
    /// - `element`: Effective schema element to validate against. This may come
    ///   from the current runtime registry or from the inline wire contract.
    /// - `payload`: Raw payload bytes to validate.
    ///
    /// # Returns
    /// - `Ok(())` when the payload is compatible with `element`.
    /// - `Err(TelemetryError)` when the payload length or encoding does not
    ///   match the required shape.
    #[inline]
    fn validate_payload_against_element(
        element: MessageElement,
        payload: &[u8],
    ) -> TelemetryResult<()> {
        match element {
            MessageElement::Static(need, dt, _) => {
                let need = need * data_type_size(dt);
                if payload.len() != need {
                    return Err(TelemetryError::SizeMismatch {
                        expected: need,
                        got: payload.len(),
                    });
                }
                Ok(())
            }
            MessageElement::Dynamic(_, _) => {
                validate_dynamic_len_and_content_for_element(element, payload)
            }
        }
    }

    /// Create a packet while preserving wire-contract metadata from a decoded frame.
    ///
    /// This constructor is used by deserialization paths that may carry a
    /// migration-safe wire contract. The contract can supply:
    ///
    /// - `wire_shape`: an inline `MessageElement` that keeps payload decoding
    ///   stable even if the local runtime schema changed after the packet was
    ///   serialized.
    /// - `wire_target_senders`: a frozen list of destination-holder sender
    ///   hashes that routers and relays use to keep in-flight forwarding bound
    ///   to the originally intended remote holders.
    ///
    /// # Parameters
    /// - `ty`: Logical message type ID carried by the frame.
    /// - `endpoints`: Logical destination endpoints from the endpoint bitmap.
    /// - `sender`: Logical sender string decoded from the frame.
    /// - `timestamp`: Wire timestamp in milliseconds.
    /// - `payload`: Payload bytes after any decompression.
    /// - `wire_shape`: Optional inline payload shape carried by the wire
    ///   contract. When present, validation uses this value instead of the
    ///   current runtime schema entry.
    /// - `wire_target_senders`: Frozen destination-holder hashes carried by the
    ///   wire contract. These are routing metadata, not application payload.
    ///
    /// # Returns
    /// - `Ok(Packet)` when all packet invariants and payload validation pass.
    /// - `Err(TelemetryError)` when endpoints are empty or the payload is
    ///   incompatible with the effective schema element.
    #[inline]
    pub(crate) fn new_with_wire_contract(
        ty: DataType,
        endpoints: &[DataEndpoint],
        sender: &str,
        timestamp: u64,
        payload: Arc<[u8]>,
        wire_shape: Option<MessageElement>,
        wire_target_senders: Arc<[u64]>,
    ) -> TelemetryResult<Self> {
        if endpoints.is_empty() {
            return Err(TelemetryError::EmptyEndpoints);
        }

        let element = wire_shape.unwrap_or(message_meta(ty).element);
        Self::validate_payload_against_element(element, &payload)?;

        Ok(Self {
            ty,
            data_size: payload.len(),
            sender: sender.into(),
            endpoints: Arc::<[DataEndpoint]>::from(endpoints),
            timestamp,
            payload: StandardSmallPayload::new(&payload),
            wire_shape,
            wire_target_senders,
        })
    }

    /// Create a packet from a raw payload, validating against `message_meta(ty)`.
    ///
    /// Checks:
    /// - `endpoints` is non-empty.
    /// - For static element count:
    ///   - `payload.len() == element_count * data_type_size(get_data_type(ty))`.
    /// - For dynamic:
    ///   - Length and encoding are validated by [`validate_dynamic_len_and_content`].
    /// # Arguments
    /// - `ty`: logical message type (schema selector).
    /// - `endpoints`: destination endpoint list (must be non-empty).
    /// - `sender`: logical sender identifier (e.g. device or subsystem name).
    /// - `timestamp`: timestamp in milliseconds.
    /// - `payload`: raw payload bytes.
    /// # Returns
    /// - `Ok(Packet)` if validation passes.
    /// - `Err(TelemetryError)` if validation fails.
    /// # Errors
    /// - [`TelemetryError::EmptyEndpoints`] if `endpoints` is empty.
    /// - [`TelemetryError::SizeMismatch`] if the payload size does not match
    ///   the expected size for static element counts, or is not a multiple
    ///   of the element width for dynamic types.
    /// - [`TelemetryError::InvalidUtf8`] if the payload is a string
    ///   type and is not valid UTF-8 .
    pub fn new(
        ty: DataType,
        endpoints: &[DataEndpoint],
        sender: &str,
        timestamp: u64,
        payload: Arc<[u8]>,
    ) -> TelemetryResult<Self> {
        Self::new_with_wire_contract(
            ty,
            endpoints,
            sender,
            timestamp,
            payload,
            None,
            Arc::<[u64]>::from([]),
        )
    }

    /// Resolve the schema element this packet should use for validation,
    /// formatting, and typed payload access.
    ///
    /// Normal locally-built packets use the current runtime schema entry for
    /// `self.ty`. Packets decoded from migration-safe frames may instead carry
    /// `self.wire_shape`, which takes precedence so an in-flight payload can
    /// still be interpreted after runtime schema churn.
    #[inline]
    fn effective_element(&self) -> MessageElement {
        self.wire_shape.unwrap_or(message_meta(self.ty).element)
    }

    /// Return the effective primitive payload kind for this packet.
    ///
    /// This is derived from `effective_element()` so typed decode helpers keep
    /// following any inline wire shape when one is present.
    #[inline]
    fn effective_data_type(&self) -> MessageDataType {
        self.effective_element().data_type()
    }

    /// Return the effective message class for this packet.
    ///
    /// Like `effective_data_type()`, this honors inline wire-shape metadata so
    /// formatting remains consistent for in-flight packets across schema
    /// changes.
    #[inline]
    fn effective_message_class(&self) -> MessageClass {
        self.effective_element().message_type()
    }

    /// Compute a stable 64-bit identifier for this packet.
    ///
    /// This is *not* serialized – it is derived locally from:
    /// - sender
    /// - logical type (DataType)
    /// - endpoints
    /// - timestamp
    /// - payload bytes
    ///
    /// Identical packets on different boards/links will compute the same ID,
    /// so the Router/Relay can use it to drop duplicates.
    #[inline]
    pub fn packet_id(&self) -> u64 {
        // Seed with an arbitrary non-zero constant.
        let mut h: u64 = 0x9E37_79B9_7F4A_7C15;

        // Sender (string)
        h = hash_bytes_u64(h, self.sender.as_bytes());

        // Logical type as string
        h = hash_bytes_u64(h, get_message_name(self.ty).as_bytes());

        // Endpoints (as their string names)
        for ep in self.endpoints.iter() {
            h = hash_bytes_u64(h, ep.as_str().as_bytes());
        }

        // Timestamp + data_size as bytes
        h = hash_bytes_u64(h, &self.timestamp.to_le_bytes());
        h = hash_bytes_u64(h, &self.data_size.to_le_bytes());

        // Payload bytes
        h = hash_bytes_u64(h, self.payload());

        h
    }

    /// Generic helper: decode the payload as a Vec<T> in little-endian,
    /// after checking the runtime data kind and size.
    #[inline]
    fn _as_le_bytes<T>(&self, expected_kind: MessageDataType) -> TelemetryResult<Vec<T>>
    where
        T: LeDecode,
    {
        self.ensure_kind(expected_kind)?;

        let bytes: &[u8] = self.payload();
        let width = T::WIDTH;

        if !bytes.len().is_multiple_of(width) {
            // Packet should already be validated; if not, surface a size error.
            return Err(TelemetryError::SizeMismatch {
                expected: (bytes.len() / width) * width,
                got: bytes.len(),
            });
        }

        let count = bytes.len() / width;
        let mut out = Vec::with_capacity(count);

        for chunk in bytes.chunks_exact(width) {
            out.push(T::from_le(chunk));
        }

        Ok(out)
    }

    /// Validate basic invariants:
    ///
    /// - `endpoints` is non-empty.
    /// - `payload.len() == data_size`.
    /// - For static element count:
    ///   - `data_size == element_count * data_type_size(get_data_type(ty))`.
    /// - For dynamic:
    ///   - Length and encoding are validated by [`validate_dynamic_len_and_content`].
    /// # Returns
    /// - `Ok(())` if validation passes.
    /// - `Err(TelemetryError)` if validation fails.
    pub fn validate(&self) -> TelemetryResult<()> {
        if self.endpoints.is_empty() {
            return Err(TelemetryError::EmptyEndpoints);
        }
        if self.payload.len() != self.data_size {
            return Err(TelemetryError::SizeMismatch {
                expected: self.data_size,
                got: self.payload.len(),
            });
        }

        Self::validate_payload_against_element(self.effective_element(), &self.payload)
    }

    /* ---- Getters ---- */
    /// Get the message data type.
    /// This is the logical schema selector.
    #[inline]
    pub fn data_type(&self) -> DataType {
        self.ty
    }

    /// Get the sender identifier.
    /// This is typically a device or subsystem name.
    #[inline]
    pub fn sender(&self) -> &str {
        &self.sender
    }

    /// Get the destination endpoints for this message.
    #[inline]
    pub fn endpoints(&self) -> &[DataEndpoint] {
        &self.endpoints
    }

    /// Get the timestamp in milliseconds.
    #[inline]
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Get the payload size in bytes.
    #[inline]
    pub fn data_size(&self) -> usize {
        self.data_size
    }

    /// Get the raw payload bytes.
    #[inline]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Return the optional inline wire shape preserved from deserialization.
    ///
    /// This is crate-visible because serialization and routing need to keep the
    /// contract intact when a packet is forwarded again.
    #[inline]
    pub(crate) fn wire_shape(&self) -> Option<MessageElement> {
        self.wire_shape
    }

    /// Return the frozen destination-holder hashes preserved from the wire contract.
    ///
    /// Routers and relays use this list to avoid delivering an in-flight packet
    /// to newly-learned holders that were not part of the original delivery
    /// contract when the packet was serialized.
    #[inline]
    pub(crate) fn wire_target_senders(&self) -> &[u64] {
        &self.wire_target_senders
    }

    /// Header-only string (no decoded data).
    ///
    /// Example:
    /// `Type: FOO, Data Size: 8, Sender: dev0, Endpoints: [EP_A, EP_B], Timestamp: 1234 (1s 234ms)`
    /// # Returns
    /// - Human-readable string with header fields.
    pub fn header_string(&self) -> String {
        let mut out = String::with_capacity(DEFAULT_STRING_CAPACITY);

        let _ = write!(
            &mut out,
            "Type: {}, Data Size: {}, Sender: {}, Endpoints: [",
            get_message_name(self.ty),
            self.data_size,
            self.sender.as_ref(),
        );
        for (i, ep) in self.endpoints.iter().enumerate() {
            if i != 0 {
                out.push_str(", ");
            }
            out.push_str(ep.as_str());
        }
        out.push_str("], Timestamp: ");
        let _ = write!(&mut out, "{}", self.timestamp);

        out.push_str(" (");
        append_human_time(&mut out, self.timestamp);
        out.push(')');
        out
    }

    /// Borrow the payload as UTF-8 without trailing NULs (no allocation).
    ///
    /// Returns `None` if the message `DataType` is not a `String` type or if
    /// the payload is not valid UTF-8 (after trimming trailing NUL).
    /// # Returns
    /// - `Some(&str)` if the payload is a valid UTF-8 string.
    /// - `None` otherwise.
    #[inline]
    pub fn data_as_utf8_ref(&self) -> Option<&str> {
        if self.effective_data_type() != MessageDataType::String {
            return None;
        }
        let bytes = &self.payload;
        let end = bytes.iter().rposition(|&b| b != 0).map(|i| i + 1)?;
        core::str::from_utf8(&bytes[..end]).ok()
    }

    /// Helper: append decoded numeric/float elements to `s`.
    ///
    /// - Uses `LeBytes::from_le_slice` with fixed-width chunks.
    /// - Floats (`f32`/`f64`) are formatted with a fixed precision
    ///   [`STRING_PRECISION`].
    #[inline]
    fn data_to_string<T>(&self, s: &mut String)
    where
        T: LeBytes + core::fmt::Display + 'static,
    {
        let it = self.payload.chunks_exact(T::WIDTH);
        let mut first = true;

        for chunk in it {
            if !first {
                s.push_str(", ");
            }
            first = false;

            let v = T::from_le_slice(chunk);

            // If this is a float type, use precision; otherwise, default formatting.
            if TypeId::of::<T>() == TypeId::of::<f32>() || TypeId::of::<T>() == TypeId::of::<f64>()
            {
                // `{:.*}` = "use this precision argument"
                let _ = write!(s, "{:.*}", STRING_PRECISION, v);
            } else {
                let _ = write!(s, "{v}");
            }
        }
    }

    /// Full pretty string including decoded data portion.
    ///
    /// - String payloads are rendered as `"..."`
    /// - Numeric/bool payloads are rendered as comma-separated values
    /// - Hex payloads are delegated to [`Packet::to_hex_string`]
    /// # Returns
    /// - Human-readable string with header and decoded data.
    pub fn as_string(&self) -> String {
        let mut s = String::from("{");
        s.push_str(&self.header_string());

        if self.payload.is_empty() {
            s.push_str(", Data: (<NoData>)}");
            return s;
        }

        match self.effective_message_class() {
            MessageClass::Data => {
                s.push_str(", Data: (");
            }
            MessageClass::Error => {
                s.push_str(", Error: (");
            }
            MessageClass::Warning => {
                s.push_str(", Warning: (");
            }
        }

        if let Some(msg) = self.data_as_utf8_ref() {
            s.push('"');
            s.push_str(msg);
            s.push_str("\")}");
            return s;
        }

        match self.effective_data_type() {
            MessageDataType::Float64 => {
                self.data_to_string::<f64>(&mut s);
            }
            MessageDataType::Float32 => {
                self.data_to_string::<f32>(&mut s);
            }
            MessageDataType::UInt128 => {
                self.data_to_string::<u128>(&mut s);
            }
            MessageDataType::UInt64 => {
                self.data_to_string::<u64>(&mut s);
            }
            MessageDataType::UInt32 => {
                self.data_to_string::<u32>(&mut s);
            }
            MessageDataType::UInt16 => {
                self.data_to_string::<u16>(&mut s);
            }
            MessageDataType::UInt8 => {
                self.data_to_string::<u8>(&mut s);
            }
            MessageDataType::Int128 => {
                self.data_to_string::<i128>(&mut s);
            }
            MessageDataType::Int64 => {
                self.data_to_string::<i64>(&mut s);
            }
            MessageDataType::Int32 => {
                self.data_to_string::<i32>(&mut s);
            }
            MessageDataType::Int16 => {
                self.data_to_string::<i16>(&mut s);
            }
            MessageDataType::Int8 => {
                self.data_to_string::<i8>(&mut s);
            }
            MessageDataType::Bool => {
                // Interpret any nonzero as true.
                let mut it = self.payload.iter().peekable();
                while let Some(b) = it.next() {
                    let _ = write!(s, "{}", *b != 0);
                    if it.peek().is_some() {
                        s.push_str(", ");
                    }
                }
            }
            MessageDataType::String => {
                // Already handled above via `data_as_utf8_ref`.
            }
            MessageDataType::Binary => return self.to_hex_string(),
            MessageDataType::NoData => {
                s.push_str("<no data>");
            }
        }

        s.push_str(")}");
        s
    }

    /// Hex dump variant of [`Packet::as_string`].
    ///
    /// Produces:
    ///
    /// `Type: ..., Data Size: ..., ..., Timestamp: ... (...), Data (hex): 0xNN 0xNN ...`
    /// # Returns
    /// - Human-readable string with header and hex-formatted data.
    pub fn to_hex_string(&self) -> String {
        // Header first.
        let mut s = self.header_string();
        s.push_str(", Data (hex):");

        if !self.payload.is_empty() {
            // Reserve roughly 5 chars per byte: " 0xNN".
            s.reserve(self.payload.len().saturating_mul(5));
            for &b in self.payload.iter() {
                let _ = write!(&mut s, " 0x{:02x}", b);
            }
        }
        s
    }

    // =========================================================================
    // Typed data accessors
    // =========================================================================

    /// Ensure this packet's element type matches `expected`.
    #[inline]
    fn ensure_kind(&self, expected: MessageDataType) -> TelemetryResult<()> {
        let dt = self.effective_data_type();
        if dt != expected {
            return Err(TelemetryError::TypeMismatch {
                expected: data_type_size(expected),
                got: data_type_size(dt),
            });
        }
        Ok(())
    }

    impl_data_as_prim! {
        data_as_f32,   f32,   MessageDataType::Float32;
        data_as_f64,   f64,   MessageDataType::Float64;

        data_as_u8,    u8,    MessageDataType::UInt8;
        data_as_u16,   u16,   MessageDataType::UInt16;
        data_as_u32,   u32,   MessageDataType::UInt32;
        data_as_u64,   u64,   MessageDataType::UInt64;
        data_as_u128,  u128,  MessageDataType::UInt128;

        data_as_i8,    i8,    MessageDataType::Int8;
        data_as_i16,   i16,   MessageDataType::Int16;
        data_as_i32,   i32,   MessageDataType::Int32;
        data_as_i64,   i64,   MessageDataType::Int64;
        data_as_i128,  i128,  MessageDataType::Int128;
    }

    /// Decode payload as `bool`s. Any non-zero byte is treated as `true`.
    #[inline]
    pub fn data_as_bool(&self) -> TelemetryResult<Vec<bool>> {
        self.ensure_kind(MessageDataType::Bool)?;
        Ok(self.payload.iter().map(|&b| b != 0).collect())
    }

    /// Decode payload as a string (for String type).
    #[inline]
    pub fn data_as_string(&self) -> TelemetryResult<String> {
        self.ensure_kind(MessageDataType::String)?;

        let bytes = &self.payload;
        let end = bytes
            .iter()
            .rposition(|&b| b != 0)
            .map(|i| i + 1)
            .unwrap_or(0);

        if end == 0 {
            return Ok(String::new());
        }

        let s = core::str::from_utf8(&bytes[..end])
            .map_err(|_| TelemetryError::InvalidUtf8)?
            .to_string();
        Ok(s)
    }

    /// Decode payload as raw bytes (for Binary type).
    #[inline]
    pub fn data_as_binary(&self) -> TelemetryResult<Vec<u8>> {
        self.ensure_kind(MessageDataType::Binary)?;
        Ok(self.payload.to_vec())
    }

    /// Internal helper: build a packet from a slice of primitive values
    /// encoded as little-endian, using the given sender.
    ///
    /// Works for all numeric types (integer and float) as long as the schema's
    /// element width matches `T`'s width. Not used for String/Binary/Bool.
    fn from_prim_le_slice_with_sender<T>(
        ty: DataType,
        values: &[T],
        endpoints: &[DataEndpoint],
        sender: &str,
        timestamp: u64,
    ) -> TelemetryResult<Self>
    where
        T: Copy + 'static,
    {
        let dt = get_data_type(ty);

        // Only allow numeric-ish types here; String/Binary/Bool are handled elsewhere.

        if dt == MessageDataType::Bool
            || dt == MessageDataType::String
            || dt == MessageDataType::Binary
        {
            // For these, use dedicated constructors (bool / string / binary).
            return Err(TelemetryError::BadArg);
        }
        let element_size = data_type_size(dt);

        // Ensure T's width matches what the schema expects.
        if element_size != size_of::<T>() {
            return Err(TelemetryError::TypeMismatch {
                expected: element_size,
                got: size_of::<T>(),
            });
        }

        let total_bytes = values.len() * element_size;

        // If the schema has a static element count, enforce it up front.
        if let MessageElement::Static(exact, _, _) = message_meta(ty).element {
            let exact_bytes = exact * element_size;
            if total_bytes != exact_bytes {
                return Err(TelemetryError::SizeMismatch {
                    expected: exact_bytes,
                    got: total_bytes,
                });
            }
        }

        let mut bytes = vec![0u8; total_bytes];
        unsafe { bytes.set_len(total_bytes) };

        for (i, v) in values.iter().copied().enumerate() {
            let offset = i * element_size;
            let dst = &mut bytes[offset..offset + element_size];

            // Copy the raw memory of `v` into the buffer.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    &v as *const T as *const u8,
                    dst.as_mut_ptr(),
                    element_size,
                );
            }

            // Normalize to little-endian on big-endian targets.
            // On little-endian this block is compiled out.
            #[cfg(target_endian = "big")]
            {
                dst.reverse();
            }
        }

        Self::new(ty, endpoints, sender, timestamp, Arc::<[u8]>::from(bytes))
    }

    /// Same as `from_prim_le_slice_with_sender` but uses `DEVICE_IDENTIFIER`
    /// as the sender (mirrors `from_u8_slice`, `from_f32_slice`).
    #[inline]
    pub fn from_prim_le_slice<T>(
        ty: DataType,
        values: &[T],
        endpoints: &[DataEndpoint],
        timestamp: u64,
    ) -> TelemetryResult<Self>
    where
        T: Copy + 'static,
    {
        Self::from_prim_le_slice_with_sender(ty, values, endpoints, DEVICE_IDENTIFIER, timestamp)
    }

    // -------------------------------------------------------------------------
    // Convenience wrappers for all numeric types
    // -------------------------------------------------------------------------
    impl_from_prim_slices! {
        from_u8_slice,   u8;
        from_u16_slice,  u16;
        from_i8_slice,   i8;
        from_i16_slice,  i16;
        from_u32_slice,  u32;
        from_i32_slice,  i32;
        from_u64_slice,  u64;
        from_i64_slice,  i64;
        from_u128_slice, u128;
        from_i128_slice, i128;
        from_f32_slice,  f32;
        from_f64_slice,  f64;
    }

    /// Builds a packet with an empty payload for types whose schema allows zero bytes.
    #[inline]
    pub fn from_no_data(
        ty: DataType,
        endpoints: &[DataEndpoint],
        timestamp: u64,
    ) -> TelemetryResult<Self> {
        let meta = message_meta(ty);
        match meta.element {
            MessageElement::Static(need, _, _) => {
                if need != 0 {
                    return Err(TelemetryError::SizeMismatch {
                        expected: need,
                        got: 0,
                    });
                }
            }
            MessageElement::Dynamic(_, _) => {
                // Dynamic with zero-length payload is OK.
            }
        }

        Self::new(
            ty,
            endpoints,
            DEVICE_IDENTIFIER,
            timestamp,
            Arc::<[u8]>::from([]),
        )
    }

    /// Bool constructor: encodes each bool as a single byte (0 / 1).
    #[inline]
    pub fn from_bool_slice(
        ty: DataType,
        values: &[bool],
        endpoints: &[DataEndpoint],
        timestamp: u64,
    ) -> TelemetryResult<Self> {
        if get_data_type(ty) != MessageDataType::Bool {
            return Err(TelemetryError::TypeMismatch {
                expected: data_type_size(get_data_type(ty)),
                got: size_of::<bool>(),
            });
        }

        let total_bytes = values.len();
        if let MessageElement::Static(exact, _, _) = message_meta(ty).element
            && total_bytes != exact
        {
            return Err(TelemetryError::SizeMismatch {
                expected: exact,
                got: total_bytes,
            });
        }

        let mut bytes = Vec::with_capacity(total_bytes);
        bytes.extend(values.iter().map(|b| if *b { 1u8 } else { 0u8 }));

        Self::new(
            ty,
            endpoints,
            DEVICE_IDENTIFIER,
            timestamp,
            Arc::<[u8]>::from(bytes),
        )
    }

    /// String constructor (dynamic length). Trailing NULs are not added;
    /// `new()` + `validate_dynamic_len_and_content` will do UTF-8 validation.
    #[inline]
    pub fn from_str_slice(
        ty: DataType,
        s: &str,
        endpoints: &[DataEndpoint],
        timestamp: u64,
    ) -> TelemetryResult<Self> {
        if get_data_type(ty) != MessageDataType::String {
            return Err(TelemetryError::TypeMismatch {
                expected: data_type_size(get_data_type(ty)),
                got: 1,
            });
        }

        let bytes: Arc<[u8]> = Arc::from(s.as_bytes());
        Self::new(ty, endpoints, DEVICE_IDENTIFIER, timestamp, bytes)
    }
}

// ============================================================================
// Time formatting (no_std-friendly, UTC or uptime)
// ============================================================================

#[inline]
fn div_mod_u64(n: u64, d: u64) -> (u64, u64) {
    (n / d, n % d)
}

// Howard Hinnant–style civil-from-days (proleptic Gregorian).
fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    // epoch (1970-01-01) has days=0
    z += 719_468; // shift to civil base
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = (yoe as i32) + era as i32 * 400;
    let doy = (doe - (365 * yoe + yoe / 4 - yoe / 100)) as i32; // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
    let y = y + (m <= 2) as i32; // year
    (y, m as u32, d as u32)
}

/// Append a human-readable timestamp to `out`, either uptime (`hh:mm:ss.mmm`)
/// or UTC epoch like `YYYY-MM-DD HH:MM:SS.mmmZ`, depending on threshold.
fn append_human_time(out: &mut String, total_ms: u64) {
    if total_ms >= EPOCH_MS_THRESHOLD {
        // Unix epoch path.
        let (secs, sub_ms) = div_mod_u64(total_ms, 1_000);
        let days = (secs / 86_400) as i64;
        let sod = (secs % 86_400) as u32; // seconds of day
        let (year, month, day) = civil_from_days(days);
        let hour = sod / 3_600;
        let min = (sod % 3_600) / 60;
        let sec = sod % 60;
        let _ = Write::write_fmt(
            out,
            format_args!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}Z",
                year, month, day, hour, min, sec, sub_ms as u32
            ),
        );
    } else {
        // Uptime-style duration.
        let hours = total_ms / 3_600_000;
        let minutes = (total_ms % 3_600_000) / 60_000;
        let seconds = (total_ms % 60_000) / 1_000;
        let milliseconds = total_ms % 1_000;
        if hours > 0 {
            let _ = Write::write_fmt(
                out,
                format_args!("{hours}h {minutes:02}m {seconds:02}s {milliseconds:03}ms"),
            );
        } else if minutes > 0 {
            let _ = Write::write_fmt(
                out,
                format_args!("{minutes}m {seconds:02}s {milliseconds:03}ms"),
            );
        } else {
            let _ = Write::write_fmt(out, format_args!("{seconds}s {milliseconds:03}ms"));
        }
    }
}

// ============================================================================
// Display impl
// ============================================================================

impl core::fmt::Display for Packet {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.write_str(&Packet::as_string(self))
    }
}

impl ByteCost for Packet {
    #[inline]
    fn byte_cost(&self) -> usize {
        size_of::<Self>()
            + self.sender.len()
            + self.endpoints.len() * size_of::<DataEndpoint>()
            + self.payload.byte_cost()
    }
}
