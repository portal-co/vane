/**
 * vane JS perf structures — the JS ports of the native PageMap and
 * direct-mapped TLB (vane-arch/src/lib.rs PageMap, vane-riscv TlbCache).
 *
 * Plain-script IIFE (no import/export) so it can be pasted verbatim into an
 * emitted runtime (wsdom bootstrap embeds it) or loaded in Node; installs
 * `globalThis.__vanePerf` and returns the same object.
 *
 * Design parity with the native swap:
 * - PageMap: the guest page map is a flat Int32Array page-index table over
 *   the low 4 GiB (65536 slots of 64 KiB pages, -1 = unallocated) plus an
 *   overflow Map for page numbers >= 65536. Pages are lazily allocated into
 *   a pool; pool indices never change once allocated (stable DataView
 *   cache), mirroring the native "never resized so raw pointers stay valid"
 *   rule. Iteration is ascending page number: direct slots 0.., then sorted
 *   overflow — byte-identical seeding/snapshot order to the native side.
 * - Tlb: fixed 4096-slot direct-mapped cache keyed on (vpn, asid) with a
 *   splitmix-style hash; collisions evict. Misses fall back to the caller
 *   (the walk), exactly like the native TlbCache.
 * - PagedRuntime: TLB in front of a Sv39-shaped page-table walk over the
 *   PageMap (single/multi-level and 32/64-bit entry shapes matching the
 *   emitter's flags). One call answers "DataView for this vaddr", which is
 *   what the emitted `data()` helper needs.
 */
