//! In-memory database support: a port of
//! `com.healthmarketscience.jackcess.util.MemFileChannel` and of the
//! `DatabaseBuilder.setChannel(FileChannel)` entry points of jackcess.
//!
//! Java lets a caller run a whole database without any disk file:
//!
//! ```java
//! MemFileChannel channel = MemFileChannel.newChannel();
//! Database db = new DatabaseBuilder()
//!     .setFileFormat(Database.FileFormat.V2003)
//!     .setChannel(channel)
//!     .create();
//! // ... use db ...
//! channel.transferTo(someOutputStream); // the database bytes
//! ```
//!
//! This module offers the same workflow with [`MemFileChannel`] (a
//! `Vec<u8>`-backed random access channel) and [`MemDatabase`]:
//!
//! ```no_run
//! use jackcess::{format::FileFormat, mem};
//!
//! # fn main() -> jackcess::Result<()> {
//! let mut db = mem::create(FileFormat::V2003)?;   // like DatabaseBuilder.create
//! // ... use db exactly like a `Database` (MemDatabase derefs to it) ...
//! let bytes = db.to_bytes();                     // like MemFileChannel.transferTo
//! let mut reopened = mem::open(&bytes)?;         // like DatabaseBuilder.open over a MemFileChannel
//! # Ok(())
//! # }
//! ```
//!
//! # Semantics of [`MemFileChannel`]
//!
//! The channel mirrors `java.nio.channels.FileChannel` as implemented by
//! `MemFileChannel` (which is what jackcess' `PageChannel` uses):
//!
//! * the file size is the number of meaningful bytes (Java's `_size`), so
//!   [`to_bytes`](MemFileChannel::to_bytes) returns exactly `size()` bytes;
//! * a write beyond the end grows the file and the gap reads as zero bytes
//!   (Java allocates zeroed chunks, this port zero-fills the `Vec`);
//! * [`truncate`](MemFileChannel::truncate) only ever shrinks the file (like
//!   `FileChannel.truncate`) and clamps the channel position to the new size;
//! * `read`/`write` advance the channel position, `read_at`/`write_at` do not
//!   (they are the positional `FileChannel.read(ByteBuffer,long)` forms);
//! * a read at or past the end returns `0` bytes (the port's [`FileIo`]
//!   contract; `java.nio` returns `-1`);
//! * [`MemFileChannel::append`] writes at the current end of the file (the
//!   `RandomAccessFile("rw")` seek-to-end pattern jackcess does not use, kept
//!   because it is the natural "append" spelling of the channel contract);
//! * a read-only channel ([`MemFileChannel::from_bytes_read_only`]) refuses
//!   writes and truncation with the same error a read-only Java channel throws
//!   (`NonWritableChannelException`).
//!
//! # Database construction
//!
//! Java's `DatabaseImpl` has a package visible `open(Path, readOnly,
//! FileChannel, ...)` / `create(FileFormat, Path, FileChannel, ...)` pair which
//! `DatabaseBuilder.setChannel` uses, so a `Database` is built directly on top
//! of a `FileChannel`.  The port exposes the same entry points as
//! [`Database::open_with_channel`] and [`Database::create_with_channel`], and
//! [`MemDatabase`] uses them: constructing an in-memory database touches no
//! file system at all (previously a short lived scratch file was used and
//! deleted before the constructor returned).  Every read, write and flush goes
//! through [`MemFileChannel`].

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::database::Database;
use crate::error::{Error, Result};
use crate::format::FileFormat;
use crate::pagechannel::FileIo;

/// The name Java reports for a database which has no file
/// (`DatabaseImpl.getName()` -> `"<UNKNOWN.DB>"`).  A database opened over a
/// [`MemFileChannel`] has no path, so [`MemDatabase`] uses this name too (it
/// only shows up in foreign key error messages).
pub const UNKNOWN_NAME: &str = "<UNKNOWN.DB>";

/// `java.nio.channels.NonWritableChannelException`, as the same
/// [`crate::error::Error::Io`] `database.rs` uses for its read-only wrapper.
fn non_writable() -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "NonWritableChannelException",
    ))
}

