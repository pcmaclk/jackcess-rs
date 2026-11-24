//! Port of `com.healthmarketscience.jackcess.impl.PageChannel`.

use crate::bytebuf::ByteBuf;
use crate::crypt::codec::{CodecHandler, CodecProvider, DummyHandler};
use crate::error::{Error, Result};
use crate::format::{Charset, JetFormat};
use crate::page_types;
use crate::usagemap::UsageMap;

pub const INVALID_PAGE_NUMBER: i32 = -1;
/// Global usage map always lives on page 1
pub const PAGE_GLOBAL_USAGE_MAP: i32 = 1;
/// Global usage map always lives at row 0
pub const ROW_GLOBAL_USAGE_MAP: usize = 0;
const FIRST_REUSABLE_PAGE_NUMBER: i32 = PAGE_GLOBAL_USAGE_MAP + 1;

/// Random access file abstraction (a minimal `FileChannel`).
pub trait FileIo {
    fn size(&mut self) -> Result<u64>;
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize>;
    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<usize>;
    fn truncate(&mut self, size: u64) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
}

/// `std::fs::File` based channel.
pub struct FileChannel {
    file: std::fs::File,
}

impl FileChannel {
    pub fn new(file: std::fs::File) -> FileChannel {
        FileChannel { file }
    }

    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<FileChannel> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(FileChannel::new(f))
    }

    pub fn open_read_only<P: AsRef<std::path::Path>>(path: P) -> Result<FileChannel> {
        let f = std::fs::File::open(path)?;
        Ok(FileChannel::new(f))
    }
}

impl FileIo for FileChannel {
    fn size(&mut self) -> Result<u64> {
        use std::io::Seek;
        Ok(self.file.seek(std::io::SeekFrom::End(0))?)
    }

    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        use std::io::{Read, Seek, SeekFrom};
        self.file.seek(SeekFrom::Start(offset))?;
        Ok(self.file.read(buf)?)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<usize> {
        use std::io::{Seek, SeekFrom, Write};
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)?;
        Ok(buf.len())
    }

    fn truncate(&mut self, size: u64) -> Result<()> {
        self.file.set_len(size)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        use std::io::Write;
        self.file.flush()?;
        Ok(())
    }
}

/// In-memory channel, mirroring Jackcess' `MemFileChannel`.
pub struct MemChannel {
    pub data: Vec<u8>,
}

impl MemChannel {
    pub fn new() -> MemChannel {
        MemChannel { data: Vec::new() }
    }

    pub fn from_bytes(data: Vec<u8>) -> MemChannel {
        MemChannel { data }
    }
}

