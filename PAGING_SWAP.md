# Encrypted paging of guest RAM (M5, PLAN_LINUX_WASM.md §6)

Guest physical RAM is split into fixed **64 KiB pages**. A bounded resident
set holds the hot pages; cold pages are evicted to a host-provided backing
store, each page **encrypted and authenticated** so the backing store is
untrusted. This lets an 8 GiB guest run with a ~4 GiB resident cap, the rest
paged out.

## Components

- **`vane-swap` crate** (`no_std` + `alloc` core logic): the [`PageCache`]
  (resident-set manager), [`Crypter`] (XChaCha20-Poly1305), the [`SwapStore`]
  trait, and backends.
- **`SwapStore`**: a flat array of page records addressed by slot index.
  Implementations:
  - `FileSwap` (`std` feature) — a file of `slots × RECORD_BYTES`, used by
    `vane-cli` on native and `wasm32-wasip1` (preopened file).
  - `MemSwap` — in-memory, for tests.
  - Browser backends (OPFS sync handle, IndexedDB) are M6 (§6.2 item 1).
- **Mem integration**: `vane_arch::Mem` gains an optional
  `swap: Option<spin::Mutex<PageCache>>` plus `swap_base`/`swap_len`. When
  set, accesses in `[swap_base, swap_base+swap_len)` route through the
  `PageCache` from `read_byte`/`write_byte`; the `get_page` raw-pointer fast
  paths in `step_one` (fetch/load/store) are forced onto the swap-aware byte
  path when swap is active (a resident frame may be evicted, so the raw
  pointer is unsafe).
- **Reactor API**: `enable_swap`, `swap_active`, `swap_stats`,
  `take_swap_error`, `pin_swap_range`.
- **CLI**: `--swap <file> --resident <bytes> [--swap-key <file>]` in machine
  mode. Boot artifacts (OpenSBI, kernel, initrd, DTB, fw_dynamic_info) are
  **pinned** (never evicted).

## On-disk format

Each swapped page is one fixed-size record:

```
[ nonce (24 B) | ciphertext (65536 B) | Poly1305 tag (16 B) ]   = 65576 B
```

The backing file is `slots × RECORD_BYTES`; record `i` is at offset
`i * RECORD_BYTES`. One slot per RAM page.

## Security invariants (§6.2 item 3)

- **Cipher**: XChaCha20-Poly1305 via the `chacha20poly1305` crate — pure Rust,
  `no_std`, constant-time-friendly in software, and the *identical* code path
  on every host (browser, WASI, WASIX, native). There is deliberately **no
  WebCrypto fast path** (WebCrypto has no XChaCha20): one implementation
  everywhere removes a class of per-target divergence (same spirit as D5).
- **Nonce discipline**: a fresh 192-bit nonce is drawn from the host CSPRNG
  **on every page write**. XChaCha20's nonce is large enough that random
  nonces are collision-safe for the lifetime of a key, so there is *no
  generation counter to persist, journal, or get wrong* — the main reason for
  choosing it over GCM. The nonce travels with the record.
- **AAD**: the swap slot index is bound as additional authenticated data, so a
  record cannot be relocated to a different slot without detection.
- **Key management**: the 256-bit key is drawn from the host CSPRNG per VM
  instance, or read from `--swap-key <file>` (32 bytes). The key never touches
  disk. A fresh random key per session trivially invalidates old swap — by
  design; persistence of encrypted pages across sessions is only possible with
  a user-supplied key.
- **Integrity**: a tag-verification failure on read is a hard VM error
  ([`SwapError::Integrity`]), surfaced to the host via
  `Reactor::take_swap_error()` and reported — never silent zeros (S5).
- **Zero pages**: an all-zero page is never written to swap; it is
  re-materialized on fault. (Huge win for fresh large guests.)

## Eviction

Clock / second-chance over the resident pool with dirty tracking: a page
written since fault-in is written back (encrypted) on eviction; a clean page
is dropped (its swap slot, if any, still holds valid content). A page whose
content is all-zero is recorded as `Zero` (no swap slot consumed). Pages can
be **pinned** so they are never evicted.

> **Pinned set**: the kernel image, DTB, OpenSBI, and (§7.4) active Sv39
> page-table pages must not be evicted — the MMU walker reads page tables
> during translation, and a walker fault must not recurse into the swap
> layer. Currently boot artifacts are pinned; page-table pages are serviced
> through the same (non-recursive) fault path, since swap servicing performs
> no guest translation. Pinning page-table pages is a future optimization.

## Accounting

`Reactor::swap_stats()` returns [`MemStats`]: resident/cap, faults, evictions,
writebacks, swap bytes read/written, zero-page evictions, and integrity
failures. This feeds the S4 report and the browser UI.

## Testing

- Unit tests (`vane-swap/src/tests.rs`): crypto roundtrip, tamper detection,
  slot-AAD binding, zero-page elision, eviction/dirty/pinned semantics,
  integrity after cycling, and the S5 tamper case.
- File-backend S5 (`vane-swap/tests/file_swap.rs`, `std` feature): records on
  disk are ciphertext (no long plaintext runs); tampering with a record on
  disk is detected as `Integrity` on fault-in.
- Mem integration (`vane-arch/tests/mem_swap.rs`): RAM region routes through
  the `PageCache`; non-RAM regions use the plain page store.
- **Gate** (`harness/run-swap-gate.sh`): the `payload-swap-stress` guest
  writes/verifies a per-page xorshift64* pattern across a 6 MiB region with a
  512 KiB resident cap (≈88 pages cycle through the encrypted swap) and
  reports `S4:<checksum>:OK`, on native and `wasm32-wasip1`.

## Full-scale S4

The S4 exit criterion at full scale (8 GiB guest, 4 GiB resident, ≥4 GiB
through swap, e.g. `stress-ng --vm`) is a long-running benchmark gated on
interpreter throughput, like the Linux boot gate — run it as a nightly job,
not per-commit CI. The mechanism it exercises (pages cycling through the
encrypted store with checksum verification) is exactly what the swap gate and
unit tests validate at reduced scale.
