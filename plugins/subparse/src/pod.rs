// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: MIT

//! Plain-old-data casts for the wire records. The one place this crate reads a record out of a
//! byte slice without copying it field by field.

#![allow(unsafe_code)]

use std::mem::{align_of, size_of};

#[cfg(target_endian = "big")]
compile_error!(
    "the subtitle wire layouts are little endian, a big-endian host needs a swapping reader"
);

/// A `#[repr(C)]` record with no implicit padding whose every bit pattern is a valid value.
///
/// # Safety
/// Implementors are `#[repr(C)]`, contain only integers, floats, byte arrays and other `Pod`
/// types, declare every padding byte as a field, and are valid for any bit pattern.
pub unsafe trait Pod: Copy + 'static {}

unsafe impl Pod for u8 {}
unsafe impl Pod for u16 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
unsafe impl Pod for f32 {}

/// `bytes` as a slice of `T`, `None` unless the start is aligned and the length a multiple of
/// the record size.
#[inline]
pub fn cast_slice<T: Pod>(bytes: &[u8]) -> Option<&[T]> {
    let size = size_of::<T>();
    if size == 0
        || !bytes.len().is_multiple_of(size)
        || !bytes.as_ptr().addr().is_multiple_of(align_of::<T>())
    {
        return None;
    }
    // SAFETY: the pointer is aligned for `T` and the length covers whole records. `T: Pod`
    // promises every bit pattern is a valid `T`, so initialised bytes form valid records. The
    // returned slice borrows `bytes` for as long as it lives.
    Some(unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<T>(), bytes.len() / size) })
}

/// The first record of `bytes` as a `T`, `None` unless aligned and long enough.
#[inline]
pub fn cast_ref<T: Pod>(bytes: &[u8]) -> Option<&T> {
    let head = bytes.get(..size_of::<T>())?;
    cast_slice::<T>(head)?.first()
}

/// The bytes of one record, in memory order.
#[inline]
pub fn as_bytes<T: Pod>(v: &T) -> &[u8] {
    // SAFETY: `T: Pod` has no padding, so every byte of the value is initialised. The slice
    // borrows `v`.
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

/// The bytes of a run of records, in memory order.
#[inline]
pub fn slice_as_bytes<T: Pod>(v: &[T]) -> &[u8] {
    // SAFETY: as `as_bytes`, over `v.len()` consecutive records.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), std::mem::size_of_val(v)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_misaligned_or_ragged_slice_is_refused() {
        let buf = [0u8; 16];
        let at = buf.as_ptr().addr();
        let (aligned, odd) =
            if at.is_multiple_of(4) { (&buf[..], &buf[1..]) } else { (&buf[3..], &buf[..]) };
        assert!(cast_slice::<u32>(odd).is_none());
        assert!(cast_slice::<u32>(&aligned[..aligned.len() / 4 * 4]).is_some());
        assert!(cast_slice::<u32>(&aligned[..5]).is_none());
        assert!(cast_ref::<u32>(&aligned[..3]).is_none());
    }

    #[test]
    fn bytes_round_trip() {
        let v = [1u32, 2, 3];
        let b = slice_as_bytes(&v);
        assert_eq!(b.len(), 12);
        assert_eq!(cast_slice::<u32>(b), Some(&v[..]));
        assert_eq!(as_bytes(&7u16), &[7, 0]);
    }
}
