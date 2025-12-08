# Paging System Implementation - vane

See `r5-abi-specs/PAGING.md` for the complete paging specification.

## vane-Specific Implementation

**Target:** RISC-V JIT for JavaScript/WebAssembly

**Unique Feature: Nested Paging**

Three modes via `PagingMode` enum:
1. **Legacy** - On-demand BTreeMap allocation (default)
2. **Shared** - Explicit page table (compatible with other backends)
3. **Both** - Nested paging (shared table IN legacy virtual memory)

**Nested Paging Architecture:**
The shared page table is stored within the legacy system's virtual address space. All page table accesses go through `get_page()`, allowing the page table itself to benefit from on-demand allocation.

**API Methods:**
- `translate_shared()` - Single-level nested translation
- `translate_shared_multilevel()` - 3-level nested translation
- `generate_shared_paging_js()` - Inline JavaScript for single-level
- `generate_multilevel_paging_js()` - Inline JavaScript for multi-level

**Example:**
```rust
let mut mem = Mem::default();
mem.paging_mode = PagingMode::Both;
mem.shared_page_table_vaddr = Some(0x100000); // In legacy virt space
let phys = mem.translate_shared(vaddr);
```

See `src/lib.rs` for implementation details.