impl Default for MemChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl FileIo for MemChannel {
    fn size(&mut self) -> Result<u64> {
        Ok(self.data.len() as u64)
    }

    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let off = offset as usize;
        if off >= self.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.data.len() - off);
        buf[..n].copy_from_slice(&self.data[off..off + n]);
        Ok(n)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<usize> {
        let off = offset as usize;
        if self.data.len() < off + buf.len() {
            self.data.resize(off + buf.len(), 0);
        }
        self.data[off..off + buf.len()].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn truncate(&mut self, size: u64) -> Result<()> {
        self.data.resize(size as usize, 0);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

pub struct PageChannel {
    pub io: Box<dyn FileIo>,
    pub format: JetFormat,
    pub auto_sync: bool,
    /// handler for the current database encoding type
    /// (`PageChannel._codecHandler`)
    pub codec: Box<dyn CodecHandler>,
    pub reuse_free_pages: bool,
    reuse_search_start: i32,
    pub write_count: i32,
    pub charset: Charset,
    pub global_usage_map: Option<UsageMap>,
    /// `DatabaseImpl._allowAutoNumInsert`: the database level policy for
    /// inserting explicit auto-number values.
    ///
    /// The Java library keeps this flag on the database, but a [`crate::table::Table`]
    /// here has no database handle and the channel is the object every table
    /// holds, so the policy lives here.  A table can still override it
    /// (`TableImpl._allowAutoNumInsert`, see
    /// [`crate::table::Table::allow_auto_number_insert`]).
    pub allow_auto_number_insert: bool,
}

impl PageChannel {
    pub fn new(io: Box<dyn FileIo>, format: JetFormat, auto_sync: bool) -> PageChannel {
        PageChannel {
            io,
            format,
            auto_sync,
            codec: Box::new(DummyHandler),
            reuse_free_pages: false,
            reuse_search_start: FIRST_REUSABLE_PAGE_NUMBER,
            write_count: 0,
            charset: Charset::Utf16Le,
            global_usage_map: None,
            allow_auto_number_insert: false,
        }
    }

    /// `PageChannel.initialize(database, codecProvider)`: asks the provider for
    /// the handler of this database's encoding.  The handler may read page 0
    /// while it is created (the jet codec takes its key from there), which is
    /// why this runs before [`PageChannel::initialize`] reads the usage map.
    ///
    /// Java does the two in one method; the port splits them because
    /// [`PageChannel::initialize`] already exists and reads the global usage
    /// map, which has to go through the codec.
    pub fn install_codec(&mut self, codec_provider: &dyn CodecProvider) -> Result<()> {
        let codec = codec_provider.create_handler(self, self.charset)?;
        self.codec = codec;
        Ok(())
    }

    /// Replaces the codec handler (used by the tests and by callers which
    /// build a handler directly).
    pub fn set_codec(&mut self, codec: Box<dyn CodecHandler>) {
        self.codec = codec;
    }

    /// `PageChannel.setReuseFreePages` / `isReuseFreePages`.
    pub fn set_reuse_free_pages(&mut self, reuse: bool) {
        self.reuse_free_pages = reuse;
    }

    pub fn is_reuse_free_pages(&self) -> bool {
        self.reuse_free_pages
    }

    /// `PageChannel.initialize` - loads the global usage map.
    pub fn initialize(&mut self) -> Result<()> {
        let map = UsageMap::read(self, PAGE_GLOBAL_USAGE_MAP, ROW_GLOBAL_USAGE_MAP, true)?;
        self.global_usage_map = Some(map);
        Ok(())
    }

    pub fn is_writing(&self) -> bool {
        self.write_count > 0
    }

    fn assert_writing(&self) -> Result<()> {
        if !self.is_writing() {
            return Err(Error::IllegalState("No write operation in progress".into()));
        }
        Ok(())
    }

    pub fn start_write(&mut self) {
        self.write_count += 1;
    }

    pub fn start_exclusive_write(&mut self) -> Result<()> {
        if self.write_count != 0 {
            return Err(Error::IllegalArgument(
                "Another write operation is currently in progress".into(),
            ));
        }
        self.start_write();
        Ok(())
    }

    pub fn finish_write(&mut self) -> Result<()> {
        self.assert_writing()?;
        self.write_count -= 1;
        if self.write_count == 0 && self.auto_sync {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.io.flush()
    }

    fn get_next_page_number(&self, size: u64) -> i32 {
        (size / self.format.page_size as u64) as i32
    }

    fn get_whole_page_file_size(&mut self) -> Result<u64> {
        let size = self.io.size()?;
        if size % self.format.page_size as u64 != 0 {
            return Err(Error::Jackcess(format!(
                "Database corrupted, file size {} is not multiple of page size {}",
                size, self.format.page_size
            )));
        }
        Ok(size)
    }

    fn get_page_offset(&self, page_number: i32) -> u64 {
        page_number as u64 * self.format.page_size as u64
    }

    fn validate_page_number(&mut self, page_number: i32) -> Result<()> {
        let size = self.io.size()?;
        let next = self.get_next_page_number(size);
        if page_number <= INVALID_PAGE_NUMBER || page_number >= next {
            return Err(Error::Jackcess(format!(
                "invalid page number {}",
                page_number
            )));
        }
        Ok(())
    }

    /// Reads a whole page into the given buffer (like
    /// `readPage(ByteBuffer,int)`).  The buffer is decoded by the codec
    /// handler, so the caller always sees a plain page.
    pub fn read_page(&mut self, buffer: &mut ByteBuf, page_number: i32) -> Result<()> {
        if page_number == 0 {
            self.read_root_page(buffer)?;
            return Ok(());
        }
        self.validate_page_number(page_number)?;
        buffer.clear();
        self.read_fully(buffer, self.get_page_offset(page_number))?;
        buffer.flip();
        if self.codec.can_decode_inline() {
            self.codec.decode_page_inline(buffer, page_number)?;
        } else {
            // the codec cannot decode on top of the input buffer
            let in_page = buffer.dup();
            self.codec.decode_page(&in_page, buffer, page_number)?;
        }
        Ok(())
    }

    fn read_fully(&mut self, buffer: &mut ByteBuf, offset: u64) -> Result<()> {
        let page_size = self.format.page_size;
        let mut bytes_read = 0usize;
        buffer.set_position(0)?;
        while bytes_read < page_size {
            let mut tmp = vec![0u8; page_size - bytes_read];
            let read = self.io.read_at(&mut tmp, offset + bytes_read as u64)?;
            if read == 0 {
                return Err(Error::Jackcess(format!(
                    "Failed attempting to read {} bytes, only read {}",
                    page_size, bytes_read
                )));
            }
            buffer.put_bytes(&tmp[..read])?;
            bytes_read += read;
        }
        Ok(())
    }

    /// `readRootPage` - reads page 0 and de-masks the header.
    pub fn read_root_page(&mut self, buffer: &mut ByteBuf) -> Result<()> {
        buffer.clear();
        buffer.set_position(0)?;
        self.read_fully(buffer, 0)?;
        buffer.flip();
        self.apply_header_mask(buffer)?;
        Ok(())
    }

    /// Writes the whole page.
    pub fn write_page(&mut self, page: &ByteBuf, page_number: i32) -> Result<()> {
        self.write_page_at(page, page_number, 0)
    }

    /// Writes part of a page starting at `page_offset`.  The written bytes are
    /// encoded by the codec handler; page 0 is never encoded and only has its
    /// header mask applied.
    pub fn write_page_at(
        &mut self,
        page: &ByteBuf,
        page_number: i32,
        page_offset: usize,
    ) -> Result<()> {
        self.assert_writing()?;
        self.validate_page_number(page_number)?;

        if page_offset > page.limit() {
            return Err(Error::IllegalArgument(
                "page offset beyond buffer limit".into(),
            ));
        }
        let mut page_offset = page_offset;
        let mut write_len = page.limit() - page_offset;
        if write_len + page_offset > self.format.page_size {
            return Err(Error::IllegalArgument(format!(
                "Page buffer is too large, size {}",
                write_len + page_offset
            )));
        }

        let bytes: Vec<u8> = if page_number == 0 {
            // re-mask header (page 0 never has additional encoding)
            let mut encoded = page.dup();
            encoded.set_position(page_offset)?;
            self.apply_header_mask(&mut encoded)?;
            encoded.get_bytes(write_len)?
        } else {
            let mut to_encode = page.dup();
            if !self.codec.can_encode_partial_page()
                && page_offset > 0
                && write_len < self.format.page_size
            {
                // current codec handler cannot encode part of a page, so need
                // to copy the modified part into the current page contents in
                // a temp buffer so that we can encode the entire page
                let mut full_page = self.create_page_buffer();
                self.read_page(&mut full_page, page_number)?;
                full_page.set_position(page_offset)?;
                full_page.put_bytes(&to_encode.remaining_bytes()[..write_len])?;
                full_page.rewind();
                to_encode = full_page;
                page_offset = 0;
                write_len = self.format.page_size;
            }
            // re-encode page
            let encoded = self
                .codec
                .encode_page(&to_encode, page_number, page_offset)?;
            encoded.bytes()[page_offset..page_offset + write_len].to_vec()
        };
        let offset = self.get_page_offset(page_number) + page_offset as u64;
        self.write_fully(&bytes, offset)?;
        Ok(())
    }

    fn write_fully(&mut self, bytes: &[u8], offset: u64) -> Result<()> {
        let mut written = 0usize;
        while written < bytes.len() {
            let n = self
                .io
                .write_at(&bytes[written..], offset + written as u64)?;
            if n == 0 {
                return Err(Error::Jackcess(format!(
                    "Failed attempting to write {} bytes, only wrote {}",
                    bytes.len(),
                    written
                )));
            }
            written += n;
        }
        Ok(())
    }

    /// `allocateNewPage` - grows the file (page reuse is optional).
    pub fn allocate_new_page(&mut self) -> Result<i32> {
        self.assert_writing()?;
        let size = self.get_whole_page_file_size()?;
        let page_number = self.get_next_page_number(size);

        if self.reuse_free_pages {
            if let Some(free) = self.find_free_page(page_number) {
                let mut map = self.global_usage_map.take().unwrap();
                let r = map.remove_page_number(self, free);
                self.global_usage_map = Some(map);
                r?;
                return Ok(free);
            }
        }

        if size >= self.format.max_database_size {
            return Err(Error::Jackcess(format!(
                "Database is at maximum size {}",
                self.format.max_database_size
            )));
        }

        // extend the file with one byte at the end of the page
        let offset = size + (self.format.page_size as u64 - 1);
        self.write_fully(&[0u8], offset)?;

        let mut map = self.global_usage_map.take().unwrap();
        let r = map.remove_page_number(self, page_number);
        self.global_usage_map = Some(map);
        r?;
        Ok(page_number)
    }

    /// Allocates a page without updating the global usage map (used by the
    /// global usage map itself).
    pub fn allocate_new_page_internal(&mut self) -> Result<i32> {
        let size = self.get_whole_page_file_size()?;
        let page_number = self.get_next_page_number(size);
        if size >= self.format.max_database_size {
            return Err(Error::Jackcess(format!(
                "Database is at maximum size {}",
                self.format.max_database_size
            )));
        }
        let offset = size + (self.format.page_size as u64 - 1);
        self.write_fully(&[0u8], offset)?;
        Ok(page_number)
    }

    fn find_free_page(&mut self, next_page_number: i32) -> Option<i32> {
        let map = self.global_usage_map.as_ref()?;
        let page_number = map.get_next_page_number_from(self.reuse_search_start);
        if page_number < FIRST_REUSABLE_PAGE_NUMBER || page_number >= next_page_number {
            self.reuse_search_start = next_page_number + 1;
            None
        } else {
            self.reuse_search_start = page_number + 1;
            Some(page_number)
        }
    }

    pub fn truncate_free_space(&mut self) -> Result<i32> {
        self.assert_writing()?;
        let size = self.get_whole_page_file_size()?;
        let next_page_number = self.get_next_page_number(size);
        let mut first_free = next_page_number;
        if let Some(map) = self.global_usage_map.as_ref() {
            while first_free > FIRST_REUSABLE_PAGE_NUMBER && map.holds_page_number(first_free - 1) {
                first_free -= 1;
            }
        }
        let page_count = next_page_number - first_free;
        if page_count == 0 {
            return Ok(0);
        }
        let off = self.get_page_offset(first_free);
        self.io.truncate(off)?;
        Ok(page_count)
    }

    pub fn deallocate_page(&mut self, page_number: i32) -> Result<()> {
        self.assert_writing()?;
        self.validate_page_number(page_number)?;
        // wipe the header
        self.io.write_at(
            &[page_types::INVALID, 0, 0, 0],
            self.get_page_offset(page_number),
        )?;
        let mut map = self.global_usage_map.take().unwrap();
        let r = map.add_page_number(self, page_number);
        self.global_usage_map = Some(map);
        r?;
        if page_number < self.reuse_search_start && page_number >= FIRST_REUSABLE_PAGE_NUMBER {
            self.reuse_search_start = page_number;
        }
        Ok(())
    }

    pub fn create_page_buffer(&self) -> ByteBuf {
        ByteBuf::allocate(self.format.page_size)
    }

    pub fn create_buffer(&self, size: usize) -> ByteBuf {
        ByteBuf::allocate(size)
    }

    fn apply_header_mask(&self, buffer: &mut ByteBuf) -> Result<()> {
        let mask = self.format.header_mask.to_vec();
        for (idx, m) in mask.iter().enumerate() {
            let pos = idx + self.format.offset_masked_header;
            let b = buffer.get_u8_at(pos)? ^ m;
            buffer.put_u8_at(pos, b)?;
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    //! Allocation of free pages (`allocateNewPage`'s re-use rule) and
    //! `truncateFreeSpace`, against a real (template created) database file.

    use super::*;
    use crate::format::JetFormat;

    /// Test scratch directory inside the workspace (the sandbox does not allow
    /// writes outside it for spawned test processes).
    fn test_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn new_channel(name: &str) -> PageChannel {
        let path = test_tmp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, include_bytes!("../resources/empty2003.mdb")).unwrap();
        let mut ch = PageChannel::new(
            Box::new(FileChannel::open(&path).unwrap()),
            JetFormat::version_4(),
            true,
        );
        ch.initialize().unwrap();
        ch
    }

    /// `PageChannel.allocateNewPage` hands a deallocated page out again once
    /// `setReuseFreePages` is enabled, instead of growing the file.
    #[test]
    fn deallocated_page_is_reused_when_enabled() {
        let mut ch = new_channel("pagechannel_reuse.mdb");
        let size_before = ch.io.size().unwrap();

        ch.start_write();
        let first = ch.allocate_new_page().unwrap();
        let second = ch.allocate_new_page().unwrap();
        assert_eq!(second, first + 1);

        // without the flag the allocator keeps growing the file
        ch.deallocate_page(first).unwrap();
        let grown = ch.allocate_new_page().unwrap();
        assert_eq!(grown, second + 1);

        // a deallocated page is handed out again, but the re-use rule always
        // takes the lowest free page of the database first (the template
        // leaves a run of free pages below the first page this test allocated),
        // so a re-used allocation must not grow the file
        ch.deallocate_page(second).unwrap();
        ch.deallocate_page(grown).unwrap();
        ch.set_reuse_free_pages(true);
        assert!(ch.is_reuse_free_pages());
        let size_at_reuse = ch.io.size().unwrap();
        let reused = ch.allocate_new_page().unwrap();
        assert!(
            reused < first,
            "the re-use rule takes free pages before growing the file"
        );
        let next = ch.allocate_new_page().unwrap();
        assert_eq!(next, reused + 1, "then the next free page");
        ch.finish_write().unwrap();

        // the file did not grow for the reused allocations
        assert_eq!(ch.io.size().unwrap(), size_at_reuse);
        assert_eq!(size_at_reuse, size_before + 3 * ch.format.page_size as u64);
    }

    /// `PageChannel.truncateFreeSpace` drops the run of free pages at the end
    /// of the file and returns how many it dropped.
    #[test]
    fn trailing_free_pages_are_truncated() {
        let mut ch = new_channel("pagechannel_truncate.mdb");
        let page_size = ch.format.page_size as u64;

        ch.start_write();
        let first = ch.allocate_new_page().unwrap();
        let second = ch.allocate_new_page().unwrap();
        let before = ch.io.size().unwrap();
        assert_eq!(before, (second as u64 + 1) * page_size);

        // one trailing free page
        ch.deallocate_page(second).unwrap();
        assert_eq!(ch.truncate_free_space().unwrap(), 1);
        assert_eq!(ch.io.size().unwrap(), (second as u64) * page_size);

        // an allocated page ends the run
        ch.deallocate_page(first).unwrap();
        // `first` is not the last page anymore (`second` was cut off), and the
        // file now ends with `first`, so it is dropped
        assert_eq!(ch.truncate_free_space().unwrap(), 1);
        assert_eq!(ch.io.size().unwrap(), first as u64 * page_size);

        // nothing free at the end any more
        assert_eq!(ch.truncate_free_space().unwrap(), 0);
        ch.finish_write().unwrap();
    }
}
