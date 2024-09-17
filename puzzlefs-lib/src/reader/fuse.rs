use log::{debug, warn};
use os_pipe::PipeWriter;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::raw::c_int;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::thread;

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, ReplyData, ReplyEntry, ReplyOpen, Request,
    TimeOrNow,
};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use std::time::{Duration, SystemTime};

use crate::format::{DirEnt, Inode, InodeMode, Result, WireFormatError};

use super::puzzlefs::{file_read, PuzzleFS};

pub enum PipeDescriptor {
    UnnamedPipe(PipeWriter),
    NamedPipe(PathBuf),
}

pub struct Fuse {
    pfs: PuzzleFS,
    sender: Option<std::sync::mpsc::Sender<()>>,
    init_notify: Option<PipeDescriptor>,
    // TODO: LRU cache inodes or something. I had problems fiddling with the borrow checker for the
    // cache, so for now we just do each lookup every time.
}

fn mode_to_fuse_type(inode: &Inode) -> Result<FileType> {
    Ok(match inode.mode {
        InodeMode::File { .. } => FileType::RegularFile,
        InodeMode::Dir { .. } => FileType::Directory,
        InodeMode::Fifo { .. } => FileType::NamedPipe,
        InodeMode::Chr { .. } => FileType::CharDevice,
        InodeMode::Blk { .. } => FileType::BlockDevice,
        InodeMode::Lnk { .. } => FileType::Symlink,
        InodeMode::Sock { .. } => FileType::Socket,
        _ => return Err(WireFormatError::from_errno(Errno::EINVAL)),
    })
}

impl Fuse {
    pub fn new(
        pfs: PuzzleFS,
        sender: Option<std::sync::mpsc::Sender<()>>,
        init_notify: Option<PipeDescriptor>,
    ) -> Fuse {
        Fuse {
            pfs,
            sender,
            init_notify,
        }
    }

    fn _lookup(&mut self, parent: u64, name: &OsStr) -> Result<FileAttr> {
        let dir = self.pfs.find_inode(parent)?;
        let ino = dir.dir_lookup(name.as_bytes())?;
        self._getattr(ino)
    }

    fn _getattr(&mut self, ino: u64) -> Result<FileAttr> {
        let ic = self.pfs.find_inode(ino)?;
        let kind = mode_to_fuse_type(&ic)?;
        let len = ic.file_len().unwrap_or(0);
        Ok(FileAttr {
            ino: ic.ino,
            size: len,
            blocks: 0,
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            crtime: SystemTime::UNIX_EPOCH,
            kind,
            perm: ic.permissions,
            nlink: 0,
            uid: ic.uid,
            gid: ic.gid,
            rdev: 0,
            blksize: 0,
            flags: 0,
        })
    }

    fn _open(&self, flags_i: i32, reply: ReplyOpen) {
        let allowed_flags = OFlag::O_RDONLY
            | OFlag::O_PATH
            | OFlag::O_NONBLOCK
            | OFlag::O_DIRECTORY
            | OFlag::O_NOFOLLOW
            | OFlag::O_NOATIME;
        let flags = OFlag::from_bits_truncate(flags_i);
        if !allowed_flags.contains(flags) {
            warn!("invalid flags {flags:?}, only allowed {allowed_flags:?}");
            reply.error(Errno::EROFS as i32)
        } else {
            // stateless open for now, slower maybe
            reply.opened(0, flags_i.try_into().unwrap());
        }
    }

    fn _read(&mut self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let inode = self.pfs.find_inode(ino)?;
        let mut buf = vec![0_u8; size as usize];
        let read = file_read(
            &self.pfs.oci,
            &inode,
            offset as usize,
            &mut buf,
            &self.pfs.verity_data,
        )?;
        buf.truncate(read);
        Ok(buf)
    }

    fn _readdir(&mut self, ino: u64, offset: i64, reply: &mut fuser::ReplyDirectory) -> Result<()> {
        let inode = self.pfs.find_inode(ino)?;
        let entries = inode.dir_entries()?;
        for (index, DirEnt { name, ino: ino_r }) in entries.iter().enumerate().skip(offset as usize)
        {
            let ino = *ino_r;
            let inode = self.pfs.find_inode(ino)?;
            let kind = mode_to_fuse_type(&inode)?;

            // if the buffer is full, let's skip the extra lookups
            if reply.add(ino, (index + 1) as i64, kind, OsStr::from_bytes(name)) {
                break;
            }
        }

        Ok(())
    }

    fn _readlink(&mut self, ino: u64) -> Result<OsString> {
        let inode = self.pfs.find_inode(ino)?;
        let error = WireFormatError::from_errno(Errno::EINVAL);
        let kind = mode_to_fuse_type(&inode)?;
        match kind {
            FileType::Symlink => inode
                .additional
                .and_then(|add| add.symlink_target.map(OsString::from_vec))
                .ok_or(error),
            _ => Err(error),
        }
    }