var __vanePerf = (function () {
  'use strict';
  const PAGE_BITS = 16;
  const PAGE_SIZE = 1 << PAGE_BITS; // 64 KiB
  const DIRECT_SLOTS = 1 << 16; // 65536 pages = low 4 GiB
  const TLB_SLOTS = 4096;

  class PageMap {
    /** Page number for an address (safe below 2^53 — sv39/low-48 addrs). */
    static pageOf(addr) {
      return Math.floor(addr / PAGE_SIZE);
    }
    /** Byte offset of `addr` within its 64 KiB page. */
    static offsetOf(addr) {
      return addr % PAGE_SIZE;
    }

    constructor() {
      this.direct = new Int32Array(DIRECT_SLOTS).fill(-1);
      this.overflow = new Map(); // pageNo (>= 65536) -> pool index
      this.pool = []; // Uint8Array(PAGE_SIZE) per allocated page
      this.dvCache = []; // DataView per pool slot, allocated on first use
    }

    _slot(pageNo) {
      if (pageNo < DIRECT_SLOTS) return this.direct[pageNo];
      const s = this.overflow.get(pageNo);
      return s === undefined ? -1 : s;
    }

    _alloc(pageNo) {
      const idx = this.pool.length;
      this.pool.push(new Uint8Array(PAGE_SIZE));
      this.dvCache.push(null);
      if (pageNo < DIRECT_SLOTS) this.direct[pageNo] = idx;
      else this.overflow.set(pageNo, idx);
      return idx;
    }

    /** The page containing `addr`, allocating on touch. */
    ensure(addr) {
      const pageNo = PageMap.pageOf(addr);
      let idx = this._slot(pageNo);
      if (idx < 0) idx = this._alloc(pageNo);
      return this.pool[idx];
    }

    /** The page containing `addr`, or null when unallocated (read paths). */
    page(addr) {
      const idx = this._slot(PageMap.pageOf(addr));
      return idx < 0 ? null : this.pool[idx];
    }

    /** DataView over the page containing `addr`, cached per pool slot. */
    dv(addr) {
      const pageNo = PageMap.pageOf(addr);
      let idx = this._slot(pageNo);
      if (idx < 0) idx = this._alloc(pageNo);
      let d = this.dvCache[idx];
      if (d === null) {
        d = new DataView(this.pool[idx].buffer);
        this.dvCache[idx] = d;
      }
      return d;
    }

    /** Little-endian u64 read at `addr`. */
    readU64(addr) {
      return this.dv(addr).getBigUint64(PageMap.offsetOf(addr), true);
    }

    /** Little-endian u32 read at `addr`. */
    readU32(addr) {
      return this.dv(addr).getUint32(PageMap.offsetOf(addr), true);
    }

    /** Bulk write `bytes` at `addr` (allocation-on-touch, like the native
     *  store path); returns the number of bytes written. */
    writeBytes(addr, bytes) {
      let written = 0;
      while (written < bytes.length) {
        const pageOff = PageMap.offsetOf(addr + written);
        const page = this.ensure(addr + written);
        const n = Math.min(PAGE_SIZE - pageOff, bytes.length - written);
        page.set(bytes.subarray(written, written + n), pageOff);
        written += n;
      }
      return written;
    }

    /** Replace the full contents of `pageNo` (seeding path). */
    setPage(pageNo, bytes) {
      let idx = this._slot(pageNo);
      if (idx < 0) idx = this._alloc(pageNo);
      this.pool[idx].set(bytes.length === PAGE_SIZE ? bytes : bytes.subarray(0, PAGE_SIZE), 0);
    }

    /** Number of allocated pages (cold path). */
    len() {
      let n = this.overflow.size;
      for (let i = 0; i < DIRECT_SLOTS; i++) if (this.direct[i] >= 0) n++;
      return n;
    }

    /** Ascending page-number iteration: [pageNo, Uint8Array] pairs — direct
     *  slots 0.., then sorted overflow. Matches the native PageMap::iter
     *  order (snapshot/seeding byte-identicality). */
    *iter() {
      for (let i = 0; i < DIRECT_SLOTS; i++) {
        if (this.direct[i] >= 0) yield [i, this.pool[this.direct[i]]];
      }
      const keys = Array.from(this.overflow.keys()).sort((a, b) => a - b);
      for (const k of keys) yield [k, this.pool[this.overflow.get(k)]];
    }
  }

  class Tlb {
    /** splitmix-style deterministic hash of (vpn, asid) — mirrors the
     *  native TlbCache's hash family. */
    static hash(vpn, asid) {
      let h = (Math.imul(vpn >>> 0, 0x9e3779b1) ^ Math.imul(asid, 0x85ebca77)) >>> 0;
      if (vpn > 0xffffffff) h = (h ^ Math.imul(Math.floor(vpn / 4294967296), 0xc2b2ae3d)) >>> 0;
      h ^= h >>> 16;
      h = Math.imul(h, 0x7feb352d) >>> 0;
      h ^= h >>> 15;
      h = Math.imul(h, 0x846ca68b) >>> 0;
      h ^= h >>> 16;
      return h >>> 0;
    }

    constructor(size) {
      this.size = size || TLB_SLOTS;
      // tag = vpn+1 (0 = empty), vpn up to 2^48 — f64 is exact.
      this.tags = new Float64Array(this.size);
      this.asids = new Int32Array(this.size);
      // base = guest-visible byte offset of the page base (f64 exact < 2^53).
      this.bases = new Float64Array(this.size);
    }

    index(vaddr, asid) {
      const vpn = Math.floor(vaddr / PAGE_SIZE);
      return Tlb.hash(vpn, asid) % this.size;
    }

    /** Translated page-base byte offset for (vaddr, asid), or -1 on miss. */
    lookup(vaddr, asid) {
      const i = this.index(vaddr, asid);
      const vpn = Math.floor(vaddr / PAGE_SIZE);
      if (this.tags[i] === vpn + 1 && this.asids[i] === asid) return this.bases[i];
      return -1;
    }

    /** Fill/replace (evict-on-collision) the slot for (vaddr, asid). */
    fill(vaddr, asid, base) {
      const i = this.index(vaddr, asid);
      this.tags[i] = Math.floor(vaddr / PAGE_SIZE) + 1;
      this.asids[i] = asid;
      this.bases[i] = base;
    }

    flush() {
      this.tags.fill(0);
    }
  }

  /**
   * TLB-fronted page-table walk over a PageMap — the JS twin of the native
   * sv39_walk + TlbCache pairing. `cfg`:
   *   pt   — page-table base vaddr (Number or BigInt; coerced)
   *   sd   — security-directory base vaddr (Number or BigInt; coerced)
   *   multilevel — walk l3 -> l2 -> l1 (sv39 shape) when true
   *   bits32 — 32-bit page-table entries (use_32bit_paging flag)
   * `asid` defaults to 0 (single-address-space guests).
   */
  class PagedRuntime {
    constructor(cfg) {
      this.cfg = cfg;
      this.pt = BigInt(cfg.pt);
      this.sd = BigInt(cfg.sd);
      this.map = new PageMap();
      this.tlb = new Tlb();
    }

    _entrySize() {
      return this.cfg.bits32 ? 4n : 8n;
    }

    /** Table-entry read, always BigInt (uniform for the walk arithmetic). */
    _read(addr) {
      return this.cfg.bits32
        ? BigInt(this.map.readU32(Number(addr)))
        : this.map.readU64(Number(addr));
    }

    _walk(v) {
      const esz = this._entrySize();
      let pagePointer;
      if (this.cfg.multilevel) {
        // sv39 shape: l3 -> l2 -> l1, each index multiplying the ENTRY size
        // within the table the previous level points at (mirrors the
        // emitted read_u64(l2_table + l2_idx<<3) chains).
        const l3Idx = Math.floor(v / 1099511627776) % 65536; // (v >> 40) & 0xFFFF
        const l2Table = this._read(this.pt + BigInt(l3Idx) * esz);
        const l2Idx = Math.floor(v / 4294967296) % 65536; // (v >> 32) & 0xFFFF
        const l1Table = this._read(BigInt(l2Table) + BigInt(l2Idx) * esz);
        const l1Idx = PageMap.pageOf(v) % 65536; // (v >> 16) & 0xFFFF
        pagePointer = this._read(BigInt(l1Table) + BigInt(l1Idx) * esz);
      } else {
        const pageNum = PageMap.pageOf(v);
        pagePointer = this._read(this.pt + BigInt(pageNum) * esz);
      }
      // sec-directory indirection (same shape as the emitted walks).
      if (this.cfg.bits32) {
        const secIdx32 = Number(pagePointer & 0xffn);
        const low24_32 = Number((pagePointer >> 8n) & 0xffffffn);
        const secEntry32 = this.map.readU64(Number(this.sd + BigInt(secIdx32) * 8n));
        const top8 = Number((secEntry32 >> 56n) & 0xffn);
        return Number((BigInt(top8) << 24n) | BigInt(low24_32)) * PAGE_SIZE + PageMap.offsetOf(v);
      }
      const secIdx = Number(pagePointer & 0xffffn);
      const low48 = pagePointer >> 16n;
      const secEntry = this.map.readU64(Number(this.sd + BigInt(secIdx) * 8n));
      const top16 = (secEntry >> 48n) & 0xffffn;
      const physBase = (top16 << 48n) | low48;
      return Number(physBase) + PageMap.offsetOf(v);
    }

    /** (DataView, byte offset within it) for vaddr — the unambiguous form.
     *  TLB hit answers without walking; misses walk once and fill the TLB. */
    dvAt(vaddr, asid) {
      asid = asid | 0;
      let base = this.tlb.lookup(vaddr, asid);
      if (base < 0) {
        // _walk returns the full phys address (base + offset); the TLB
        // caches the page base, so strip the offset before filling.
        base = this._walk(vaddr) - PageMap.offsetOf(vaddr);
        this.tlb.fill(vaddr, asid, base);
      }
      const addr = base + PageMap.offsetOf(vaddr);
      return [this.map.dv(addr), PageMap.offsetOf(addr)];
    }
  }

  return {
    PAGE_SIZE: PAGE_SIZE,
    DIRECT_SLOTS: DIRECT_SLOTS,
    TLB_SLOTS: TLB_SLOTS,
    PageMap: PageMap,
    Tlb: Tlb,
    PagedRuntime: PagedRuntime,
  };
})();
if (typeof globalThis !== 'undefined') globalThis.__vanePerf = __vanePerf;