/// A `Vec<u8>`-backed random access channel, the port of
/// `com.healthmarketscience.jackcess.util.MemFileChannel`.
///
/// A channel is a cheap handle: cloning it (or calling
/// [`handle`](MemFileChannel::handle)) shares the same buffer, while every
/// handle keeps its own channel position, like two Java channel objects
/// reading the same file would.
#[derive(Clone)]
pub struct MemFileChannel {
    /// The file contents.  Shared so that a [`MemDatabase`] can keep a handle
    /// after the page channel owns the boxed [`FileIo`].
    data: Arc<Mutex<Vec<u8>>>,
    /// Current read/write position of *this* handle (`MemFileChannel._position`).
    position: u64,
    /// `true` for the read-only form (`MemFileChannel.ReadOnlyChannel`).
    read_only: bool,
}

impl Default for MemFileChannel {
    fn default() -> MemFileChannel {
        MemFileChannel::new()
    }
}

impl std::fmt::Debug for MemFileChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemFileChannel")
            .field("size", &self.len())
            .field("position", &self.position)
            .field("read_only", &self.read_only)
            .finish()
    }
}

impl MemFileChannel {
    /// `MemFileChannel.newChannel()`: an empty read/write channel.
    pub fn new() -> MemFileChannel {
        MemFileChannel::with_capacity(0)
    }

    /// An empty read/write channel whose buffer has the given capacity.
    pub fn with_capacity(capacity: usize) -> MemFileChannel {
        MemFileChannel {
            data: Arc::new(Mutex::new(Vec::with_capacity(capacity))),
            position: 0,
            read_only: false,
        }
    }

    /// `MemFileChannel.newChannel(File)`: a read/write channel containing the
    /// given bytes.  Later modifications do not affect the source buffer (the
    /// bytes are copied).
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> MemFileChannel {
        MemFileChannel {
            data: Arc::new(Mutex::new(bytes.into())),
            position: 0,
            read_only: false,
        }
    }

    /// `MemFileChannel.newChannel(File, "r")`: a read-only channel containing
    /// the given bytes.
    pub fn from_bytes_read_only(bytes: impl Into<Vec<u8>>) -> MemFileChannel {
        MemFileChannel {
            read_only: true,
            ..MemFileChannel::from_bytes(bytes)
        }
    }

    /// A second handle on the same buffer with its own position (initially 0)
    /// and the same read-only flag.
    pub fn handle(&self) -> MemFileChannel {
        MemFileChannel {
            data: Arc::clone(&self.data),
            position: 0,
            read_only: self.read_only,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<u8>> {
        // a poisoned mutex only means another thread panicked while holding
        // the buffer; the bytes are still a plain Vec, so recover them
        self.data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `true` for the read-only form.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// `FileChannel.size()`: the number of meaningful bytes in the file.
    pub fn len(&self) -> u64 {
        self.lock().len() as u64
    }

    /// `true` when the file has no bytes.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// `FileChannel.position()`: this handle's current position.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// `FileChannel.position(long)`: moves this handle's position.  A position
    /// beyond the end is allowed (the next write grows the file, the next read
    /// returns 0 bytes).
    pub fn set_position(&mut self, position: u64) -> Result<()> {
        self.position = position;
        Ok(())
    }

    /// `FileChannel.read(ByteBuffer)`: reads at the current position and
    /// advances it.  Returns the number of bytes read, 0 at end of file.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let read = self.read_at(buf, self.position)?;
        self.position += read as u64;
        Ok(read)
    }

    /// `FileChannel.read(ByteBuffer,long)`: reads at `offset` without changing
    /// the position.  Returns the number of bytes read, 0 at/past end of file.
    pub fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let offset = match usize::try_from(offset) {
            Ok(offset) => offset,
            // a position beyond the addressable buffer is beyond the end
            Err(_) => return Ok(0),
        };
        let bytes = self.lock();
        if offset >= bytes.len() {
            return Ok(0);
        }
        let count = buf.len().min(bytes.len() - offset);
        buf[..count].copy_from_slice(&bytes[offset..offset + count]);
        Ok(count)
    }

    /// `FileChannel.write(ByteBuffer)`: writes at the current position and
    /// advances it.  A write beyond the end grows the file (the gap is zero
    /// filled).
    pub fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let written = self.write_at(buf, self.position)?;
        self.position += written as u64;
        Ok(written)
    }

