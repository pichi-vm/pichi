// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

//! FUSE protocol: wire structs, the nodeid/handle tables, and opcode dispatch.
//!
//! virtio-fs carries the ordinary Linux FUSE protocol over virtqueues instead
//! of `/dev/fuse`. Each request is a descriptor chain whose device-readable
//! descriptors hold `fuse_in_header` + op-specific input, and whose
//! device-writable descriptors receive `fuse_out_header` + op-specific output.
//! [`dispatch`] consumes a flattened input buffer and returns the full reply
//! bytes (`None` for the few replyless ops: FORGET / BATCH_FORGET / INTERRUPT).
//!
//! Scope: a **read-only** server. Every mutating opcode is answered `EROFS`
//! before the backend is consulted; only the read path (INIT/LOOKUP/GETATTR/
//! OPEN/READ/READDIR/READLINK/STATFS and bookkeeping) is implemented. Unknown
//! opcodes return `ENOSYS`, which the guest kernel caches as "unsupported".

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use crate::backend::{Attr, FsBackend};

// --- protocol version we present ------------------------------------------
const FUSE_KERNEL_VERSION: u32 = 7;
const FUSE_KERNEL_MINOR_VERSION: u32 = 31;

// --- opcodes (subset; see include/uapi/linux/fuse.h) ----------------------
const FUSE_LOOKUP: u32 = 1;
const FUSE_FORGET: u32 = 2;
const FUSE_GETATTR: u32 = 3;
const FUSE_READLINK: u32 = 5;
const FUSE_OPEN: u32 = 14;
const FUSE_READ: u32 = 15;
const FUSE_STATFS: u32 = 17;
const FUSE_RELEASE: u32 = 18;
const FUSE_FSYNC: u32 = 20;
const FUSE_GETXATTR: u32 = 22;
const FUSE_LISTXATTR: u32 = 23;
const FUSE_FLUSH: u32 = 25;
const FUSE_INIT: u32 = 26;
const FUSE_OPENDIR: u32 = 27;
const FUSE_READDIR: u32 = 28;
const FUSE_RELEASEDIR: u32 = 29;
const FUSE_FSYNCDIR: u32 = 30;
const FUSE_ACCESS: u32 = 34;
const FUSE_INTERRUPT: u32 = 36;
const FUSE_DESTROY: u32 = 38;
const FUSE_BATCH_FORGET: u32 = 42;
const FUSE_LSEEK: u32 = 46;
const FUSE_SYNCFS: u32 = 50;

// --- errno values returned to the guest (negated on the wire) -------------
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EBADF: i32 = 9;
const EACCES: i32 = 13;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;
const ENOSYS: i32 = 38;

const FUSE_IN_HEADER_LEN: usize = 40;
const FUSE_OUT_HEADER_LEN: usize = 16;
const FUSE_NAME_OFFSET: usize = 24; // offsetof(fuse_dirent, name)
const FUSE_ROOT_ID: u64 = 1;

/// `open(2)` access-mode mask; a nonzero masked value means write intent.
const O_ACCMODE: u32 = 0o3;

/// Cap a single READ/READDIR payload so a hostile descriptor length can't make
/// us allocate without bound. Larger than any realistic FUSE max_read.
const MAX_PAYLOAD: u32 = 1 << 20;

// ---------------------------------------------------------------------------
// Little-endian readers + a growable reply writer.
// ---------------------------------------------------------------------------

fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().expect("4 bytes")))
}

fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().expect("8 bytes")))
}

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn with_header() -> Self {
        // Reserve the out_header; filled in by `finish`.
        Self {
            buf: vec![0u8; FUSE_OUT_HEADER_LEN],
        }
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    fn pad(&mut self, n: usize) {
        let new_len = self.buf.len() + n;
        self.buf.resize(new_len, 0);
    }
    /// Stamp the out_header (len, error=0, unique) and return the buffer.
    fn finish(mut self, unique: u64) -> Vec<u8> {
        let len = self.buf.len() as u32;
        self.buf[0..4].copy_from_slice(&len.to_le_bytes());
        self.buf[4..8].copy_from_slice(&0i32.to_le_bytes());
        self.buf[8..16].copy_from_slice(&unique.to_le_bytes());
        self.buf
    }
}

