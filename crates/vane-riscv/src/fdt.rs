//! Minimal FDT (flattened device tree) writer — M4 of PLAN_LINUX_WASM.md.
//!
//! Builds a real `.dtb` in memory: header, empty reserve map, structure
//! block, strings block. `no_std`, big-endian, 4-byte aligned, per the
//! Devicetree Spec.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;
const FDT_VERSION: u32 = 17;
const FDT_COMPAT: u32 = 16;

/// Flattened device tree builder. Nodes are opened with
/// [`FdtBuilder::begin_node`] and closed with [`FdtBuilder::end_node`];
/// properties are added with the `prop_*` methods. [`FdtBuilder::finish`]
/// produces the blob.
pub struct FdtBuilder {
    strings: Vec<u8>,
    string_offsets: BTreeMap<Vec<u8>, u32>,
    body: Vec<u8>,
}

fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

impl FdtBuilder {
    pub fn new() -> Self {
        let mut b = Self {
            strings: Vec::new(),
            string_offsets: BTreeMap::new(),
            body: Vec::new(),
        };
        // Offset 0 is the empty string.
        b.strings.push(0);
        b.string_offsets.insert(Vec::new(), 0);
        b
    }

    fn string_offset(&mut self, name: &str) -> u32 {
        let key = name.as_bytes().to_vec();
        if let Some(&off) = self.string_offsets.get(&key) {
            return off;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(&key);
        self.strings.push(0);
        self.string_offsets.insert(key, off);
        off
    }

    fn token(&mut self, t: u32) {
        self.body.extend_from_slice(&t.to_be_bytes());
    }

    pub fn begin_node(&mut self, name: &str) {
        self.token(FDT_BEGIN_NODE);
        self.body.extend_from_slice(name.as_bytes());
        self.body.push(0);
        pad4(&mut self.body);
    }

    pub fn end_node(&mut self) {
        self.token(FDT_END_NODE);
    }

    /// Raw property bytes (already serialized, big-endian inside).
    pub fn prop_raw(&mut self, name: &str, data: &[u8]) {
        self.token(FDT_PROP);
        self.body
            .extend_from_slice(&(data.len() as u32).to_be_bytes());
        let off = self.string_offset(name);
        self.body.extend_from_slice(&off.to_be_bytes());
        self.body.extend_from_slice(data);
        pad4(&mut self.body);
    }

    pub fn prop_u32(&mut self, name: &str, v: u32) {
        self.prop_raw(name, &v.to_be_bytes());
    }

    /// A u64 encoded as two big-endian cells (FDT convention for 64-bit
    /// addresses with #address-cells = 2).
    pub fn prop_u64(&mut self, name: &str, v: u64) {
        let mut d = Vec::with_capacity(8);
        d.extend_from_slice(&((v >> 32) as u32).to_be_bytes());
        d.extend_from_slice(&(v as u32).to_be_bytes());
        self.prop_raw(name, &d);
    }

    /// A (addr, size) pair as four cells (#address-cells = 2, #size-cells = 2).
    pub fn prop_reg(&mut self, name: &str, addr: u64, size: u64) {
        let mut d = Vec::with_capacity(16);
        for cell in [
            (addr >> 32) as u32,
            addr as u32,
            (size >> 32) as u32,
            size as u32,
        ] {
            d.extend_from_slice(&cell.to_be_bytes());
        }
        self.prop_raw(name, &d);
    }

    pub fn prop_str(&mut self, name: &str, v: &str) {
        let mut d = v.as_bytes().to_vec();
        d.push(0);
        self.prop_raw(name, &d);
    }

    pub fn prop_str_list(&mut self, name: &str, vs: &[&str]) {
        let mut d = Vec::new();
        for v in vs {
            d.extend_from_slice(v.as_bytes());
            d.push(0);
        }
        self.prop_raw(name, &d);
    }

    /// A list of u32 cells.
    pub fn prop_cells(&mut self, name: &str, vs: &[u32]) {
        let mut d = Vec::with_capacity(vs.len() * 4);
        for v in vs {
            d.extend_from_slice(&v.to_be_bytes());
        }
        self.prop_raw(name, &d);
    }

    /// Empty property (boolean flag, e.g. `interrupt-controller`).
    pub fn prop_flag(&mut self, name: &str) {
        self.prop_raw(name, &[]);
    }

    /// Serialize to the final blob.
    pub fn finish(mut self) -> Vec<u8> {
        self.token(FDT_END);
        pad4(&mut self.body);

        let header_len = 40u32;
        let rsvmap_len = 16u32; // one empty (0, 0) entry terminates the map
        let off_struct = header_len + rsvmap_len;
        let off_strings = off_struct + self.body.len() as u32;
        let totalsize = off_strings + self.strings.len() as u32;

        let mut out = Vec::with_capacity(totalsize as usize);
        let fields = [
            FDT_MAGIC,
            totalsize,
            off_struct,
            off_strings,
            header_len, // off_mem_rsvmap
            FDT_VERSION,
            FDT_COMPAT,
            0, // boot_cpuid_phys
            self.strings.len() as u32,
            self.body.len() as u32,
        ];
        for f in fields {
            out.extend_from_slice(&f.to_be_bytes());
        }
        out.extend_from_slice(&[0; 16]); // empty reserve map
        out.extend_from_slice(&self.body);
        out.extend_from_slice(&self.strings);
        out
    }
}

impl Default for FdtBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_parseable_header() {
        let mut f = FdtBuilder::new();
        f.begin_node("");
        f.prop_str("model", "vane");
        f.prop_u32("#address-cells", 2);
        f.end_node();
        let blob = f.finish();
        assert_eq!(&blob[0..4], &FDT_MAGIC.to_be_bytes());
        let totalsize = u32::from_be_bytes([blob[4], blob[5], blob[6], blob[7]]);
        assert_eq!(totalsize as usize, blob.len());
        // "model" appears in the strings block
        let strings_off = u32::from_be_bytes([blob[12], blob[13], blob[14], blob[15]]) as usize;
        let strings = &blob[strings_off..];
        assert!(strings.windows(6).any(|w| w == b"model\0"));
    }
}