    /// `FileChannel.write(ByteBuffer,long)`: writes at `offset` without
    /// changing the position.
    pub fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<usize> {
        if self.read_only {
            return Err(non_writable());
        }
        let offset = usize::try_from(offset).map_err(|_| {
            Error::IllegalArgument("write position does not fit in a buffer".into())
        })?;
        let end = offset.checked_add(buf.len()).ok_or_else(|| {
            Error::IllegalArgument("write position does not fit in a buffer".into())
        })?;
        let mut bytes = self.lock();
        if bytes.len() < end {
            // Java allocates zeroed chunks, so the gap between the old end and
            // the write offset reads as zeros
            bytes.resize(end, 0);
        }
        bytes[offset..end].copy_from_slice(buf);
        Ok(buf.len())
    }

    /// Appends `buf` at the end of the file (`RandomAccessFile` seek-to-end +
    /// write): the file grows, the position ends up after the written bytes.
    pub fn append(&mut self, buf: &[u8]) -> Result<usize> {
        self.position = self.len();
        self.write(buf)
    }

    /// `FileChannel.truncate(long)`: shrinks the file to `new_size`.  Like
    /// `java.nio`, a `new_size` greater than the current size does *not* grow
    /// the file.  The position is clamped to the new size
    /// (`MemFileChannel.truncate`).
    pub fn truncate(&mut self, new_size: u64) -> Result<()> {
        if self.read_only {
            return Err(non_writable());
        }
        if new_size < self.position {
            self.position = new_size;
        }
        let mut bytes = self.lock();
        if new_size < bytes.len() as u64 {
            bytes.truncate(new_size as usize);
        }
        Ok(())
    }

    /// `FileChannel.force(boolean)`: nothing to do for a memory channel.
    pub fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// The whole file contents (`MemFileChannel.transferTo` to a byte array).
    pub fn to_bytes(&self) -> Vec<u8> {
        self.lock().clone()
    }

    /// Consumes the channel and returns its contents.  When other handles on
    /// the same buffer are still alive the bytes are copied instead of moved.
    pub fn into_bytes(self) -> Vec<u8> {
        match Arc::try_unwrap(self.data) {
            Ok(mutex) => mutex
                .into_inner()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            Err(shared) => shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        }
    }

    /// Writes the whole file contents to `path`, like
    /// `MemFileChannel.transferTo(OutputStream)` / `transferTo(FileChannel)`.
    pub fn write_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        std::fs::write(path, self.to_bytes())?;
        Ok(())
    }
}

impl FileIo for MemFileChannel {
    fn size(&mut self) -> Result<u64> {
        Ok(self.len())
    }

    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        MemFileChannel::read_at(self, buf, offset)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<usize> {
        MemFileChannel::write_at(self, buf, offset)
    }

    fn truncate(&mut self, size: u64) -> Result<()> {
        MemFileChannel::truncate(self, size)
    }

    fn flush(&mut self) -> Result<()> {
        MemFileChannel::flush(self)
    }
}

/// A [`Database`] whose pages live in a [`MemFileChannel`].
///
/// It dereferences to [`Database`], so it is used exactly like the file backed
/// [`Database`]; the only additions are the byte accessors
/// ([`to_bytes`](MemDatabase::to_bytes), [`into_bytes`](MemDatabase::into_bytes),
/// [`save_as`](MemDatabase::save_as)) and the channel handle.
pub struct MemDatabase {
    db: Database,
    channel: MemFileChannel,
}

impl std::fmt::Debug for MemDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemDatabase")
            .field("format", &self.db.format)
            .field("size", &self.channel.len())
            .finish()
    }
}

