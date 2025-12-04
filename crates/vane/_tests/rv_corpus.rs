//! Test suite for RiscV test programs from rv-corpus repository
//!
//! This test runner loads ELF binaries from the rv-corpus git submodule and
//! executes them using the vane emulator. The tests use wasm-bindgen-test
//! to run in a browser environment.
//!
//! The implementation:
//! - Uses the `elf` crate for safe ELF parsing (no unsafe code)
//! - Uses the safe `write_byte` interface from `Mem` for memory initialization
//! - Leverages existing emulation code from vane/vane-jit (no reimplementation)
//! - Tests are generated using the `rv_test!` macro for easy addition
//!
//! ## Adding New Tests
//!
//! To add a new test from the rv-corpus submodule, use the `rv_test!` macro:
//!
//! ```rust
//! rv_test!(
//!     test_name,           // Test function name
//!     "rv64i",             // ISA variant directory (rv32i, rv64i, rv32im, rv64im, etc.)
//!     "01_basic_64bit",    // Test binary name (without path)
//!     "Description of what this test covers"
//! );
//! ```
#![cfg(target_arch = "wasm32")]

extern crate wasm_bindgen_test;

use wasm_bindgen_test::*;

use crate::*;
use vane_jit::Mem;

wasm_bindgen_test_configure!(run_in_browser);

/// ELF parser for loading RiscV binaries into memory
///
/// This implementation uses the `elf` crate for safe ELF parsing without
/// any unsafe code. It loads PT_LOAD segments from the ELF file into the
/// emulator's memory using the safe `write_byte` interface.
struct ElfLoader {
    data: Vec<u8>,
}

impl ElfLoader {
    fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    fn load_into_memory(&self, mem: &mut Mem) -> Result<u64, String> {
        let elf_file = elf::ElfBytes::<elf::endian::AnyEndian>::minimal_parse(&self.data)
            .map_err(|e| format!("Failed to parse ELF: {}", e))?;

        // Get the entry point
        let entry_point = elf_file.ehdr.e_entry;

        // Load program headers
        if let Some(segments) = elf_file.segments() {
            for segment in segments.iter() {
                // Only load PT_LOAD segments
                if segment.p_type == elf::abi::PT_LOAD {
                    let vaddr = segment.p_vaddr;
                    let file_offset = segment.p_offset as usize;
                    let file_size = segment.p_filesz as usize;
                    let mem_size = segment.p_memsz as usize;

                    // Copy data from the file using safe write_byte interface
                    // Note: No unsafe code is used here - we rely on Mem::write_byte
                    // which provides safe memory access
                    if file_size > 0 {
                        let segment_data = &self.data[file_offset..file_offset + file_size];

                        for (i, &byte) in segment_data.iter().enumerate() {
                            let addr = vaddr + i as u64;
                            mem.write_byte(addr, byte);
                        }
                    }

                    // Zero-fill the rest if mem_size > file_size
                    for i in file_size..mem_size {
                        let addr = vaddr + i as u64;
                        mem.write_byte(addr, 0);
                    }
                }
            }
        }

        Ok(entry_point)
    }
}

/// Helper to create a Reactor with loaded memory
fn create_reactor_with_binary(binary_data: &[u8]) -> Result<(Reactor, u64), String> {
    let loader = ElfLoader::new(binary_data.to_vec());
    let mut mem = Mem::default();
    let entry_point = loader.load_into_memory(&mut mem)?;

    // Create the reactor
    let reactor = Reactor::new_with_mem(mem);

    Ok((reactor, entry_point))
}

/// Macro to easily create test cases from the rv-corpus submodule
///
/// This macro generates a wasm-bindgen test function that loads and executes
/// a binary from the rv-corpus submodule.
///
/// # Usage
///
/// ```rust
/// rv_test!(
///     test_function_name,
///     "isa_variant",        // e.g., "rv64i", "rv32i", "rv64im"
///     "binary_name",        // e.g., "01_basic_64bit"
///     "Test description"
/// );
/// ```
macro_rules! rv_test {
    ($test_name:ident, $isa:expr, $binary:expr, $description:expr, $kind:ident) => {
        #[wasm_bindgen_test]
        async fn $test_name() {
            let binary_data = include_bytes!(concat!("rv-corpus/", $isa, "/", $binary));

            let (reactor, entry_point) = create_reactor_with_binary(binary_data)
                .expect(&format!("Failed to load {}/{} binary", $isa, $binary));

            // Run the test starting from the entry point
            let result = reactor.$kind(entry_point).await;

            // Check that execution completes successfully
            match result {
                Ok(_) => {
                    // Test passed
                }
                Err(e) => {
                    if !crate::has_success(e.clone()) {
                        let err_str = format!("{:?}", e);
                        panic!("{} test failed: {}", $description, err_str);
                    }
                }
            }
        }
    };
}

// RV64I Tests
rv_test!(
    test_rv64i_basic_64bit,
    "rv64i",
    "01_basic_64bit",
    "RV64I basic 64-bit operations: 64-bit register operations, word operations (ADDIW, SLLIW, SRLIW, SRAIW, ADDW, SUBW, etc.), 64-bit loads/stores (LD, SD, LWU), sign extension",
    jit_run
);

rv_test!(
    test_rv64i_basic_64bit_interp,
    "rv64i",
    "01_basic_64bit",
    "RV64I basic 64-bit operations: 64-bit register operations, word operations (ADDIW, SLLIW, SRLIW, SRAIW, ADDW, SUBW, etc.), 64-bit loads/stores (LD, SD, LWU), sign extension",
    interp
);

// RV64IM Tests
rv_test!(
    test_rv64im_multiply_divide,
    "rv64im",
    "01_multiply_divide_64",
    "RV64IM multiply/divide: 64-bit multiplication (MUL, MULH, MULHU, MULHSU), division (DIV, DIVU, REM, REMU), word operations (MULW, DIVW, DIVUW, REMW, REMUW), overflow handling",
    jit_run
);

// RV32I Tests - NOP and HINT instructions with test_mode enabled
// This test exercises the HINT instruction parsing and logging behavior
rv_test!(
    test_rv32i_nop_and_hints_test_mode,
    "rv32i",
    "06_nop_and_hints",
    "RV32I NOP and HINT instructions with test_mode: logs HINT markers (addi x0, x0, N) to console",
    interp_test_mode
);

// RV32I Tests - NOP and HINT with regular interpreter (no logging)
rv_test!(
    test_rv32i_nop_and_hints_interp,
    "rv32i",
    "06_nop_and_hints",
    "RV32I NOP and HINT instructions: verifies NOPs and HINTs execute correctly without affecting state",
    interp
);
