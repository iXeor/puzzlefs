pub trait MediaType {
    fn name() -> &'static str;
}

const PUZZLEFS_ROOTFS: &str = "application/vnd.puzzlefs.image.rootfs.v1";

pub struct Rootfs {}

impl MediaType for Rootfs {
    fn name() -> &'static str {
        PUZZLEFS_ROOTFS
    }
}

const PUZZLEFS_CHUNK_DATA: &str = "application/vnd.puzzlefs.image.layer.puzzlefs.v1";

pub struct Chunk {}

impl MediaType for Chunk {
    fn name() -> &'static str {
        PUZZLEFS_CHUNK_DATA
    }
}
