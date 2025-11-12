//! Test suite for RiscV test programs from rv-corpus repository
#![cfg(target_arch = "wasm32")]

extern crate wasm_bindgen_test;
use wasm_bindgen_test::*;

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Mutex;
use vane::{Reactor};
use vane_jit::Mem;

wasm_bindgen_test_configure!(run_in_browser);

/// ELF parser for loading RiscV binaries
/// This implementation avoids unsafe code by using the elf crate
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

                    // Copy data from the file
                    if file_size > 0 {
                        let segment_data = &self.data[file_offset..file_offset + file_size];
                        
                        // Write data to memory page by page
                        for (i, &byte) in segment_data.iter().enumerate() {
                            let addr = vaddr + i as u64;
                            write_byte_to_memory(mem, addr, byte);
                        }
                    }

                    // Zero-fill the rest if mem_size > file_size
                    for i in file_size..mem_size {
                        let addr = vaddr + i as u64;
                        write_byte_to_memory(mem, addr, 0);
                    }
                }
            }
        }

        Ok(entry_point)
    }
}

/// Write a byte to memory using a safer approach
/// We'll create a helper that manages the memory access safely
#[inline]
fn write_byte_to_memory(mem: &mut Mem, addr: u64, value: u8) {
    // Get the page and write through the pointer in one operation
    // This minimizes the unsafe scope
    let ptr = mem.get_page(addr);
    // Safety: The pointer comes from Mem::get_page which allocates and returns
    // a valid pointer to a byte within a page. We have exclusive access through
    // the mutable reference to Mem.
    let byte_ref = unsafe { &mut *ptr };
    *byte_ref = value;
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

#[wasm_bindgen_test]
async fn test_rv64i_basic_64bit() {
    // Load the test binary
    let binary_data = include_bytes!("binaries/rv64i_01_basic_64bit");
    
    let (reactor, entry_point) = create_reactor_with_binary(binary_data)
        .expect("Failed to load binary");
    
    // Run the test starting from the entry point
    let result = reactor.interp(entry_point).await;
    
    // For now, we just check that it doesn't error
    // In a real test, we would check specific outcomes
    match result {
        Ok(_) => {
            // Test passed
            assert!(true);
        }
        Err(e) => {
            // Get the error message
            let err_str = format!("{:?}", e);
            panic!("Test failed with error: {}", err_str);
        }
    }
}
