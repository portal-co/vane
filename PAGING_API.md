# Paging System API for CoreJS and Reactor

This document describes how the paging system is now integrated into the `CoreJS` JavaScript code generator and `Reactor` types.

## Overview

The paging system can now be configured at runtime through the generated `Reactor` type. The paging configuration is automatically used when generating JavaScript code for memory access.

## Changes Made

### 1. Enhanced `Flags` Structure

The `Flags` struct in `vane-jit/src/template.rs` now includes paging configuration:

```rust
pub struct Flags {
    pub test_mode: bool,
    pub paging_mode: Option<PagingMode>,
    pub shared_page_table_vaddr: Option<u64>,
    pub use_32bit_paging: bool,
    pub use_multilevel_paging: bool,
}
```

**New constructor:**
```rust
Flags::with_paging(
    test_mode: bool,
    paging_mode: PagingMode,
    shared_page_table_vaddr: Option<u64>,
    use_32bit_paging: bool,
    use_multilevel_paging: bool,
)
```

### 2. Dynamic JavaScript Generation in `CoreJS`

The `CoreJS` struct now generates different JavaScript `data` functions based on the paging mode:

- **Legacy Mode (default)**: Uses `$.get_page()` directly
  ```javascript
  data = (p => { p = $.get_page(p); return new DataView($._sys('memory').buffer, p); })
  ```

- **Shared/Both Mode with Single-Level Paging**: 
  - 64-bit physical addresses: Inline page table lookup with 8-byte entries
  - 32-bit physical addresses: Inline page table lookup with 4-byte entries

- **Shared/Both Mode with Multi-Level Paging**:
  - 64-bit physical addresses: 3-level page table with 8-byte entries
  - 32-bit physical addresses: 3-level page table with 4-byte entries

### 3. Reactor Type API

New JavaScript methods added to generated `Reactor` types:

#### Get/Set Paging Mode
```javascript
reactor.get_paging_mode()  // Returns: "legacy", "shared", or "both"
reactor.set_paging_mode("shared")  // Sets the paging mode
```

#### Get/Set Shared Page Table Address
```javascript
reactor.get_shared_page_table_vaddr()  // Returns: u64 or null
reactor.set_shared_page_table_vaddr(0x1000000n)  // Sets the virtual address of the page table
```

## Usage Examples

### Example 1: Legacy Mode (Default)
```javascript
let reactor = new Reactor();
// Default mode is "legacy" - no configuration needed
// Memory access goes through $.get_page() directly
```

### Example 2: Shared Mode with Single-Level Paging
```javascript
let reactor = new Reactor();

// Configure shared paging
reactor.set_paging_mode("shared");
reactor.set_shared_page_table_vaddr(0x1000000n);

// Now generated JavaScript will use inline page table translation
// The page table must be set up at virtual address 0x1000000
```

### Example 3: Both Mode (Nested Paging)
```javascript
let reactor = new Reactor();

// Configure nested paging (page table stored in legacy virtual memory)
reactor.set_paging_mode("both");
reactor.set_shared_page_table_vaddr(0x1000000n);

// The shared page table at 0x1000000 will be accessed through legacy get_page()
// This provides two-level translation:
// 1. Shared page table lookup (in legacy virtual space)
// 2. Legacy page allocation for physical memory
```

## How It Works

1. **Configuration**: When you set the paging mode and page table address on the Reactor, these values are stored in the `Mem` structure.

2. **Code Generation**: When `jit_code()` is called, it reads the current paging configuration from `Mem` and creates `Flags` with these settings.

3. **JavaScript Output**: The `CoreJS` struct generates JavaScript with the appropriate `data` function based on the flags:
   - Reads `paging_mode`, `shared_page_table_vaddr`, `use_32bit_paging`, and `use_multilevel_paging`
   - Generates inline page table lookup code for Shared/Both modes
   - Falls back to legacy mode if page table address is not configured

4. **Runtime**: The generated JavaScript code performs address translation according to the configured paging mode.

## Advanced Configuration

Currently, `use_32bit_paging` and `use_multilevel_paging` are hardcoded to `false` in the macro. To use these features, you can modify the macro or expose additional configuration methods:

```rust
// In vane-meta-gen/src/lib.rs, modify the jit_code function:
let flags = Flags::with_paging(
    test_mode,
    paging_mode,
    shared_page_table_vaddr,
    true,  // Enable 32-bit paging
    false, // Disable multi-level paging
);
```

Or add additional setter methods to the Reactor type for these options.

## See Also

- `PAGING.md` - Complete paging system specification
- `crates/vane-jit/src/lib.rs` - Paging implementation with translation functions
- `crates/vane-jit/src/template.rs` - CoreJS and Flags structures
- `crates/vane-meta-gen/src/lib.rs` - Reactor type generation macro