impl MemDatabase {
    /// Creates a new empty database of the given format entirely in memory,
    /// the equivalent of
    /// `new DatabaseBuilder().setFileFormat(fmt).setChannel(MemFileChannel
    /// .newChannel()).create()`.
    ///
    /// No file system is touched: the embedded empty template is written
    /// straight into the memory channel ([`Database::create_with_channel`]).
    /// Formats which jackcess cannot create ([`FileFormat::V1997`],
    /// [`FileFormat::Msisam`], ...) fail like [`Database::create`] (the message
    /// names no file, since there is none).
    pub fn create(file_format: FileFormat) -> Result<MemDatabase> {
        let channel = MemFileChannel::new();
        let db = Database::create_with_channel(file_format, Box::new(channel.handle()))?;
        Ok(MemDatabase { db, channel })
    }

    /// Opens an existing database from its bytes, entirely in memory, the
    /// equivalent of
    /// `new DatabaseBuilder().setChannel(MemFileChannel.newChannel(in)).open()`.
    ///
    /// The bytes are copied, so the caller keeps ownership of the slice.  No
    /// file system is touched: the database is opened directly over the memory
    /// channel ([`Database::open_with_channel`]).
    pub fn open(bytes: impl AsRef<[u8]>) -> Result<MemDatabase> {
        MemDatabase::open_impl(bytes.as_ref().to_vec(), false)
    }

    /// Opens an existing database read-only from its bytes
    /// (`DatabaseBuilder.setReadOnly(true)` over a read-only `MemFileChannel`).
    pub fn open_read_only(bytes: impl AsRef<[u8]>) -> Result<MemDatabase> {
        MemDatabase::open_impl(bytes.as_ref().to_vec(), true)
    }

    fn open_impl(bytes: Vec<u8>, read_only: bool) -> Result<MemDatabase> {
        let channel = if read_only {
            MemFileChannel::from_bytes_read_only(bytes)
        } else {
            MemFileChannel::from_bytes(bytes)
        };
        let db = Database::open_with_channel(Box::new(channel.handle()), read_only)?;
        Ok(MemDatabase { db, channel })
    }

    /// The underlying file backed-ish [`Database`] (all page I/O already goes
    /// through the memory channel).
    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Mutable access to the underlying [`Database`].
    pub fn database_mut(&mut self) -> &mut Database {
        &mut self.db
    }

    /// The memory channel holding the database pages, for callers which want
    /// the [`FileIo`] view (`size`, `read_at`, ...).
    pub fn channel(&self) -> &MemFileChannel {
        &self.channel
    }

    /// The current database bytes, like `MemFileChannel.transferTo`.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.channel.to_bytes()
    }

    /// Flushes and returns the database bytes, releasing the database.
    pub fn into_bytes(self) -> Vec<u8> {
        let MemDatabase { mut db, channel } = self;
        // `Database::flush` only flushes the channel, but do it so a caller
        // which relies on the auto-sync state gets the same behaviour as
        // `Database::close`
        let _ = db.flush();
        drop(db); // releases the page channel's handle on the shared buffer
        channel.into_bytes()
    }

    /// Writes the current database bytes to `path`, like
    /// `MemFileChannel.transferTo(FileChannel)` on a database file.
    pub fn save_as<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        self.db.flush()?;
        self.channel.write_to_file(path)
    }

    /// Splits the database from its channel (the database keeps using the
    /// channel, the returned handle exposes its bytes).
    pub fn into_parts(self) -> (Database, MemFileChannel) {
        (self.db, self.channel)
    }
}

impl std::ops::Deref for MemDatabase {
    type Target = Database;

    fn deref(&self) -> &Database {
        &self.db
    }
}

impl std::ops::DerefMut for MemDatabase {
    fn deref_mut(&mut self) -> &mut Database {
        &mut self.db
    }
}

/// Creates a new empty database of the given format entirely in memory
/// (`mem::create(fmt)`; see [`MemDatabase::create`]).
pub fn create(file_format: FileFormat) -> Result<MemDatabase> {
    MemDatabase::create(file_format)
}