    fn _listxattr(&mut self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.pfs.find_inode(ino)?;
        let xattr_list = inode
            .additional
            .map(|add| {
                add.xattrs
                    .iter()
                    .flat_map(|x| {
                        CString::new(x.key.as_slice())
                            .expect("xattr is a valid string")
                            .as_bytes_with_nul()
                            .to_vec()
                    })
                    .collect::<Vec<u8>>()
            })
            .unwrap_or_else(Vec::<u8>::new);

        Ok(xattr_list)
    }

    fn _getxattr(&mut self, ino: u64, name: &OsStr) -> Result<Vec<u8>> {
        let inode = self.pfs.find_inode(ino)?;
        inode
            .additional
            .and_then(|add| {
                add.xattrs
                    .into_iter()
                    .find(|elem| elem.key == name.as_bytes())
            })
            .map(|xattr| xattr.val)
            .ok_or_else(|| WireFormatError::from_errno(Errno::ENODATA))
    }
}

impl Drop for Fuse {
    fn drop(&mut self) {
        // This code should be in the destroy function inside the Filesystem implementation
        // Unfortunately, destroy is not getting called: https://github.com/zargony/fuse-rs/issues/151
        // This is fixed in fuser, which we're not using right now: https://github.com/cberner/fuser/issues/153
        if let Some(sender) = &self.sender {
            sender.send(()).unwrap();
        }
    }
}

impl Filesystem for Fuse {
    fn init(
        &mut self,
        _req: &Request<'_>,
        _config: &mut KernelConfig,
    ) -> std::result::Result<(), c_int> {
        if let Some(init_notify) = self.init_notify.take() {
            match init_notify {
                PipeDescriptor::UnnamedPipe(mut pipe_writer) => {
                    if let Err(e) = pipe_writer.write_all(b"s") {
                        warn!("unsuccessful send! {e}");
                    }
                }
                PipeDescriptor::NamedPipe(named_pipe) => {
                    // since opening a pipe for writing blocks until the reading end is opened
                    // create a new thread so the filesystem can be used even if nobody is reading from the pipe
                    thread::spawn(move || {
                        let md = fs::metadata(&named_pipe);
                        match md {
                            Err(e) => {
                                warn!("cannot get file metadata, {e}");
                                return;
                            }
                            Ok(md) => {
                                if !md.file_type().is_fifo() {
                                    warn!(
                                        "the provided file {} is not a fifo!",
                                        named_pipe.display()
                                    );
                                    return;
                                }
                            }
                        }
                        let file = OpenOptions::new().write(true).open(&named_pipe);
                        match file {
                            Ok(mut file) => {
                                if let Err(e) = file.write_all(b"s") {
                                    warn!("cannot write to pipe {}, {e}", named_pipe.display());
                                }
                            }
                            Err(e) => {
                                warn!("cannot open pipe {}, {e}", named_pipe.display());
                            }
                        }
                    });
                }
            }
        }
        Ok(())
    }

