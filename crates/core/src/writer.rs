//! The output side of an image build: a file xdvdfs writes into at sector offsets.
//!
//! xdvdfs lays files out in the order it copies them, so writes arrive almost entirely in
//! sequence. This seeks only when a write does not continue the last one, which keeps a
//! buffered writer buffered, and tracks the image length itself because the file's own length
//! lags whatever is still in the buffer.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use xdvdfs::blockdev::BlockDeviceWrite;

pub struct IsoWriter {
    file: BufWriter<File>,
    pos: u64,
    len: u64,
}

impl IsoWriter {
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self { file: BufWriter::with_capacity(4 << 20, file), pos: 0, len: 0 })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Flushes and syncs. Call once the image is complete.
    pub fn finish(mut self) -> io::Result<u64> {
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        Ok(self.len)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if offset != self.pos {
            self.file.seek(SeekFrom::Start(offset))?;
        }
        self.file.write_all(buf)?;
        self.pos = offset + buf.len() as u64;
        self.len = self.len.max(self.pos);
        Ok(())
    }
}

#[async_trait::async_trait]
impl BlockDeviceWrite<io::Error> for IsoWriter {
    async fn write(&mut self, offset: u64, buffer: &[u8]) -> Result<(), io::Error> {
        self.write_at(offset, buffer)
    }

    async fn len(&mut self) -> Result<u64, io::Error> {
        Ok(self.len)
    }
}
