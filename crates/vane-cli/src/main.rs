use std::{fs::read, path::PathBuf};
use clap::Parser;

/// Simple CLI to run a RISC-V ELF binary using the vane emulator library
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to input ELF binary
    input: PathBuf,

    /// Use JIT (default true)
    #[arg(long, default_value_t = true)]
    jit: bool,

    /// Enable test mode (enables HINT logging)
    #[arg(long, default_value_t = false)]
    test_mode: bool,

    /// Paging mode: legacy, shared, both
    #[arg(long, default_value = "legacy")]
    paging: String,

    /// Shared page table base virtual address (optional)
    #[arg(long)]
    shared_page_table_vaddr: Option<u64>,

    /// Shared security directory virtual address (optional)
    #[arg(long)]
    shared_security_directory_vaddr: Option<u64>,

    /// Use 32-bit paging
    #[arg(long, default_value_t = false)]
    use_32bit_paging: bool,

    /// Use multilevel paging
    #[arg(long, default_value_t = false)]
    use_multilevel_paging: bool,
}

fn main() -> Result<(), String> {
    let args = Args::parse();

    let data = read(&args.input).map_err(|e| format!("Failed to read input: {}", e))?;

    // Load ELF into memory using the same loader as tests
    struct ElfLoader { data: Vec<u8> }
    impl ElfLoader {
        fn new(data: Vec<u8>) -> Self { Self { data } }
        fn load_into_memory(&self, mem: &mut vane::Mem) -> Result<u64, String> {
            let elf_file = elf::ElfBytes::<elf::endian::AnyEndian>::minimal_parse(&self.data)
                .map_err(|e| format!("Failed to parse ELF: {}", e))?;
            let entry_point = elf_file.ehdr.e_entry;
            if let Some(segments) = elf_file.segments() {
                for segment in segments.iter() {
                    if segment.p_type == elf::abi::PT_LOAD {
                        let vaddr = segment.p_vaddr;
                        let file_offset = segment.p_offset as usize;
                        let file_size = segment.p_filesz as usize;
                        let mem_size = segment.p_memsz as usize;
                        if file_size > 0 {
                            let segment_data = &self.data[file_offset..file_offset + file_size];
                            for (i, &byte) in segment_data.iter().enumerate() {
                                let addr = vaddr + i as u64;
                                mem.write_byte(addr, byte);
                            }
                        }
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

    // Prepare memory and reactor
    let loader = ElfLoader::new(data);
    let mut mem = vane::Mem::default();
    let entry = loader.load_into_memory(&mut mem)?;

    let reactor = vane::Reactor::new_with_mem(mem);

    // Configure flags
    reactor.set_test_mode(args.test_mode);
    match args.paging.as_str() {
        "legacy" => reactor.set_paging_mode("legacy"),
        "shared" => reactor.set_paging_mode("shared"),
        "both" => reactor.set_paging_mode("both"),
        _ => reactor.set_paging_mode("legacy"),
    }
    reactor.set_use_32bit_paging(args.use_32bit_paging);
    reactor.set_use_multilevel_paging(args.use_multilevel_paging);
    reactor.set_shared_page_table_vaddr(args.shared_page_table_vaddr);
    reactor.set_shared_security_directory_vaddr(args.shared_security_directory_vaddr);

    // Run
    let result = if args.jit {
        // call jit_run which is async-like but returns a JsValue in wasm; here we call the method directly
        // The generated API uses async fn jit_run on Reactor; in native Rust it's a method returning a future.
        futures::executor::block_on(reactor.jit_run(entry))
    } else {
        futures::executor::block_on(reactor.interp(entry))
    };

    match result {
        Ok(_) => {
            println!("Execution completed (OK)");
            Ok(())
        }
        Err(e) => {
            // The wasm bindings usually return JsValue errors; here we map to string
            let s = format!("Execution error: {:?}", e);
            Err(s)
        }
    }
}
