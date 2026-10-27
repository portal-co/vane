//! Executable-memory region for JIT code: an `mmap`'d region written RW, then
//! flipped to RX (W^X) once all code is emitted, plus the instruction-cache
//! flush the host CPU needs before executing freshly-written code.

/// A bump-allocated executable region. One per reactor; freed with the reactor.
pub struct ExecRegion {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl ExecRegion {
    /// Allocate a `cap`-byte region, initially read-write.
    pub fn new(cap: usize) -> std::io::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                cap,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ExecRegion {
            ptr: ptr as *mut u8,
            len: 0,
            cap,
        })
    }

    /// Bump-allocate `code.len()` bytes (4-aligned) and write `code`. Returns
    /// the executable base for this chunk (valid after `finalize`).
    pub fn write(&mut self, code: &[u8]) -> Option<*const u8> {
        let aligned = (code.len() + 3) & !3;
        if self.len + aligned > self.cap {
            return None;
        }
        let dst = unsafe { self.ptr.add(self.len) };
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), dst, code.len()) };
        self.len += aligned;
        Some(dst as *const u8)
    }

    /// Flip the region to read-execute (W^X) and flush the icache. Call once
    /// after all code is written.
    ///
    /// # Safety
    /// The region must contain valid, complete machine code; no further writes.
    pub unsafe fn finalize(&self) -> std::io::Result<()> {
        let r = libc::mprotect(
            self.ptr as *mut _,
            self.cap,
            libc::PROT_READ | libc::PROT_EXEC,
        );
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        flush_icache(self.ptr, self.len);
        Ok(())
    }
}

impl Drop for ExecRegion {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut _, self.cap) };
    }
}

// The region is exclusively owned; the raw pointer is only mutated under `&mut
// self` (write) and only read (executed) after `finalize`.
unsafe impl Send for ExecRegion {}
unsafe impl Sync for ExecRegion {}

/// Flush the data/instruction caches over `[ptr, ptr+len)` so freshly-written
/// code is visible to the instruction fetcher.
fn flush_icache(ptr: *mut u8, len: usize) {
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    unsafe {
        // libSystem's icache invalidation (the documented macOS mechanism).
        extern "C" {
            fn sys_icache_invalidate(start: *const u8, len: usize);
        }
        sys_icache_invalidate(ptr, len);
    }
    #[cfg(all(target_arch = "aarch64", not(target_os = "macos")))]
    unsafe {
        // Portable aarch64: dc cvau (clean D-cache to PoU) + ic ivau
        // (invalidate I-cache to PoU) per cache line, then dsb + isb. We use a
        // conservative 64-byte line (the architecturally-safe minimum granule).
        let start = ptr as usize;
        let end = start + len;
        let mut line = start & !63;
        while line < end {
            core::arch::asm!(
                "dc cvau, {0}",
                "ic ivau, {0}",
                in(reg) line,
                options(nostack, preserves_flags)
            );
            line += 64;
        }
        core::arch::asm!("dsb ish", "isb", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        // x86_64 is self-coherent (no explicit icache flush needed).
        let _ = (ptr, len);
    }
}