/// A bare `fuse_out_header` carrying an error (negated errno) and no payload.
fn error_reply(unique: u64, errno: i32) -> Vec<u8> {
    let mut buf = vec![0u8; FUSE_OUT_HEADER_LEN];
    buf[0..4].copy_from_slice(&(FUSE_OUT_HEADER_LEN as u32).to_le_bytes());
    buf[4..8].copy_from_slice(&(-errno).to_le_bytes());
    buf[8..16].copy_from_slice(&unique.to_le_bytes());
    buf
}

fn errno_of(e: &io::Error) -> i32 {
    if let Some(raw) = e.raw_os_error() {
        return raw;
    }
    match e.kind() {
        io::ErrorKind::NotFound => ENOENT,
        io::ErrorKind::PermissionDenied => EACCES,
        io::ErrorKind::InvalidInput => EINVAL,
        _ => EIO,
    }
}

fn write_attr(w: &mut Writer, a: &Attr) {
    w.u64(a.ino);
    w.u64(a.size);
    w.u64(a.blocks);
    w.u64(0); // atime
    w.u64(a.mtime);
    w.u64(0); // ctime
    w.u32(0); // atimensec
    w.u32(a.mtime_nsec);
    w.u32(0); // ctimensec
    w.u32(a.mode);
    w.u32(a.nlink);
    w.u32(a.uid);
    w.u32(a.gid);
    w.u32(a.rdev);
    w.u32(4096); // blksize
    w.u32(0); // flags
}

// ---------------------------------------------------------------------------
// Server state: nodeid ↔ relative-path tables and open handles.
// ---------------------------------------------------------------------------

struct Node {
    rel: PathBuf,
    lookups: u64,
}

enum Handle {
    File(PathBuf),
    Dir(Vec<DirentRec>),
}

struct DirentRec {
    ino: u64,
    name: String,
    dt: u32,
}

/// All mutable virtio-fs server state, guarded by one mutex in the device.
pub(crate) struct FsState {
    nodes: HashMap<u64, Node>,
    by_path: HashMap<PathBuf, u64>,
    next_node: u64,
    handles: HashMap<u64, Handle>,
    next_fh: u64,
}

impl FsState {
    pub(crate) fn new() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            FUSE_ROOT_ID,
            Node {
                rel: PathBuf::new(),
                lookups: 1,
            },
        );
        let mut by_path = HashMap::new();
        by_path.insert(PathBuf::new(), FUSE_ROOT_ID);
        Self {
            nodes,
            by_path,
            next_node: 2,
            handles: HashMap::new(),
            next_fh: 1,
        }
    }

    fn rel_of(&self, nodeid: u64) -> Option<PathBuf> {
        self.nodes.get(&nodeid).map(|n| n.rel.clone())
    }

    /// Return the nodeid for `rel`, allocating one on first lookup. Each call
    /// bumps the kernel-visible lookup count (balanced by FORGET).
    fn intern(&mut self, rel: PathBuf) -> u64 {
        if let Some(&id) = self.by_path.get(&rel) {
            if let Some(node) = self.nodes.get_mut(&id) {
                node.lookups += 1;
            }
            return id;
        }
        let id = self.next_node;
        self.next_node += 1;
        self.by_path.insert(rel.clone(), id);
        self.nodes.insert(id, Node { rel, lookups: 1 });
        id
    }

    fn forget(&mut self, nodeid: u64, nlookup: u64) {
        if nodeid == FUSE_ROOT_ID {
            return;
        }
        if let Some(node) = self.nodes.get_mut(&nodeid) {
            node.lookups = node.lookups.saturating_sub(nlookup);
            if node.lookups == 0 {
                let rel = node.rel.clone();
                self.nodes.remove(&nodeid);
                self.by_path.remove(&rel);
            }
        }
    }

    fn add_handle(&mut self, h: Handle) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, h);
        fh
    }
}

// ---------------------------------------------------------------------------
// Dispatch.
// ---------------------------------------------------------------------------

