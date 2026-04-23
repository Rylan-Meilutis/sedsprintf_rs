//! Helper traits and macros for enum representations, little-endian numeric
//! serialization, and typed logging.
//!
//! This module is primarily used by the FFI layers (`c_api`, `py_api`) and the
//! core router/telemetry code to:
//! - Enforce that certain enums are `#[repr(u32)]` / `#[repr(i32)]` at
//!   compile time (`ReprU32Enum`, `ReprI32Enum`).
//! - Provide a uniform little-endian serialization interface (`LeBytes` +
//!   `impl_letype_num!`).
//! - Implement a typed logging helper macro (`do_vec_log_typed!`) that
//!   reinterprets raw C buffers into typed vectors and routes them through
//!   the Rust `Router`.

// ============================================================================
//  ReprU32Enum / ReprI32Enum: enum layout helpers
// ============================================================================

/// The `ReprU32Enum` trait marks enums that are represented as `u32` in
/// serialized / FFI form.
///
/// Types implementing this trait are expected to be:
/// - `#[repr(u32)]`
/// - fieldless enums (C-like)
/// - `Copy`
///
/// The `MAX` associated constant is the maximum valid discriminant, used by
/// helpers like `try_enum_from_u32`.
pub trait ReprU32Enum: Copy + Sized {
    /// Maximum valid numeric value for this enum (inclusive).
    const MAX: u32;
    /// Convert a valid raw value to this type.
    fn from_u32(x: u32) -> Option<Self>;
}

/// Implement [`ReprU32Enum`] for a concrete `#[repr(u32)]` enum and perform
/// a compile-time size check to ensure it really is the same size as `u32`.
///
/// # Example
///
/// ```text
/// #[repr(u32)]
/// enum Foo {
///     A = 0,
///     B = 1,
/// }
///
/// impl_repr_u32_enum!(Foo, 1);
/// ```
#[macro_export]
macro_rules! impl_repr_u32_enum {
    ($ty:ty, $max:expr) => {
        // Compile-time size check for this concrete type.
        const _: [(); size_of::<$ty>()] = [(); size_of::<u32>()];

        impl $crate::macros::ReprU32Enum for $ty {
            const MAX: u32 = $max;

            #[inline]
            fn from_u32(x: u32) -> Option<Self> {
                // SAFETY: `E` is promised to be a fieldless #[repr(u32)] enum (thus 4 bytes, Copy).
                Some(unsafe { (&x as *const u32 as *const Self).read() })
            }
        }
    };
}

/// The `ReprI32Enum` trait marks enums that are represented as `i32` in
/// serialized / FFI form.
///
/// Types implementing this trait are expected to be:
/// - `#[repr(i32)]`
/// - fieldless enums (C-like)
/// - `Copy`
///
/// `MIN` / `MAX` define the inclusive numeric range of valid values.
pub trait ReprI32Enum: Copy + Sized {
    /// Maximum valid numeric value for this enum (inclusive).
    const MAX: i32;
    /// Minimum valid numeric value for this enum (inclusive).
    const MIN: i32;
}

/// Implement [`ReprI32Enum`] for a concrete `#[repr(i32)]` enum and perform
/// a compile-time size check to ensure it really is the same size as `i32`.
///
/// # Example
///
/// ```text
/// #[repr(i32)]
/// enum ErrCode {
///     Foo = -1,
///     Bar = -2,
/// }
///
/// impl_repr_i32_enum!(ErrCode, ErrCode::Foo as i32, ErrCode::Bar as i32);
/// ```
#[macro_export]
macro_rules! impl_repr_i32_enum {
    ($ty:ty, $max:expr, $min:expr) => {
        // Compile-time size check for this concrete type.
        const _: [(); size_of::<$ty>()] = [(); size_of::<i32>()];

        impl $crate::macros::ReprI32Enum for $ty {
            const MAX: i32 = $max;
            const MIN: i32 = $min;
        }
    };
}

// ============================================================================
//  LeBytes helper macro (numeric → little-endian bytes)
// ============================================================================

/// Implement the [`LeBytes`] trait for a numeric type with a fixed byte width.
///
/// This is used to unify little-endian serialization for primitive types
/// like `u16`, `u32`, `f32`, etc.
///
/// `$t`: concrete numeric type (e.g. `u16`, `i32`, `f32`)
/// `$w`: width in bytes (`size_of::<$t>()`)
///
/// # Example
///
/// ```text
/// impl_letype_num!(u32, 4);
/// impl_letype_num!(f32, 4);
/// ```
#[macro_export]
macro_rules! impl_letype_num {
    ($t:ty, $w:expr) => {
        impl $crate::router::LeBytes for $t {
            const WIDTH: usize = $w;

            #[inline]
            fn write_le(self, out: &mut [u8]) {
                assert_eq!(out.len(), Self::WIDTH, "write_le: wrong out slice len");
                out.copy_from_slice(&self.to_le_bytes());
            }

            #[inline]
            fn from_le_slice(bytes: &[u8]) -> Self {
                assert_eq!(bytes.len(), Self::WIDTH, "from_le_slice: wrong slice len");
                let arr: [u8; $w] = bytes.try_into().expect("slice length mismatch");
                <$t>::from_le_bytes(arr)
            }
        }
    };
}

