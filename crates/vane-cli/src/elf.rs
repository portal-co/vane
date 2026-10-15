//! Minimal ELF64 little-endian loader for RISC-V binaries.
//!
//! Loads `PT_LOAD` segments at their physical address (`p_paddr`, falling
//! back to `p_vaddr`) and returns the entry point. This intentionally covers
//! only what vane needs today (statically linked rv64 test payloads); it is
//! not a general-purpose ELF crate.

use vane::Reactor;

/// Errors returned by [`load`].
#[derive(Debug)]
pub enum ElfError {
    /// File is too small to contain an ELF64 header.
    Truncated,
    /// Bad magic, wrong class (need ELF64), or wrong endianness (need LE).
    NotElf64Le,
    /// Not a RISC-V binary (`e_machine != EM_RISCV`).
    WrongMachine,
    /// A program header or segment extends past the end of the file.
    BadSegment,
}

impl core::fmt::Display for ElfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ElfError::Truncated => write!(f, "file too small for an ELF64 header"),
            ElfError::NotElf64Le => write!(f, "not a little-endian ELF64 file"),
            ElfError::WrongMachine => write!(f, "not a RISC-V (EM_RISCV) binary"),
            ElfError::BadSegment => write!(f, "program header/segment extends past end of file"),
        }
    }
}

impl std::error::Error for ElfError {}

const EM_RISCV: u16 = 243;
const PT_LOAD: u32 = 1;

fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn u64le(b: &[u8], off: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(w)
}

/// Load all `PT_LOAD` segments of `bytes` into `reactor`'s guest memory and
/// return the ELF entry point.
pub fn load(reactor: &Reactor, bytes: &[u8]) -> Result<u64, ElfError> {
    if bytes.len() < 64 {
        return Err(ElfError::Truncated);
    }
    if bytes[0..4] != [0x7f, b'E', b'L', b'F'] || bytes[4] != 2 || bytes[5] != 1 {
        return Err(ElfError::NotElf64Le);
    }
    if u16le(bytes, 18) != EM_RISCV {
        return Err(ElfError::WrongMachine);
    }
    let entry = u64le(bytes, 24);
    let phoff = u64le(bytes, 32) as usize;
    let phentsize = u16le(bytes, 54) as usize;
    let phnum = u16le(bytes, 56) as usize;
    if phentsize < 56 {
        return Err(ElfError::BadSegment);
    }
    for i in 0..phnum {
        let ph = phoff
            .checked_add(i * phentsize)
            .ok_or(ElfError::BadSegment)?;
        if ph + 56 > bytes.len() {
            return Err(ElfError::BadSegment);
        }
        if u32le(bytes, ph) != PT_LOAD {
            continue;
        }
        let p_offset = u64le(bytes, ph + 8) as usize;
        let p_vaddr = u64le(bytes, ph + 16);
        let p_paddr = u64le(bytes, ph + 24);
        let p_filesz = u64le(bytes, ph + 32) as usize;
        let p_memsz = u64le(bytes, ph + 40) as usize;
        let dest = if p_paddr != 0 { p_paddr } else { p_vaddr };
        let end = p_offset.checked_add(p_filesz).ok_or(ElfError::BadSegment)?;
        if end > bytes.len() {
            return Err(ElfError::BadSegment);
        }
        reactor.write_bytes(dest, &bytes[p_offset..end]);
        // Zero the BSS tail (memsz beyond filesz). Pages start zeroed, so
        // only explicit zeroing across a previously written page matters;
        // write it unconditionally for correctness on reload.
        if p_memsz > p_filesz {
            reactor.write_bytes(
                dest.wrapping_add(p_filesz as u64),
                &vec![0u8; p_memsz - p_filesz],
            );
        }
    }
    Ok(entry)
}
