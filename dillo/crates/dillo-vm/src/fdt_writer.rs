//! Minimal FDT v17 writer for dillo's host overlay.
//!
//! Hand-rolled because `vm-fdt`'s node-name validator (correctly per
//! DTS spec) rejects names starting with `_`, but the FDT overlay
//! convention names the subnode `__overlay__` and `devtree`'s parser
//! expects exactly that name. Rather than fork vm-fdt or patch
//! devtree's parser, we write the small set of node shapes dillo
//! needs by hand.
//!
//! Scope is intentionally narrow: emit one root node with N
//! `fragment@K` children, each containing a `target-path` string
//! property + an `__overlay__` subnode whose own contents are
//! `cpu@N` / `memory@N` nodes with `reg` + mirrored properties.

#![allow(clippy::cast_possible_truncation)]

const FDT_MAGIC: u32 = 0xD00DFEED;
const FDT_VERSION: u32 = 17;
const FDT_LAST_COMPATIBLE: u32 = 16;

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;

/// Builder that accumulates structure and strings blocks, then emits
/// a complete FDT v17 blob via [`Self::finish`].
pub(crate) struct FdtBuilder {
    structure: Vec<u8>,
    strings: Vec<u8>,
    string_offsets: Vec<(String, u32)>,
}

impl FdtBuilder {
    pub(crate) fn new() -> Self {
        Self {
            structure: Vec::with_capacity(1024),
            strings: Vec::with_capacity(256),
            string_offsets: Vec::new(),
        }
    }

    pub(crate) fn begin_node(&mut self, name: &str) {
        self.structure
            .extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad4();
    }

    pub(crate) fn end_node(&mut self) {
        self.structure
            .extend_from_slice(&FDT_END_NODE.to_be_bytes());
    }

    pub(crate) fn property(&mut self, name: &str, value: &[u8]) {
        let off = self.intern_string(name);
        self.structure.extend_from_slice(&FDT_PROP.to_be_bytes());
        self.structure
            .extend_from_slice(&(value.len() as u32).to_be_bytes());
        self.structure.extend_from_slice(&off.to_be_bytes());
        self.structure.extend_from_slice(value);
        self.pad4();
    }

    pub(crate) fn property_u32(&mut self, name: &str, v: u32) {
        self.property(name, &v.to_be_bytes());
    }

    pub(crate) fn property_string(&mut self, name: &str, v: &str) {
        let mut bytes = Vec::with_capacity(v.len() + 1);
        bytes.extend_from_slice(v.as_bytes());
        bytes.push(0);
        self.property(name, &bytes);
    }

    /// Emit a `reg = <hi32 lo32 hi32 lo32>` property from a single
    /// (base, size) pair, encoded as four big-endian u32 cells.
    pub(crate) fn property_reg_2cells(&mut self, name: &str, base: u64, size: u64) {
        let cells: [u32; 4] = [
            (base >> 32) as u32,
            base as u32,
            (size >> 32) as u32,
            size as u32,
        ];
        let mut bytes = Vec::with_capacity(16);
        for c in cells {
            bytes.extend_from_slice(&c.to_be_bytes());
        }
        self.property(name, &bytes);
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        // Append FDT_END at end of structure block.
        self.structure.extend_from_slice(&FDT_END.to_be_bytes());

        // Header is 40 bytes.
        let header_len: u32 = 40;
        // Memreserve block: one terminator (16 bytes of zeros).
        let memrsv_len: u32 = 16;
        // Layout: [header][memreserve][structure][strings]
        let off_mem_rsvmap = header_len;
        let off_dt_struct = off_mem_rsvmap + memrsv_len;
        let size_dt_struct = self.structure.len() as u32;
        let off_dt_strings = off_dt_struct + size_dt_struct;
        let size_dt_strings = self.strings.len() as u32;
        let totalsize = off_dt_strings + size_dt_strings;

        let mut out = Vec::with_capacity(totalsize as usize);
        out.extend_from_slice(&FDT_MAGIC.to_be_bytes());
        out.extend_from_slice(&totalsize.to_be_bytes());
        out.extend_from_slice(&off_dt_struct.to_be_bytes());
        out.extend_from_slice(&off_dt_strings.to_be_bytes());
        out.extend_from_slice(&off_mem_rsvmap.to_be_bytes());
        out.extend_from_slice(&FDT_VERSION.to_be_bytes());
        out.extend_from_slice(&FDT_LAST_COMPATIBLE.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // boot_cpuid_phys
        out.extend_from_slice(&size_dt_strings.to_be_bytes());
        out.extend_from_slice(&size_dt_struct.to_be_bytes());
        // Memreserve terminator.
        out.extend_from_slice(&[0u8; 16]);
        // Structure block.
        out.extend_from_slice(&self.structure);
        // Strings block.
        out.extend_from_slice(&self.strings);
        out
    }

    fn pad4(&mut self) {
        while self.structure.len() % 4 != 0 {
            self.structure.push(0);
        }
    }

    fn intern_string(&mut self, name: &str) -> u32 {
        if let Some((_, off)) = self.string_offsets.iter().find(|(n, _)| n == name) {
            return *off;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.string_offsets.push((name.to_string(), off));
        off
    }
}
