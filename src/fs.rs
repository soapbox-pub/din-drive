//! The FUSE filesystem. Metadata operations are answered from the local tree;
//! reads that need the network run on the tokio runtime and reply when done.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use tokio::runtime::Handle as Runtime;

use crate::drive::{self, Drive, Kind2};
use crate::keys::ROOT;

const TTL: Duration = Duration::from_secs(1);

struct Inodes {
    by_ino: HashMap<u64, String>,
    by_id: HashMap<String, u64>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        let mut s = Self {
            by_ino: HashMap::new(),
            by_id: HashMap::new(),
            next: 2,
        };
        s.by_ino.insert(1, ROOT.to_string());
        s.by_id.insert(ROOT.to_string(), 1);
        s
    }

    fn ino(&mut self, d: &str) -> u64 {
        if let Some(&i) = self.by_id.get(d) {
            return i;
        }
        let i = self.next;
        self.next += 1;
        self.by_ino.insert(i, d.to_string());
        self.by_id.insert(d.to_string(), i);
        i
    }
}

struct OpenFile {
    d: String,
    /// The staging file, for handles opened for writing.
    file: Option<Arc<File>>,
    written: bool,
}

#[derive(Default)]
struct Handles {
    map: Mutex<HashMap<u64, OpenFile>>,
    next: AtomicU64,
}

