use vane_jit::{Mem, PagingMode};

#[test]
fn integration_example() {
    assert_eq!(2 + 2, 4);
}

#[test]
fn test_memory_isolation() {
    // Test that separate memory instances don't interfere
    let mut mem1 = Mem::default();
    let mut mem2 = Mem::default();
    
    mem1.write_byte(0x1000, 0xAA);
    mem2.write_byte(0x1000, 0xBB);
    
    assert_eq!(mem1.read_byte(0x1000), 0xAA);
    assert_eq!(mem2.read_byte(0x1000), 0xBB);
}

#[test]
fn test_paging_mode_switching() {
    let mut mem = Mem::default();
    
    // Start in legacy mode
    assert_eq!(mem.paging_mode, PagingMode::Legacy);
    
    // Write some data in legacy mode
    mem.write_byte(0x1000, 0x42);
    
    // Switch to shared mode
    mem.paging_mode = PagingMode::Shared;
    
    // Data should still be accessible
    assert_eq!(mem.read_byte(0x1000), 0x42);
}

#[test]
fn test_large_address_space() {
    let mut mem = Mem::default();
    
    // Test writing to widely separated addresses
    let addresses = [
        0x0000_0000,
        0x0001_0000,
        0x0100_0000,
        0x1000_0000,
        0xFFFF_0000,
    ];
    
    for (i, &addr) in addresses.iter().enumerate() {
        mem.write_byte(addr, i as u8);
    }
    
    for (i, &addr) in addresses.iter().enumerate() {
        assert_eq!(mem.read_byte(addr), i as u8);
    }
}

#[test]
fn test_page_alignment() {
    let mut mem = Mem::default();
    
    // Test that page boundaries are handled correctly
    // Page size is 64KB (0x10000)
    let page_size = 0x10000;
    
    mem.write_byte(page_size - 1, 0xFF); // Last byte of page 0
    mem.write_byte(page_size, 0x00);     // First byte of page 1
    
    assert_eq!(mem.read_byte(page_size - 1), 0xFF);
    assert_eq!(mem.read_byte(page_size), 0x00);
}

#[test]
fn test_memory_stress() {
    let mut mem = Mem::default();
    
    // Write a pattern across multiple pages
    for i in 0..1000 {
        let addr = i * 100;
        mem.write_byte(addr, (i % 256) as u8);
    }
    
    // Verify the pattern
    for i in 0..1000 {
        let addr = i * 100;
        assert_eq!(mem.read_byte(addr), (i % 256) as u8);
    }
}

#[test]
fn test_shared_paging_setup() {
    let mut mem = Mem::default();
    mem.paging_mode = PagingMode::Both;
    
    // Test that shared_page_table_vaddr can be set
    mem.shared_page_table_vaddr = Some(0x100000);
    assert_eq!(mem.shared_page_table_vaddr, Some(0x100000));
}

#[test]
fn test_wasm_legacy_translation_identity() {
    let mem = Mem::default();
    
    // With wasm_base = 0, translation should be identity mapping
    let vaddr = 0x12345;
    let result = mem.translate_to_wasm_legacy(vaddr, 0);
    
    // result = 0 + (0x1 * 65536) + 0x2345 = 0x12345
    assert_eq!(result, vaddr);
}