    fn destroy(&mut self) {}
    fn forget(&mut self, _req: &Request<'_>, _ino: u64, _nlookup: u64) {}

    // puzzlefs is readonly, so we can ignore a bunch of requests
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: fuser::ReplyAttr,
    ) {
        debug!("setattr not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn mknod(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        debug!("mknod not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        debug!("mkdir not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn unlink(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("unlink not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn rmdir(&mut self, _req: &Request<'_>, _parent: u64, _name: &OsStr, reply: fuser::ReplyEmpty) {
        debug!("rmdir not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _link: &Path,
        reply: ReplyEntry,
    ) {
        debug!("symlink not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _newparent: u64,
        _newname: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("rename not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn link(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _newparent: u64,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        debug!("link not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _offset: i64,
        _data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        debug!("write not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("flush not supported!");
        reply.error(Errno::ENOSYS as i32)
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("fsync not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("fsyncdir not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: fuser::ReplyEmpty,
    ) {
        reply.error(Errno::EROFS as i32)
    }

    fn removexattr(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _name: &OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("removexattr not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        debug!("create not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn getlk(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _typ: i32,
        _pid: u32,
        reply: fuser::ReplyLock,
    ) {
        debug!("getlk not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn setlk(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _typ: i32,
        _pid: u32,
        _sleep: bool,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("setlk not supported!");
        reply.error(Errno::EROFS as i32)
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self._lookup(parent, name) {
            Ok(attr) => {
                // http://libfuse.github.io/doxygen/structfuse__entry__param.html
                let ttl = Duration::new(u64::MAX, 0);
                let generation = 0;
                reply.entry(&ttl, &attr, generation)
            }
            Err(e) => {
                debug!("cannot lookup parent: {parent}, name {name:?} {e}!");
                reply.error(e.to_errno());
            }
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: fuser::ReplyAttr) {
        match self._getattr(ino) {
            Ok(attr) => {
                // http://libfuse.github.io/doxygen/structfuse__entry__param.html
                let ttl = Duration::new(u64::MAX, 0);
                reply.attr(&ttl, &attr)
            }
            Err(e) => {
                debug!("cannot getattr for ino {ino} {e}!");
                reply.error(e.to_errno())
            }
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match self._readlink(ino) {
            Ok(symlink) => reply.data(symlink.as_bytes()),
            Err(e) => {
                debug!("cannot readlink ino: {ino} {e}!");
                reply.error(e.to_errno())
            }
        }
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, flags: i32, reply: ReplyOpen) {
        self._open(flags, reply)
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        // TODO: why i64 from the fuse API here?
        let uoffset: u64 = offset.try_into().unwrap();
        match self._read(ino, uoffset, size) {
            Ok(data) => reply.data(data.as_slice()),
            Err(e) => {
                debug!("cannot read ino {ino}, offset: {uoffset} {e}!");
                reply.error(e.to_errno())
            }
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        // TODO: purge from our cache here? dcache should save us too...
        reply.ok()
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, flags: i32, reply: ReplyOpen) {
        self._open(flags, reply)
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: fuser::ReplyDirectory,
    ) {
        match self._readdir(ino, offset, &mut reply) {
            Ok(_) => reply.ok(),
            Err(e) => {
                debug!("cannot readdir ino: {ino}, offset {offset} {e}!");
                reply.error(e.to_errno())
            }
        }
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: fuser::ReplyEmpty,
    ) {
        // TODO: again maybe purge from cache?
        reply.ok()
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: fuser::ReplyStatfs) {
        reply.statfs(
            0,   // blocks
            0,   // bfree
            0,   // bavail
            0,   // files
            0,   // ffree
            0,   // bsize
            256, // namelen
            0,   // frsize
        )
    }

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: fuser::ReplyXattr,
    ) {
        match self._getxattr(ino, name) {
            Ok(xattr) => {
                let xattr_len: u32 = xattr
                    .len()
                    .try_into()
                    .expect("xattrs should not exceed u32");
                if size == 0 {
                    reply.size(xattr_len)
                } else if xattr_len <= size {
                    reply.data(&xattr)
                } else {
                    reply.error(Errno::ERANGE as i32)
                }
            }
            Err(e) => {
                debug!("cannot getxattr, ino: {ino}, name {name:?} {e}!");
                reply.error(e.to_errno())
            }
        }
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: fuser::ReplyXattr) {
        match self._listxattr(ino) {
            Ok(xattr) => {
                let xattr_len: u32 = xattr
                    .len()
                    .try_into()
                    .expect("xattrs should not exceed u32");
                if size == 0 {
                    reply.size(xattr_len)
                } else if xattr_len <= size {
                    reply.data(&xattr)
                } else {
                    reply.error(Errno::ERANGE as i32)
                }
            }
            Err(e) => {
                debug!("cannot listxattr, ino {ino}, size {size} {e}!");
                reply.error(e.to_errno())
            }
        }
    }

    fn access(&mut self, _req: &Request<'_>, _ino: u64, _mask: i32, reply: fuser::ReplyEmpty) {
        reply.ok()
    }

    fn bmap(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _blocksize: u32,
        _idx: u64,
        reply: fuser::ReplyBmap,
    ) {
        reply.error(Errno::ENOLCK as i32)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::Path;

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use crate::builder::build_test_fs;
    use crate::oci::Image;

    #[test]
    fn test_fuse() {
        let dir = tempdir().unwrap();
        let image = Image::new(dir.path()).unwrap();
        build_test_fs(Path::new("src/builder/test/test-1"), &image, "test").unwrap();
        let mountpoint = tempdir().unwrap();
        let _bg = crate::reader::spawn_mount::<&str>(
            image,
            "test",
            Path::new(mountpoint.path()),
            &[],
            None,
            None,
            None,
        )
        .unwrap();
        let ents = fs::read_dir(mountpoint.path())
            .unwrap()
            .collect::<io::Result<Vec<fs::DirEntry>>>()
            .unwrap();
        assert_eq!(ents.len(), 1);
        assert_eq!(
            ents[0].path().strip_prefix(mountpoint.path()).unwrap(),
            Path::new("SekienAkashita.jpg")
        );

        let mut hasher = Sha256::new();
        let mut f = fs::File::open(ents[0].path()).unwrap();
        io::copy(&mut f, &mut hasher).unwrap();
        let digest = hasher.finalize();
        const FILE_DIGEST: &str =
            "d9e749d9367fc908876749d6502eb212fee88c9a94892fb07da5ef3ba8bc39ed";
        assert_eq!(hex::encode(digest), FILE_DIGEST);
    }
}