/// Parse one FUSE request from `input` and produce its reply bytes. Returns
/// `None` for replyless opcodes. `input` is the concatenation of the request's
/// device-readable descriptors (header + op input).
pub(crate) fn dispatch(
    state: &mut FsState,
    backend: &dyn FsBackend,
    input: &[u8],
) -> Option<Vec<u8>> {
    if input.len() < FUSE_IN_HEADER_LEN {
        // Can't even read a header → nothing addressable to reply to.
        return None;
    }
    let opcode = rd_u32(input, 4)?;
    let unique = rd_u64(input, 8)?;
    let nodeid = rd_u64(input, 16)?;
    let args = &input[FUSE_IN_HEADER_LEN..];

    match opcode {
        FUSE_INIT => Some(handle_init(unique, args)),
        FUSE_GETATTR => Some(handle_getattr(state, backend, unique, nodeid)),
        FUSE_LOOKUP => Some(handle_lookup(state, backend, unique, nodeid, args)),
        FUSE_FORGET => {
            let nlookup = rd_u64(args, 0).unwrap_or(0);
            state.forget(nodeid, nlookup);
            None
        }
        FUSE_BATCH_FORGET => {
            handle_batch_forget(state, args);
            None
        }
        FUSE_OPEN => Some(handle_open(state, backend, unique, nodeid, args)),
        FUSE_READ => Some(handle_read(state, backend, unique, args)),
        FUSE_RELEASE | FUSE_RELEASEDIR => {
            let fh = rd_u64(args, 0).unwrap_or(0);
            state.handles.remove(&fh);
            Some(Writer::with_header().finish(unique))
        }
        FUSE_OPENDIR => Some(handle_opendir(state, backend, unique, nodeid)),
        FUSE_READDIR => Some(handle_readdir(state, unique, args)),
        FUSE_READLINK => Some(handle_readlink(state, backend, unique, nodeid)),
        FUSE_STATFS => Some(handle_statfs(unique)),
        // Nothing to do on a read-only share; acknowledge success.
        FUSE_FLUSH | FUSE_FSYNC | FUSE_FSYNCDIR | FUSE_SYNCFS | FUSE_ACCESS | FUSE_DESTROY => {
            Some(Writer::with_header().finish(unique))
        }
        FUSE_INTERRUPT => None, // best-effort: we never leave a request pending
        // xattrs unsupported → ENOSYS so the kernel stops asking.
        FUSE_GETXATTR | FUSE_LISTXATTR | FUSE_LSEEK => Some(error_reply(unique, ENOSYS)),
        // Every mutating opcode: read-only share.
        op if is_mutating(op) => Some(error_reply(unique, EROFS)),
        _ => Some(error_reply(unique, ENOSYS)),
    }
}

/// Opcodes that would modify the filesystem; all rejected with EROFS.
fn is_mutating(op: u32) -> bool {
    // SETATTR=4, SYMLINK=6, MKNOD=8, MKDIR=9, UNLINK=10, RMDIR=11, RENAME=12,
    // LINK=13, WRITE=16, SETXATTR=21, REMOVEXATTR=24, CREATE=35, FALLOCATE=43,
    // RENAME2=45, COPY_FILE_RANGE=47, SETUPMAPPING=48, REMOVEMAPPING=49.
    matches!(
        op,
        4 | 6 | 8 | 9 | 10 | 11 | 12 | 13 | 16 | 21 | 24 | 35 | 43 | 45 | 47 | 48 | 49
    )
}

fn handle_init(unique: u64, args: &[u8]) -> Vec<u8> {
    // fuse_init_in: major, minor, max_readahead, flags (we only need minor).
    let their_minor = rd_u32(args, 4).unwrap_or(FUSE_KERNEL_MINOR_VERSION);
    let minor = their_minor.min(FUSE_KERNEL_MINOR_VERSION);
    let max_readahead = rd_u32(args, 8).unwrap_or(0);

    let mut w = Writer::with_header();
    w.u32(FUSE_KERNEL_VERSION); // major
    w.u32(minor); // minor
    w.u32(max_readahead); // max_readahead (echo)
    w.u32(0); // flags (negotiate nothing extra)
    w.bytes(&0u16.to_le_bytes()); // max_background
    w.bytes(&0u16.to_le_bytes()); // congestion_threshold
    w.u32(MAX_PAYLOAD); // max_write
    w.u32(1); // time_gran (1 ns)
    w.bytes(&0u16.to_le_bytes()); // max_pages
    w.bytes(&0u16.to_le_bytes()); // map_alignment
    w.u32(0); // flags2
    w.pad(7 * 4); // unused[7]
    w.finish(unique)
}