// ============================================================================
//  do_vec_log_typed: C-FFI → typed Vec<T> logger helper
// ============================================================================

/// Helper macro used by the C FFI (`c_api`) to:
///
/// 1. Reinterpret a raw pointer + count as a sequence of elements of type
///    `$elem_ty` using unaligned little-endian reads.
/// 2. Log those elements through a `Router` using the unified
///    `call_log_or_queue` helper.
/// 3. Map any vectorization failure into a `TelemetryError::Io` and return a
///    C-style status code.
///
/// This keeps the FFI functions small and consistent.
///
/// **Parameters**
///
/// - `$router_ptr`: `*mut SedsRouter`
/// - `$ty`: `DataType`
/// - `$ts_opt`: `Option<u64>` timestamp
/// - `$queue`: `bool` (true = queue, false = immediate log)
/// - `$data_ptr`: `*const c_void` from C
/// - `$count`: `usize` number of elements
/// - `$elem_ty`: concrete Rust element type (e.g. `u16`, `i32`, `f32`)
#[macro_export]
macro_rules! do_vec_log_typed {
    (
        $router_ptr:expr,   // *mut SedsRouter
        $ty:expr,           // DataType
        $ts_opt:expr,       // Option<u64>
        $queue:expr,        // bool
        $data_ptr:expr,     // *const c_void
        $count:expr,        // usize
        $elem_ty:ty         // concrete element type, e.g. u16 / f32
    ) => {{
        use alloc::vec::Vec;
        use core::mem;

        let mut tmp: Vec<$elem_ty> = Vec::with_capacity($count);
        let base = $data_ptr as *const u8;

        match $crate::c_api::vectorize_data::<$elem_ty>(
            base,
            $count,
            mem::size_of::<$elem_ty>(),
            &mut tmp,
        ) {
            Ok(()) => $crate::c_api::ok_or_status($crate::c_api::call_log_or_queue::<$elem_ty>(
                $router_ptr,
                $ty,
                $ts_opt,
                &tmp,
                $queue,
            )),
            Err(_e) => {
                $crate::c_api::status_from_err($crate::TelemetryError::Io("vectorize_data failed"))
            }
        }
    }};
}

/// Helper macros for implementing `Packet` constructors and
/// accessors for primitive slices.
#[macro_export]
macro_rules! impl_from_prim_slices {
    ($($fn_name:ident, $elem_ty:ty);+ $(;)?) => {
        $(
            #[doc = concat!("Build a packet from a slice of little-endian `", stringify!($elem_ty), "` values.")]
            #[inline]
            pub fn $fn_name(
                ty: DataType,
                values: &[$elem_ty],
                endpoints: &[DataEndpoint],
                timestamp: u64,
            ) -> TelemetryResult<Self> {
                Self::from_prim_le_slice(ty, values, endpoints, timestamp)
            }
        )+
    };
}
/// Helper macros for implementing `Packet` data accessors for
/// primitive slices.
#[macro_export]
macro_rules! impl_data_as_prim {
    ($($name:ident, $ty:ty, $variant:expr);+ $(;)?) => {
        $(
            #[doc = concat!("Decode payload as little-endian `", stringify!($ty), "` values.")]
            #[inline]
            pub fn $name(&self) -> TelemetryResult<Vec<$ty>> {
                self._as_le_bytes::<$ty>($variant)
            }
        )+
    };
}

/// Implement the [`LeDecode`] trait for a numeric type with fixed size.
///
/// This is used to unify little-endian deserialization for primitive types
/// like `u16`, `u32`, `f32`, etc.
///
/// `$ty`: concrete numeric type (e.g. `u16`, `i32`, `f32`)
///# Example
/// ```text
/// impl_ledecode_auto!(u32);
/// impl_ledecode_auto!(f32);
/// ```
#[macro_export]
macro_rules! impl_ledecode_auto {
    ($ty:ty) => {
        impl LeDecode for $ty {
            const WIDTH: usize = core::mem::size_of::<$ty>();
            #[inline]
            fn from_le(slice: &[u8]) -> Self {
                let arr: [u8; core::mem::size_of::<$ty>()] =
                    slice.try_into().expect("slice length mismatch");
                <$ty>::from_le_bytes(arr)
            }
        }
    };
}