impl Handles {
    fn add(&self, h: OpenFile) -> FileHandle {
        let fh = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.map.lock().unwrap().insert(fh, h);
        FileHandle(fh)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, OpenFile>> {
        self.map.lock().unwrap()
    }
}

pub struct DriveFs {
    drive: Arc<Drive>,
    rt: Runtime,
    inodes: Mutex<Inodes>,
    handles: Arc<Handles>,
    uid: u32,
    gid: u32,
}

fn errno(e: &anyhow::Error) -> Errno {
    match e.downcast_ref::<drive::Error>() {
        Some(drive::Error::NotFound) => Errno::ENOENT,
        Some(drive::Error::Exists) => Errno::EEXIST,
        Some(drive::Error::InvalidName) | Some(drive::Error::Invalid) => Errno::EINVAL,
        Some(drive::Error::NotEmpty) => Errno::ENOTEMPTY,
        Some(drive::Error::NotDir) => Errno::ENOTDIR,
        Some(drive::Error::IsDir) => Errno::EISDIR,
        Some(drive::Error::NeedsOpen) | Some(drive::Error::NoDrive) => Errno::EIO,
        None => {
            tracing::warn!("{e:#}");
            Errno::EIO
        }
    }
}

fn time(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

impl DriveFs {
    pub fn new(drive: Arc<Drive>, rt: Runtime) -> Self {
        Self {
            drive,
            rt,
            inodes: Mutex::new(Inodes::new()),
            handles: Arc::new(Handles::default()),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn id(&self, ino: INodeNo) -> Option<String> {
        self.inodes.lock().unwrap().by_ino.get(&ino.0).cloned()
    }

    fn ino(&self, d: &str) -> INodeNo {
        INodeNo(self.inodes.lock().unwrap().ino(d))
    }

    fn attr(&self, d: &str) -> Option<FileAttr> {
        if !self.drive.is_visible(d) {
            return None;
        }
        let info = self.drive.info(d)?;
        let (kind, perm, nlink) = match info.kind {
            Kind2::Dir => (FileType::Directory, 0o755, 2),
            Kind2::File => (FileType::RegularFile, 0o644, 1),
        };
        let t = time(info.mtime);
        Some(FileAttr {
            ino: self.ino(d),
            size: info.size,
            blocks: info.size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 65536,
            flags: 0,
        })
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<(String, String), Errno> {
        let p = self.id(parent).ok_or(Errno::ENOENT)?;
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        Ok((p, name.to_string()))
    }
}

impl Filesystem for DriveFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let (p, name) = match self.child(parent, name) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        match self.drive.lookup(&p, &name).and_then(|d| self.attr(&d)) {
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.id(ino).and_then(|d| self.attr(&d)) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(d) = self.id(ino) else {
            return reply.error(Errno::ENOENT);
        };
        if let Some(size) = size {
            let handle_file = fh.and_then(|fh| {
                let mut handles = self.handles.lock();
                let h = handles.get_mut(&fh.0)?;
                h.written = true;
                h.file.clone()
            });
            let result = match handle_file {
                Some(f) => f.set_len(size).map_err(anyhow::Error::from),
                None => self.rt.block_on(async {
                    let f = self.drive.open_write(&d, size == 0).await?;
                    f.set_len(size)?;
                    self.drive.release_write(&d, true)
                }),
            };
            if let Err(e) = result {
                return reply.error(errno(&e));
            }
        }
        if let Some(t) = mtime {
            let t = match t {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => SystemTime::now(),
            };
            let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            if let Err(e) = self.drive.set_mtime(&d, secs) {
                return reply.error(errno(&e));
            }
        }
        match self.attr(&d) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some(d) = self.id(ino) else {
            return reply.error(Errno::ENOENT);
        };
        if d != ROOT && !self.drive.content(&d).is_some_and(|c| c.is_dir()) {
            return reply.error(Errno::ENOTDIR);
        }
        let parent = self.drive.parent(&d).map(|p| self.ino(&p)).unwrap_or(INodeNo(1));
        let mut entries = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent, FileType::Directory, "..".to_string()),
        ];
        for (name, child) in self.drive.children(&d) {
            let kind = match self.drive.info(&child).map(|i| i.kind) {
                Some(Kind2::Dir) => FileType::Directory,
                Some(Kind2::File) => FileType::RegularFile,
                None => continue,
            };
            entries.push((self.ino(&child), kind, name));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let (p, name) = match self.child(parent, name) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        match self.drive.mkdir(&p, &name) {
            Ok(d) => match self.attr(&d) {
                Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
                None => reply.error(Errno::EIO),
            },
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let (p, name) = match self.child(parent, name) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        let result = self.drive.create_file(&p, &name).and_then(|d| {
            let f = self.rt.block_on(self.drive.open_write(&d, true))?;
            Ok((d, f))
        });
        match result {
            Ok((d, f)) => {
                let Some(attr) = self.attr(&d) else {
                    return reply.error(Errno::EIO);
                };
                let fh = self.handles.add(OpenFile {
                    d,
                    file: Some(Arc::new(f)),
                    written: true,
                });
                reply.created(&TTL, &attr, Generation(0), fh, FopenFlags::empty());
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(d) = self.id(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.drive.content(&d) {
            Some(c) if c.is_file() => {}
            Some(_) => return reply.error(Errno::EISDIR),
            None => return reply.error(Errno::ENOENT),
        }
        if flags.acc_mode() == OpenAccMode::O_RDONLY {
            let fh = self.handles.add(OpenFile {
                d,
                file: None,
                written: false,
            });
            return reply.opened(fh, FopenFlags::empty());
        }
        let truncate = flags.0 & libc::O_TRUNC != 0;
        let drive = self.drive.clone();
        let handles = self.handles.clone();
        self.rt.spawn(async move {
            match drive.open_write(&d, truncate).await {
                Ok(f) => {
                    let fh = handles.add(OpenFile {
                        d,
                        file: Some(Arc::new(f)),
                        written: truncate,
                    });
                    reply.opened(fh, FopenFlags::empty());
                }
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let (d, file) = {
            let handles = self.handles.lock();
            let Some(h) = handles.get(&fh.0) else {
                return reply.error(Errno::EBADF);
            };
            (h.d.clone(), h.file.clone())
        };
        if let Some(f) = file {
            let mut buf = vec![0u8; size as usize];
            let mut n = 0;
            while n < buf.len() {
                match f.read_at(&mut buf[n..], offset + n as u64) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(_) => return reply.error(Errno::EIO),
                }
            }
            return reply.data(&buf[..n]);
        }
        let drive = self.drive.clone();
        self.rt.spawn(async move {
            match drive.read(&d, offset, size as usize).await {
                Ok(data) => reply.data(&data),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let file = {
            let mut handles = self.handles.lock();
            let Some(h) = handles.get_mut(&fh.0) else {
                return reply.error(Errno::EBADF);
            };
            h.written = true;
            h.file.clone()
        };
        let Some(f) = file else {
            return reply.error(Errno::EBADF);
        };
        match f.write_all_at(data, offset) {
            Ok(()) => reply.written(data.len() as u32),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let file = self.handles.lock().get(&fh.0).and_then(|h| h.file.clone());
        if let Some(f) = file {
            let _ = f.sync_data();
        }
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let h = self.handles.lock().remove(&fh.0);
        if let Some(h) = h
            && h.file.is_some()
            && let Err(e) = self.drive.release_write(&h.d, h.written)
        {
            return reply.error(errno(&e));
        }
        reply.ok();
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let (p, name) = match self.child(parent, name) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        let Some(d) = self.drive.lookup(&p, &name) else {
            return reply.error(Errno::ENOENT);
        };
        match self.drive.remove(&d, false) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let (p, name) = match self.child(parent, name) {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        let Some(d) = self.drive.lookup(&p, &name) else {
            return reply.error(Errno::ENOENT);
        };
        match self.drive.remove(&d, true) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let ((p, name), (np, newname)) = match (self.child(parent, name), self.child(newparent, newname)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return reply.error(e),
        };
        if flags.contains(RenameFlags::RENAME_EXCHANGE) || flags.contains(RenameFlags::RENAME_WHITEOUT) {
            return reply.error(Errno::EINVAL);
        }
        let Some(d) = self.drive.lookup(&p, &name) else {
            return reply.error(Errno::ENOENT);
        };
        if flags.contains(RenameFlags::RENAME_NOREPLACE) && self.drive.lookup(&np, &newname).is_some() {
            return reply.error(Errno::EEXIST);
        }
        match self.drive.rename(&d, &np, &newname) {
            Ok(()) => {
                if let Some(target) = self.drive.lookup(&np, &newname)
                    && target != d
                {
                    // A file renamed over another: the drive keeps the target's
                    // node, but the kernel keeps the source's inode at the new
                    // name. Point that inode, and open handles, at the target.
                    let mut inodes = self.inodes.lock().unwrap();
                    if let Some(ino) = inodes.by_id.remove(&d) {
                        if let Some(old) = inodes.by_id.insert(target.clone(), ino) {
                            inodes.by_ino.remove(&old);
                        }
                        inodes.by_ino.insert(ino, target.clone());
                    }
                    drop(inodes);
                    for h in self.handles.lock().values_mut().filter(|h| h.d == d) {
                        h.d = target.clone();
                    }
                }
                reply.ok()
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // There's no fixed capacity; report plenty of free space.
        let blocks = 1u64 << 32;
        reply.statfs(blocks, blocks, blocks, 1 << 24, 1 << 24, 4096, 255, 4096);
    }
}
