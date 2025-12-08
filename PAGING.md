# Paging System Implementation - vane

See `r5-abi-specs/PAGING.md` for the complete paging specification.

## vane-Specific Implementation

**Target:** RISC-V JIT with JavaScript/WebAssembly hybrid execution

**Special Features:**
- Three paging modes: Legacy, Shared, Both
- **Nested Paging**: Shared page table stored IN legacy virtual memory
- JavaScript code generation for inline translation
- Configurable 32-bit or 64-bit physical addressing

**Paging Modes:**

1. **Legacy Mode** (default)
   - On-demand page allocation via BTreeMap
   - No explicit page table
   - Used for basic memory management

2. **Shared Mode**
   - Explicit page table compatible with other backends
   - Can be inlined into JavaScript for performance
   - Supports both 32-bit and 64-bit physical addressing

3. **Both Mode** (Nested Paging)
   - Page table stored at virtual address in legacy system
   - Two-level translation for controlled mapping
   - All table accesses go through `get_page()`

**API Functions:**

*64-bit Physical Addressing:*
- `translate_shared()` - single-level nested translation
- `translate_shared_multilevel()` - 3-level nested translation
- `generate_shared_paging_js()` - inline JS for single-level
- `generate_multilevel_paging_js()` - inline JS for multi-level

*32-bit Physical Addressing (4 GiB limit):*
- `translate_shared_32()` - single-level nested translation
- `translate_shared_multilevel_32()` - 3-level nested translation
- `generate_shared_paging_js_32()` - inline JS for single-level
- `generate_multilevel_paging_js_32()` - inline JS for multi-level

**Example:**
```rust
let mut mem = Mem::default();
mem.paging_mode = PagingMode::Both;
mem.shared_page_table_vaddr = Some(0x1000000);

// 64-bit physical addresses
let phys_addr = mem.translate_shared(vaddr);

// 32-bit physical addresses
let phys_addr = mem.translate_shared_32(vaddr);
```

**Nested Architecture:**
```
Virtual Address (64-bit)
       ↓
Shared Page Table Lookup (4 or 8 byte entries)
  - Table at vaddr in legacy space
  - Accessed via get_page()
       ↓
Physical Address (32-bit or 64-bit)
       ↓
Legacy Page Allocation
  - On-demand allocation
       ↓
Physical Memory
```

See `crates/vane-jit/src/lib.rs` for implementation details.