fn handle_getattr(
    state: &mut FsState,
    backend: &dyn FsBackend,
    unique: u64,
    nodeid: u64,
) -> Vec<u8> {
    let Some(rel) = state.rel_of(nodeid) else {
        return error_reply(unique, ENOENT);
    };
    match backend.stat(&rel) {
        Ok(attr) => {
            let mut w = Writer::with_header();
            w.u64(1); // attr_valid (seconds)
            w.u32(0); // attr_valid_nsec
            w.u32(0); // dummy
            write_attr(&mut w, &attr);
            w.finish(unique)
        }
        Err(e) => error_reply(unique, errno_of(&e)),
    }
}

fn handle_lookup(
    state: &mut FsState,
    backend: &dyn FsBackend,
    unique: u64,
    nodeid: u64,
    args: &[u8],
) -> Vec<u8> {
    let Some(parent) = state.rel_of(nodeid) else {
        return error_reply(unique, ENOENT);
    };
    // Name is a NUL-terminated string filling the rest of the request.
    let name_bytes = match args.iter().position(|&b| b == 0) {
        Some(end) => &args[..end],
        None => args,
    };
    let Ok(name) = std::str::from_utf8(name_bytes) else {
        return error_reply(unique, ENOENT);
    };
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return error_reply(unique, EINVAL);
    }
    let rel = parent.join(name);
    match backend.stat(&rel) {
        Ok(attr) => {
            let child = state.intern(rel);
            let mut w = Writer::with_header();
            w.u64(child); // nodeid
            w.u64(0); // generation
            w.u64(1); // entry_valid (seconds)
            w.u64(1); // attr_valid (seconds)
            w.u32(0); // entry_valid_nsec
            w.u32(0); // attr_valid_nsec
            write_attr(&mut w, &attr);
            w.finish(unique)
        }
        Err(e) => error_reply(unique, errno_of(&e)),
    }
}

fn handle_batch_forget(state: &mut FsState, args: &[u8]) {
    let Some(count) = rd_u32(args, 0) else { return };
    // fuse_batch_forget_in is { count u32, dummy u32 }; entries follow.
    let mut off = 8;
    for _ in 0..count {
        let (Some(nodeid), Some(nlookup)) = (rd_u64(args, off), rd_u64(args, off + 8)) else {
            break;
        };
        state.forget(nodeid, nlookup);
        off += 16;
    }
}

fn handle_open(
    state: &mut FsState,
    backend: &dyn FsBackend,
    unique: u64,
    nodeid: u64,
    args: &[u8],
) -> Vec<u8> {
    // fuse_open_in { flags u32, open_flags u32 }; reject any write intent.
    let flags = rd_u32(args, 0).unwrap_or(0);
    if flags & O_ACCMODE != 0 {
        return error_reply(unique, EROFS);
    }
    let Some(rel) = state.rel_of(nodeid) else {
        return error_reply(unique, ENOENT);
    };
    match backend.stat(&rel) {
        Ok(attr) if attr.is_dir() => return error_reply(unique, EPERM),
        Ok(_) => {}
        Err(e) => return error_reply(unique, errno_of(&e)),
    }
    let fh = state.add_handle(Handle::File(rel));
    open_reply(unique, fh)
}

fn handle_opendir(
    state: &mut FsState,
    backend: &dyn FsBackend,
    unique: u64,
    nodeid: u64,
) -> Vec<u8> {
    let Some(rel) = state.rel_of(nodeid) else {
        return error_reply(unique, ENOENT);
    };
    let self_ino = backend.stat(&rel).map_or(nodeid, |a| a.ino);
    let entries = match backend.readdir(&rel) {
        Ok(entries) => entries,
        Err(e) => return error_reply(unique, errno_of(&e)),
    };
    // Snapshot the listing so READDIR offsets are stable across calls. The
    // synthesized "." / ".." come first.
    let mut recs = vec![
        DirentRec {
            ino: self_ino,
            name: ".".to_string(),
            dt: crate::backend::DT_DIR,
        },
        DirentRec {
            ino: self_ino,
            name: "..".to_string(),
            dt: crate::backend::DT_DIR,
        },
    ];
    for e in entries {
        recs.push(DirentRec {
            ino: e.attr.ino,
            name: e.name,
            dt: e.attr.dirent_type(),
        });
    }
    let fh = state.add_handle(Handle::Dir(recs));
    open_reply(unique, fh)
}

fn open_reply(unique: u64, fh: u64) -> Vec<u8> {
    let mut w = Writer::with_header();
    w.u64(fh);
    w.u32(0); // open_flags
    w.u32(0); // padding
    w.finish(unique)
}

