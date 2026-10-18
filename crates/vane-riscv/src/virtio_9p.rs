//! virtio-9p host-filesystem sharing (PLAN_VIRTIO_WEVAL workstream C /
//! milestone V3).
//!
//! A single-virtqueue device speaking **9P2000.L** (the Linux `9p` client's
//! dialect), backed by a pluggable [`Host9p`] filesystem. The device is
//! host-agnostic; the backend maps 9P operations onto a host fs — natively a
//! confined directory, or the os-emulation `AsyncUnixHost` fs surface
//! (openat/getdents64/fstat/read/write/lseek) via an adapter, so the same
//! device is served by a native, WASI, or filtered os-emulation backend.
//!
//! Implemented message subset (mount + ls + read + write + create):
//! Tversion, Tattach, Twalk, Tstatfs, Tgetattr, Tlopen, Tlcreate, Tread,
//! Twrite, Treaddir, Tclunk, Tflush, Tmkdir. All paths are confined beneath
//! the backend's root (no `..` escape).

use crate::virtio::{DEV_9P, DescriptorChain, QueueMem, VirtioDevice};
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// 9P2000.L message types.
const T_STATFS: u8 = 8;
const T_LOPEN: u8 = 12;
const T_LCREATE: u8 = 32;
const T_GETATTR: u8 = 24;
const T_READDIR: u8 = 40;
const T_MKDIR: u8 = 72;
const T_VERSION: u8 = 100;
const T_ATTACH: u8 = 104;
const T_FLUSH: u8 = 108;
const T_WALK: u8 = 110;
const T_READ: u8 = 116;
const T_WRITE: u8 = 118;
const T_CLUNK: u8 = 120;

const R_LERROR: u8 = 7;

// QID types.
const QT_DIR: u8 = 0x80;
const QT_FILE: u8 = 0x00;

/// A 9P QID (unique file identifier).
#[derive(Clone, Copy)]
struct Qid {
    qtype: u8,
    version: u32,
    path: u64,
}

/// File metadata the device reports (a 9P2000.L getattr result subset).
#[derive(Clone, Copy, Default)]
pub struct Meta9p {
    pub mode: u32,
    pub size: u64,
    pub is_dir: bool,
}

/// The host filesystem the 9p device serves. All operations are confined to
/// the backend's root by the device. Sync: a local backend completes
/// immediately; a non-local one would defer via the transport's
/// `process_or_defer` (as virtio-net does).
pub trait Host9p {
    /// Metadata for a root-relative path (Err(ENOENT) if absent).
    fn stat(&mut self, path: &str) -> Result<Meta9p, i32>;
    /// Open a root-relative path; returns a handle. `flags` are Linux open
    /// flags (O_RDONLY/O_WRONLY/O_RDWR/O_CREAT/...).
    fn open(&mut self, path: &str, flags: u32) -> Result<u64, i32>;
    /// Read into `buf` at `offset`; returns bytes read.
    fn read(&mut self, handle: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32>;
    /// Write `data` at `offset`; returns bytes written.
    fn write(&mut self, handle: u64, offset: u64, data: &[u8]) -> Result<usize, i32>;
    /// Create a directory.
    fn mkdir(&mut self, path: &str, mode: u32) -> Result<(), i32>;
    /// List a directory's entries as (name, is_dir).
    fn readdir(&mut self, path: &str) -> Result<Vec<(String, bool)>, i32>;
    /// Close a handle.
    fn close(&mut self, handle: u64);
    /// Total/free blocks for statfs (in 512-byte units is fine).
    fn statfs(&mut self) -> (u64, u64) {
        (1 << 20, 1 << 20)
    }
}

/// Per-fid state.
struct Fid {
    /// Root-relative path this fid refers to.
    path: String,
    /// Open handle, set by lopen/lcreate.
    handle: Option<u64>,
    /// QID reported for this fid.
    qid: Qid,
}

/// A virtio-9p device over a [`Host9p`] backend.
pub struct Virtio9p<B: Host9p> {
    backend: B,
    fids: BTreeMap<u32, Fid>,
    /// Monotonic QID path counter (unique per fid).
    next_qid: u64,
    /// Negotiated msize.
    msize: u32,
    /// Mount tag (the guest mounts `-t 9p <tag>`).
    tag: String,
}

impl<B: Host9p> Virtio9p<B> {
    pub fn new(backend: B, tag: &str) -> Self {
        Virtio9p {
            backend,
            fids: BTreeMap::new(),
            next_qid: 1,
            msize: 8192,
            tag: tag.to_string(),
        }
    }

