//! File-backed swap store: a flat file of `slot_count × RECORD_BYTES`,
//! record `i` at offset `i * RECORD_BYTES`. Works on native and on
//! `wasm32-wasip1` (a preopened file); both expose `std::fs::File`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::{SwapError, SwapStore, RECORD_BYTES};

pub struct FileSwap {
    file: File,
    slots: u64,
}

impl FileSwap {
    /// Open (creating if needed) a swap file holding `slots` page records.
    /// The file is sized to `slots * RECORD_BYTES` (sparse where the host
    /// filesystem allows). Existing content past that size is truncated.
    pub fn create(path: &Path, slots: u64) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        file.set_len(slots * RECORD_BYTES as u64)?;
        Ok(FileSwap { file, slots })
    }

    fn offset(slot: u64) -> u64 {
        slot * RECORD_BYTES as u64
    }
}

impl SwapStore for FileSwap {
    fn read_record(&mut self, slot: u64, buf: &mut [u8; RECORD_BYTES]) -> Result<(), SwapError> {
        if slot >= self.slots {
            return Err(SwapError::Store);
        }
        self.file
            .seek(SeekFrom::Start(Self::offset(slot)))
            .and_then(|_| self.file.read_exact(&mut buf[..]))
            .map_err(|_| SwapError::Store)
    }
    fn write_record(&mut self, slot: u64, buf: &[u8; RECORD_BYTES]) -> Result<(), SwapError> {
        if slot >= self.slots {
            return Err(SwapError::Store);
        }
        self.file
            .seek(SeekFrom::Start(Self::offset(slot)))
            .and_then(|_| self.file.write_all(&buf[..]))
            .map_err(|_| SwapError::Store)
    }
    fn slot_count(&self) -> u64 {
        self.slots
    }
}