fn handle_read(state: &mut FsState, backend: &dyn FsBackend, unique: u64, args: &[u8]) -> Vec<u8> {
    // fuse_read_in { fh u64, offset u64, size u32, ... }.
    let (Some(fh), Some(offset), Some(size)) = (rd_u64(args, 0), rd_u64(args, 8), rd_u32(args, 16))
    else {
        return error_reply(unique, EINVAL);
    };
    let size = size.min(MAX_PAYLOAD);
    let rel = match state.handles.get(&fh) {
        Some(Handle::File(rel)) => rel.clone(),
        Some(Handle::Dir(_)) => return error_reply(unique, EBADF),
        None => return error_reply(unique, EBADF),
    };
    match backend.read(&rel, offset, size) {
        Ok(data) => {
            let mut w = Writer::with_header();
            w.bytes(&data);
            w.finish(unique)
        }
        Err(e) => error_reply(unique, errno_of(&e)),
    }
}

fn handle_readdir(state: &mut FsState, unique: u64, args: &[u8]) -> Vec<u8> {
    let (Some(fh), Some(offset), Some(size)) = (rd_u64(args, 0), rd_u64(args, 8), rd_u32(args, 16))
    else {
        return error_reply(unique, EINVAL);
    };
    let size = size.min(MAX_PAYLOAD) as usize;
    let Some(Handle::Dir(recs)) = state.handles.get(&fh) else {
        return error_reply(unique, EBADF);
    };
    let mut w = Writer::with_header();
    let mut emitted = 0usize;
    // Each entry's implicit offset is its 1-based index; the guest passes back
    // the last offset it consumed.
    for (i, rec) in recs.iter().enumerate() {
        let entry_off = (i as u64) + 1;
        if entry_off <= offset {
            continue;
        }
        let padded = dirent_size(rec.name.len());
        if emitted + padded > size {
            break;
        }
        push_dirent(&mut w, rec.ino, entry_off, rec.dt, &rec.name);
        emitted += padded;
    }
    w.finish(unique)
}

fn dirent_size(namelen: usize) -> usize {
    let total = FUSE_NAME_OFFSET + namelen;
    total.div_ceil(8) * 8
}

fn push_dirent(w: &mut Writer, ino: u64, off: u64, dt: u32, name: &str) {
    w.u64(ino);
    w.u64(off);
    w.u32(name.len() as u32);
    w.u32(dt);
    w.bytes(name.as_bytes());
    let pad = dirent_size(name.len()) - (FUSE_NAME_OFFSET + name.len());
    w.pad(pad);
}

fn handle_readlink(
    state: &mut FsState,
    backend: &dyn FsBackend,
    unique: u64,
    nodeid: u64,
) -> Vec<u8> {
    let Some(rel) = state.rel_of(nodeid) else {
        return error_reply(unique, ENOENT);
    };
    match backend.readlink(&rel) {
        Ok(target) => {
            let mut w = Writer::with_header();
            w.bytes(&target);
            w.finish(unique)
        }
        Err(e) => error_reply(unique, errno_of(&e)),
    }
}

