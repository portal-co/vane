//! Test suite for RiscV test programs from rv-corpus repository
//! 
//! This test runner loads ELF binaries from the rv-corpus repository and
//! executes them using the vane emulator. The tests use wasm-bindgen-test
//! to run in a browser environment.
//!
//! The implementation:
//! - Uses the `elf` crate for safe ELF parsing (no unsafe code)
//! - Uses the safe `write_byte` interface from `Mem` for memory initialization
//! - Leverages existing emulation code from vane/vane-jit (no reimplementation)
//! - Tests RV64I and RV64IM instruction sets
#![cfg(target_arch = "wasm32")]

extern crate wasm_bindgen_test;
use wasm_bindgen_test::*;

use vane::Reactor;
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

/// Test RV64I basic 64-bit operations
/// 
/// This test loads and executes the rv64i/01_basic_64bit test from rv-corpus
/// which tests 64-bit specific instructions including:
/// - 64-bit register operations
/// - Word operations (ADDIW, SLLIW, SRLIW, SRAIW, ADDW, SUBW, etc.)
/// - 64-bit loads and stores (LD, SD, LWU)
/// - Sign extension behavior in 64-bit mode
#[wasm_bindgen_test]
async fn test_rv64i_basic_64bit() {
    let binary_data = include_bytes!("binaries/rv64i_01_basic_64bit");
    
    let (reactor, entry_point) = create_reactor_with_binary(binary_data)
        .expect("Failed to load rv64i_01_basic_64bit binary");
    
    // Run the test starting from the entry point
    let result = reactor.interp(entry_point).await;
    
    // Check that execution completes successfully
    // The test binary should execute without errors
    match result {
        Ok(_) => {
            // Test passed - execution completed successfully
        }
        Err(e) => {
            let err_str = format!("{:?}", e);
            panic!("RV64I basic 64-bit test failed: {}", err_str);
        }
    }
}

/// Test RV64IM multiply/divide operations
/// 
/// This test loads and executes the rv64im/01_multiply_divide_64 test from rv-corpus
/// which tests the M extension (multiplication and division) including:
/// - 64-bit multiplication operations (MUL, MULH, MULHU, MULHSU)
/// - 64-bit division operations (DIV, DIVU, REM, REMU)
/// - Word operations (MULW, DIVW, DIVUW, REMW, REMUW)
/// - Overflow and division by zero handling in 64-bit mode
#[wasm_bindgen_test]
async fn test_rv64im_multiply_divide() {
    let binary_data = include_bytes!("binaries/rv64im_01_multiply_divide_64");
    
    let (reactor, entry_point) = create_reactor_with_binary(binary_data)
        .expect("Failed to load rv64im_01_multiply_divide_64 binary");
    
    // Run the test starting from the entry point
    let result = reactor.interp(entry_point).await;
    
    // Check that execution completes successfully
    match result {
        Ok(_) => {
            // Test passed - multiply/divide operations work correctly
        }
        Err(e) => {
            let err_str = format!("{:?}", e);
            panic!("RV64IM multiply/divide test failed: {}", err_str);
        }
    }
}