/// Opens a database from its bytes entirely in memory (`mem::open(bytes)`;
/// see [`MemDatabase::open`]).
pub fn open(bytes: impl AsRef<[u8]>) -> Result<MemDatabase> {
    MemDatabase::open(bytes)
}

/// Opens a database read-only from its bytes (`mem::open_read_only(bytes)`).
pub fn open_read_only(bytes: impl AsRef<[u8]>) -> Result<MemDatabase> {
    MemDatabase::open_read_only(bytes)
}

/// Creates a new in-memory database and returns the [`Database`] together with
/// the [`MemFileChannel`] holding its pages.  This is the by-value form of
/// [`MemDatabase`] for callers which prefer to hold both parts explicitly
/// (`let (mut db, channel) = mem::create_channel(fmt)?;` ... `channel.to_bytes()`).
pub fn create_channel(file_format: FileFormat) -> Result<(Database, MemFileChannel)> {
    Ok(MemDatabase::create(file_format)?.into_parts())
}

/// Opens a database from its bytes and returns the [`Database`] together with
/// the [`MemFileChannel`] holding its pages (the by-value form of
/// [`MemDatabase`]).
pub fn open_channel(bytes: impl AsRef<[u8]>) -> Result<(Database, MemFileChannel)> {
    Ok(MemDatabase::open(bytes)?.into_parts())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnSpec;
    use crate::database::Database;
    use crate::datatype::DataType;
    use crate::format::FileFormat;
    use crate::tablecreator::{IndexSpec, TableSpec};
    use crate::value::Value;
    use std::path::PathBuf;

    fn test_tmp_dir() -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// A write past the end grows the file, zero fills the gap and leaves the
    /// file size at the end of the written bytes.
    #[test]
    fn write_past_end_grows_and_zero_fills() {
        let ch = MemFileChannel::new();
        assert_eq!(ch.len(), 0);
        assert!(ch.is_empty());

        let mut ch2 = ch.handle();
        assert_eq!(ch2.write_at(b"abc", 0).unwrap(), 3);
        assert_eq!(ch2.len(), 3);

        // a write beyond the end zero-fills the hole, like the zeroed chunks
        // MemFileChannel allocates
        assert_eq!(ch2.write_at(b"z", 7).unwrap(), 1);
        assert_eq!(ch2.len(), 8);
        let mut all = [0u8; 8];
        assert_eq!(ch2.read_at(&mut all, 0).unwrap(), 8);
        assert_eq!(&all, b"abc\0\0\0\0z");

        // the independent handle sees the same file
        assert_eq!(ch.len(), 8);
        assert_eq!(ch.to_bytes(), b"abc\0\0\0\0z".to_vec());
    }

    /// `truncate` only ever shrinks, frees the tail and clamps the position.
    #[test]
    fn truncate_shrinks_and_clamps_position() {
        let mut ch = MemFileChannel::from_bytes((0u8..10).collect::<Vec<u8>>());
        ch.set_position(8).unwrap();
        ch.truncate(4).unwrap();
        assert_eq!(ch.len(), 4);
        assert_eq!(ch.position(), 4, "position is clamped to the new size");
        assert_eq!(ch.to_bytes(), vec![0, 1, 2, 3]);

        // a read at the (new) end yields nothing
        let mut buf = [0u8; 4];
        assert_eq!(ch.read(&mut buf).unwrap(), 0);

        // like FileChannel.truncate, a larger size does not grow the file
        ch.truncate(10).unwrap();
        assert_eq!(ch.len(), 4);
        assert_eq!(ch.position(), 4);
    }

    /// Positional and sequential reads/writes share one position, like
    /// `FileChannel`.
    #[test]
    fn position_controls_sequential_io() {
        let mut ch = MemFileChannel::new();
        assert_eq!(ch.write(b"hello").unwrap(), 5);
        assert_eq!(ch.position(), 5);
        ch.set_position(1).unwrap();
        assert_eq!(ch.write(b"EY").unwrap(), 2);
        assert_eq!(ch.position(), 3);
        assert_eq!(ch.to_bytes(), b"hEYlo".to_vec());

        ch.set_position(0).unwrap();
        let mut buf = [0u8; 5];
        assert_eq!(ch.read(&mut buf).unwrap(), 5);
        assert_eq!(ch.position(), 5);
        assert_eq!(&buf, b"hEYlo");
        // at the end of the file
        assert_eq!(ch.read(&mut buf).unwrap(), 0);
        assert_eq!(ch.position(), 5);

        // reading beyond the end (and a position far beyond it) is just EOF
        assert_eq!(ch.read_at(&mut buf, 99).unwrap(), 0);
        assert_eq!(ch.read_at(&mut buf, u64::MAX).unwrap(), 0);

        // positioning past the end is allowed
        ch.set_position(20).unwrap();
        assert_eq!(ch.read(&mut buf).unwrap(), 0);
    }

    /// Appending writes at the current end regardless of the position.
    #[test]
    fn append_writes_at_end() {
        let mut ch = MemFileChannel::from_bytes(b"abc".to_vec());
        ch.set_position(1).unwrap();
        assert_eq!(ch.append(b"de").unwrap(), 2);
        assert_eq!(ch.len(), 5);
        assert_eq!(ch.position(), 5);
        assert_eq!(ch.to_bytes(), b"abcde".to_vec());
    }

    /// The read-only form refuses every mutation with the Java
    /// `NonWritableChannelException` error.
    #[test]
    fn read_only_channel_refuses_writes() {
        let mut ch = MemFileChannel::from_bytes_read_only(b"abc".to_vec());
        assert!(ch.is_read_only());

        let mut buf = [0u8; 3];
        assert_eq!(ch.read_at(&mut buf, 0).unwrap(), 3);
        assert_eq!(&buf, b"abc");

        let err = ch.write_at(b"x", 0).unwrap_err();
        assert!(err.to_string().contains("NonWritableChannelException"));
        assert!(ch.truncate(1).is_err());
        assert_eq!(ch.to_bytes(), b"abc".to_vec());
    }

    /// The bytes are shared between handles and `into_bytes` moves them out
    /// when no other handle is alive.
    #[test]
    fn handles_share_the_buffer() {
        let ch = MemFileChannel::from_bytes(b"abc".to_vec());
        let mut other = ch.handle();
        other.write_at(b"x", 0).unwrap();
        assert_eq!(ch.to_bytes(), b"xbc".to_vec());

        let bytes = ch.into_bytes();
        assert_eq!(bytes, b"xbc".to_vec());
        // the remaining handle still owns the buffer
        assert_eq!(other.to_bytes(), b"xbc".to_vec());
    }

    /// A table created over the memory channel has the exact same bytes as the
    /// same table created in a disk file (the deterministic catalog timestamp
    /// makes the two databases comparable).
    #[test]
    fn in_memory_database_matches_disk_database() {
        const NOW_MS: i64 = 1_584_246_645_000;

        fn build(db: &mut Database) {
            let spec = TableSpec::new(
                "Test",
                vec![
                    ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                    ColumnSpec::new("Name", DataType::Text).with_length(30),
                    ColumnSpec::new("Note", DataType::Text).with_length(80),
                ],
            )
            .with_index(IndexSpec::primary_key(
                "PrimaryKey",
                vec![("ID".to_string(), true)],
            ));
            db.create_table(&spec).unwrap();
            let rows: Vec<Vec<Value>> = (0..4)
                .map(|i| {
                    vec![
                        Value::Null,
                        Value::Text(format!("name{}", i)),
                        Value::Text(format!("note {}", i)),
                    ]
                })
                .collect();
            db.add_rows("Test", &rows).unwrap();
        }

        let mut mem_db = MemDatabase::create(FileFormat::V2003).unwrap();
        mem_db.now_ms = Some(NOW_MS);
        build(&mut mem_db);
        mem_db.flush().unwrap();
        let mem_bytes = mem_db.to_bytes();

        let path = test_tmp_dir().join("mem_roundtrip_2003.mdb");
        let _ = std::fs::remove_file(&path);
        let mut disk_db = Database::create(FileFormat::V2003, &path).unwrap();
        disk_db.now_ms = Some(NOW_MS);
        build(&mut disk_db);
        disk_db.flush().unwrap();
        drop(disk_db);
        let disk_bytes = std::fs::read(&path).unwrap();

        assert_eq!(
            mem_bytes, disk_bytes,
            "the memory channel must produce the same pages as a disk file"
        );
        assert_eq!(mem_bytes.len() as u64 % 2048, 0);
        assert_eq!(mem_db.file_format, Some(FileFormat::V2003));

        // the memory bytes can be reopened in memory and read back
        let mut reopened = MemDatabase::open(&mem_bytes).unwrap();
        let rows = reopened.read_rows_with_ids("Test").unwrap();
        assert_eq!(rows.len(), 4);
        let first = rows[0].1.as_ref().unwrap();
        assert_eq!(first[1], Value::Text("name0".to_string()));
        assert_eq!(first[2], Value::Text("note 0".to_string()));
        assert_eq!(
            rows[3].1.as_ref().unwrap()[1],
            Value::Text("name3".to_string())
        );
        assert_eq!(
            reopened.file_format, None,
            "opening does not know the format"
        );
        assert_eq!(reopened.name, UNKNOWN_NAME);
    }

    /// `into_bytes` returns the same bytes as `to_bytes` and the database can
    /// still be saved with `save_as`.
    #[test]
    fn database_bytes_can_be_taken_back() {
        let mut mem_db = MemDatabase::create(FileFormat::V2007).unwrap();
        mem_db.now_ms = Some(1_600_000_000_000);
        let spec = TableSpec::new(
            "T",
            vec![ColumnSpec::new("ID", DataType::Long).with_auto_number(true)],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        mem_db.create_table(&spec).unwrap();
        mem_db.add_rows("T", &[vec![Value::Null]]).unwrap();
        mem_db.flush().unwrap();

        let path = test_tmp_dir().join("mem_save_as.accdb");
        let _ = std::fs::remove_file(&path);
        mem_db.save_as(&path).unwrap();
        let saved = std::fs::read(&path).unwrap();
        let bytes = mem_db.to_bytes();
        assert_eq!(saved, bytes);

        let taken = mem_db.into_bytes();
        assert_eq!(taken, bytes);

        // and the bytes open again as a database
        let mut reopened = MemDatabase::open(&taken).unwrap();
        assert_eq!(reopened.read_rows_with_ids("T").unwrap().len(), 1);
    }

    /// A read-only memory database reads rows but refuses writes.
    #[test]
    fn read_only_database_refuses_writes() {
        let mut mem_db = MemDatabase::create(FileFormat::V2003).unwrap();
        mem_db.now_ms = Some(1_584_246_645_000);
        let spec = TableSpec::new("T", vec![ColumnSpec::new("ID", DataType::Long)]);
        mem_db.create_table(&spec).unwrap();
        mem_db.add_rows("T", &[vec![Value::Long(1)]]).unwrap();
        mem_db.flush().unwrap();
        let bytes = mem_db.to_bytes();

        let mut rd = MemDatabase::open_read_only(&bytes).unwrap();
        assert!(rd.is_read_only());
        assert_eq!(rd.read_rows_with_ids("T").unwrap().len(), 1);
        let err = rd.add_rows("T", &[vec![Value::Long(2)]]).unwrap_err();
        assert!(
            err.to_string().contains("NonWritableChannelException")
                || format!("{:?}", err).contains("NonWritableChannelException"),
            "unexpected error: {err}"
        );
    }

    /// Serializes the tests which temporarily change the environment.
    static MEM_TMPDIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Sets `JACKCESS_MEM_TMPDIR` for the duration of `body` and restores the
    /// previous value afterwards.  The variable used to redirect the scratch
    /// file; it must not matter any more.
    fn with_mem_tmpdir<F: FnOnce() -> Result<()>>(dir: &std::path::Path, body: F) -> Result<()> {
        let _guard = MEM_TMPDIR_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("JACKCESS_MEM_TMPDIR");
        std::env::set_var("JACKCESS_MEM_TMPDIR", dir);
        let result = body();
        match previous {
            Some(value) => std::env::set_var("JACKCESS_MEM_TMPDIR", value),
            None => std::env::remove_var("JACKCESS_MEM_TMPDIR"),
        }
        result
    }

    fn read_dir_names(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// Constructing a memory database must not create any file, in particular
    /// not the old `JACKCESS_MEM_TMPDIR` scratch file.
    #[test]
    fn no_scratch_file_is_created() {
        let dir = test_tmp_dir().join(format!("mem-no-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        with_mem_tmpdir(&dir, || {
            let mut db = MemDatabase::create(FileFormat::V2003)?;
            db.now_ms = Some(1_584_246_645_000);
            let spec = TableSpec::new("T", vec![ColumnSpec::new("ID", DataType::Long)]);
            db.create_table(&spec)?;
            db.add_rows("T", &[vec![Value::Long(1)]])?;
            db.flush()?;
            let bytes = db.to_bytes();
            let mut reopened = MemDatabase::open(&bytes)?;
            assert_eq!(reopened.read_rows_with_ids("T")?.len(), 1);
            Ok(())
        })
        .unwrap();

        assert_eq!(
            read_dir_names(&dir),
            Vec::<String>::new(),
            "an in-memory database must not create a scratch file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scratch location which cannot be used as a directory (or does not
    /// exist) does not matter: nothing is written there.
    #[test]
    fn create_works_without_a_usable_scratch_directory() {
        let blocker = test_tmp_dir().join(format!("mem-scratch-blocker-{}", std::process::id()));
        std::fs::write(&blocker, b"not a directory").unwrap();
        with_mem_tmpdir(&blocker, || {
            let db = MemDatabase::create(FileFormat::V2007)?;
            let bytes = db.to_bytes();
            let reopened = MemDatabase::open(&bytes)?;
            assert_eq!(reopened.file_format, None);
            Ok(())
        })
        .unwrap();
        // a directory which does not exist is no better and no worse
        let missing = test_tmp_dir().join("mem-scratch-missing-dir");
        let _ = std::fs::remove_dir_all(&missing);
        with_mem_tmpdir(&missing, || {
            let db = MemDatabase::create(FileFormat::V2003)?;
            assert_eq!(db.to_bytes().len() as u64 % 2048, 0);
            Ok(())
        })
        .unwrap();
        let _ = std::fs::remove_file(&blocker);
    }

    /// `Database::create_with_channel` / `Database::open_with_channel` are the
    /// channel entry points `MemDatabase` is built on (the port of
    /// `DatabaseBuilder.setChannel`); they report no file and keep the default
    /// name.
    #[test]
    fn channel_entry_points_match_the_memory_database() {
        let channel = MemFileChannel::new();
        let mut db =
            Database::create_with_channel(FileFormat::V2016, Box::new(channel.handle())).unwrap();
        assert_eq!(db.file_format, Some(FileFormat::V2016));
        assert!(db.get_path().is_none());
        assert_eq!(db.name, UNKNOWN_NAME);
        db.now_ms = Some(1_621_000_000_000);
        let spec = TableSpec::new("T", vec![ColumnSpec::new("ID", DataType::Long)]);
        db.create_table(&spec).unwrap();
        db.add_rows("T", &[vec![Value::Long(7)]]).unwrap();
        db.flush().unwrap();
        let bytes = channel.to_bytes();
        drop(db);

        let mut reopened = Database::open_with_channel(
            Box::new(MemFileChannel::from_bytes(bytes.clone()).handle()),
            false,
        )
        .unwrap();
        assert_eq!(reopened.get_path(), None);
        assert_eq!(reopened.name, UNKNOWN_NAME);
        assert_eq!(reopened.read_rows_with_ids("T").unwrap().len(), 1);

        // the read-only flag wraps the channel, so writes are refused
        let mut read_only = Database::open_with_channel(
            Box::new(MemFileChannel::from_bytes(bytes.clone()).handle()),
            true,
        )
        .unwrap();
        assert!(read_only.is_read_only());
        assert!(read_only.add_rows("T", &[vec![Value::Long(8)]]).is_err());
    }
}