fn handle_statfs(unique: u64) -> Vec<u8> {
    // fuse_statfs_out wraps a kstatfs { blocks, bfree, bavail, files, ffree u64;
    // bsize, namelen, frsize u32; padding u32; spare[6] u32 }. Plausible values
    // for a read-only share.
    let mut w = Writer::with_header();
    w.u64(0); // blocks
    w.u64(0); // bfree
    w.u64(0); // bavail
    w.u64(0); // files
    w.u64(0); // ffree
    w.u32(4096); // bsize
    w.u32(255); // namelen
    w.u32(4096); // frsize
    w.u32(0); // padding
    w.pad(6 * 4); // spare[6]
    w.finish(unique)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{DT_DIR, DT_REG, MapFs, S_IFMT, S_IFREG};

    /// Build a minimal `fuse_in_header` + args input buffer.
    fn request(opcode: u32, unique: u64, nodeid: u64, args: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        let len = (FUSE_IN_HEADER_LEN + args.len()) as u32;
        b.extend_from_slice(&len.to_le_bytes());
        b.extend_from_slice(&opcode.to_le_bytes());
        b.extend_from_slice(&unique.to_le_bytes());
        b.extend_from_slice(&nodeid.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // uid
        b.extend_from_slice(&0u32.to_le_bytes()); // gid
        b.extend_from_slice(&0u32.to_le_bytes()); // pid
        b.extend_from_slice(&0u32.to_le_bytes()); // total_extlen + padding
        b.extend_from_slice(args);
        b
    }

    fn out_error(reply: &[u8]) -> i32 {
        i32::from_le_bytes(reply[4..8].try_into().unwrap())
    }

    #[test]
    fn init_returns_our_version() {
        let mut state = FsState::new();
        let fs = MapFs::new();
        // fuse_init_in: major=7, minor=99, max_readahead=0, flags=0.
        let mut args = Vec::new();
        args.extend_from_slice(&7u32.to_le_bytes());
        args.extend_from_slice(&99u32.to_le_bytes());
        args.extend_from_slice(&0u32.to_le_bytes());
        args.extend_from_slice(&0u32.to_le_bytes());
        let reply = dispatch(&mut state, &fs, &request(FUSE_INIT, 1, 0, &args)).unwrap();
        assert_eq!(out_error(&reply), 0);
        let major = rd_u32(&reply, FUSE_OUT_HEADER_LEN).unwrap();
        let minor = rd_u32(&reply, FUSE_OUT_HEADER_LEN + 4).unwrap();
        assert_eq!(major, FUSE_KERNEL_VERSION);
        // Negotiated down to ours.
        assert_eq!(minor, FUSE_KERNEL_MINOR_VERSION);
    }

    #[test]
    fn getattr_root_is_directory() {
        let mut state = FsState::new();
        let fs = MapFs::new().with_file("a.txt", "a");
        let reply = dispatch(
            &mut state,
            &fs,
            &request(FUSE_GETATTR, 2, FUSE_ROOT_ID, &[]),
        )
        .unwrap();
        assert_eq!(out_error(&reply), 0);
        // attr starts after out_header + fuse_attr_out preamble (16 bytes).
        let mode = rd_u32(&reply, FUSE_OUT_HEADER_LEN + 16 + 8 * 6 + 4 * 3).unwrap();
        assert_eq!(mode & S_IFMT, crate::backend::S_IFDIR);
    }

    #[test]
    fn lookup_then_open_then_read_roundtrips() {
        let mut state = FsState::new();
        let fs = MapFs::new().with_file("hello.txt", "hi there");

        // LOOKUP "hello.txt" under root.
        let mut name = b"hello.txt".to_vec();
        name.push(0);
        let reply = dispatch(
            &mut state,
            &fs,
            &request(FUSE_LOOKUP, 3, FUSE_ROOT_ID, &name),
        )
        .unwrap();
        assert_eq!(out_error(&reply), 0);
        let child = rd_u64(&reply, FUSE_OUT_HEADER_LEN).unwrap();
        assert!(child >= 2);
        // attr.mode lives after the 40-byte fuse_entry_out preamble.
        let mode = rd_u32(&reply, FUSE_OUT_HEADER_LEN + 40 + 8 * 6 + 4 * 3).unwrap();
        assert_eq!(mode & S_IFMT, S_IFREG);

        // OPEN it read-only.
        let mut open_in = Vec::new();
        open_in.extend_from_slice(&0u32.to_le_bytes()); // flags = O_RDONLY
        open_in.extend_from_slice(&0u32.to_le_bytes());
        let reply = dispatch(&mut state, &fs, &request(FUSE_OPEN, 4, child, &open_in)).unwrap();
        assert_eq!(out_error(&reply), 0);
        let fh = rd_u64(&reply, FUSE_OUT_HEADER_LEN).unwrap();

        // READ the whole file.
        let mut read_in = Vec::new();
        read_in.extend_from_slice(&fh.to_le_bytes());
        read_in.extend_from_slice(&0u64.to_le_bytes()); // offset
        read_in.extend_from_slice(&64u32.to_le_bytes()); // size
        read_in.extend_from_slice(&0u32.to_le_bytes()); // read_flags
        read_in.extend_from_slice(&0u64.to_le_bytes()); // lock_owner
        read_in.extend_from_slice(&0u32.to_le_bytes()); // flags
        read_in.extend_from_slice(&0u32.to_le_bytes()); // padding
        let reply = dispatch(&mut state, &fs, &request(FUSE_READ, 5, child, &read_in)).unwrap();
        assert_eq!(out_error(&reply), 0);
        assert_eq!(&reply[FUSE_OUT_HEADER_LEN..], b"hi there");
    }

    #[test]
    fn lookup_missing_is_enoent() {
        let mut state = FsState::new();
        let fs = MapFs::new();
        let mut name = b"nope".to_vec();
        name.push(0);
        let reply = dispatch(
            &mut state,
            &fs,
            &request(FUSE_LOOKUP, 6, FUSE_ROOT_ID, &name),
        )
        .unwrap();
        assert_eq!(out_error(&reply), -ENOENT);
    }

    #[test]
    fn opendir_readdir_lists_entries() {
        let mut state = FsState::new();
        let fs = MapFs::new().with_file("a.txt", "a").with_file("b.txt", "b");
        let reply = dispatch(
            &mut state,
            &fs,
            &request(FUSE_OPENDIR, 7, FUSE_ROOT_ID, &[]),
        )
        .unwrap();
        assert_eq!(out_error(&reply), 0);
        let fh = rd_u64(&reply, FUSE_OUT_HEADER_LEN).unwrap();

        let mut read_in = Vec::new();
        read_in.extend_from_slice(&fh.to_le_bytes());
        read_in.extend_from_slice(&0u64.to_le_bytes()); // offset
        read_in.extend_from_slice(&4096u32.to_le_bytes()); // size
        read_in.extend_from_slice(&[0u8; 12]); // rest of fuse_read_in
        let reply = dispatch(
            &mut state,
            &fs,
            &request(FUSE_READDIR, 8, FUSE_ROOT_ID, &read_in),
        )
        .unwrap();
        assert_eq!(out_error(&reply), 0);

        // Walk the dirent stream and collect names.
        let mut names = Vec::new();
        let mut off = FUSE_OUT_HEADER_LEN;
        while off + FUSE_NAME_OFFSET <= reply.len() {
            let namelen = rd_u32(&reply, off + 16).unwrap() as usize;
            let dt = rd_u32(&reply, off + 20).unwrap();
            let name = std::str::from_utf8(
                &reply[off + FUSE_NAME_OFFSET..off + FUSE_NAME_OFFSET + namelen],
            )
            .unwrap()
            .to_string();
            names.push((name, dt));
            off += dirent_size(namelen);
        }
        assert!(names.iter().any(|(n, dt)| n == "." && *dt == DT_DIR));
        assert!(names.iter().any(|(n, dt)| n == "a.txt" && *dt == DT_REG));
        assert!(names.iter().any(|(n, _)| n == "b.txt"));
    }

    #[test]
    fn open_for_write_is_erofs() {
        let mut state = FsState::new();
        let fs = MapFs::new().with_file("a.txt", "a");
        let mut name = b"a.txt".to_vec();
        name.push(0);
        let reply = dispatch(
            &mut state,
            &fs,
            &request(FUSE_LOOKUP, 9, FUSE_ROOT_ID, &name),
        )
        .unwrap();
        let child = rd_u64(&reply, FUSE_OUT_HEADER_LEN).unwrap();
        let mut open_in = Vec::new();
        open_in.extend_from_slice(&2u32.to_le_bytes()); // O_RDWR
        open_in.extend_from_slice(&0u32.to_le_bytes());
        let reply = dispatch(&mut state, &fs, &request(FUSE_OPEN, 10, child, &open_in)).unwrap();
        assert_eq!(out_error(&reply), -EROFS);
    }

    #[test]
    fn write_opcode_is_erofs() {
        let mut state = FsState::new();
        let fs = MapFs::new();
        // FUSE_WRITE = 16.
        let reply = dispatch(&mut state, &fs, &request(16, 11, FUSE_ROOT_ID, &[0u8; 40])).unwrap();
        assert_eq!(out_error(&reply), -EROFS);
    }

    #[test]
    fn unknown_opcode_is_enosys() {
        let mut state = FsState::new();
        let fs = MapFs::new();
        let reply = dispatch(&mut state, &fs, &request(9999, 12, FUSE_ROOT_ID, &[])).unwrap();
        assert_eq!(out_error(&reply), -ENOSYS);
    }

    #[test]
    fn forget_is_replyless() {
        let mut state = FsState::new();
        let fs = MapFs::new();
        let args = 1u64.to_le_bytes();
        assert!(
            dispatch(
                &mut state,
                &fs,
                &request(FUSE_FORGET, 13, FUSE_ROOT_ID, &args)
            )
            .is_none()
        );
    }
}
