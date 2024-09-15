use nix::errno::Errno;
use std::backtrace::Backtrace;
use std::cmp::min;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};
use std::sync::Arc;

use crate::format::{
    DirEnt, Ino, Inode, InodeMode, Result, RootfsReader, VerityData, WireFormatError,
};
use crate::oci::Image;

pub const PUZZLEFS_IMAGE_MANIFEST_VERSION: u64 = 3;

pub(crate) fn file_read(
    oci: &Image,
    inode: &Inode,
    offset: usize,
    data: &mut [u8],
    verity_data: &Option<VerityData>,
) -> Result<usize> {
    let chunks = match &inode.mode {
        InodeMode::File { chunks } => chunks,
        _ => return Err(WireFormatError::from_errno(Errno::ENOTDIR)),
    };

    // TODO: fix all this casting...
    let end = offset + data.len();

    let mut file_offset = 0;
    let mut buf_offset = 0;
    for chunk in chunks {
        // have we read enough?
        if file_offset > end {
            break;
        }

        // should we skip this chunk?
        if file_offset + (chunk.len as usize) < offset {
            file_offset += chunk.len as usize;
            continue;
        }

        let addl_offset = if offset > file_offset {
            offset - file_offset
        } else {
            0
        };

        // ok, need to read this chunk; how much?
        let left_in_buf = data.len() - buf_offset;
        let to_read = min(left_in_buf, chunk.len as usize - addl_offset);

        let start = buf_offset;
        let finish = start + to_read;
        file_offset += addl_offset;

        // how many did we actually read?
        let n = oci.fill_from_chunk(
            chunk.blob,
            addl_offset as u64,
            &mut data[start..finish],
            verity_data,
        )?;
        file_offset += n;
        buf_offset += n;
    }

    // discard any extra if we hit EOF
    Ok(buf_offset)
}

pub struct PuzzleFS {
    pub oci: Arc<Image>,
    rootfs: RootfsReader,
    pub verity_data: Option<VerityData>,
    pub manifest_verity: Option<Vec<u8>>,
}

impl PuzzleFS {
    pub fn open(oci: Image, tag: &str, manifest_verity: Option<&[u8]>) -> Result<PuzzleFS> {
        let rootfs = oci.open_rootfs_blob(tag, manifest_verity)?;

        if rootfs.get_manifest_version()? != PUZZLEFS_IMAGE_MANIFEST_VERSION {
            return Err(WireFormatError::InvalidImageVersion(
                format!(
                    "got {}, expected {}",
                    rootfs.get_manifest_version()?,
                    PUZZLEFS_IMAGE_MANIFEST_VERSION
                ),
                Backtrace::capture(),
            ));
        }

        let verity_data = if manifest_verity.is_some() {
            Some(rootfs.get_verity_data()?)
        } else {
            None
        };

        Ok(PuzzleFS {
            oci: Arc::new(oci),
            rootfs,
            verity_data,
            manifest_verity: manifest_verity.map(|e| e.to_vec()),
        })
    }

    pub fn find_inode(&self, ino: u64) -> Result<Inode> {
        self.rootfs.find_inode(ino)
    }

    // lookup performs a path-based lookup in this puzzlefs
    pub fn lookup(&self, p: &Path) -> Result<Option<Inode>> {
        let components = p.components().collect::<Vec<Component<'_>>>();
        if !matches!(components[0], Component::RootDir) {
            return Err(WireFormatError::from_errno(Errno::EINVAL));
        }

        let mut cur = self.find_inode(1)?;

        // TODO: better path resolution with .. and such?
        for comp in components.into_iter().skip(1) {
            match comp {
                Component::Normal(p) => {
                    if let InodeMode::Dir { dir_list } = cur.mode {
                        if let Some(DirEnt { ino, name: _ }) = dir_list
                            .entries
                            .into_iter()
                            .find(|dir_entry| dir_entry.name == p.as_bytes())
                        {
                            cur = self.find_inode(ino)?;
                            continue;
                        }
                    }
                    return Ok(None);
                }
                _ => return Err(WireFormatError::from_errno(Errno::EINVAL)),
            }
        }

        Ok(Some(cur))
    }

    pub fn max_inode(&self) -> Result<Ino> {
        self.rootfs.max_inode()
    }
}

pub struct FileReader<'a> {
    oci: &'a Image,
    inode: &'a Inode,
    offset: usize,
    len: usize,
}

impl<'a> FileReader<'a> {
    pub fn new(oci: &'a Image, inode: &'a Inode) -> Result<FileReader<'a>> {
        let len = inode.file_len()? as usize;
        Ok(FileReader {
            oci,
            inode,
            offset: 0,
            len,
        })
    }
}

impl io::Read for FileReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let to_read = min(self.len - self.offset, buf.len());
        if to_read == 0 {
            return Ok(0);
        }

        let read = file_read(
            self.oci,
            self.inode,
            self.offset,
            &mut buf[0..to_read],
            &None,
        )
        .map_err(|e| io::Error::from_raw_os_error(e.to_errno()))?;
        self.offset += read;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use crate::builder::build_test_fs;

    use super::*;

    #[test]
    fn test_file_reader() {
        // make ourselves a test image
        let oci_dir = tempdir().unwrap();
        let image = Image::new(oci_dir.path()).unwrap();
        build_test_fs(Path::new("src/builder/test/test-1"), &image, "test").unwrap();
        let pfs = PuzzleFS::open(image, "test", None).unwrap();

        let inode = pfs.find_inode(2).unwrap();
        let mut reader = FileReader::new(&pfs.oci, &inode).unwrap();
        let mut hasher = Sha256::new();

        assert_eq!(io::copy(&mut reader, &mut hasher).unwrap(), 109466);
        let digest = hasher.finalize();
        assert_eq!(
            hex::encode(digest),
            "d9e749d9367fc908876749d6502eb212fee88c9a94892fb07da5ef3ba8bc39ed"
        );
        assert_eq!(pfs.max_inode().unwrap(), 2);
    }

    #[test]
    fn test_path_lookup() {
        let oci_dir = tempdir().unwrap();
        let image = Image::new(oci_dir.path()).unwrap();
        build_test_fs(Path::new("src/builder/test/test-1"), &image, "test").unwrap();
        let pfs = PuzzleFS::open(image, "test", None).unwrap();

        assert_eq!(pfs.lookup(Path::new("/")).unwrap().unwrap().ino, 1);
        assert_eq!(
            pfs.lookup(Path::new("/SekienAkashita.jpg"))
                .unwrap()
                .unwrap()
                .ino,
            2
        );
        assert!(pfs.lookup(Path::new("/notexist")).unwrap().is_none());
        pfs.lookup(Path::new("./invalid-path")).unwrap_err();
        pfs.lookup(Path::new("invalid-path")).unwrap_err();
    }
}
