# Custom Paging System for RISC-V JIT with Nested Paging Support

See rift/PAGING.md for the general paging architecture. This document covers vane-specific implementation.

## vane Paging: 64KB Pages with Three Modes

vane uses **64KB pages** with three operational modes:

- Page Number: bits [63:16]
- Page Offset: bits [15:0]

### Mode 1: Legacy (Default)
- On-demand page allocation via BTreeMap
- `Mem::get_page(vaddr)` returns `*mut u8` pointer
- Best for sparse address spaces
- No setup required

### Mode 2: Shared
- Explicit page table compatible with rift/r52x/speet
- Can be inlined into generated JavaScript
- Supports single-level and multi-level (3-level) tables
- Better performance for dense address spaces

### Mode 3: Both (Nested Paging)
- **Shared page table stored WITHIN legacy virtual memory**
- Two-level translation:
  1. Legacy system handles page table storage and allocation
  2. Shared system provides explicit address translation
- Page table accessed via `get_page()` - benefits from on-demand allocation
- Allows controlled memory mapping within virtual address space

## Nested Paging Architecture (Mode: Both)

```
Virtual Address
      ↓
[Shared Page Table Translation]
  - Table stored at vaddr in legacy space
  - Accessed via get_page() 
  - Returns intermediate address
      ↓
[Legacy Page Allocation]
  - On-demand page allocation
  - Returns physical pointer
      ↓
Physical Memory
```

## Usage

```rust
use vane_jit::{Mem, PagingMode};

let mut mem = Mem::default();

// Legacy mode (default)
mem.paging_mode = PagingMode::Legacy;
let ptr = mem.get_page(0x10000);

// Shared mode
mem.paging_mode = PagingMode::Shared;
mem.shared_page_table_vaddr = Some(0x100000);
let phys = mem.translate_shared(0x80000000);

// Both mode (nested)
mem.paging_mode = PagingMode::Both;
mem.shared_page_table_vaddr = Some(0x100000);
let phys = mem.translate_shared(0x80000000);
```

## Benefits of Nested Paging

1. **Controlled Translation**: Explicit address mapping via page table
2. **On-Demand Tables**: Page tables allocated only as needed
3. **Memory Efficiency**: Sparse page tables benefit from legacy allocation
4. **Flexibility**: Can modify page table entries at runtime
5. **Compatibility**: Shares same table format as rift/r52x/speet
