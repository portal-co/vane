//! Snapshot serialization (PLAN_VIRTIO_WEVAL workstream W1.5).
//!
//! A versioned, checksummed binary encoding for machine snapshots so a
//! snapshot becomes a standalone artifact (feed to Weval, store, transfer).
//! Zero pages are omitted (the snapshot already stores only non-zero pages);
//! swap-backed RAM is materialized at capture (see `Reactor::snapshot`); the
//! lineage (`id`/`parent`) round-trips.
//!
//! Layout: `magic | version | payload | checksum` where the checksum is
//! FNV-1a over `magic|version|payload`. The payload is little-endian fields;
//! lengths/counts are u32.

/// Serialization/deserialization errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SerError {
    /// Ran out of bytes while decoding.
    Truncated,
    /// Bad magic or unsupported version.
    BadHeader,
    /// Checksum mismatch (corruption).
    Checksum,
    /// A field was structurally invalid.
    Invalid(&'static str),
}

use alloc::vec::Vec;

const MAGIC: &[u8; 4] = b"VSNP";
const VERSION: u32 = 1;

/// A little-endian writer.
#[derive(Default)]
pub struct Writer {
    pub b: alloc::vec::Vec<u8>,
}
impl Writer {
    pub fn new() -> Self {
        Writer { b: Vec::new() }
    }
    pub fn u8(&mut self, v: u8) {
        self.b.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }
    pub fn u16(&mut self, v: u16) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bytes(&mut self, d: &[u8]) {
        self.b.extend_from_slice(d);
    }
    /// A length-prefixed byte string (u32 len + bytes).
    pub fn blob(&mut self, d: &[u8]) {
        self.u32(d.len() as u32);
        self.bytes(d);
    }
    /// A length-prefixed UTF-8 string.
    pub fn str(&mut self, s: &str) {
        self.blob(s.as_bytes());
    }
}

/// A little-endian reader over a byte slice.
pub struct Reader<'a> {
    pub b: &'a [u8],
    pub pos: usize,
}
impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }
    pub fn u8(&mut self) -> Result<u8, SerError> {
        let v = *self.b.get(self.pos).ok_or(SerError::Truncated)?;
        self.pos += 1;
        Ok(v)
    }
    pub fn bool(&mut self) -> Result<bool, SerError> {
        Ok(self.u8()? != 0)
    }
    pub fn u16(&mut self) -> Result<u16, SerError> {
        Ok(u16::from_le_bytes([self.u8()?, self.u8()?]))
    }
    pub fn u32(&mut self) -> Result<u32, SerError> {
        Ok(u32::from_le_bytes([
            self.u8()?,
            self.u8()?,
            self.u8()?,
            self.u8()?,
        ]))
    }
    pub fn u64(&mut self) -> Result<u64, SerError> {
        let lo = self.u32()? as u64;
        let hi = self.u32()? as u64;
        Ok(lo | (hi << 32))
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], SerError> {
        let end = self.pos.checked_add(n).ok_or(SerError::Truncated)?;
        let s = self.b.get(self.pos..end).ok_or(SerError::Truncated)?;
        self.pos = end;
        Ok(s)
    }
    pub fn blob(&mut self) -> Result<&'a [u8], SerError> {
        let n = self.u32()? as usize;
        self.bytes(n)
    }
    pub fn str(&mut self) -> Result<alloc::string::String, SerError> {
        let b = self.blob()?;
        Ok(alloc::string::String::from_utf8_lossy(b).into_owned())
    }
}

/// FNV-1a 64-bit over `data`.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Wrap a serialized payload in the versioned, checksummed envelope.
pub fn seal(payload: &[u8]) -> alloc::vec::Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(payload);
    let sum = fnv1a(&out);
    out.extend_from_slice(&sum.to_le_bytes());
    out
}

/// Unwrap the envelope, verifying magic, version, and checksum. Returns the
/// payload.
pub fn open(data: &[u8]) -> Result<&[u8], SerError> {
    if data.len() < 4 + 4 + 8 {
        return Err(SerError::Truncated);
    }
    if &data[..4] != MAGIC {
        return Err(SerError::BadHeader);
    }
    let version = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    if version != VERSION {
        return Err(SerError::BadHeader);
    }
    let (body, sum_bytes) = data.split_at(data.len() - 8);
    let sum = u64::from_le_bytes(sum_bytes.try_into().map_err(|_| SerError::Truncated)?);
    if fnv1a(body) != sum {
        return Err(SerError::Checksum);
    }
    Ok(&body[8..])
}