    fn alloc_qid(&mut self, is_dir: bool) -> Qid {
        let q = Qid {
            qtype: if is_dir { QT_DIR } else { QT_FILE },
            version: 0,
            path: self.next_qid,
        };
        self.next_qid += 1;
        q
    }
}

// ── 9P wire helpers ─────────────────────────────────────────────────────────

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn u8(&mut self) -> u8 {
        let v = self.b.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        v
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    fn u64(&mut self) -> u64 {
        let lo = self.u32() as u64;
        let hi = self.u32() as u64;
        lo | (hi << 32)
    }
    fn str(&mut self) -> String {
        let n = self.u16() as usize;
        let mut s = Vec::new();
        for _ in 0..n {
            s.push(self.u8());
        }
        String::from_utf8_lossy(&s).into_owned()
    }
}

struct Writer {
    b: Vec<u8>,
}
impl Writer {
    fn new() -> Self {
        Writer { b: Vec::new() }
    }
    fn u8(&mut self, v: u8) {
        self.b.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u16(s.len() as u16);
        self.b.extend_from_slice(s.as_bytes());
    }
    fn bytes(&mut self, d: &[u8]) {
        self.b.extend_from_slice(d);
    }
    fn qid(&mut self, q: Qid) {
        self.u8(q.qtype);
        self.u32(q.version);
        self.u64(q.path);
    }
}

impl<B: Host9p> Virtio9p<B> {
    /// Handle one 9P request message, returning the response message body
    /// (type + tag + payload, without the leading size word — the caller
    /// prepends it).
    fn handle(&mut self, msg: &[u8]) -> Vec<u8> {
        let mut r = Reader { b: msg, pos: 0 };
        let mtype = r.u8();
        let tag = r.u16();
        let mut w = Writer::new();
        // Response type is request type + 1 (except errors → Rlerror).
        let ok = |w: &mut Writer| {
            w.u8(mtype + 1);
            w.u16(tag);
        };
        let err = |w: &mut Writer, e: i32| {
            w.u8(R_LERROR);
            w.u16(tag);
            w.u32((-e) as u32);
        };

        match mtype {
            T_VERSION => {
                let msize = r.u32();
                let _ver = r.str();
                self.msize = self.msize.min(msize);
                ok(&mut w);
                w.u32(self.msize);
                w.str("9P2000.L");
            }
            T_FLUSH => {
                ok(&mut w);
            }
            T_ATTACH => {
                let fid = r.u32();
                let _afid = r.u32();
                let _uname = r.str();
                let _aname = r.str();
                let _n_uname = r.u32();
                let qid = self.alloc_qid(true);
                self.fids.insert(
                    fid,
                    Fid {
                        path: String::new(),
                        handle: None,
                        qid,
                    },
                );
                ok(&mut w);
                w.qid(qid);
            }
            T_WALK => {
                let fid = r.u32();
                let newfid = r.u32();
                let nwnames = r.u16();
                let mut names = Vec::new();
                for _ in 0..nwnames {
                    names.push(r.str());
                }
                let Some(base) = self.fids.get(&fid).map(|f| f.path.clone()) else {
                    err(&mut w, 2);
                    return w.b;
                };
                let mut cur = base;
                let mut qids = Vec::new();
                let mut failed = false;
                for name in &names {
                    let joined = join(&cur, name);
                    match self.backend.stat(&joined) {
                        Ok(meta) => {
                            let qid = self.alloc_qid(meta.is_dir);
                            qids.push(qid);
                            cur = joined;
                        }
                        Err(e) => {
                            // Walk of zero names is a clone; a mid-walk miss
                            // errors unless some qids were produced.
                            if qids.is_empty() {
                                err(&mut w, e);
                                return w.b;
                            }
                            failed = true;
                            break;
                        }
                    }
                }
                if !failed {
                    let is_dir = self.backend.stat(&cur).map(|m| m.is_dir).unwrap_or(true);
                    let qid = if names.is_empty() {
                        self.fids.get(&fid).map(|f| f.qid).unwrap_or(Qid {
                            qtype: QT_DIR,
                            version: 0,
                            path: 0,
                        })
                    } else {
                        *qids.last().unwrap()
                    };
                    self.fids.insert(
                        newfid,
                        Fid {
                            path: cur,
                            handle: None,
                            qid: if names.is_empty() {
                                qid
                            } else {
                                let _ = is_dir;
                                qid
                            },
                        },
                    );
                }
                ok(&mut w);
                w.u16(qids.len() as u16);
                for q in qids {
                    w.qid(q);
                }
            }
            T_STATFS => {
                let _fid = r.u32();
                let (blocks, bfree) = self.backend.statfs();
                ok(&mut w);
                w.u32(0x0102_1997); // 9P magic (V9FS_MAGIC)
                w.u32(512); // bsize
                w.u64(blocks);
                w.u64(bfree);
                w.u64(0); // files
                w.u64(0); // ffree
                w.u64(0); // fsid
                w.u32(255); // namelen
            }
            T_GETATTR => {
                let fid = r.u32();
                let _mask = r.u64();
                let Some(f) = self.fids.get(&fid) else {
                    err(&mut w, 2);
                    return w.b;
                };
                match self.backend.stat(&f.path) {
                    Ok(meta) => {
                        ok(&mut w);
                        w.u64(u64::MAX); // valid mask
                        w.qid(f.qid);
                        w.u32(meta.mode | if meta.is_dir { 0o040000 } else { 0o100000 });
                        w.u32(1); // uid
                        w.u32(1); // gid
                        w.u64(1); // nlink
                        w.u64(0); // rdev
                        w.u64(meta.size);
                        w.u64(512); // blksize
                        w.u64(meta.size.div_ceil(512)); // blocks
                        for _ in 0..8 {
                            w.u64(0); // atime/mtime/ctime sec+nsec
                        }
                        w.u64(0); // btime
                        w.u64(0); // gen
                        w.u64(0); // data_version
                    }
                    Err(e) => {
                        err(&mut w, e);
                        return w.b;
                    }
                }
            }
            T_LOPEN | T_LCREATE => {
                let fid = r.u32();
                let flags = r.u32();
                if mtype == T_LCREATE {
                    let _name = r.str();
                    let _perm = r.u32();
                    let _gid = r.u32();
                }
                let Some(f) = self.fids.get(&fid).map(|f| f.path.clone()) else {
                    err(&mut w, 2);
                    return w.b;
                };
                match self.backend.open(&f, flags) {
                    Ok(handle) => {
                        if let Some(fe) = self.fids.get_mut(&fid) {
                            fe.handle = Some(handle);
                        }
                        ok(&mut w);
                        w.qid(Qid {
                            qtype: QT_FILE,
                            version: 0,
                            path: handle,
                        });
                        w.u32(8192); // iounit
                    }
                    Err(e) => {
                        err(&mut w, e);
                        return w.b;
                    }
                }
            }
            T_READ => {
                let fid = r.u32();
                let offset = r.u64();
                let count = r.u32();
                let Some(f) = self.fids.get(&fid) else {
                    err(&mut w, 2);
                    return w.b;
                };
                let Some(handle) = f.handle else {
                    err(&mut w, 9); // EBADF
                    return w.b;
                };
                let mut buf = alloc::vec![0u8; count.min(self.msize) as usize];
                match self.backend.read(handle, offset, &mut buf) {
                    Ok(n) => {
                        ok(&mut w);
                        w.u32(n as u32);
                        w.bytes(&buf[..n]);
                    }
                    Err(e) => {
                        err(&mut w, e);
                        return w.b;
                    }
                }
            }
            T_WRITE => {
                let fid = r.u32();
                let offset = r.u64();
                let count = r.u32() as usize;
                let data = &msg[r.pos..r.pos + count.min(msg.len().saturating_sub(r.pos))];
                let Some(f) = self.fids.get(&fid) else {
                    err(&mut w, 2);
                    return w.b;
                };
                let Some(handle) = f.handle else {
                    err(&mut w, 9);
                    return w.b;
                };
                match self.backend.write(handle, offset, data) {
                    Ok(n) => {
                        ok(&mut w);
                        w.u32(n as u32);
                    }
                    Err(e) => {
                        err(&mut w, e);
                        return w.b;
                    }
                }
            }
            T_READDIR => {
                let fid = r.u32();
                let offset = r.u64();
                let count = r.u32() as usize;
                let Some(f) = self.fids.get(&fid) else {
                    err(&mut w, 2);
                    return w.b;
                };
                match self.backend.readdir(&f.path) {
                    Ok(entries) => {
                        // 9P2000.L readdir entries: qid(13) offset(8) type(1)
                        // name(string). Emit those past `offset`, capped.
                        let mut body = Writer::new();
                        let mut pos = 0u64;
                        for (name, is_dir) in &entries {
                            let qid = Qid {
                                qtype: if *is_dir { QT_DIR } else { QT_FILE },
                                version: 0,
                                path: 0,
                            };
                            let mut e = Writer::new();
                            e.qid(qid);
                            e.u64(pos + 1); // next offset
                            e.u8(if *is_dir { 4 } else { 8 }); // DT_DIR/DT_REG
                            e.str(name);
                            let entry_len = e.b.len() as u64;
                            if pos >= offset && (body.b.len() + e.b.len()) <= count {
                                body.bytes(&e.b);
                            }
                            pos += entry_len.max(1);
                        }
                        ok(&mut w);
                        w.u32(body.b.len() as u32);
                        w.bytes(&body.b);
                    }
                    Err(e) => {
                        err(&mut w, e);
                        return w.b;
                    }
                }
            }
            T_MKDIR => {
                let fid = r.u32();
                let name = r.str();
                let mode = r.u32();
                let _gid = r.u32();
                let Some(f) = self.fids.get(&fid).map(|f| f.path.clone()) else {
                    err(&mut w, 2);
                    return w.b;
                };
                let path = join(&f, &name);
                match self.backend.mkdir(&path, mode) {
                    Ok(()) => {
                        let qid = self.alloc_qid(true);
                        ok(&mut w);
                        w.qid(qid);
                    }
                    Err(e) => {
                        err(&mut w, e);
                        return w.b;
                    }
                }
            }
            T_CLUNK => {
                let fid = r.u32();
                if let Some(f) = self.fids.remove(&fid)
                    && let Some(h) = f.handle
                {
                    self.backend.close(h);
                }
                ok(&mut w);
            }
            _ => {
                err(&mut w, 38); // ENOSYS
                return w.b;
            }
        }
        w.b
    }
}

/// Join a root-relative dir and a name, rejecting `..` escapes.
fn join(dir: &str, name: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for comp in name.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    parts.join("/")
}

impl<B: Host9p> VirtioDevice for Virtio9p<B> {
    fn device_id(&self) -> u32 {
        DEV_9P
    }
    fn num_queues(&self) -> usize {
        1
    }
    fn read_config(&self, offset: u64, buf: &mut [u8]) {
        // virtio-9p config: tag_len (u16) @ 0, then the tag bytes.
        for (i, b) in buf.iter_mut().enumerate() {
            let o = offset + i as u64;
            *b = if o == 0 {
                (self.tag.len() & 0xFF) as u8
            } else if o == 1 {
                ((self.tag.len() >> 8) & 0xFF) as u8
            } else {
                let ti = (o - 2) as usize;
                self.tag.as_bytes().get(ti).copied().unwrap_or(0)
            };
        }
    }

    fn process(&mut self, _queue: usize, chain: &DescriptorChain, mem: &mut dyn QueueMem) -> u32 {
        // The request is the readable buffers; the response goes to the first
        // writable buffer. 9P message = size(u32) + type(u8) + tag(u16) + ...
        let req = chain.read_request(mem);
        if req.len() < 7 {
            return 0;
        }
        let body = self.handle(&req[4..]); // skip the size word
        let mut resp = Writer::new();
        resp.u32((4 + body.len()) as u32); // size includes itself
        resp.bytes(&body);
        chain.write_response(mem, &resp.b)
    }
}
