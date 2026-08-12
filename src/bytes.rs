//! Little-endian field access.
//!
//! Every ext2/3/4 on-disk integer is little-endian regardless of host. Fields
//! are read and written one at a time through these helpers rather than by
//! casting a `repr(C)` struct over a buffer — that keeps the code free of
//! `unsafe`, correct on big-endian hosts, and honest about padding.

/// Read a `u8` at `off`.
#[inline]
pub(crate) fn get_u8(buf: &[u8], off: usize) -> u8 {
    buf[off]
}

/// Read a little-endian `u16` at `off`.
#[inline]
pub(crate) fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

/// Read a little-endian `u32` at `off`.
#[inline]
pub(crate) fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Read a little-endian `u64` at `off`.
#[inline]
pub(crate) fn get_u64(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

/// Write a `u8` at `off`.
#[inline]
pub(crate) fn put_u8(buf: &mut [u8], off: usize, v: u8) {
    buf[off] = v;
}

/// Write a little-endian `u16` at `off`.
#[inline]
pub(crate) fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

/// Write a little-endian `u32` at `off`.
#[inline]
pub(crate) fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Write a little-endian `u64` at `off`.
#[inline]
pub(crate) fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Copy a fixed-width byte field, zero-padding or truncating as needed.
///
/// ext4 string fields are not NUL-terminated; a name that exactly fills the
/// field has no terminator, which is why this truncates rather than reserving a
/// byte.
#[inline]
pub(crate) fn put_bytes(buf: &mut [u8], off: usize, len: usize, src: &[u8]) {
    let n = src.len().min(len);
    buf[off..off + n].copy_from_slice(&src[..n]);
    for b in &mut buf[off + n..off + len] {
        *b = 0;
    }
}

/// Read a fixed-width byte field into an array.
#[inline]
pub(crate) fn get_array<const N: usize>(buf: &[u8], off: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&buf[off..off + N]);
    out
}

/// Interpret a fixed-width, optionally NUL-padded field as a string.
#[inline]
pub(crate) fn field_to_string(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}
