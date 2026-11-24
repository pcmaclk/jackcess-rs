//! OLE compound-document (OLE2 / Compound File Binary) reading utilities.
//!
//! This module ports the OLE support of Java Jackcess 5.0.4:
//!
//! * `com.healthmarketscience.jackcess.impl.OleUtil` parses the wrapper which
//!   Access stores in an `OLE` column (the "package" header, the simple
//!   package stream, links, and "other" embedded content), exposed to users
//!   through [`OleBlob`](https://jackcess.sourceforge.io/apidocs/com/healthmarketscience/jackcess/util/OleBlob.html)
//!   (`ContentType`, `Content`, `LinkContent`, `SimplePackageContent`,
//!   `CompoundContent`, ...).
//! * `com.healthmarketscience.jackcess.impl.CompoundOleUtil` reads the
//!   compound storage through Apache POI's `POIFSFileSystem`.  The Rust port
//!   has no POI dependency, so this module contains a complete reader for the
//!   compound file format (header, DIFAT/FAT, directory, mini-FAT and the
//!   mini stream) which mirrors the behaviour (and error messages) of POI
//!   5.4.1, the version Jackcess 5.0.4 depends on.
//!
//! Two layers are provided:
//!
//! * [`CompoundFile`] / [`DirectoryEntry`] / [`CompoundHeader`] – the low-level
//!   compound file reader, with raw entry names, timestamps and stream access.
//! * [`OleBlob`] / [`Content`] – the `OleBlob`-style Access OLE wrapper,
//!   including `CompoundContent::entries()` / `get_entry()` and the
//!   Jackcess-specific encoded entry names (`URLEncoder`-encoded components,
//!   `/` separated).
//!
//! Java Jackcess 5.0.4 does *not* expose methods named `getStreams`,
//! `getStream(String)`, `getStreamName` or `getData`; the closest equivalents
//! are [`CompoundContent::entries`], [`CompoundContent::entry`],
//! [`CompoundContent::has_contents_entry`], [`Content::content_type`],
//! [`Content::file_name`] and [`Content::data`].  [`CompoundFile::streams`] and
//! [`CompoundFile::stream`] are convenience helpers over the low-level layer.

use std::cell::{OnceCell, RefCell};
use std::collections::HashSet;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Constants (OleUtil / POIFSConstants)
// ---------------------------------------------------------------------------

/// `OleUtil.PACKAGE_SIGNATURE`.
pub const PACKAGE_SIGNATURE: u16 = 0x1C15;
/// `OleUtil.COMPOUND_STORAGE_SIGNATURE`.
pub const COMPOUND_STORAGE_SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const COMPOUND_STORAGE_SIGNATURE_U64: u64 = 0xE11A_B1A1_E011_CFD0;
const OLE_VERSION: i32 = 0x0501;
const SIMPLE_PACKAGE_TYPE: &str = "Package";
const PACKAGE_STREAM_SIGNATURE: u16 = 0x02;
const PS_EMBEDDED_FILE: i32 = 0x03_0000;
const PS_LINKED_FILE: i32 = 0x01_0000;

/// `POIFSConstants.END_OF_CHAIN` (`-2` as an int).
pub const END_OF_CHAIN: i32 = -2;
/// `POIFSConstants.UNUSED_BLOCK` (`-1` as an int).
pub const UNUSED_BLOCK: i32 = -1;
/// `POIFSConstants.FREESECT`.
pub const FREE_SECT: i32 = -1;
/// `POIFSConstants.BIG_BLOCK_MINIMUM_DOCUMENT_SIZE`.
pub const BIG_BLOCK_MINIMUM_DOCUMENT_SIZE: u32 = 0x1000;
/// `POIFSConstants.SMALL_BLOCK_SIZE`.
pub const SMALL_BLOCK_SIZE: usize = 0x40;
/// `POIFSConstants.PROPERTY_SIZE`.
pub const PROPERTY_SIZE: usize = 0x80;
/// Maximum number of DIFAT entries which fit in the header.
const MAX_BAT_IN_HEADER: usize = 109;

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn u16_at(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}

fn u32_at(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

fn u64_at(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
        data[off + 4],
        data[off + 5],
        data[off + 6],
        data[off + 7],
    ])
}

fn hex_upper_u64(value: u64) -> String {
    format!("0x{:016X}", value)
}

// ---------------------------------------------------------------------------
// Compound file header
// ---------------------------------------------------------------------------

/// The 512-byte compound file header (`POIFS` `HeaderBlock`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompoundHeader {
    /// Header CLSID (all zero in practically every file).
    pub clsid: [u8; 16],
    /// Minor version (`0x003E` for v3, `0x003E`+ for v4).
    pub minor_version: u16,
    /// Major version: 3 (512-byte sectors) or 4 (4096-byte sectors).
    pub major_version: u16,
    /// Byte order marker (`0xFFFE`).
    pub byte_order: u16,
    /// Sector shift: 9 (512) or 12 (4096).
    pub sector_shift: u16,
    /// Mini sector shift (always 6, i.e. 64-byte mini sectors).
    pub mini_sector_shift: u16,
    /// Number of directory sectors (v4 only, 0 in v3 files).
    pub num_dir_sectors: u32,
    /// Number of FAT sectors (`HeaderBlock.getBATCount`).
    pub num_fat_sectors: u32,
    /// First directory sector (`HeaderBlock.getPropertyStart`).
    pub first_dir_sector: u32,
    /// Transaction signature.
    pub transaction_signature: u32,
    /// Mini stream cutoff size (normally 4096).
    pub mini_stream_cutoff_size: u32,
    /// First mini-FAT sector (`HeaderBlock.getSBATStart`).
    pub first_mini_fat_sector: u32,
    /// Number of mini-FAT sectors (`HeaderBlock.getSBATCount`).
    pub num_mini_fat_sectors: u32,
    /// First DIFAT sector (`HeaderBlock.getXBATIndex`).
    pub first_difat_sector: u32,
    /// Number of DIFAT sectors (`HeaderBlock.getXBATCount`).
    pub num_difat_sectors: u32,
    /// The 109 DIFAT entries stored in the header.
    pub difat: [u32; MAX_BAT_IN_HEADER],
}

impl CompoundHeader {
    /// Parses the header from the first 512 bytes of `data`.
    ///
    /// Short inputs are zero padded, exactly like POI's `HeaderBlock` (which
    /// always receives a 512 byte buffer).  Errors mirror POI:
    /// `NotOLE2FileException` for a bad signature and `IOException` for an
    /// unsupported block size.
    pub fn parse(data: &[u8]) -> Result<CompoundHeader> {
        let mut buf = [0u8; 512];
        let n = data.len().min(512);
        buf[..n].copy_from_slice(&data[..n]);

        let signature = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        if signature != COMPOUND_STORAGE_SIGNATURE_U64 {
            return Err(Error::Jackcess(format!(
                "Invalid header signature; read {}, expected {} - \
                 Your file appears not to be a valid OLE2 document",
                hex_upper_u64(signature),
                hex_upper_u64(COMPOUND_STORAGE_SIGNATURE_U64)
            )));
        }

        // POI checks `_data[30]` (the low byte of the sector shift).
        let sector_shift = buf[30];
        if sector_shift != 9 && sector_shift != 12 {
            return Err(Error::Jackcess(format!(
                "Unsupported blocksize  (2^{}). Expected 2^9 or 2^12.",
                sector_shift
            )));
        }

        let mut clsid = [0u8; 16];
        clsid.copy_from_slice(&buf[8..24]);
        let mut difat = [0u32; MAX_BAT_IN_HEADER];
        for (i, slot) in difat.iter_mut().enumerate() {
            *slot = u32_at(&buf, 76 + i * 4);
        }

        Ok(CompoundHeader {
            clsid,
            minor_version: u16_at(&buf, 24),
            major_version: u16_at(&buf, 26),
            byte_order: u16_at(&buf, 28),
            sector_shift: u16::from(sector_shift),
            mini_sector_shift: u16_at(&buf, 32),
            num_dir_sectors: u32_at(&buf, 40),
            num_fat_sectors: u32_at(&buf, 44),
            first_dir_sector: u32_at(&buf, 48),
            transaction_signature: u32_at(&buf, 52),
            mini_stream_cutoff_size: u32_at(&buf, 56),
            first_mini_fat_sector: u32_at(&buf, 60),
            num_mini_fat_sectors: u32_at(&buf, 64),
            first_difat_sector: u32_at(&buf, 68),
            num_difat_sectors: u32_at(&buf, 72),
            difat,
        })
    }

    /// The big block (sector) size in bytes: 512 or 4096.
    pub fn sector_size(&self) -> usize {
        1usize << self.sector_shift
    }

    /// The mini sector size in bytes (normally 64).
    pub fn mini_sector_size(&self) -> usize {
        1usize << self.mini_sector_shift
    }

    /// FAT entries per sector.
    pub fn fat_entries_per_sector(&self) -> usize {
        self.sector_size() / 4
    }
}

// ---------------------------------------------------------------------------
// Compound file directory entries
// ---------------------------------------------------------------------------

/// The type of a compound file directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryType {
    /// Empty/unused property slot.
    Empty,
    /// Storage (directory).
    Storage,
    /// Stream (document).
    Stream,
    /// Root entry.
    Root,
}

impl EntryType {
    fn from_u8(value: u8) -> EntryType {
        match value {
            1 => EntryType::Storage,
            2 => EntryType::Stream,
            5 => EntryType::Root,
            _ => EntryType::Empty,
        }
    }
}

/// One 128-byte directory entry of a compound file.
///
/// Field names follow the compound file specification; the accessors mirror
/// the information POI's `Property` classes expose (plus the raw FILETIME
/// timestamps and the storage CLSID from the raw property).
#[derive(Clone, Debug)]
pub struct DirectoryEntry {
    index: usize,
    name: String,
    entry_type: EntryType,
    color: u8,
    left_sibling: i32,
    right_sibling: i32,
    child: i32,
    clsid: [u8; 16],
    state_bits: u32,
    creation_time: u64,
    modified_time: u64,
    start_sector: u32,
    size: u64,
    parent: Option<usize>,
    children: Vec<usize>,
}

impl DirectoryEntry {
    /// The index of this entry in the directory (the root is index 0).
    pub fn index(&self) -> usize {
        self.index
    }

    /// The raw (decoded UTF-16) entry name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The entry type.
    pub fn entry_type(&self) -> EntryType {
        self.entry_type
    }

    /// `true` for a storage (directory) entry.
    pub fn is_storage(&self) -> bool {
        self.entry_type == EntryType::Storage
    }

    /// `true` for a stream (document) entry.
    pub fn is_stream(&self) -> bool {
        self.entry_type == EntryType::Stream
    }

    /// `true` for the root entry.
    pub fn is_root(&self) -> bool {
        self.entry_type == EntryType::Root
    }

    /// Node colour (0 = red, 1 = black).
    pub fn color(&self) -> u8 {
        self.color
    }

    /// Index of the left sibling (`-1` when unused).
    pub fn left_sibling(&self) -> i32 {
        self.left_sibling
    }

    /// Index of the right sibling (`-1` when unused).
    pub fn right_sibling(&self) -> i32 {
        self.right_sibling
    }

    /// Index of the first child (`-1` when unused).
    pub fn child(&self) -> i32 {
        self.child
    }

    /// Storage CLSID bytes.
    pub fn clsid(&self) -> &[u8; 16] {
        &self.clsid
    }

    /// CLSID formatted like Java's `ClassID.toString()`.
    pub fn clsid_string(&self) -> String {
        format_class_id(&self.clsid)
    }

    /// User flags / state bits.
    pub fn state_bits(&self) -> u32 {
        self.state_bits
    }

    /// Raw creation time (`FILETIME`, 100ns ticks since 1601-01-01).
    pub fn creation_time(&self) -> u64 {
        self.creation_time
    }

    /// Raw last-modified time (`FILETIME`).
    pub fn modified_time(&self) -> u64 {
        self.modified_time
    }

    /// The start sector of the stream (or of the mini stream for the root).
    pub fn start_sector(&self) -> u32 {
        self.start_sector
    }

    /// The raw 64-bit stream size field.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The stream size as POI's `Property.getSize()` sees it (lower 32 bits).
    pub fn size32(&self) -> u32 {
        self.size as u32
    }

    /// The parent entry index (`None` for the root and orphaned entries).
    pub fn parent(&self) -> Option<usize> {
        self.parent
    }

    /// Child entry indices in POI's `DirectoryNode` order.
    pub fn children(&self) -> &[usize] {
        &self.children
    }
}

/// Formats 16 CLSID bytes like POI's `ClassID.toString()`
/// (`{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, upper case).
pub fn format_class_id(bytes: &[u8; 16]) -> String {
    format!(
        "{{{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-\
         {:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        bytes[3],
        bytes[2],
        bytes[1],
        bytes[0],
        bytes[5],
        bytes[4],
        bytes[7],
        bytes[6],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

// ---------------------------------------------------------------------------
// Compound file reader
// ---------------------------------------------------------------------------

/// A parsed OLE2 / Compound File Binary document.
///
/// The reader mirrors `POIFSFileSystem`:
///
/// * the header and directory are parsed eagerly;
/// * FAT sectors (including DIFAT chained ones) and the mini-FAT are loaded
///   eagerly, just like `readCoreContents()`;
/// * stream contents (and the root mini stream) are read lazily when a stream
///   is requested, so malformed chains only fail when the affected stream is
///   read, like POI.
#[derive(Debug)]
pub struct CompoundFile {
    data: Vec<u8>,
    header: CompoundHeader,
    sector_size: usize,
    fat: Vec<u32>,
    fat_blocks: usize,
    mini_fat: Vec<u32>,
    mini_fat_blocks: usize,
    entries: Vec<DirectoryEntry>,
    mini: RefCell<MiniStream>,
}

#[derive(Debug, Default)]
struct MiniStream {
    blocks: Vec<Vec<u8>>,
    next: i32,
    started: bool,
    done: bool,
    used: HashSet<i32>,
}

impl CompoundFile {
    /// Parses a complete compound file.
    pub fn parse(data: &[u8]) -> Result<CompoundFile> {
        let header = CompoundHeader::parse(data)?;
        let sector_size = header.sector_size();

        let mut file = CompoundFile {
            data: data.to_vec(),
            header,
            sector_size,
            fat: Vec::new(),
            fat_blocks: 0,
            mini_fat: Vec::new(),
            mini_fat_blocks: 0,
            entries: Vec::new(),
            mini: RefCell::new(MiniStream::default()),
        };
        // `POIFSFileSystem(FileChannel)` (the constructor Jackcess uses, see
        // `CompoundOleUtil.getFileSystem`, which passes a `MemFileChannel`)
        // does *not* run `sanityCheckBlockCount`; only the `InputStream`
        // constructor does.  A bogus FAT sector count is therefore reported by
        // the first FAT lookup, exactly like POI.
        //
        // POI creates one `ChainLoopDetector` for the file before reading the
        // FAT and reuses it for the DIFAT and mini-FAT chains, so those three
        // share one claimed-block set.
        let mut core_claimed: HashSet<i32> = HashSet::new();
        file.load_fat(&mut core_claimed)?;
        file.load_directory()?;
        file.load_mini_fat(&mut core_claimed)?;
        Ok(file)
    }

    /// The parsed header.
    pub fn header(&self) -> &CompoundHeader {
        &self.header
    }

    /// The big-block size in bytes.
    pub fn sector_size(&self) -> usize {
        self.sector_size
    }

    /// The FAT, flattened across the loaded FAT sectors (POI's `_bat_blocks`).
    pub fn fat(&self) -> &[u32] {
        &self.fat
    }

    /// The number of FAT sectors actually loaded.
    pub fn fat_sector_count(&self) -> usize {
        self.fat_blocks
    }

    /// The mini-FAT, flattened across the loaded mini-FAT sectors.
    pub fn mini_fat(&self) -> &[u32] {
        &self.mini_fat
    }

    /// The number of mini-FAT sectors actually loaded.
    pub fn mini_fat_sector_count(&self) -> usize {
        self.mini_fat_blocks
    }

    /// All directory entries, including the root and empty slots.
    pub fn directory_entries(&self) -> &[DirectoryEntry] {
        &self.entries
    }

    /// The root entry.
    pub fn root(&self) -> &DirectoryEntry {
        &self.entries[0]
    }

    /// The children of the entry with the given index, in POI order.
    pub fn children_of(&self, index: usize) -> &[usize] {
        &self.entries[index].children
    }

    /// Looks up a direct child of `parent` by raw name.
    fn child_by_name(&self, parent: usize, name: &str) -> Option<usize> {
        self.entries[parent]
            .children
            .iter()
            .copied()
            .find(|&child| self.entries[child].name == name)
    }

    /// Reads the stream of the entry with the given directory index.
    pub fn read_stream(&self, index: usize) -> Result<Vec<u8>> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| Error::IllegalArgument(format!("invalid entry index {}", index)))?;
        if !entry.is_stream() && !entry.is_root() {
            return Err(Error::IllegalArgument(format!(
                "entry {} is not a stream",
                entry.name
            )));
        }
        let size = entry.size32();
        if size == 0 {
            // `POIFSDocument.getBlockIterator` returns an empty iterator for
            // zero length documents, whatever the start sector says.
            return Ok(Vec::new());
        }
        let small = size < BIG_BLOCK_MINIMUM_DOCUMENT_SIZE;
        let block_size = if small {
            self.header.mini_sector_size()
        } else {
            self.sector_size
        };
        let needed = (size as usize).div_ceil(block_size);

        let mut out = Vec::with_capacity(needed * block_size);
        let mut used: HashSet<i32> = HashSet::new();
        let mut next = entry.start_sector as i32;
        for _ in 0..needed {
            if next == END_OF_CHAIN {
                return Err(Error::Jackcess(
                    "Can't read from a new stream before it has been written to".to_string(),
                ));
            }
            if small {
                // POI's `POIFSMiniStore.getChainLoopDetector` sizes the block
                // array with `ceil(rootSize / 64)`.
                let mini_blocks = (self.root().size32() as usize).div_ceil(SMALL_BLOCK_SIZE);
                claim(next, &mut used, mini_blocks)?;
                out.extend_from_slice(&self.mini_block(next)?);
                next = self.mini_fat_next(next)?;
            } else {
                let file_blocks = self.file_block_count();
                claim(next, &mut used, file_blocks)?;
                out.extend_from_slice(&self.get_block(next)?);
                next = self.fat_next(next)?;
            }
        }
        out.truncate(size as usize);
        Ok(out)
    }

    /// Reads a stream by its raw path (for example `/WordDocument` or
    /// `/Storage1/Doc1`).
    pub fn stream(&self, path: &str) -> Result<Vec<u8>> {
        let index = self.entry_index(path)?;
        self.read_stream(index)
    }

    /// Returns the raw paths of all document entries, recursively.
    pub fn streams(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_streams(0, "", &mut out);
        out
    }

    fn collect_streams(&self, index: usize, prefix: &str, out: &mut Vec<String>) {
        for &child in &self.entries[index].children {
            let entry = &self.entries[child];
            let path = format!("{}/{}", prefix, entry.name);
            match entry.entry_type {
                EntryType::Storage => self.collect_streams(child, &path, out),
                EntryType::Stream => out.push(path),
                _ => {}
            }
        }
    }

    /// Finds the index of an entry by raw, `/` separated path.
    pub fn entry_index(&self, path: &str) -> Result<usize> {
        let mut current = 0usize;
        let mut found = false;
        for component in path.split('/').filter(|s| !s.is_empty()) {
            let child = self
                .child_by_name(current, component)
                .ok_or_else(|| Error::Jackcess(format!("Could not find document {}", path)))?;
            current = child;
            found = true;
        }
        if !found {
            return Ok(0);
        }
        if self.entries[current].is_storage() {
            return Err(Error::Jackcess(format!("Could not find document {}", path)));
        }
        Ok(current)
    }

    // -- internal parsing ---------------------------------------------------

    fn file_block_count(&self) -> usize {
        self.data.len().div_ceil(self.sector_size)
    }

    fn get_block(&self, offset: i32) -> Result<Vec<u8>> {
        read_block_at(&self.data, &self.header, offset)
    }

    fn fat_next(&self, offset: i32) -> Result<i32> {
        self.fat_lookup(offset, &self.fat, self.fat_blocks)
    }

    fn mini_fat_next(&self, offset: i32) -> Result<i32> {
        self.fat_lookup(offset, &self.mini_fat, self.mini_fat_blocks)
    }

    fn fat_lookup(&self, offset: i32, fat: &[u32], blocks: usize) -> Result<i32> {
        let eps = self.header.fat_entries_per_sector();
        if offset < 0 {
            let which = offset / eps as i32;
            return Err(Error::Jackcess(format!(
                "Index {} out of bounds for length {}",
                which, blocks
            )));
        }
        let index = offset as usize;
        if index >= blocks.saturating_mul(eps) {
            let which = index / eps;
            return Err(Error::Jackcess(format!(
                "Index {} out of bounds for length {}",
                which, blocks
            )));
        }
        Ok(fat[index] as i32)
    }

    fn read_bat_sector(
        &mut self,
        sector: i32,
        claimed: &mut HashSet<i32>,
        out: &mut Vec<u32>,
        blocks: &mut usize,
    ) -> Result<()> {
        let file_blocks = self.file_block_count();
        claim(sector, claimed, file_blocks)?;
        let block = {
            let header = &self.header;
            let data = &self.data;
            read_block_at(data, header, sector)?
        };
        for chunk in block.as_chunks::<4>().0 {
            out.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        *blocks += 1;
        Ok(())
    }

    fn load_fat(&mut self, claimed: &mut HashSet<i32>) -> Result<()> {
        let header_bats = (self.header.num_fat_sectors as usize).min(MAX_BAT_IN_HEADER);
        let mut fat = Vec::new();
        let mut blocks = 0usize;
        for i in 0..header_bats {
            let sector = self.header.difat[i] as i32;
            self.read_bat_sector(sector, claimed, &mut fat, &mut blocks)?;
        }

        let mut remaining = self.header.num_fat_sectors as i64 - header_bats as i64;
        let eps = self.header.fat_entries_per_sector();
        let mut next = self.header.first_difat_sector as i32;
        for _ in 0..self.header.num_difat_sectors {
            let file_blocks = self.file_block_count();
            claim(next, claimed, file_blocks)?;
            let block = read_block_at(&self.data, &self.header, next)?.to_vec();
            let xbat_fats = remaining.min((eps - 1) as i64).max(0) as usize;
            for j in 0..xbat_fats {
                let sector = u32_at(&block, j * 4) as i32;
                if sector == UNUSED_BLOCK || sector == END_OF_CHAIN {
                    break;
                }
                self.read_bat_sector(sector, claimed, &mut fat, &mut blocks)?;
            }
            remaining -= xbat_fats as i64;
            next = u32_at(&block, (eps - 1) * 4) as i32;
        }

        self.fat = fat;
        self.fat_blocks = blocks;
        Ok(())
    }

    fn load_directory(&mut self) -> Result<()> {
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut claimed: HashSet<i32> = HashSet::new();
        let mut next = self.header.first_dir_sector as i32;
        loop {
            if next == END_OF_CHAIN {
                break;
            }
            let file_blocks = self.file_block_count();
            claim(next, &mut claimed, file_blocks)?;
            chunks.push(read_block_at(&self.data, &self.header, next)?.to_vec());
            next = self.fat_next(next)?;
        }

        let mut entries = Vec::new();
        for chunk in &chunks {
            for off in (0..chunk.len()).step_by(PROPERTY_SIZE) {
                entries.push(parse_directory_entry(
                    &chunk[off..off + PROPERTY_SIZE],
                    entries.len(),
                ));
            }
        }
        if entries.is_empty() || !entries[0].is_root() {
            return Err(Error::Jackcess(
                "Invalid format, cannot convert property to RootProperty".to_string(),
            ));
        }
        self.entries = entries;
        self.build_tree()?;
        Ok(())
    }

    fn build_tree(&mut self) -> Result<()> {
        let count = self.entries.len();
        let mut parents: Vec<Option<usize>> = vec![None; count];
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut budget = count.saturating_mul(4) + 16;
        populate_tree(0, &self.entries, &mut parents, &mut children, &mut budget)?;
        for (i, entry) in self.entries.iter_mut().enumerate() {
            entry.parent = parents[i];
            entry.children = std::mem::take(&mut children[i]);
        }
        Ok(())
    }

    fn load_mini_fat(&mut self, claimed: &mut HashSet<i32>) -> Result<()> {
        let mut values = Vec::new();
        let mut blocks = 0usize;
        let mut next = self.header.first_mini_fat_sector as i32;
        let count = self.header.num_mini_fat_sectors;
        for _ in 0..count {
            if next == END_OF_CHAIN {
                break;
            }
            let file_blocks = self.file_block_count();
            claim(next, claimed, file_blocks)?;
            let block = read_block_at(&self.data, &self.header, next)?.to_vec();
            for chunk in block.as_chunks::<4>().0 {
                values.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            blocks += 1;
            next = self.fat_next(next)?;
        }
        self.mini_fat = values;
        self.mini_fat_blocks = blocks;
        Ok(())
    }

    /// Returns the 64-byte mini block with the given mini sector index.
    fn mini_block(&self, index: i32) -> Result<Vec<u8>> {
        if index < 0 {
            return Err(Error::Jackcess(format!(
                "Index {} out of bounds for length {}",
                index,
                self.entries[0].size32()
            )));
        }
        let byte_offset = index as usize * SMALL_BLOCK_SIZE;
        let big_index = byte_offset / self.sector_size;
        let big_offset = byte_offset % self.sector_size;

        let mut state = self.mini.borrow_mut();
        if !state.started {
            state.started = true;
            state.next = self.entries[0].start_sector as i32;
            if state.next == END_OF_CHAIN {
                state.done = true;
            }
        }
        while state.blocks.len() <= big_index && !state.done {
            if state.next == END_OF_CHAIN {
                state.done = true;
                break;
            }
            let file_blocks = self.file_block_count();
            claim(state.next, &mut state.used, file_blocks)?;
            let block = read_block_at(&self.data, &self.header, state.next)?.to_vec();
            state.next = self.fat_next(state.next)?;
            state.blocks.push(block);
        }
        let Some(block) = state.blocks.get(big_index) else {
            if state.blocks.is_empty() {
                return Err(Error::Jackcess(
                    "Can't read from a new stream before it has been written to".to_string(),
                ));
            }
            return Err(Error::Jackcess(
                "Can't read past the end of the stream".to_string(),
            ));
        };
        Ok(block[big_offset..big_offset + SMALL_BLOCK_SIZE].to_vec())
    }
}

fn read_block_at(data: &[u8], header: &CompoundHeader, offset: i32) -> Result<Vec<u8>> {
    let sector_size = header.sector_size();
    let block_wanted = offset as i64 + 1;
    let start = block_wanted * sector_size as i64;
    if start < 0 || start >= data.len() as i64 {
        // POI's `FileBackedDataSource.read` throws `IndexOutOfBoundsException`
        // ("Position ... past the end of the file") for a block which starts
        // at or after the end of the data.
        return Err(Error::Jackcess(format!("Block {} not found", offset)));
    }
    let start = start as usize;
    let end = (start + sector_size).min(data.len());
    let mut block = vec![0u8; sector_size];
    block[..end - start].copy_from_slice(&data[start..end]);
    // POI reads into a full-size, zero-filled heap buffer, so a final partial
    // sector is zero padded instead of failing (Jackcess' memory channel does
    // the same).
    Ok(block)
}

fn claim(offset: i32, used: &mut HashSet<i32>, file_blocks: usize) -> Result<()> {
    if offset as i64 >= file_blocks as i64 {
        // POI's ChainLoopDetector ignores blocks beyond the current end of the
        // data (they are legal while writing).
        return Ok(());
    }
    if offset < 0 {
        return Err(Error::Jackcess(format!(
            "Index {} out of bounds for length {}",
            offset, file_blocks
        )));
    }
    if !used.insert(offset) {
        return Err(Error::Jackcess(format!(
            "Potential loop detected - Block {} was already claimed but was just requested again",
            offset
        )));
    }
    Ok(())
}

fn parse_directory_entry(raw: &[u8], index: usize) -> DirectoryEntry {
    let entry_type_raw = raw[66];
    let entry_type = EntryType::from_u8(entry_type_raw);

    let name_size = u16_at(raw, 64);
    let name_chars = (name_size as usize / 2).saturating_sub(1);
    let mut units = Vec::with_capacity(name_chars.min(32));
    for i in 0..name_chars.min(32) {
        units.push(u16_at(raw, i * 2));
    }
    let name = String::from_utf16_lossy(&units);

    let mut clsid = [0u8; 16];
    clsid.copy_from_slice(&raw[80..96]);

    DirectoryEntry {
        index,
        name,
        entry_type,
        color: raw[67],
        left_sibling: u32_at(raw, 68) as i32,
        right_sibling: u32_at(raw, 72) as i32,
        child: u32_at(raw, 76) as i32,
        clsid,
        state_bits: u32_at(raw, 96),
        creation_time: u64_at(raw, 100),
        modified_time: u64_at(raw, 108),
        start_sector: u32_at(raw, 116),
        size: u64_at(raw, 120),
        parent: None,
        children: Vec::new(),
    }
}

fn valid_entry_index(index: i32, count: usize) -> bool {
    index >= 0 && (index as usize) < count
}

/// Port of POI's `PropertyTable.populatePropertyTree`.
fn populate_tree(
    parent: usize,
    entries: &[DirectoryEntry],
    parents: &mut [Option<usize>],
    children: &mut [Vec<usize>],
    budget: &mut usize,
) -> Result<()> {
    let child = entries[parent].child;
    if !valid_entry_index(child, entries.len()) {
        return Ok(());
    }
    let mut stack: Vec<i32> = vec![child];
    while let Some(index) = stack.pop() {
        if !valid_entry_index(index, entries.len()) {
            continue;
        }
        if *budget == 0 {
            return Err(Error::Jackcess(
                "Invalid format, property tree is too deep".to_string(),
            ));
        }
        *budget -= 1;
        let index = index as usize;
        if entries[index].entry_type == EntryType::Empty {
            // POI's property list contains `null` for these and skips them.
            continue;
        }
        children[parent].push(index);
        parents[index] = Some(parent);
        if entries[index].entry_type == EntryType::Storage {
            populate_tree(index, entries, parents, children, budget)?;
        }
        let prev = entries[index].left_sibling;
        if valid_entry_index(prev, entries.len()) {
            stack.push(prev);
        }
        let next = entries[index].right_sibling;
        if valid_entry_index(next, entries.len()) {
            stack.push(next);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Access OLE wrapper (OleUtil)
// ---------------------------------------------------------------------------

/// The decoded type of an Access OLE value (`OleBlob.ContentType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentType {
    /// The blob contains a link (file path) to external content.
    Link,
    /// The blob contains a simple package (embedded file plus names).
    SimplePackage,
    /// The blob contains an OLE2 compound storage.
    CompoundStorage,
    /// The wrapper is understood but the content is not.
    Other,
    /// The blob is not a valid OLE wrapper.
    Unknown,
}

impl ContentType {
    /// The Java enum constant name.
    pub fn name(&self) -> &'static str {
        match self {
            ContentType::Link => "LINK",
            ContentType::SimplePackage => "SIMPLE_PACKAGE",
            ContentType::CompoundStorage => "COMPOUND_STORAGE",
            ContentType::Other => "OTHER",
            ContentType::Unknown => "UNKNOWN",
        }
    }
}

/// An Access OLE column value (`OleBlob`, parsed with `OleUtil.parseBlob`).
#[derive(Clone, Debug)]
pub struct OleBlob {
    bytes: Vec<u8>,
}

impl OleBlob {
    /// `OleBlob.Builder.fromInternalData(byte[])` / `OleUtil.parseBlob`.
    pub fn from_internal_data(bytes: &[u8]) -> OleBlob {
        OleBlob {
            bytes: bytes.to_vec(),
        }
    }

    /// Creates a blob from an owned byte vector.
    pub fn from_vec(bytes: Vec<u8>) -> OleBlob {
        OleBlob { bytes }
    }

    /// The raw (Access-internal) blob bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `OleBlob.length()` (the length of the whole wrapped value).
    pub fn length(&self) -> usize {
        self.bytes.len()
    }

    /// `OleBlob.getContent()`.
    pub fn content(&self) -> Result<Content<'_>> {
        parse_content(self)
    }
}

/// The content of an [`OleBlob`] (`OleBlob.Content`).
// Mirrors `OleBlob.Content`: each variant owns the parsed package objects, and
// boxing one variant would make every accessor indirect for no real gain.
#[expect(
    clippy::large_enum_variant,
    reason = "mirrors OleBlob.Content; variants own parsed package state"
)]
#[derive(Debug)]
pub enum Content<'a> {
    /// `ContentType::UNKNOWN`.
    Unknown(UnknownContent<'a>),
    /// `ContentType::LINK`.
    Link(LinkContent<'a>),
    /// `ContentType::SIMPLE_PACKAGE`.
    SimplePackage(SimplePackageContent<'a>),
    /// `ContentType::OTHER`.
    Other(OtherContent<'a>),
    /// `ContentType::COMPOUND_STORAGE`.
    CompoundStorage(CompoundContent<'a>),
}

impl<'a> Content<'a> {
    /// `Content.getType()`.
    pub fn content_type(&self) -> ContentType {
        match self {
            Content::Unknown(_) => ContentType::Unknown,
            Content::Link(_) => ContentType::Link,
            Content::SimplePackage(_) => ContentType::SimplePackage,
            Content::Other(_) => ContentType::Other,
            Content::CompoundStorage(_) => ContentType::CompoundStorage,
        }
    }

    /// The owning blob.
    pub fn blob(&self) -> &'a OleBlob {
        match self {
            Content::Unknown(c) => c.blob,
            Content::Link(c) => c.package.embedded.blob,
            Content::SimplePackage(c) => c.package.embedded.blob,
            Content::Other(c) => c.package.embedded.blob,
            Content::CompoundStorage(c) => c.package.embedded.blob,
        }
    }

    /// `PackageContent.getPrettyName()` for package contents.
    pub fn pretty_name(&self) -> Option<&str> {
        self.package().map(|p| p.pretty_name.as_str())
    }

    /// `PackageContent.getClassName()` for package contents.
    pub fn class_name(&self) -> Option<&str> {
        self.package().map(|p| p.class_name.as_str())
    }

    /// `PackageContent.getTypeName()` for package contents.
    pub fn type_name(&self) -> Option<&str> {
        self.package().map(|p| p.type_name.as_str())
    }

    /// `SimplePackageContent` / `LinkContent` file name.
    pub fn file_name(&self) -> Option<&str> {
        match self {
            Content::Link(c) => Some(&c.file_name),
            Content::SimplePackage(c) => Some(&c.file_name),
            _ => None,
        }
    }

    /// `LinkContent.getLinkPath()`.
    pub fn link_path(&self) -> Option<&str> {
        match self {
            Content::Link(c) => Some(&c.link_path),
            _ => None,
        }
    }

    /// `SimplePackageContent` / `LinkContent` file path.
    pub fn file_path(&self) -> Option<&str> {
        match self {
            Content::Link(c) => Some(&c.file_path),
            Content::SimplePackage(c) => Some(&c.file_path),
            _ => None,
        }
    }

    /// `SimplePackageContent.getLocalFilePath()`.
    pub fn local_file_path(&self) -> Option<&str> {
        match self {
            Content::SimplePackage(c) => Some(&c.local_file_path),
            _ => None,
        }
    }

    /// `EmbeddedContent.length()` for embedded contents (the data block
    /// length; `-1` for links and unknown content).
    pub fn length(&self) -> i64 {
        match self {
            Content::Link(_) => -1,
            Content::Unknown(_) => -1,
            Content::SimplePackage(c) => c.package.embedded.length,
            Content::Other(c) => c.package.embedded.length,
            Content::CompoundStorage(c) => c.package.embedded.length,
        }
    }

    /// The embedded bytes (`EmbeddedContent.getStream()` / `writeTo`).
    pub fn data(&self) -> Result<&'a [u8]> {
        match self {
            Content::Link(_) => Err(Error::IllegalArgument(
                "link content has no embedded data".to_string(),
            )),
            Content::Unknown(_) => Err(Error::IllegalArgument(
                "unknown content has no embedded data".to_string(),
            )),
            Content::SimplePackage(c) => c.package.embedded.data(),
            Content::Other(c) => c.package.embedded.data(),
            Content::CompoundStorage(c) => c.package.embedded.data(),
        }
    }

    fn package(&self) -> Option<&PackageContent<'a>> {
        match self {
            Content::Link(c) => Some(&c.package),
            Content::SimplePackage(c) => Some(&c.package),
            Content::Other(c) => Some(&c.package),
            Content::CompoundStorage(c) => Some(&c.package),
            Content::Unknown(_) => None,
        }
    }

    /// Downcast to [`LinkContent`].
    pub fn as_link(&self) -> Option<&LinkContent<'a>> {
        match self {
            Content::Link(c) => Some(c),
            _ => None,
        }
    }

    /// Downcast to [`SimplePackageContent`].
    pub fn as_simple_package(&self) -> Option<&SimplePackageContent<'a>> {
        match self {
            Content::SimplePackage(c) => Some(c),
            _ => None,
        }
    }

    /// Downcast to [`OtherContent`].
    pub fn as_other(&self) -> Option<&OtherContent<'a>> {
        match self {
            Content::Other(c) => Some(c),
            _ => None,
        }
    }

    /// Downcast to [`CompoundContent`].
    pub fn as_compound(&self) -> Option<&CompoundContent<'a>> {
        match self {
            Content::CompoundStorage(c) => Some(c),
            _ => None,
        }
    }
}

/// Common embedded content (`OleUtil.EmbeddedContentImpl`).
#[derive(Debug)]
pub struct EmbeddedContent<'a> {
    blob: &'a OleBlob,
    position: i64,
    length: i64,
}

impl<'a> EmbeddedContent<'a> {
    /// `EmbeddedContent.length()`.
    pub fn length(&self) -> i64 {
        self.length
    }

    /// The embedded data (`getStream()` / `writeTo`).
    ///
    /// Java's `EmbeddedContentImpl.getStream()` wraps the byte array in a
    /// `ByteArrayInputStream`, which clamps the requested range to the end of
    /// the array instead of failing.  A negative length therefore yields an
    /// empty stream, and a declared length past the end yields the remaining
    /// bytes (the `length()` accessor still reports the declared length).
    pub fn data(&self) -> Result<&'a [u8]> {
        if self.position < 0 {
            return Err(Error::Jackcess(format!(
                "Index {} out of bounds for length {}",
                self.position,
                self.blob.bytes.len()
            )));
        }
        let start = self.position as usize;
        if start > self.blob.bytes.len() {
            return Ok(&self.blob.bytes[self.blob.bytes.len()..]);
        }
        if self.length < 0 {
            return Ok(&self.blob.bytes[start..start]);
        }
        let end = start
            .saturating_add(self.length as usize)
            .min(self.blob.bytes.len());
        Ok(&self.blob.bytes[start..end])
    }
}

/// Common package content (`OleUtil.EmbeddedPackageContentImpl`).
#[derive(Debug)]
pub struct PackageContent<'a> {
    embedded: EmbeddedContent<'a>,
    pretty_name: String,
    class_name: String,
    type_name: String,
}

impl<'a> PackageContent<'a> {
    /// `PackageContent.getPrettyName()`.
    pub fn pretty_name(&self) -> &str {
        &self.pretty_name
    }

    /// `PackageContent.getClassName()`.
    pub fn class_name(&self) -> &str {
        &self.class_name
    }

    /// `PackageContent.getTypeName()`.
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// `EmbeddedContent.length()`.
    pub fn length(&self) -> i64 {
        self.embedded.length
    }

    /// `EmbeddedContent.getStream()`.
    pub fn data(&self) -> Result<&'a [u8]> {
        self.embedded.data()
    }
}

/// `ContentType::UNKNOWN` content (`OleUtil.UnknownContentImpl`).
#[derive(Debug)]
pub struct UnknownContent<'a> {
    blob: &'a OleBlob,
}

impl<'a> UnknownContent<'a> {
    /// The owning blob.
    pub fn blob(&self) -> &'a OleBlob {
        self.blob
    }
}

/// `ContentType::LINK` content (`OleUtil.LinkContentImpl`).
#[derive(Debug)]
pub struct LinkContent<'a> {
    package: PackageContent<'a>,
    file_name: String,
    link_path: String,
    file_path: String,
}

impl<'a> LinkContent<'a> {
    /// `LinkContent.getFileName()`.
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// `LinkContent.getLinkPath()`.
    pub fn link_path(&self) -> &str {
        &self.link_path
    }

    /// `LinkContent.getFilePath()`.
    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    /// The package metadata.
    pub fn package(&self) -> &PackageContent<'a> {
        &self.package
    }

    /// `LinkContent.getLinkStream()`.
    pub fn link_stream(&self) -> Result<Vec<u8>> {
        std::fs::read(&self.link_path).map_err(Error::Io)
    }
}

/// `ContentType::SIMPLE_PACKAGE` content
/// (`OleUtil.SimplePackageContentImpl`).
#[derive(Debug)]
pub struct SimplePackageContent<'a> {
    package: PackageContent<'a>,
    file_name: String,
    file_path: String,
    local_file_path: String,
}

impl<'a> SimplePackageContent<'a> {
    /// `SimplePackageContent.getFileName()`.
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// `SimplePackageContent.getFilePath()`.
    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    /// `SimplePackageContent.getLocalFilePath()`.
    pub fn local_file_path(&self) -> &str {
        &self.local_file_path
    }

    /// The package metadata.
    pub fn package(&self) -> &PackageContent<'a> {
        &self.package
    }

    /// `EmbeddedContent.length()` (the embedded file length).
    pub fn length(&self) -> i64 {
        self.package.embedded.length
    }

    /// `EmbeddedContent.getStream()` (the embedded file bytes).
    pub fn data(&self) -> Result<&'a [u8]> {
        self.package.embedded.data()
    }
}

/// `ContentType::OTHER` content (`OleUtil.OtherContentImpl`).
#[derive(Debug)]
pub struct OtherContent<'a> {
    package: PackageContent<'a>,
}

impl<'a> OtherContent<'a> {
    /// The package metadata.
    pub fn package(&self) -> &PackageContent<'a> {
        &self.package
    }

    /// `EmbeddedContent.length()`.
    pub fn length(&self) -> i64 {
        self.package.embedded.length
    }

    /// `EmbeddedContent.getStream()`.
    pub fn data(&self) -> Result<&'a [u8]> {
        self.package.embedded.data()
    }
}

/// `ContentType::COMPOUND_STORAGE` content
/// (`CompoundOleUtil.CompoundContentImpl`).
#[derive(Debug)]
pub struct CompoundContent<'a> {
    package: PackageContent<'a>,
    file: OnceCell<CompoundFile>,
    error: RefCell<Option<String>>,
}

impl<'a> CompoundContent<'a> {
    fn new(package: PackageContent<'a>) -> CompoundContent<'a> {
        CompoundContent {
            package,
            file: OnceCell::new(),
            error: RefCell::new(None),
        }
    }

    /// The package metadata.
    pub fn package(&self) -> &PackageContent<'a> {
        &self.package
    }

    /// `EmbeddedContent.length()` (the compound data block length).
    pub fn length(&self) -> i64 {
        self.package.embedded.length
    }

    /// The raw compound file bytes.
    pub fn data(&self) -> Result<&'a [u8]> {
        self.package.embedded.data()
    }

    /// The lazily parsed compound file (`POIFSFileSystem`).
    pub fn compound_file(&self) -> Result<&CompoundFile> {
        if let Some(file) = self.file.get() {
            return Ok(file);
        }
        if let Some(message) = self.error.borrow().as_ref() {
            return Err(Error::Jackcess(message.clone()));
        }
        let data = self.data()?;
        match CompoundFile::parse(data) {
            Ok(file) => Ok(self.file.get_or_init(|| file)),
            Err(err) => {
                let message = err.to_string();
                *self.error.borrow_mut() = Some(message);
                Err(err)
            }
        }
    }

    /// `CompoundContent.iterator()`: all document entries, recursively.
    ///
    /// Entry names are the encoded, `/` separated names Jackcess produces:
    /// every component is `URLEncoder.encode(name, "UTF-8")` and nested
    /// directories add another `/` to the prefix (so a document inside one
    /// storage is named `//doc`, matching `CompoundOleUtil.getEntries`).
    pub fn entries(&self) -> Result<Vec<CompoundEntry<'_>>> {
        let file = self.compound_file()?;
        let mut out = Vec::new();
        collect_entries(file, 0, "/", &mut out);
        Ok(out)
    }

    /// `CompoundContent.getEntry(String)`.
    pub fn entry(&self, entry_name: &str) -> Result<CompoundEntry<'_>> {
        let file = self.compound_file()?;
        let mut components = Vec::new();
        for part in entry_name.split('/') {
            if part.is_empty() {
                continue;
            }
            components.push(url_decode(part)?);
        }
        let mut current = 0usize;
        let mut found: Option<usize> = None;
        let mut iter = components.iter().peekable();
        while let Some(component) = iter.next() {
            let Some(child) = file.child_by_name(current, component) else {
                return Err(Error::Jackcess(format!("no such entry: \"{}\"", component)));
            };
            let entry = &file.entries[child];
            if entry.is_storage() {
                current = child;
            } else if entry.is_stream() && iter.peek().is_none() {
                found = Some(child);
            } else {
                break;
            }
        }
        let Some(index) = found else {
            return Err(Error::Jackcess(format!(
                "Could not find document {}",
                entry_name
            )));
        };
        Ok(CompoundEntry {
            file,
            index,
            name: entry_name.to_string(),
        })
    }

    /// `CompoundContent.hasContentsEntry()` (a case-sensitive root entry named
    /// `CONTENTS`).
    pub fn has_contents_entry(&self) -> Result<bool> {
        let file = self.compound_file()?;
        Ok(file.child_by_name(0, "CONTENTS").is_some())
    }

    /// `CompoundContent.getContentsEntry()`.
    pub fn contents_entry(&self) -> Result<CompoundEntry<'_>> {
        self.entry("CONTENTS")
    }
}

/// A document entry of a [`CompoundContent`]
/// (`CompoundContent.Entry`).
#[derive(Debug)]
pub struct CompoundEntry<'a> {
    file: &'a CompoundFile,
    index: usize,
    name: String,
}

impl<'a> CompoundEntry<'a> {
    /// `Entry.getName()` (the encoded, `/` separated name).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The underlying directory entry.
    pub fn directory_entry(&self) -> &'a DirectoryEntry {
        &self.file.entries[self.index]
    }

    /// `Entry.length()`.
    pub fn length(&self) -> i64 {
        i64::from(self.file.entries[self.index].size32())
    }

    /// `Entry.getStream()` / `writeTo`.
    pub fn data(&self) -> Result<Vec<u8>> {
        self.file.read_stream(self.index)
    }
}

fn collect_entries<'a>(
    file: &'a CompoundFile,
    index: usize,
    prefix: &str,
    out: &mut Vec<CompoundEntry<'a>>,
) {
    for &child in &file.entries[index].children {
        let entry = &file.entries[child];
        match entry.entry_type {
            EntryType::Storage => {
                let child_prefix = format!("{}/", prefix);
                collect_entries(file, child, &child_prefix, out);
            }
            EntryType::Stream => out.push(CompoundEntry {
                file,
                index: child,
                name: format!("{}{}", prefix, url_encode(&entry.name)),
            }),
            _ => {}
        }
    }
}

// -- URL encoding used by Jackcess for compound entry names -----------------

/// Equivalent to Java's `URLEncoder.encode(value, "UTF-8")` for the
/// characters which can appear in compound entry names.
pub fn url_encode(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '*' | '_' => out.push(ch),
            ' ' => out.push('+'),
            _ => {
                let mut buf = [0u8; 4];
                for byte in ch.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{:02X}", byte));
                }
            }
        }
    }
    out
}

/// Equivalent to Java's `URLDecoder.decode(value, "UTF-8")`.
pub fn url_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(Error::IllegalArgument(
                        "URLDecoder: Incomplete trailing escape (%) pattern".to_string(),
                    ));
                }
                let hi = hex_val(bytes[i + 1])?;
                let lo = hex_val(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out)
        .map_err(|_| Error::IllegalArgument("URLDecoder: Illegal hex characters".to_string()))
}

fn hex_val(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::IllegalArgument(
            "URLDecoder: Illegal hex characters in escape (%) pattern".to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// OleUtil.parseContent
// ---------------------------------------------------------------------------

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    limit: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Cursor<'a> {
        Cursor {
            data,
            pos: 0,
            limit: data.len(),
        }
    }

    fn with_limit(data: &'a [u8], pos: usize, limit: usize) -> Cursor<'a> {
        Cursor { data, pos, limit }
    }

    fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.pos)
    }

    fn position(&self) -> usize {
        self.pos
    }

    fn set_position(&mut self, pos: i64) -> Result<()> {
        if pos < 0 {
            return Err(Error::IllegalArgument(format!(
                "newPosition < 0: ({} < 0)",
                pos
            )));
        }
        if pos > self.limit as i64 {
            return Err(Error::IllegalArgument(format!(
                "newPosition > limit: ({} > {})",
                pos, self.limit
            )));
        }
        self.pos = pos as usize;
        Ok(())
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.pos + len > self.limit {
            return Err(Error::BufferUnderflow(String::new()));
        }
        let slice = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    fn read_u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn read_i16(&mut self) -> Result<i16> {
        Ok(self.read_u16()? as i16)
    }

    fn read_i32(&mut self) -> Result<i32> {
        let b = self.bytes(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_i64(&mut self) -> Result<i64> {
        let b = self.bytes(8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
}

fn decode_ascii(bytes: &[u8]) -> String {
    // Java's US_ASCII charset decoder replaces every non-ASCII byte with
    // U+FFFD.
    bytes
        .iter()
        .map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' })
        .collect()
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 1 < bytes.len() {
        units.push(u16::from_le_bytes([bytes[i], bytes[i + 1]]));
        i += 2;
    }
    let mut text = String::from_utf16_lossy(&units);
    if i < bytes.len() {
        text.push('\u{FFFD}');
    }
    text
}

fn read_str_at(cursor: &mut Cursor<'_>, off: i64, len: i64, utf16: bool) -> Result<String> {
    if off < 0 || len < 0 || (off + len) > cursor.data.len() as i64 {
        return Err(Error::Jackcess(format!(
            "Range [{}, {}) out of bounds for length {}",
            off,
            off.saturating_add(len),
            cursor.data.len()
        )));
    }
    if len == 0 {
        // OleUtil.readStr ends with `str.charAt(str.length() - 1)`.
        return Err(Error::Jackcess("String index out of range: -1".to_string()));
    }
    let start = off as usize;
    let end = start + len as usize;
    let slice = &cursor.data[start..end];
    let mut text = if utf16 {
        decode_utf16le(slice)
    } else {
        decode_ascii(slice)
    };
    if text.ends_with('\0') {
        text.pop();
    }
    cursor.set_position(off + len)?;
    Ok(text)
}

fn read_str(cursor: &mut Cursor<'_>, off: i64, len: i64) -> Result<String> {
    read_str_at(cursor, off, len, false)
}

fn read_zero_term_str(cursor: &mut Cursor<'_>) -> Result<String> {
    let off = cursor.position();
    while cursor.pos < cursor.limit {
        let byte = cursor.data[cursor.pos];
        cursor.pos += 1;
        if byte == 0 {
            break;
        }
    }
    let len = cursor.pos - off;
    cursor.pos = off;
    read_str(cursor, off as i64, len as i64)
}

fn parse_content(blob: &OleBlob) -> Result<Content<'_>> {
    let data = &blob.bytes;
    let mut cursor = Cursor::new(data);
    if cursor.remaining() < 2 || cursor.read_u16()? != PACKAGE_SIGNATURE {
        return Ok(Content::Unknown(UnknownContent { blob }));
    }

    let header_size = cursor.read_i16()? as i64;
    let _obj_type = cursor.read_i32()?;
    let pretty_name_len = cursor.read_i16()? as i64;
    let class_name_len = cursor.read_i16()? as i64;
    let pretty_name_off = cursor.read_i16()? as i64;
    let class_name_off = cursor.read_i16()? as i64;
    let _obj_size = cursor.read_i32()?;
    let pretty_name = read_str(&mut cursor, pretty_name_off, pretty_name_len)?;
    let class_name = read_str(&mut cursor, class_name_off, class_name_len)?;
    cursor.set_position(header_size)?;

    let ole_ver = cursor.read_i32()?;
    let _format = cursor.read_i32()?;
    if ole_ver != OLE_VERSION {
        return Ok(Content::Unknown(UnknownContent { blob }));
    }

    let type_name_len = cursor.read_i32()? as i64;
    let type_name_pos = cursor.position() as i64;
    let type_name = read_str(&mut cursor, type_name_pos, type_name_len)?;
    cursor.read_i64()?;
    let data_block_len = cursor.read_i32()? as i64;
    let data_block_pos = cursor.position() as i64;

    if type_name.eq_ignore_ascii_case(SIMPLE_PACKAGE_TYPE) {
        return create_simple_package(
            blob,
            pretty_name,
            class_name,
            type_name,
            data,
            data_block_pos,
            data_block_len,
        );
    }

    let remaining = data.len() as i64 - data_block_pos;
    if remaining >= COMPOUND_STORAGE_SIGNATURE.len() as i64
        && data_block_pos >= 0
        && (data_block_pos as usize + 8) <= data.len()
        && data[data_block_pos as usize..data_block_pos as usize + 8] == COMPOUND_STORAGE_SIGNATURE
    {
        let package = PackageContent {
            embedded: EmbeddedContent {
                blob,
                position: data_block_pos,
                length: data_block_len,
            },
            pretty_name,
            class_name,
            type_name,
        };
        return Ok(Content::CompoundStorage(CompoundContent::new(package)));
    }

    let package = PackageContent {
        embedded: EmbeddedContent {
            blob,
            position: data_block_pos,
            length: data_block_len,
        },
        pretty_name,
        class_name,
        type_name,
    };
    Ok(Content::Other(OtherContent { package }))
}

#[allow(clippy::too_many_arguments)]
fn create_simple_package<'a>(
    blob: &'a OleBlob,
    pretty_name: String,
    class_name: String,
    type_name: String,
    data: &'a [u8],
    data_block_pos: i64,
    data_block_len: i64,
) -> Result<Content<'a>> {
    let limit = data_block_pos + data_block_len;
    if data_block_pos < 0 || limit > data.len() as i64 {
        return Err(Error::IllegalArgument(format!(
            "newLimit > capacity: ({} > {})",
            limit,
            data.len()
        )));
    }
    let mut cursor = Cursor::with_limit(data, data_block_pos as usize, limit as usize);

    let package_sig = cursor.read_u16()?;
    if package_sig != PACKAGE_STREAM_SIGNATURE {
        let package = PackageContent {
            embedded: EmbeddedContent {
                blob,
                position: data_block_pos,
                length: data_block_len,
            },
            pretty_name,
            class_name,
            type_name,
        };
        return Ok(Content::Other(OtherContent { package }));
    }

    let mut file_name = read_zero_term_str(&mut cursor)?;
    let mut file_path = read_zero_term_str(&mut cursor)?;
    let package_type = cursor.read_i32()?;

    if package_type == PS_EMBEDDED_FILE {
        let local_path_len = cursor.read_i32()? as i64;
        let local_pos = cursor.position() as i64;
        let mut local_file_path = read_str(&mut cursor, local_pos, local_path_len)?;

        let data_len = cursor.read_i32()? as i64;
        let data_pos = cursor.position() as i64;
        cursor.set_position(data_pos + data_len)?;

        // The trailing strings are stored in reverse order (local path, file
        // name, file path) and use UTF-16, so they are preferred over the
        // ASCII versions found earlier.
        let mut str_num = 0;
        while cursor.remaining() >= 4 {
            let len = cursor.read_i32()? as i64;
            let pos = cursor.position() as i64;
            let value = read_str_at(&mut cursor, pos, len * 2, true)?;
            match str_num {
                0 => local_file_path = value,
                1 => file_name = value,
                2 => file_path = value,
                _ => {}
            }
            str_num += 1;
        }

        let package = PackageContent {
            embedded: EmbeddedContent {
                blob,
                position: data_pos,
                length: data_len,
            },
            pretty_name,
            class_name,
            type_name,
        };
        return Ok(Content::SimplePackage(SimplePackageContent {
            package,
            file_name,
            file_path,
            local_file_path,
        }));
    }

    if package_type == PS_LINKED_FILE {
        cursor.read_u16()?;
        let link_path = read_zero_term_str(&mut cursor)?;
        let package = PackageContent {
            embedded: EmbeddedContent {
                blob,
                position: -1,
                length: -1,
            },
            pretty_name,
            class_name,
            type_name,
        };
        return Ok(Content::Link(LinkContent {
            package,
            file_name,
            link_path,
            file_path,
        }));
    }

    let package = PackageContent {
        embedded: EmbeddedContent {
            blob,
            position: data_block_pos,
            length: data_block_len,
        },
        pretty_name,
        class_name,
        type_name,
    };
    Ok(Content::Other(OtherContent { package }))
}

// ---------------------------------------------------------------------------
// OleUtil.createBlob (OleBlob.Builder)
// ---------------------------------------------------------------------------

/// `OleUtil.PACKAGE_OBJECT_TYPE`.
const PACKAGE_OBJECT_TYPE: i32 = 0x02;
/// `OleUtil.OLE_FORMAT`.
const OLE_FORMAT: i32 = 0x02;
/// `OleUtil.LINK_HEADER`.
const LINK_HEADER: u16 = 0x01;
/// `OleUtil.PACKAGE_FOOTER`.
const PACKAGE_FOOTER: [u8; 12] = [
    0x01, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0xAD, 0x05, 0xFE,
];

/// Builder for an [`OleBlob`], the port of
/// `com.healthmarketscience.jackcess.util.OleBlob.Builder`.
///
/// The Java builder is a nested class (`OleBlob.Builder`); Rust has no nested
/// types, so the module level [`OleBlobBuilder`] is the equivalent (the alias
/// [`Builder`] keeps the Java name).
///
/// ```ignore
/// let blob = OleBlobBuilder::new()
///     .set_simple_package_bytes(b"hello")
///     .to_blob()?;
/// let blob = OleBlob::builder()
///     .set_link_path("C:\\data\\file.txt")
///     .to_blob()?;
/// ```
///
/// Like the Java class, `to_blob` is the "finish" step (`Ok`/`Finish` in the
/// task wording): the builder performs no I/O until then except the
/// `set_*_file` helpers, which read the file eagerly (`setSimplePackage(File)`
/// in Java opens the stream and `toBlob` reads it).  The produced bytes are
/// exactly `OleUtil.createBlob`'s.
#[derive(Default)]
pub struct OleBlobBuilder {
    type_: Option<ContentType>,
    bytes: Option<Vec<u8>>,
    content_len: i64,
    file_name: Option<String>,
    file_path: Option<String>,
    pretty_name: Option<String>,
    class_name: Option<String>,
    type_name: Option<String>,
}

/// `OleBlob.Builder` (Java name).
pub type Builder = OleBlobBuilder;

impl std::fmt::Debug for OleBlobBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OleBlobBuilder")
            .field("type", &self.type_)
            .field("bytes", &self.bytes.as_ref().map(Vec::len))
            .field("content_len", &self.content_len)
            .field("file_name", &self.file_name)
            .field("file_path", &self.file_path)
            .field("pretty_name", &self.pretty_name)
            .field("class_name", &self.class_name)
            .field("type_name", &self.type_name)
            .finish()
    }
}

impl OleBlobBuilder {
    /// `OleBlob.Builder.PACKAGE_PRETTY_NAME`.
    pub const PACKAGE_PRETTY_NAME: &'static str = "Packager Shell Object";
    /// `OleBlob.Builder.PACKAGE_TYPE_NAME`.
    pub const PACKAGE_TYPE_NAME: &'static str = "Package";

    /// `new OleBlob.Builder()`.
    pub fn new() -> OleBlobBuilder {
        OleBlobBuilder::default()
    }

    /// `OleBlob.Builder.getType()`.
    pub fn get_type(&self) -> Option<ContentType> {
        self.type_
    }

    /// `OleBlob.Builder.getBytes()`.
    pub fn get_bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }

    /// `OleBlob.Builder.getStream()`.  The port builder holds the stream
    /// eagerly (see [`OleBlobBuilder::set_simple_package_stream`]), so this is
    /// `None` when the content was supplied as bytes.
    pub fn get_stream(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }

    /// `OleBlob.Builder.getContentLength()`.
    pub fn get_content_length(&self) -> i64 {
        self.content_len
    }

    /// `OleBlob.Builder.getFileName()`.
    pub fn get_file_name(&self) -> Option<&str> {
        self.file_name.as_deref()
    }

    /// `OleBlob.Builder.getFilePath()`.
    pub fn get_file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    /// `OleBlob.Builder.getPrettyName()`.
    pub fn get_pretty_name(&self) -> Option<&str> {
        self.pretty_name.as_deref()
    }

    /// `OleBlob.Builder.getClassName()`.
    pub fn get_class_name(&self) -> Option<&str> {
        self.class_name.as_deref()
    }

    /// `OleBlob.Builder.getTypeName()`.
    pub fn get_type_name(&self) -> Option<&str> {
        self.type_name.as_deref()
    }

    /// `OleBlob.Builder.setSimplePackageBytes(byte[])`.
    pub fn set_simple_package_bytes(&mut self, bytes: impl Into<Vec<u8>>) -> &mut OleBlobBuilder {
        self.bytes = Some(bytes.into());
        self.content_len = self.bytes.as_ref().map_or(0, |bytes| bytes.len() as i64);
        self.set_default_package_type();
        self.type_ = Some(ContentType::SimplePackage);
        self
    }

    /// `OleBlob.Builder.setSimplePackageStream(InputStream, long)`: the content
    /// is read eagerly, so the resulting blob is the same as Java's.
    pub fn set_simple_package_stream(
        &mut self,
        mut stream: impl std::io::Read,
        length: i64,
    ) -> Result<&mut OleBlobBuilder> {
        let mut bytes = Vec::with_capacity(length.max(0) as usize);
        stream.read_to_end(&mut bytes)?;
        self.bytes = Some(bytes);
        self.content_len = length;
        self.set_default_package_type();
        self.type_ = Some(ContentType::SimplePackage);
        Ok(self)
    }

    /// `OleBlob.Builder.setSimplePackageFileName(String)`.
    pub fn set_simple_package_file_name(
        &mut self,
        file_name: impl Into<String>,
    ) -> &mut OleBlobBuilder {
        self.file_name = Some(file_name.into());
        self.set_default_package_type();
        self.type_ = Some(ContentType::SimplePackage);
        self
    }

    /// `OleBlob.Builder.setSimplePackageFilePath(String)`.
    pub fn set_simple_package_file_path(
        &mut self,
        file_path: impl Into<String>,
    ) -> &mut OleBlobBuilder {
        self.file_path = Some(file_path.into());
        self.set_default_package_type();
        self.type_ = Some(ContentType::SimplePackage);
        self
    }

    /// `OleBlob.Builder.setSimplePackage(File)`.
    pub fn set_simple_package_file(
        &mut self,
        path: &std::path::Path,
    ) -> Result<&mut OleBlobBuilder> {
        self.file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        self.file_path = Some(java_absolute_path(path)?);
        let file = std::fs::File::open(path)?;
        let length = file.metadata()?.len() as i64;
        self.set_simple_package_stream(file, length)
    }

    /// `OleBlob.Builder.setLinkFileName(String)`.
    pub fn set_link_file_name(&mut self, file_name: impl Into<String>) -> &mut OleBlobBuilder {
        self.file_name = Some(file_name.into());
        self.set_default_package_type();
        self.type_ = Some(ContentType::Link);
        self
    }

    /// `OleBlob.Builder.setLinkPath(String)`.
    pub fn set_link_path(&mut self, link: impl Into<String>) -> &mut OleBlobBuilder {
        self.file_path = Some(link.into());
        self.set_default_package_type();
        self.type_ = Some(ContentType::Link);
        self
    }

    /// `OleBlob.Builder.setLink(File)`.
    pub fn set_link_file(&mut self, path: &std::path::Path) -> &mut OleBlobBuilder {
        self.file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        self.file_path = java_absolute_path(path).ok();
        self.set_default_package_type();
        self.type_ = Some(ContentType::Link);
        self
    }

    /// `OleBlob.Builder.setOtherBytes(byte[])`.
    pub fn set_other_bytes(&mut self, bytes: impl Into<Vec<u8>>) -> &mut OleBlobBuilder {
        self.bytes = Some(bytes.into());
        self.content_len = self.bytes.as_ref().map_or(0, |bytes| bytes.len() as i64);
        self.type_ = Some(ContentType::Other);
        self
    }

    /// `OleBlob.Builder.setOtherStream(InputStream, long)` (read eagerly).
    pub fn set_other_stream(
        &mut self,
        mut stream: impl std::io::Read,
        length: i64,
    ) -> Result<&mut OleBlobBuilder> {
        let mut bytes = Vec::with_capacity(length.max(0) as usize);
        stream.read_to_end(&mut bytes)?;
        self.bytes = Some(bytes);
        self.content_len = length;
        self.type_ = Some(ContentType::Other);
        Ok(self)
    }

    /// `OleBlob.Builder.setOther(File)`.
    pub fn set_other_file(&mut self, path: &std::path::Path) -> Result<&mut OleBlobBuilder> {
        let file = std::fs::File::open(path)?;
        let length = file.metadata()?.len() as i64;
        self.set_other_stream(file, length)
    }

    /// `OleBlob.Builder.setPackagePrettyName(String)`.
    pub fn set_package_pretty_name(
        &mut self,
        pretty_name: impl Into<String>,
    ) -> &mut OleBlobBuilder {
        self.pretty_name = Some(pretty_name.into());
        self
    }

    /// `OleBlob.Builder.setPackageClassName(String)`.
    pub fn set_package_class_name(&mut self, class_name: impl Into<String>) -> &mut OleBlobBuilder {
        self.class_name = Some(class_name.into());
        self
    }

    /// `OleBlob.Builder.setPackageTypeName(String)`.
    pub fn set_package_type_name(&mut self, type_name: impl Into<String>) -> &mut OleBlobBuilder {
        self.type_name = Some(type_name.into());
        self
    }

    /// `OleBlob.Builder.toBlob()`: the "finish" step which creates the wrapped
    /// Access OLE value (`OleUtil.createBlob`).
    pub fn to_blob(&self) -> Result<OleBlob> {
        let kind = self.type_.ok_or_else(|| {
            Error::IllegalArgument("Cannot currently create ole values of type null".to_string())
        })?;
        if !matches!(
            kind,
            ContentType::Link | ContentType::SimplePackage | ContentType::Other
        ) {
            return Err(Error::IllegalArgument(format!(
                "Cannot currently create ole values of type {}",
                kind.name()
            )));
        }

        let content_len = self.content_len;
        let content_bytes: Option<&[u8]> = self.bytes.as_deref();
        let mut package_stream_header: Vec<u8> = Vec::new();
        let mut package_stream_footer: Vec<u8> = Vec::new();

        match kind {
            ContentType::Link => {
                package_stream_header = self.write_package_stream_header()?;
                // link "content" is the (zero terminated) file path
                let link_bytes = zero_term_str_bytes(self.file_path.as_deref())?;
                return self.finish_blob(
                    link_bytes.len() as i64,
                    Some(&link_bytes),
                    &package_stream_header,
                    &package_stream_footer,
                );
            }
            ContentType::SimplePackage => {
                package_stream_header = self.write_package_stream_header()?;
                package_stream_footer = self.write_package_stream_footer()?;
            }
            ContentType::Other => {}
            _ => unreachable!(),
        }

        self.finish_blob(
            content_len,
            content_bytes,
            &package_stream_header,
            &package_stream_footer,
        )
    }

    /// Alias of [`OleBlobBuilder::to_blob`] (the builder's finish step).
    pub fn build(&self) -> Result<OleBlob> {
        self.to_blob()
    }

    /// Alias of [`OleBlobBuilder::to_blob`].
    pub fn finish(&self) -> Result<OleBlob> {
        self.to_blob()
    }

    /// `OleBlob.Builder.fromInternalData(byte[])`.
    pub fn from_internal_data(bytes: &[u8]) -> OleBlob {
        OleBlob::from_internal_data(bytes)
    }

    fn set_default_package_type(&mut self) {
        if self.pretty_name.is_none() {
            self.pretty_name = Some(Self::PACKAGE_PRETTY_NAME.to_string());
        }
        if self.class_name.is_none() {
            self.class_name = Some(Self::PACKAGE_TYPE_NAME.to_string());
        }
    }

    fn finish_blob(
        &self,
        content_len: i64,
        content_bytes: Option<&[u8]>,
        package_stream_header: &[u8],
        package_stream_footer: &[u8],
    ) -> Result<OleBlob> {
        let payload_len =
            package_stream_header.len() as i64 + package_stream_footer.len() as i64 + content_len;
        let package_header = self.write_package_header(payload_len)?;
        let total_len = package_header.len() as i64 + PACKAGE_FOOTER.len() as i64 + payload_len;
        if total_len > crate::datatype::DataType::Ole.get_max_size() as i64 {
            return Err(Error::IllegalArgument(format!(
                "Content size of {} is too large for ole column",
                total_len
            )));
        }
        if content_len > 0 {
            let bytes = content_bytes
                .ok_or_else(|| Error::Jackcess("ole content stream is missing".to_string()))?;
            if bytes.len() as i64 != content_len {
                return Err(Error::IllegalArgument(format!(
                    "ole content length {} does not match the stream length {}",
                    content_len,
                    bytes.len()
                )));
            }
        }

        let mut out = Vec::with_capacity(total_len as usize);
        out.extend_from_slice(&package_header);
        out.extend_from_slice(package_stream_header);
        if content_len > 0 {
            out.extend_from_slice(content_bytes.unwrap_or(&[]));
        }
        out.extend_from_slice(package_stream_footer);
        out.extend_from_slice(&PACKAGE_FOOTER);
        Ok(OleBlob::from_vec(out))
    }

    /// `OleUtil.writePackageHeader`.
    fn write_package_header(&self, content_len: i64) -> Result<Vec<u8>> {
        let pretty_name = zero_term_str_bytes(self.pretty_name.as_deref())?;
        let class_name = match (&self.class_name, &self.type_name) {
            (Some(class_name), _) => Some(class_name.clone()),
            (None, Some(type_name)) => Some(type_name.clone()),
            (None, None) => None,
        };
        let type_name = match (&self.type_name, &class_name) {
            (Some(type_name), _) => Some(type_name.clone()),
            (None, Some(class_name)) => Some(class_name.clone()),
            (None, None) => None,
        };
        let class_name = zero_term_str_bytes(class_name.as_deref())?;
        let type_name = zero_term_str_bytes(type_name.as_deref())?;

        let package_header_len = 20 + pretty_name.len() + class_name.len();
        let ole_header_len = 24 + type_name.len();
        let mut out = Vec::with_capacity(package_header_len + ole_header_len);

        // outer package header
        out.extend_from_slice(&PACKAGE_SIGNATURE.to_le_bytes());
        out.extend_from_slice(&(package_header_len as u16).to_le_bytes());
        out.extend_from_slice(&PACKAGE_OBJECT_TYPE.to_le_bytes());
        out.extend_from_slice(&(pretty_name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(class_name.len() as u16).to_le_bytes());
        let pretty_name_off = out.len() as u16 + 8;
        out.extend_from_slice(&pretty_name_off.to_le_bytes());
        out.extend_from_slice(&(pretty_name_off + pretty_name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(-1i32).to_le_bytes());
        out.extend_from_slice(&pretty_name);
        out.extend_from_slice(&class_name);

        // ole header
        out.extend_from_slice(&OLE_VERSION.to_le_bytes());
        out.extend_from_slice(&OLE_FORMAT.to_le_bytes());
        out.extend_from_slice(&(type_name.len() as i32).to_le_bytes());
        out.extend_from_slice(&type_name);
        out.extend_from_slice(&0i64.to_le_bytes());
        out.extend_from_slice(&(content_len as i32).to_le_bytes());
        Ok(out)
    }

    /// `OleUtil.writePackageStreamHeader`.
    fn write_package_stream_header(&self) -> Result<Vec<u8>> {
        let file_name = zero_term_str_bytes(self.file_name.as_deref())?;
        let file_path = zero_term_str_bytes(self.file_path.as_deref())?;
        let simple = self.type_ == Some(ContentType::SimplePackage);
        let header_len =
            6 + file_name.len() + file_path.len() + if simple { 8 + file_path.len() } else { 2 };
        let mut out = Vec::with_capacity(header_len);
        out.extend_from_slice(&2u16.to_le_bytes()); // PACKAGE_STREAM_SIGNATURE
        out.extend_from_slice(&file_name);
        out.extend_from_slice(&file_path);
        if simple {
            out.extend_from_slice(&PS_EMBEDDED_FILE.to_le_bytes());
            out.extend_from_slice(&(file_path.len() as i32).to_le_bytes());
            out.extend_from_slice(&file_path);
            out.extend_from_slice(&(self.content_len as i32).to_le_bytes());
        } else {
            out.extend_from_slice(&PS_LINKED_FILE.to_le_bytes());
            out.extend_from_slice(&LINK_HEADER.to_le_bytes());
        }
        Ok(out)
    }

    /// `OleUtil.writePackageStreamFooter`.
    fn write_package_stream_footer(&self) -> Result<Vec<u8>> {
        // note, these are _not_ zero terminated
        let file_name = utf16le_bytes(self.file_name.as_deref())?;
        let file_path = utf16le_bytes(self.file_path.as_deref())?;
        let footer_len = 12 + file_path.len() * 2 + file_name.len();
        let mut out = Vec::with_capacity(footer_len);
        out.extend_from_slice(&((file_path.len() / 2) as i32).to_le_bytes());
        out.extend_from_slice(&file_path);
        out.extend_from_slice(&((file_name.len() / 2) as i32).to_le_bytes());
        out.extend_from_slice(&file_name);
        out.extend_from_slice(&((file_path.len() / 2) as i32).to_le_bytes());
        out.extend_from_slice(&file_path);
        Ok(out)
    }
}

impl OleBlob {
    /// `Table.newBlob()` / `new OleBlob.Builder()`.
    pub fn builder() -> OleBlobBuilder {
        OleBlobBuilder::new()
    }

    /// `OleBlob.writeTo(OutputStream)`: writes the whole Access-internal blob.
    pub fn write_to(&self, out: &mut dyn std::io::Write) -> Result<()> {
        out.write_all(&self.bytes)?;
        Ok(())
    }
}

/// Java's `File.getAbsolutePath()`: the path against the current working
/// directory (the port does not resolve symbolic links).
fn java_absolute_path(path: &std::path::Path) -> Result<String> {
    let absolute = std::path::absolute(path)?;
    Ok(absolute.to_string_lossy().into_owned())
}

/// `OleUtil.getZeroTermStrBytes`: the string is decomposed (NFD), the
/// combining diacritical marks and the modifier letters/symbols are stripped,
/// what is left is normalized again (NFC) and encoded as US-ASCII with Java's
/// `?` replacement for unmappable characters, followed by a NUL byte.
///
/// The port implements the NFD decomposition for the Latin-1 Supplement and
/// Latin Extended-A letters (the accents `Normalizer` is used for in practice)
/// and strips the U+0300..U+036F combining mark block plus the common
/// `Lm`/`Sk` ranges.  Characters outside those tables are encoded as ASCII
/// (`?`), exactly like Java's US-ASCII encoder.
fn zero_term_str_bytes(value: Option<&str>) -> Result<Vec<u8>> {
    let value =
        value.ok_or_else(|| Error::Jackcess("ole package string is missing".to_string()))?;
    let mut ascii = Vec::with_capacity(value.len() + 1);
    for ch in value.chars() {
        // `Normalizer.normalize(str, NFD)` then strip the accents
        let ch = decompose_latin(ch);
        if is_stripped_modifier(ch) {
            continue;
        }
        if ch.is_ascii() {
            ascii.push(ch as u8);
        } else {
            // `String.getBytes(US_ASCII)` replaces an unmappable char with '?'
            ascii.push(b'?');
        }
    }
    ascii.push(0);
    Ok(ascii)
}

/// `OleUtil.OLE_UTF_CHARSET` (`UTF-16LE`) bytes, without a terminator.
fn utf16le_bytes(value: Option<&str>) -> Result<Vec<u8>> {
    let value =
        value.ok_or_else(|| Error::Jackcess("ole package string is missing".to_string()))?;
    let mut out = Vec::with_capacity(value.len() * 2);
    for unit in value.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(out)
}

/// The Latin-1 Supplement / Latin Extended-A canonical decompositions, reduced
/// to their base letter (the combining marks are always stripped afterwards).
const LATIN_DECOMPOSITIONS: &[(u32, u8)] = &[
    (0x00C0, 0x41),
    (0x00C1, 0x41),
    (0x00C2, 0x41),
    (0x00C3, 0x41),
    (0x00C4, 0x41),
    (0x00C5, 0x41),
    (0x00C7, 0x43),
    (0x00C8, 0x45),
    (0x00C9, 0x45),
    (0x00CA, 0x45),
    (0x00CB, 0x45),
    (0x00CC, 0x49),
    (0x00CD, 0x49),
    (0x00CE, 0x49),
    (0x00CF, 0x49),
    (0x00D1, 0x4E),
    (0x00D2, 0x4F),
    (0x00D3, 0x4F),
    (0x00D4, 0x4F),
    (0x00D5, 0x4F),
    (0x00D6, 0x4F),
    (0x00D9, 0x55),
    (0x00DA, 0x55),
    (0x00DB, 0x55),
    (0x00DC, 0x55),
    (0x00DD, 0x59),
    (0x00E0, 0x61),
    (0x00E1, 0x61),
    (0x00E2, 0x61),
    (0x00E3, 0x61),
    (0x00E4, 0x61),
    (0x00E5, 0x61),
    (0x00E7, 0x63),
    (0x00E8, 0x65),
    (0x00E9, 0x65),
    (0x00EA, 0x65),
    (0x00EB, 0x65),
    (0x00EC, 0x69),
    (0x00ED, 0x69),
    (0x00EE, 0x69),
    (0x00EF, 0x69),
    (0x00F1, 0x6E),
    (0x00F2, 0x6F),
    (0x00F3, 0x6F),
    (0x00F4, 0x6F),
    (0x00F5, 0x6F),
    (0x00F6, 0x6F),
    (0x00F9, 0x75),
    (0x00FA, 0x75),
    (0x00FB, 0x75),
    (0x00FC, 0x75),
    (0x00FD, 0x79),
    (0x00FF, 0x79),
    (0x0100, 0x41),
    (0x0101, 0x61),
    (0x0102, 0x41),
    (0x0103, 0x61),
    (0x0104, 0x41),
    (0x0105, 0x61),
    (0x0106, 0x43),
    (0x0107, 0x63),
    (0x0108, 0x43),
    (0x0109, 0x63),
    (0x010A, 0x43),
    (0x010B, 0x63),
    (0x010C, 0x43),
    (0x010D, 0x63),
    (0x010E, 0x44),
    (0x010F, 0x64),
    (0x0112, 0x45),
    (0x0113, 0x65),
    (0x0114, 0x45),
    (0x0115, 0x65),
    (0x0116, 0x45),
    (0x0117, 0x65),
    (0x0118, 0x45),
    (0x0119, 0x65),
    (0x011A, 0x45),
    (0x011B, 0x65),
    (0x011C, 0x47),
    (0x011D, 0x67),
    (0x011E, 0x47),
    (0x011F, 0x67),
    (0x0120, 0x47),
    (0x0121, 0x67),
    (0x0122, 0x47),
    (0x0123, 0x67),
    (0x0124, 0x48),
    (0x0125, 0x68),
    (0x0128, 0x49),
    (0x0129, 0x69),
    (0x012A, 0x49),
    (0x012B, 0x69),
    (0x012C, 0x49),
    (0x012D, 0x69),
    (0x012E, 0x49),
    (0x012F, 0x69),
    (0x0130, 0x49),
    (0x0134, 0x4A),
    (0x0135, 0x6A),
    (0x0136, 0x4B),
    (0x0137, 0x6B),
    (0x0139, 0x4C),
    (0x013A, 0x6C),
    (0x013B, 0x4C),
    (0x013C, 0x6C),
    (0x013D, 0x4C),
    (0x013E, 0x6C),
    (0x0143, 0x4E),
    (0x0144, 0x6E),
    (0x0145, 0x4E),
    (0x0146, 0x6E),
    (0x0147, 0x4E),
    (0x0148, 0x6E),
    (0x014C, 0x4F),
    (0x014D, 0x6F),
    (0x014E, 0x4F),
    (0x014F, 0x6F),
    (0x0150, 0x4F),
    (0x0151, 0x6F),
    (0x0154, 0x52),
    (0x0155, 0x72),
    (0x0156, 0x52),
    (0x0157, 0x72),
    (0x0158, 0x52),
    (0x0159, 0x72),
    (0x015A, 0x53),
    (0x015B, 0x73),
    (0x015C, 0x53),
    (0x015D, 0x73),
    (0x015E, 0x53),
    (0x015F, 0x73),
    (0x0160, 0x53),
    (0x0161, 0x73),
    (0x0162, 0x54),
    (0x0163, 0x74),
    (0x0164, 0x54),
    (0x0165, 0x74),
    (0x0168, 0x55),
    (0x0169, 0x75),
    (0x016A, 0x55),
    (0x016B, 0x75),
    (0x016C, 0x55),
    (0x016D, 0x75),
    (0x016E, 0x55),
    (0x016F, 0x75),
    (0x0170, 0x55),
    (0x0171, 0x75),
    (0x0172, 0x55),
    (0x0173, 0x75),
    (0x0174, 0x57),
    (0x0175, 0x77),
    (0x0176, 0x59),
    (0x0177, 0x79),
    (0x0178, 0x59),
    (0x0179, 0x5A),
    (0x017A, 0x7A),
    (0x017B, 0x5A),
    (0x017C, 0x7A),
    (0x017D, 0x5A),
    (0x017E, 0x7A),
    (0x01A0, 0x4F),
    (0x01A1, 0x6F),
    (0x01AF, 0x55),
    (0x01B0, 0x75),
    (0x01CD, 0x41),
    (0x01CE, 0x61),
    (0x01CF, 0x49),
    (0x01D0, 0x69),
    (0x01D1, 0x4F),
    (0x01D2, 0x6F),
    (0x01D3, 0x55),
    (0x01D4, 0x75),
    (0x01D5, 0x55),
    (0x01D6, 0x75),
    (0x01D7, 0x55),
    (0x01D8, 0x75),
    (0x01D9, 0x55),
    (0x01DA, 0x75),
    (0x01DB, 0x55),
    (0x01DC, 0x75),
    (0x01DE, 0x41),
    (0x01DF, 0x61),
    (0x01E0, 0x41),
    (0x01E1, 0x61),
    (0x01E6, 0x47),
    (0x01E7, 0x67),
    (0x01E8, 0x4B),
    (0x01E9, 0x6B),
    (0x01EA, 0x4F),
    (0x01EB, 0x6F),
    (0x01EC, 0x4F),
    (0x01ED, 0x6F),
    (0x01F0, 0x6A),
    (0x01F4, 0x47),
    (0x01F5, 0x67),
    (0x01F8, 0x4E),
    (0x01F9, 0x6E),
    (0x01FA, 0x41),
    (0x01FB, 0x61),
    (0x0200, 0x41),
    (0x0201, 0x61),
    (0x0202, 0x41),
    (0x0203, 0x61),
    (0x0204, 0x45),
    (0x0205, 0x65),
    (0x0206, 0x45),
    (0x0207, 0x65),
    (0x0208, 0x49),
    (0x0209, 0x69),
    (0x020A, 0x49),
    (0x020B, 0x69),
    (0x020C, 0x4F),
    (0x020D, 0x6F),
    (0x020E, 0x4F),
    (0x020F, 0x6F),
    (0x0210, 0x52),
    (0x0211, 0x72),
    (0x0212, 0x52),
    (0x0213, 0x72),
    (0x0214, 0x55),
    (0x0215, 0x75),
    (0x0216, 0x55),
    (0x0217, 0x75),
    (0x0218, 0x53),
    (0x0219, 0x73),
    (0x021A, 0x54),
    (0x021B, 0x74),
    (0x021E, 0x48),
    (0x021F, 0x68),
    (0x0226, 0x41),
    (0x0227, 0x61),
    (0x0228, 0x45),
    (0x0229, 0x65),
    (0x022A, 0x4F),
    (0x022B, 0x6F),
    (0x022C, 0x4F),
    (0x022D, 0x6F),
    (0x022E, 0x4F),
    (0x022F, 0x6F),
    (0x0230, 0x4F),
    (0x0231, 0x6F),
    (0x0232, 0x59),
    (0x0233, 0x79),
];

/// The base letter of a Latin diacritic (`Normalizer.normalize(str, NFD)` plus
/// the accent stripping of `OleUtil.getZeroTermStrBytes`).
fn decompose_latin(ch: char) -> char {
    let cp = ch as u32;
    match LATIN_DECOMPOSITIONS.binary_search_by_key(&cp, |(cp, _)| *cp) {
        Ok(index) => LATIN_DECOMPOSITIONS[index].1 as char,
        Err(_) => ch,
    }
}

/// The characters `OleUtil.getZeroTermStrBytes` strips after the NFD
/// decomposition: `\p{InCombiningDiacriticalMarks}` (U+0300..U+036F) and the
/// spacing modifier letters/symbols (`\p{IsLm}`, `\p{IsSk}`).
fn is_stripped_modifier(ch: char) -> bool {
    let cp = ch as u32;
    matches!(
        cp,
        0x0300..=0x036F   // Combining Diacritical Marks
        | 0x02B0..=0x02FF // Spacing Modifier Letters (Lm)
        | 0x1D2C..=0x1D6A // Phonetic Extensions (Lm)
        | 0x1D78
        | 0x1D9B..=0x1DBF
        | 0x1DC0..=0x1DFF // Combining Diacritical Marks Supplement
        | 0x2070..=0x209C // Superscripts and Subscripts (Lm/Sk)
        | 0x20D0..=0x20FF // Combining Diacritical Marks for Symbols
        | 0x2100..=0x214F // Letterlike Symbols (some Lm/Sk)
        | 0x1F3FB..=0x1F3FF // Emoji Modifier Symbols (Sk)
        | 0xFE20..=0xFE2F // Combining Half Marks
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- helpers to build compound files ------------------------------------

    const SECTOR: usize = 512;

    struct CompoundBuilder {
        data: Vec<u8>,
        /// next free sector index (sector 0 is the first sector after the
        /// 512 byte header)
        next_sector: u32,
        fat: Vec<u32>,
        difat: Vec<u32>,
    }

    impl CompoundBuilder {
        fn new() -> CompoundBuilder {
            let mut data = vec![0u8; 512];
            // signature and basic v3 header
            data[0..8].copy_from_slice(&COMPOUND_STORAGE_SIGNATURE);
            data[24..26].copy_from_slice(&0x003Eu16.to_le_bytes());
            data[26..28].copy_from_slice(&3u16.to_le_bytes());
            data[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes());
            data[30..32].copy_from_slice(&9u16.to_le_bytes());
            data[32..34].copy_from_slice(&6u16.to_le_bytes());
            data[56..60].copy_from_slice(&4096u32.to_le_bytes());
            CompoundBuilder {
                data,
                next_sector: 0,
                fat: Vec::new(),
                difat: Vec::new(),
            }
        }

        fn alloc_sector(&mut self, content: &[u8]) -> u32 {
            assert!(content.len() <= SECTOR);
            let sector = self.next_sector;
            self.next_sector += 1;
            let mut block = vec![0u8; SECTOR];
            block[..content.len()].copy_from_slice(content);
            self.data.extend_from_slice(&block);
            // default: end of chain
            self.fat.push(0xFFFF_FFFE);
            sector
        }

        fn set_fat(&mut self, sector: u32, value: u32) {
            self.fat[sector as usize] = value;
        }

        fn set_header_u32(&mut self, off: usize, value: u32) {
            self.data[off..off + 4].copy_from_slice(&value.to_le_bytes());
        }

        /// Writes the FAT into a sector allocated at the end and finishes the
        /// header.  Directory/mini-FAT sectors must already be allocated.
        fn finish(mut self) -> Vec<u8> {
            let fat_sector = self.alloc_sector(&[]);
            let mut fat_bytes = vec![0u8; SECTOR];
            for (i, value) in self.fat.iter().enumerate() {
                fat_bytes[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
            }
            let off = (fat_sector as usize + 1) * SECTOR;
            self.data[off..off + SECTOR].copy_from_slice(&fat_bytes);

            self.difat.push(fat_sector);
            self.set_header_u32(44, 1); // one FAT sector
            self.set_header_u32(76, self.difat[0]);
            // zero the remaining DIFAT entries
            for i in 1..109 {
                self.set_header_u32(76 + i * 4, 0xFFFF_FFFF);
            }
            self.data
        }
    }

    // The parameters are the fields of a compound-file directory entry, in
    // on-disk order; a struct would obscure that this writes the raw layout.
    #[expect(
        clippy::too_many_arguments,
        reason = "arguments are the on-disk DirectoryEntry field layout"
    )]
    fn dir_entry(
        name: &str,
        entry_type: u8,
        left: i32,
        right: i32,
        child: i32,
        clsid: [u8; 16],
        ctime: u64,
        mtime: u64,
        start: u32,
        size: u64,
    ) -> Vec<u8> {
        let mut raw = vec![0u8; PROPERTY_SIZE];
        let units: Vec<u16> = name.encode_utf16().collect();
        for (i, unit) in units.iter().enumerate() {
            raw[i * 2..i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        let name_size = ((units.len() + 1) * 2) as u16;
        raw[64..66].copy_from_slice(&name_size.to_le_bytes());
        raw[66] = entry_type;
        raw[67] = 1;
        raw[68..72].copy_from_slice(&(left as u32).to_le_bytes());
        raw[72..76].copy_from_slice(&(right as u32).to_le_bytes());
        raw[76..80].copy_from_slice(&(child as u32).to_le_bytes());
        raw[80..96].copy_from_slice(&clsid);
        raw[100..108].copy_from_slice(&ctime.to_le_bytes());
        raw[108..116].copy_from_slice(&mtime.to_le_bytes());
        raw[116..120].copy_from_slice(&start.to_le_bytes());
        raw[120..128].copy_from_slice(&size.to_le_bytes());
        raw
    }

    /// Builds a small v3 compound file with:
    /// * root
    /// * /Big (5000 bytes, stored in 10 regular sectors)
    /// * /Mini (100 bytes, stored in the mini stream)
    /// * /Storage1 (storage) with /Storage1/Nested (10 bytes, mini)
    fn sample_compound() -> Vec<u8> {
        // 10 sectors worth of data; the stream itself is 5000 bytes.
        let big: Vec<u8> = (0..5120).map(|i| (i % 251) as u8).collect();
        let mini: Vec<u8> = (0..100).map(|i| (200 + i) as u8).collect();
        let nested: Vec<u8> = (0..10).map(|i| (10 + i) as u8).collect();

        let mut builder = CompoundBuilder::new();

        // Directory sectors first (0 and 1, chained through the FAT), then big
        // stream sectors (2..=11), then the mini stream sector (12), then the
        // mini-FAT sector (13); the FAT sector (14) is added by `finish`.
        let dir_sector = builder.alloc_sector(&[]);
        let dir_sector2 = builder.alloc_sector(&[]);
        builder.set_fat(dir_sector, dir_sector2);
        builder.set_header_u32(48, dir_sector);
        let big_start = builder.alloc_sector(&big[..SECTOR]);
        for i in 1..10 {
            let s = builder.alloc_sector(&big[i * SECTOR..(i + 1) * SECTOR]);
            builder.set_fat(big_start + i as u32 - 1, s);
        }

        // mini stream: mini blocks 0/1 hold `mini`, block 2 holds `nested`
        let mut mini_sector_data = vec![0u8; SECTOR];
        mini_sector_data[..64].copy_from_slice(&mini[..64]);
        mini_sector_data[64..100].copy_from_slice(&mini[64..]);
        mini_sector_data[128..138].copy_from_slice(&nested);
        let mini_sector = builder.alloc_sector(&mini_sector_data);

        let mini_fat_sector = builder.alloc_sector(&[]);
        // mini FAT: block 0 -> block 1 -> end; block 2 -> end
        let mut mini_fat = vec![0u8; SECTOR];
        mini_fat[0..4].copy_from_slice(&1u32.to_le_bytes());
        mini_fat[4..8].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
        mini_fat[8..12].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
        let off = (mini_fat_sector as usize + 1) * SECTOR;
        builder.data[off..off + SECTOR].copy_from_slice(&mini_fat);

        let root_clsid = [
            0x06, 0x09, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x46,
        ];
        let storage_clsid = [1u8; 16];

        // root: child -> 1 (Storage1 is index 1)
        let root = dir_entry(
            "Root Entry",
            5,
            -1,
            -1,
            1,
            root_clsid,
            0x01D0_0000_0000_0001,
            0x01D0_0000_0000_0002,
            mini_sector,
            138,
        );
        // indirection: root child index 1 = Storage1, whose child index 3 =
        // Nested; the sibling chain of the root holds Big (2) and Mini (4).
        let storage = dir_entry(
            "Storage1",
            1,
            -1,
            2, // right sibling = Big
            3, // child = Nested (index 3)
            storage_clsid,
            0x01D0_0000_0000_0003,
            0x01D0_0000_0000_0004,
            0,
            0,
        );
        let big_entry = dir_entry(
            "Big", 2, -1, 4, // right sibling = Mini
            -1, [0u8; 16], 0, 0, big_start, 5000,
        );
        let nested_entry = dir_entry(
            "Nested",
            2,
            -1,
            -1,
            -1,
            [0u8; 16],
            0,
            0,
            2,
            nested.len() as u64,
        );
        let mini_entry = dir_entry("Mini", 2, -1, -1, -1, [0u8; 16], 0, 0, 0, mini.len() as u64);

        // The property tree is: root.child = 1; Storage1.right = 2 (Big);
        // Big.right = 4 (Mini); Storage1.child = 3 (Nested).
        // POI populates with a stack: next sibling is visited before the
        // previous one, and directories recurse before their siblings.
        let mut dir_data = vec![0u8; SECTOR * 2];
        for (i, raw) in [root, storage, big_entry, nested_entry, mini_entry]
            .iter()
            .enumerate()
        {
            dir_data[i * PROPERTY_SIZE..(i + 1) * PROPERTY_SIZE]
                .copy_from_slice(&raw[..PROPERTY_SIZE]);
        }
        let off = (dir_sector as usize + 1) * SECTOR;
        builder.data[off..off + SECTOR * 2].copy_from_slice(&dir_data);

        builder.set_header_u32(60, mini_fat_sector);
        builder.set_header_u32(64, 1);

        // Fill the FAT after all sectors are allocated.
        builder.finish()
    }

    fn sample_entry_indices(file: &CompoundFile) -> Vec<(String, usize)> {
        file.entries
            .iter()
            .map(|e| (e.name.clone(), e.index))
            .collect()
    }

    // -- header tests --------------------------------------------------------

    #[test]
    fn parses_header_fields() {
        let data = sample_compound();
        let header = CompoundHeader::parse(&data).unwrap();
        assert_eq!(header.major_version, 3);
        assert_eq!(header.minor_version, 0x3E);
        assert_eq!(header.byte_order, 0xFFFE);
        assert_eq!(header.sector_shift, 9);
        assert_eq!(header.sector_size(), 512);
        assert_eq!(header.mini_sector_shift, 6);
        assert_eq!(header.mini_sector_size(), 64);
        assert_eq!(header.num_fat_sectors, 1);
        assert_eq!(header.first_dir_sector, 0);
        assert_eq!(header.num_mini_fat_sectors, 1);
        assert_eq!(header.fat_entries_per_sector(), 128);
        assert_eq!(header.difat[0], 14); // FAT sector allocated last
    }

    #[test]
    fn rejects_invalid_signature() {
        let err = CompoundHeader::parse(&[1, 2, 3]).unwrap_err();
        assert!(err.to_string().contains("Invalid header signature"));
        let err = CompoundHeader::parse(&[]).unwrap_err();
        assert!(err
            .to_string()
            .contains("read 0x0000000000000000, expected 0xE11AB1A1E011CFD0"));
    }

    #[test]
    fn rejects_unsupported_block_size() {
        let mut data = vec![0u8; 512];
        data[0..8].copy_from_slice(&COMPOUND_STORAGE_SIGNATURE);
        data[30] = 7;
        let err = CompoundHeader::parse(&data).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unsupported blocksize  (2^7). Expected 2^9 or 2^12."
        );
    }

    #[test]
    fn reports_zero_fat_sector_count_like_poi() {
        // `POIFSFileSystem(FileChannel)` (the constructor Jackcess uses) does
        // not sanity check the FAT sector count; the first FAT lookup fails
        // with POI's block-index message instead.
        let mut data = sample_compound();
        data[44..48].copy_from_slice(&0u32.to_le_bytes());
        let err = CompoundFile::parse(&data).unwrap_err();
        assert_eq!(err.to_string(), "Index 0 out of bounds for length 0");
    }

    // -- directory / FAT tests ----------------------------------------------

    #[test]
    fn parses_directory_entries_and_tree() {
        let data = sample_compound();
        let file = CompoundFile::parse(&data).unwrap();
        let entries = sample_entry_indices(&file);
        assert_eq!(
            entries[..5],
            [
                ("Root Entry".to_string(), 0),
                ("Storage1".to_string(), 1),
                ("Big".to_string(), 2),
                ("Nested".to_string(), 3),
                ("Mini".to_string(), 4),
            ]
        );
        // The second directory sector is unused; POI's property list holds
        // `null` for those slots, which we represent as `Empty` entries.
        assert!(entries[5..]
            .iter()
            .all(|_| file.entries[5].entry_type() == EntryType::Empty));
        let root = file.root();
        assert!(root.is_root());
        assert_eq!(root.size32(), 138);
        assert_eq!(root.start_sector(), 12);
        assert_eq!(
            root.clsid_string(),
            "{00020906-0000-0000-C000-000000000046}"
        );
        assert_eq!(root.creation_time(), 0x01D0_0000_0000_0001);
        assert_eq!(root.modified_time(), 0x01D0_0000_0000_0002);
        // POI's directory node order: sibling "next" comes before "previous"
        // and storages are expanded in place.
        assert_eq!(root.children(), &[1, 2, 4]);
        assert_eq!(file.entries[1].children(), &[3]);
        assert_eq!(
            file.entries[1].clsid_string(),
            "{01010101-0101-0101-0101-010101010101}"
        );

        let storage = &file.entries[1];
        assert!(storage.is_storage());
        assert_eq!(storage.parent(), Some(0));
        assert_eq!(storage.creation_time(), 0x01D0_0000_0000_0003);
        assert_eq!(storage.modified_time(), 0x01D0_0000_0000_0004);

        let big = &file.entries[2];
        assert!(big.is_stream());
        assert_eq!(big.parent(), Some(0));
        assert_eq!(big.size(), 5000);
        assert_eq!(big.size32(), 5000);
        assert_eq!(big.start_sector(), 2);
    }

    #[test]
    fn reads_big_and_mini_streams() {
        let data = sample_compound();
        let file = CompoundFile::parse(&data).unwrap();

        let big = file.stream("/Big").unwrap();
        let expected_big: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        assert_eq!(big, expected_big);

        let mini = file.stream("/Mini").unwrap();
        let expected_mini: Vec<u8> = (0..100).map(|i| (200 + i) as u8).collect();
        assert_eq!(mini, expected_mini);

        let nested = file.stream("/Storage1/Nested").unwrap();
        let expected_nested: Vec<u8> = (0..10).map(|i| (10 + i) as u8).collect();
        assert_eq!(nested, expected_nested);

        assert_eq!(
            file.streams(),
            vec![
                "/Storage1/Nested".to_string(),
                "/Big".to_string(),
                "/Mini".to_string()
            ]
        );
    }

    #[test]
    fn reads_zero_length_stream() {
        let mut data = sample_compound();
        // Patch the Mini entry (index 4) size to zero and start sector to a
        // non-existent block; POI returns an empty stream without reading it.
        let dir_off = SECTOR;
        let mini_off = dir_off + 4 * PROPERTY_SIZE;
        data[mini_off + 116..mini_off + 120].copy_from_slice(&9999u32.to_le_bytes());
        data[mini_off + 120..mini_off + 128].copy_from_slice(&0u64.to_le_bytes());
        let file = CompoundFile::parse(&data).unwrap();
        assert_eq!(file.stream("/Mini").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn detects_missing_block() {
        let mut data = sample_compound();
        // Point the Big stream's first sector beyond the end of the file.
        let dir_off = SECTOR;
        let big_off = dir_off + 2 * PROPERTY_SIZE;
        data[big_off + 116..big_off + 120].copy_from_slice(&9999u32.to_le_bytes());
        let file = CompoundFile::parse(&data).unwrap();
        let err = file.stream("/Big").unwrap_err();
        assert_eq!(err.to_string(), "Block 9999 not found");
    }

    #[test]
    fn detects_directory_chain_loop() {
        let mut data = sample_compound();
        // The FAT sector is sector 14; directory sector 0 points to sector 1.
        let fat_off = 15 * SECTOR;
        data[fat_off..fat_off + 4].copy_from_slice(&0u32.to_le_bytes());
        let err = CompoundFile::parse(&data).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Potential loop detected - Block 0 was already claimed but was just requested again"
        );
    }

    #[test]
    fn detects_mini_fat_loop() {
        let mut data = sample_compound();
        // mini-FAT sector is sector 13; mini block 0 points to itself.
        let mini_fat_off = 14 * SECTOR;
        data[mini_fat_off..mini_fat_off + 4].copy_from_slice(&0u32.to_le_bytes());
        let file = CompoundFile::parse(&data).unwrap();
        let err = file.stream("/Mini").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Potential loop detected - Block 0 was already claimed but was just requested again"
        );
    }

    #[test]
    fn rejects_truncated_directory() {
        let data = sample_compound();
        // Keep only the header and half of the directory sector.
        let truncated = &data[..SECTOR + SECTOR / 2];
        let err = CompoundFile::parse(truncated).unwrap_err();
        assert_eq!(err.to_string(), "Block 14 not found");
    }

    // -- v4 (4096 byte sector) tests -----------------------------------------

    const V4_SECTOR: usize = 4096;
    const V4_MINI: usize = 64;

    /// Builds a v4 compound file (sector shift 12).  `use_difat` writes 110
    /// FAT sectors, more than fit in the header, so the last one has to be
    /// found through a DIFAT sector.
    struct V4Builder {
        sectors: Vec<Vec<u8>>,
        fat: Vec<u32>,
        dir_sector: u32,
        dir_sectors: u32,
        mini_fat_start: u32,
        mini_fat_sectors: u32,
        num_fat_sectors: u32,
        use_difat: bool,
        major: u16,
        num_dir_sectors_override: Option<u32>,
    }

    impl V4Builder {
        fn new() -> V4Builder {
            V4Builder {
                sectors: Vec::new(),
                fat: Vec::new(),
                dir_sector: 0,
                dir_sectors: 1,
                mini_fat_start: 0xFFFF_FFFE,
                mini_fat_sectors: 0,
                num_fat_sectors: 1,
                use_difat: false,
                major: 4,
                num_dir_sectors_override: None,
            }
        }

        fn alloc_chain(&mut self, payload: &[u8]) -> u32 {
            if payload.is_empty() {
                return 0xFFFF_FFFE;
            }
            let count = payload.len().div_ceil(V4_SECTOR);
            let first = self.sectors.len() as u32;
            for i in 0..count {
                let mut block = vec![0u8; V4_SECTOR];
                let off = i * V4_SECTOR;
                let n = (payload.len() - off).min(V4_SECTOR);
                block[..n].copy_from_slice(&payload[off..off + n]);
                self.sectors.push(block);
                self.fat.push(if i + 1 == count {
                    0xFFFF_FFFE
                } else {
                    first + 1 + i as u32
                });
            }
            first
        }

        /// Appends the FAT (and optional DIFAT) sectors, writes the header and
        /// assembles the file: 4096 byte header block (the 512 byte header
        /// followed by zero padding) followed by the sectors.
        fn finish(mut self) -> Vec<u8> {
            let first_fat = self.sectors.len() as u32;
            for _ in 0..self.num_fat_sectors {
                self.sectors.push(vec![0u8; V4_SECTOR]);
                self.fat.push(0xFFFF_FFFD); // FATSECT
            }
            let mut first_difat = 0u32;
            let mut num_difat = 0u32;
            if self.use_difat {
                first_difat = self.sectors.len() as u32;
                self.sectors.push(vec![0u8; V4_SECTOR]);
                self.fat.push(0xFFFF_FFFC); // DIFSECT
                num_difat = 1;
            }
            let eps = V4_SECTOR / 4;
            for f in 0..self.num_fat_sectors as usize {
                let mut block = vec![0u8; V4_SECTOR];
                for i in 0..eps {
                    let index = f * eps + i;
                    let value = if index < self.fat.len() {
                        self.fat[index]
                    } else {
                        0xFFFF_FFFF // FREE
                    };
                    block[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
                }
                self.sectors[first_fat as usize + f] = block;
            }
            if self.use_difat {
                let mut block = vec![0u8; V4_SECTOR];
                let extra = self.num_fat_sectors as usize - 109;
                for i in 0..extra {
                    let value = first_fat + 109 + i as u32;
                    block[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
                }
                for i in extra..eps - 1 {
                    block[i * 4..i * 4 + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                }
                block[(eps - 1) * 4..eps * 4].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
                self.sectors[first_difat as usize] = block;
            }

            let mut data = vec![0u8; (self.sectors.len() + 1) * V4_SECTOR];
            data[0..8].copy_from_slice(&COMPOUND_STORAGE_SIGNATURE);
            data[24..26].copy_from_slice(&0x003Eu16.to_le_bytes());
            data[26..28].copy_from_slice(&self.major.to_le_bytes());
            data[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes());
            data[30..32].copy_from_slice(&12u16.to_le_bytes());
            data[32..34].copy_from_slice(&6u16.to_le_bytes());
            let num_dir = self.num_dir_sectors_override.unwrap_or(self.dir_sectors);
            data[40..44].copy_from_slice(&num_dir.to_le_bytes());
            data[44..48].copy_from_slice(&self.num_fat_sectors.to_le_bytes());
            data[48..52].copy_from_slice(&self.dir_sector.to_le_bytes());
            data[56..60].copy_from_slice(&4096u32.to_le_bytes());
            data[60..64].copy_from_slice(&self.mini_fat_start.to_le_bytes());
            data[64..68].copy_from_slice(&self.mini_fat_sectors.to_le_bytes());
            let difat_start = if self.use_difat {
                first_difat
            } else {
                0xFFFF_FFFE
            };
            data[68..72].copy_from_slice(&difat_start.to_le_bytes());
            data[72..76].copy_from_slice(&num_difat.to_le_bytes());
            for i in 0..109 {
                let value = if (i as u32) < self.num_fat_sectors {
                    first_fat + i as u32
                } else {
                    0xFFFF_FFFF
                };
                data[76 + i * 4..80 + i * 4].copy_from_slice(&value.to_le_bytes());
            }
            for (i, block) in self.sectors.iter().enumerate() {
                data[(i + 1) * V4_SECTOR..(i + 2) * V4_SECTOR].copy_from_slice(block);
            }
            data
        }
    }

    /// The mini stream (64 byte mini sectors) of a v4 test file.
    #[derive(Default)]
    struct V4Mini {
        data: Vec<u8>,
        fat: Vec<u32>,
    }

    impl V4Mini {
        fn alloc(&mut self, payload: &[u8]) -> u32 {
            if payload.is_empty() {
                return 0xFFFF_FFFE;
            }
            let count = payload.len().div_ceil(V4_MINI);
            let first = self.fat.len() as u32;
            for i in 0..count {
                let off = i * V4_MINI;
                let n = (payload.len() - off).min(V4_MINI);
                let mut block = [0u8; V4_MINI];
                block[..n].copy_from_slice(&payload[off..off + n]);
                self.data.extend_from_slice(&block);
                self.fat.push(if i + 1 == count {
                    0xFFFF_FFFE
                } else {
                    first + 1 + i as u32
                });
            }
            first
        }

        fn stream_bytes(&self) -> Vec<u8> {
            let padded = self.data.len().div_ceil(V4_SECTOR) * V4_SECTOR;
            let mut out = vec![0u8; padded];
            out[..self.data.len()].copy_from_slice(&self.data);
            out
        }

        fn fat_bytes(&self) -> Vec<u8> {
            let padded = (self.fat.len() * 4).div_ceil(V4_SECTOR).max(1) * V4_SECTOR;
            let mut out = vec![0u8; padded];
            for (i, value) in self.fat.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
            }
            for i in self.fat.len()..padded / 4 {
                out[i * 4..i * 4 + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            }
            out
        }
    }

    /// A generated v4 file plus the sectors needed to corrupt it and the
    /// streams it contains.
    struct V4Sample {
        data: Vec<u8>,
        dir_sector: u32,
        first_fat_sector: u32,
        mini_fat_sector: u32,
        big_start: u32,
        mini_start: u32,
        streams: Vec<(&'static str, Vec<u8>)>,
    }

    fn v4_sequence(length: usize, start: u8) -> Vec<u8> {
        (0..length).map(|i| start.wrapping_add(i as u8)).collect()
    }

    // Same on-disk layout as `dir_entry`, but with the v4 name limit.
    #[expect(
        clippy::too_many_arguments,
        reason = "arguments are the on-disk DirectoryEntry field layout"
    )]
    fn v4_dir_entry(
        name: &str,
        entry_type: u8,
        left: i32,
        right: i32,
        child: i32,
        clsid: [u8; 16],
        ctime: u64,
        mtime: u64,
        start: u32,
        size: u64,
    ) -> Vec<u8> {
        let mut raw = vec![0u8; PROPERTY_SIZE];
        let units: Vec<u16> = name.encode_utf16().take(31).collect();
        for (i, unit) in units.iter().enumerate() {
            raw[i * 2..i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        raw[64..66].copy_from_slice(&(((units.len() + 1) * 2) as u16).to_le_bytes());
        raw[66] = entry_type;
        raw[67] = 1;
        raw[68..72].copy_from_slice(&(left as u32).to_le_bytes());
        raw[72..76].copy_from_slice(&(right as u32).to_le_bytes());
        raw[76..80].copy_from_slice(&(child as u32).to_le_bytes());
        raw[80..96].copy_from_slice(&clsid);
        raw[100..108].copy_from_slice(&ctime.to_le_bytes());
        raw[108..116].copy_from_slice(&mtime.to_le_bytes());
        raw[116..120].copy_from_slice(&start.to_le_bytes());
        raw[120..128].copy_from_slice(&size.to_le_bytes());
        raw
    }

    fn sample_v4(use_difat: bool) -> V4Sample {
        let big = v4_sequence(10000, 1);
        let mini_doc = v4_sequence(100, 200);
        let boundary_big = v4_sequence(4096, 31);
        let boundary_mini = v4_sequence(4095, 41);
        let contents = v4_sequence(64, 61);
        let nested = v4_sequence(10, 71);
        let space = v4_sequence(120, 81);
        let unicode = v4_sequence(80, 91);
        let plus = v4_sequence(40, 101);
        let deep = v4_sequence(5000, 111);

        let mut builder = V4Builder::new();
        let big_start = builder.alloc_chain(&big);
        let boundary_big_start = builder.alloc_chain(&boundary_big);
        let deep_start = builder.alloc_chain(&deep);

        let mut mini = V4Mini::default();
        let mini_start = mini.alloc(&mini_doc);
        let boundary_mini_start = mini.alloc(&boundary_mini);
        let contents_start = mini.alloc(&contents);
        let nested_start = mini.alloc(&nested);
        let space_start = mini.alloc(&space);
        let unicode_start = mini.alloc(&unicode);
        let plus_start = mini.alloc(&plus);

        let mini_stream = mini.stream_bytes();
        let mini_stream_start = builder.alloc_chain(&mini_stream);
        let mini_fat = mini.fat_bytes();
        let mini_fat_start = builder.alloc_chain(&mini_fat);

        let root_clsid = [
            0x06, 0x09, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x46,
        ];
        let storage_clsid = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let base = 0x01D0_0000_0000_0000u64;

        let entries = [
            v4_dir_entry(
                "Root Entry",
                5,
                -1,
                -1,
                1,
                root_clsid,
                base + 1,
                base + 2,
                mini_stream_start,
                mini_stream.len() as u64,
            ),
            v4_dir_entry(
                "Big",
                2,
                -1,
                2,
                -1,
                [0u8; 16],
                base + 3,
                base + 4,
                big_start,
                10000,
            ),
            v4_dir_entry(
                "Mini",
                2,
                -1,
                3,
                -1,
                [0u8; 16],
                base + 5,
                base + 6,
                mini_start,
                100,
            ),
            v4_dir_entry(
                "BoundaryBig",
                2,
                -1,
                4,
                -1,
                [0u8; 16],
                base + 7,
                base + 8,
                boundary_big_start,
                4096,
            ),
            v4_dir_entry(
                "BoundaryMini",
                2,
                -1,
                5,
                -1,
                [0u8; 16],
                base + 9,
                base + 10,
                boundary_mini_start,
                4095,
            ),
            v4_dir_entry(
                "Empty",
                2,
                -1,
                6,
                -1,
                [0u8; 16],
                base + 11,
                base + 12,
                0xFFFF_FFFE,
                0,
            ),
            v4_dir_entry(
                "CONTENTS",
                2,
                -1,
                7,
                -1,
                [0u8; 16],
                base + 13,
                base + 14,
                contents_start,
                64,
            ),
            v4_dir_entry(
                "Storage1",
                1,
                -1,
                -1,
                8,
                storage_clsid,
                base + 15,
                base + 16,
                0,
                0,
            ),
            v4_dir_entry(
                "Nested",
                2,
                -1,
                9,
                -1,
                [0u8; 16],
                base + 17,
                base + 18,
                nested_start,
                10,
            ),
            v4_dir_entry(
                "Sub",
                1,
                -1,
                11,
                10,
                storage_clsid,
                base + 19,
                base + 20,
                0,
                0,
            ),
            v4_dir_entry(
                "Deep",
                2,
                -1,
                -1,
                -1,
                [0u8; 16],
                base + 21,
                base + 22,
                deep_start,
                5000,
            ),
            v4_dir_entry(
                "Name With Space",
                2,
                -1,
                12,
                -1,
                [0u8; 16],
                base + 23,
                base + 24,
                space_start,
                120,
            ),
            v4_dir_entry(
                "\u{00DC}n\u{00EF}code",
                2,
                -1,
                13,
                -1,
                [0u8; 16],
                base + 25,
                base + 26,
                unicode_start,
                80,
            ),
            v4_dir_entry(
                "a+b",
                2,
                -1,
                -1,
                -1,
                [0u8; 16],
                base + 27,
                base + 28,
                plus_start,
                40,
            ),
        ];
        let mut dir_bytes = Vec::new();
        for entry in &entries {
            dir_bytes.extend_from_slice(entry);
        }
        let dir_sectors = dir_bytes.len().div_ceil(V4_SECTOR) as u32;
        let dir_sector = builder.alloc_chain(&dir_bytes);

        builder.dir_sector = dir_sector;
        builder.dir_sectors = dir_sectors;
        builder.mini_fat_start = mini_fat_start;
        builder.mini_fat_sectors = (mini_fat.len() / V4_SECTOR) as u32;
        builder.num_fat_sectors = if use_difat { 110 } else { 1 };
        builder.use_difat = use_difat;
        let first_fat_sector = builder.sectors.len() as u32;
        let data = builder.finish();

        V4Sample {
            data,
            dir_sector,
            first_fat_sector,
            mini_fat_sector: mini_fat_start,
            big_start,
            mini_start,
            streams: vec![
                ("/Big", big),
                ("/Mini", mini_doc),
                ("/BoundaryBig", boundary_big),
                ("/BoundaryMini", boundary_mini),
                ("/Empty", Vec::new()),
                ("/CONTENTS", contents),
                ("/Storage1/Nested", nested),
                ("/Storage1/Sub/Deep", deep),
                ("/Storage1/Name With Space", space),
                ("/Storage1/\u{00DC}n\u{00EF}code", unicode),
                ("/Storage1/a+b", plus),
            ],
        }
    }

    fn v4_offset_of_sector(_sample: &V4Sample, sector: u32) -> usize {
        (sector as usize + 1) * V4_SECTOR
    }

    #[test]
    fn v4_parses_header_fields() {
        let sample = sample_v4(false);
        let header = CompoundHeader::parse(&sample.data).unwrap();
        assert_eq!(header.major_version, 4);
        assert_eq!(header.minor_version, 0x3E);
        assert_eq!(header.byte_order, 0xFFFE);
        assert_eq!(header.sector_shift, 12);
        assert_eq!(header.sector_size(), 4096);
        assert_eq!(header.mini_sector_shift, 6);
        assert_eq!(header.mini_sector_size(), 64);
        assert_eq!(header.num_dir_sectors, 1);
        assert_eq!(header.num_fat_sectors, 1);
        assert_eq!(header.first_dir_sector, sample.dir_sector);
        assert_eq!(header.transaction_signature, 0);
        assert_eq!(header.mini_stream_cutoff_size, 4096);
        assert_eq!(header.first_mini_fat_sector, sample.mini_fat_sector);
        assert_eq!(header.num_mini_fat_sectors, 1);
        assert_eq!(header.first_difat_sector, 0xFFFF_FFFE);
        assert_eq!(header.num_difat_sectors, 0);
        assert_eq!(header.fat_entries_per_sector(), 1024);
        assert_eq!(header.difat[0], sample.first_fat_sector);
        assert_eq!(header.difat[1], 0xFFFF_FFFF);
        // The 512 byte header is followed by 3584 bytes of padding, so the
        // first sector starts at offset 4096.
        assert_eq!(
            CompoundFile::parse(&sample.data).unwrap().sector_size(),
            4096
        );
    }

    #[test]
    fn v4_reads_big_and_mini_streams() {
        let sample = sample_v4(false);
        let file = CompoundFile::parse(&sample.data).unwrap();
        for (path, expected) in &sample.streams {
            let actual = file
                .stream(path)
                .unwrap_or_else(|e| panic!("stream {path} failed: {e}"));
            assert_eq!(&actual, expected, "stream {path} differs");
        }
        // A stream of exactly 4096 bytes is a big stream, one of 4095 bytes a
        // mini stream (the cutoff is the constant 4096, not the sector size).
        let boundary_big = file.entry_index("/BoundaryBig").unwrap();
        let boundary_mini = file.entry_index("/BoundaryMini").unwrap();
        assert_eq!(file.directory_entries()[boundary_big].size32(), 4096);
        assert_eq!(file.directory_entries()[boundary_mini].size32(), 4095);
        // The mini stream itself spans two 4096 byte sectors, and the 4095
        // byte stream crosses that boundary.
        assert_eq!(file.root().size32(), 8192);
        assert_eq!(file.streams().len(), 11);
    }

    #[test]
    fn v4_parses_fat_and_mini_fat() {
        let sample = sample_v4(false);
        let file = CompoundFile::parse(&sample.data).unwrap();
        assert_eq!(file.fat_sector_count(), 1);
        assert_eq!(file.fat().len(), 1024);
        assert_eq!(file.mini_fat_sector_count(), 1);
        assert_eq!(file.mini_fat().len(), 1024);
        // The 10000 byte stream is chained through three 4096 byte sectors.
        let big = sample.big_start as usize;
        assert_eq!(file.fat()[big], sample.big_start + 1);
        assert_eq!(file.fat()[big + 1], sample.big_start + 2);
        assert_eq!(file.fat()[big + 2], 0xFFFF_FFFE);
        // The 100 byte mini stream uses two 64 byte mini sectors.
        let mini = sample.mini_start as usize;
        assert_eq!(file.mini_fat()[mini], sample.mini_start + 1);
        assert_eq!(file.mini_fat()[mini + 1], 0xFFFF_FFFE);
        // The directory is a single sector ending the chain.
        assert_eq!(file.fat()[sample.dir_sector as usize], 0xFFFF_FFFE);
        // Sectors which are not part of any chain are FREE.
        assert_eq!(file.fat()[file.fat().len() - 1], 0xFFFF_FFFF);
    }

    #[test]
    fn v4_reads_difat_sector() {
        let sample = sample_v4(true);
        let header = CompoundHeader::parse(&sample.data).unwrap();
        assert_eq!(header.num_fat_sectors, 110);
        assert_eq!(header.num_difat_sectors, 1);
        assert_ne!(header.first_difat_sector, 0xFFFF_FFFE);
        assert_eq!(header.difat[108], sample.first_fat_sector + 108);
        // The 110th FAT sector index lives in the DIFAT sector, not the header.
        let difat_sector = header.first_difat_sector;
        assert!(difat_sector > header.difat[108]);
        let file = CompoundFile::parse(&sample.data).unwrap();
        assert_eq!(file.fat_sector_count(), 110);
        assert_eq!(file.fat().len(), 110 * 1024);
        for (path, expected) in &sample.streams {
            assert_eq!(&file.stream(path).unwrap(), expected, "stream {path}");
        }
    }

    #[test]
    fn v4_reports_bad_fat_sector_count() {
        // Zero FAT sectors: POI's `getBATBlockAndIndex` fails on the empty
        // block list, exactly like the Rust FAT lookup.
        let mut sample = sample_v4(false);
        sample.data[44..48].copy_from_slice(&0u32.to_le_bytes());
        let err = CompoundFile::parse(&sample.data).unwrap_err();
        assert_eq!(err.to_string(), "Index 0 out of bounds for length 0");

        // A huge FAT count makes the reader follow the FREE entries of the
        // header DIFAT array; POI's chain loop detector indexes a boolean[]
        // with the negative block number.
        let mut sample = sample_v4(false);
        sample.data[44..48].copy_from_slice(&70000u32.to_le_bytes());
        let err = CompoundFile::parse(&sample.data).unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with("Index -1 out of bounds for length "),
            "{message}"
        );
    }

    #[test]
    fn v4_detects_fat_chain_loops() {
        // A directory chain which loops on itself.
        let sample = sample_v4(false);
        let mut data = sample.data.clone();
        let dir_fat =
            v4_offset_of_sector(&sample, sample.first_fat_sector) + sample.dir_sector as usize * 4;
        data[dir_fat..dir_fat + 4].copy_from_slice(&sample.dir_sector.to_le_bytes());
        let err = CompoundFile::parse(&data).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "Potential loop detected - Block {} was already claimed but was just requested again",
                sample.dir_sector
            )
        );

        // A big stream chain which loops on itself.
        let mut data = sample.data.clone();
        let big_fat =
            v4_offset_of_sector(&sample, sample.first_fat_sector) + sample.big_start as usize * 4;
        data[big_fat..big_fat + 4].copy_from_slice(&sample.big_start.to_le_bytes());
        let file = CompoundFile::parse(&data).unwrap();
        let err = file.stream("/Big").unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "Potential loop detected - Block {} was already claimed but was just requested again",
                sample.big_start
            )
        );

        // A mini-FAT chain which loops on itself.
        let mut data = sample.data.clone();
        let mini_fat =
            v4_offset_of_sector(&sample, sample.mini_fat_sector) + sample.mini_start as usize * 4;
        data[mini_fat..mini_fat + 4].copy_from_slice(&sample.mini_start.to_le_bytes());
        let file = CompoundFile::parse(&data).unwrap();
        let err = file.stream("/Mini").unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "Potential loop detected - Block {} was already claimed but was just requested again",
                sample.mini_start
            )
        );
    }

    #[test]
    fn v4_reports_missing_and_truncated_sectors() {
        let sample = sample_v4(false);
        // Truncated right before the FAT sector.
        let cut = v4_offset_of_sector(&sample, sample.first_fat_sector);
        let err = CompoundFile::parse(&sample.data[..cut]).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Block {} not found", sample.first_fat_sector)
        );

        // Truncated in the middle of the 4096 byte directory block.
        let cut = v4_offset_of_sector(&sample, sample.dir_sector) + 640;
        let err = CompoundFile::parse(&sample.data[..cut]).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Block {} not found", sample.first_fat_sector)
        );

        // A big stream pointing past the end of the file.
        let mut data = sample.data.clone();
        let big_entry = v4_offset_of_sector(&sample, sample.dir_sector) + 128;
        data[big_entry + 116..big_entry + 120].copy_from_slice(&9999u32.to_le_bytes());
        let file = CompoundFile::parse(&data).unwrap();
        assert_eq!(
            file.stream("/Big").unwrap_err().to_string(),
            "Block 9999 not found"
        );
    }

    #[test]
    fn v4_ignores_num_dir_sectors_and_major_version() {
        // The directory size field and the major version are informational for
        // POI (it uses the sector shift and the FAT chain), so a bogus value
        // must not change anything.
        let sample = sample_v4(false);
        let mut data = sample.data.clone();
        data[40..44].copy_from_slice(&999u32.to_le_bytes());
        data[26..28].copy_from_slice(&3u16.to_le_bytes());
        let file = CompoundFile::parse(&data).unwrap();
        for (path, expected) in &sample.streams {
            assert_eq!(&file.stream(path).unwrap(), expected, "stream {path}");
        }
    }

    #[test]
    fn v4_wraps_as_compound_ole_value() {
        let sample = sample_v4(false);
        let blob = OleBlob::from_internal_data(&PackageBuilder::compound(&sample.data));
        let content = blob.content().unwrap();
        assert_eq!(content.content_type(), ContentType::CompoundStorage);
        assert_eq!(content.length(), sample.data.len() as i64);
        let compound = content.as_compound().unwrap();
        assert!(compound.has_contents_entry().unwrap());
        assert_eq!(
            compound.contents_entry().unwrap().data().unwrap(),
            v4_sequence(64, 61)
        );
        let names: Vec<String> = compound
            .entries()
            .unwrap()
            .into_iter()
            .map(|entry| entry.name().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "/Big".to_string(),
                "/Mini".to_string(),
                "/BoundaryBig".to_string(),
                "/BoundaryMini".to_string(),
                "/Empty".to_string(),
                "/CONTENTS".to_string(),
                "//Nested".to_string(),
                "///Deep".to_string(),
                "//Name+With+Space".to_string(),
                "//%C3%9Cn%C3%AFcode".to_string(),
                "//a%2Bb".to_string(),
            ]
        );
        assert_eq!(
            compound.entry("Storage1/Nested").unwrap().data().unwrap(),
            v4_sequence(10, 71)
        );
        assert_eq!(
            compound.entry("Storage1/Sub/Deep").unwrap().data().unwrap(),
            v4_sequence(5000, 111)
        );
        // `getEntry` URL-decodes the components of the given path ...
        assert_eq!(
            compound
                .entry("Storage1/%C3%9Cn%C3%AFcode")
                .unwrap()
                .data()
                .unwrap(),
            v4_sequence(80, 91)
        );
        // ... but, exactly like Jackcess, it skips empty components, so the
        // encoded names returned by `entries()` are not accepted back.
        assert_eq!(
            compound.entry("//Nested").unwrap_err().to_string(),
            "no such entry: \"Nested\""
        );
    }

    // -- URL encoding tests --------------------------------------------------

    #[test]
    fn encodes_entry_names_like_java() {
        assert_eq!(url_encode("WordDocument"), "WordDocument");
        assert_eq!(url_encode("\u{2}OlePres000"), "%02OlePres000");
        assert_eq!(
            url_encode("\u{5}SummaryInformation"),
            "%05SummaryInformation"
        );
        assert_eq!(url_encode("a b"), "a+b");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("é"), "%C3%A9");
        assert_eq!(url_decode("%02OlePres000").unwrap(), "\u{2}OlePres000");
        assert_eq!(url_decode("a+b").unwrap(), "a b");
        assert!(url_decode("%0").is_err());
    }

    // -- Access wrapper tests ------------------------------------------------

    struct PackageBuilder;

    impl PackageBuilder {
        fn zero_term(text: &str) -> Vec<u8> {
            let mut out = text.as_bytes().to_vec();
            out.push(0);
            out
        }

        /// Outer package header (20 bytes) followed by the OLE header up to
        /// (but excluding) the data block length.  Returns the header bytes.
        fn package_prefix(
            pretty_name: &str,
            class_name: &str,
            type_name: &str,
        ) -> (Vec<u8>, usize) {
            let pretty = Self::zero_term(pretty_name);
            let class = Self::zero_term(class_name);
            let package_header_len = 20 + pretty.len() + class.len();
            let mut out = Vec::new();
            out.extend_from_slice(&PACKAGE_SIGNATURE.to_le_bytes());
            out.extend_from_slice(&(package_header_len as u16).to_le_bytes());
            out.extend_from_slice(&2i32.to_le_bytes()); // package object type
            out.extend_from_slice(&(pretty.len() as u16).to_le_bytes());
            out.extend_from_slice(&(class.len() as u16).to_le_bytes());
            // `OleUtil.writePackageHeader`: prettyNameOff is the position
            // (20) plus the 8 bytes of offsets/object size which follow.
            let pretty_off = 20;
            out.extend_from_slice(&(pretty_off as u16).to_le_bytes());
            out.extend_from_slice(&((pretty_off + pretty.len()) as u16).to_le_bytes());
            out.extend_from_slice(&(-1i32).to_le_bytes());
            out.extend_from_slice(&pretty);
            out.extend_from_slice(&class);

            let type_bytes = Self::zero_term(type_name);
            out.extend_from_slice(&OLE_VERSION.to_le_bytes());
            out.extend_from_slice(&2i32.to_le_bytes());
            out.extend_from_slice(&(type_bytes.len() as i32).to_le_bytes());
            out.extend_from_slice(&type_bytes);
            out.extend_from_slice(&0i64.to_le_bytes());
            (out, package_header_len)
        }

        const PACKAGE_FOOTER: [u8; 12] = [
            0x01, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0xAD, 0x05, 0xFE,
        ];

        /// Builds a simple-package Access blob like
        /// `OleUtil.createBlob(Builder.setSimplePackage...)`.
        fn simple_package(
            pretty_name: &str,
            class_name: &str,
            type_name: &str,
            file_name: &str,
            file_path: &str,
            local_path: &str,
            content: &[u8],
        ) -> Vec<u8> {
            let (mut out, _) = Self::package_prefix(pretty_name, class_name, type_name);

            let file = Self::zero_term(file_name);
            let path = Self::zero_term(file_path);
            let mut payload = Vec::new();
            payload.extend_from_slice(&PACKAGE_STREAM_SIGNATURE.to_le_bytes());
            payload.extend_from_slice(&file);
            payload.extend_from_slice(&path);
            payload.extend_from_slice(&PS_EMBEDDED_FILE.to_le_bytes());
            payload.extend_from_slice(&(path.len() as i32).to_le_bytes());
            payload.extend_from_slice(&path);
            payload.extend_from_slice(&(content.len() as i32).to_le_bytes());
            payload.extend_from_slice(content);
            // footer strings, in reverse order (local path, name, path)
            for text in [local_path, file_name, file_path] {
                let units: Vec<u16> = text.encode_utf16().collect();
                payload.extend_from_slice(&(units.len() as i32).to_le_bytes());
                for unit in units {
                    payload.extend_from_slice(&unit.to_le_bytes());
                }
            }

            out.extend_from_slice(&(payload.len() as i32).to_le_bytes());
            out.extend_from_slice(&payload);
            out.extend_from_slice(&Self::PACKAGE_FOOTER);
            out
        }

        fn link(file_name: &str, file_path: &str) -> Vec<u8> {
            let (mut out, _) = Self::package_prefix("Packager Shell Object", "Package", "Package");
            let mut payload = Vec::new();
            payload.extend_from_slice(&PACKAGE_STREAM_SIGNATURE.to_le_bytes());
            payload.extend_from_slice(&Self::zero_term(file_name));
            payload.extend_from_slice(&Self::zero_term(file_path));
            payload.extend_from_slice(&PS_LINKED_FILE.to_le_bytes());
            payload.extend_from_slice(&1u16.to_le_bytes());
            payload.extend_from_slice(&Self::zero_term(file_path));
            out.extend_from_slice(&(payload.len() as i32).to_le_bytes());
            out.extend_from_slice(&payload);
            out.extend_from_slice(&Self::PACKAGE_FOOTER);
            out
        }

        /// Wraps an already-built compound file in an Access OLE package.
        fn compound(compound: &[u8]) -> Vec<u8> {
            let (mut out, _) =
                Self::package_prefix("Document", "Word.Document.8", "Word.Document.8");
            out.extend_from_slice(&(compound.len() as i32).to_le_bytes());
            out.extend_from_slice(compound);
            out.extend_from_slice(&Self::PACKAGE_FOOTER);
            out
        }
    }

    #[test]
    fn parses_unknown_blobs() {
        for bytes in [&[][..], &[1u8, 2, 3][..], &[0u8; 32][..]] {
            let blob = OleBlob::from_internal_data(bytes);
            let content = blob.content().unwrap();
            assert_eq!(content.content_type(), ContentType::Unknown);
            assert_eq!(content.length(), -1);
        }
    }

    #[test]
    fn parses_simple_package() {
        let content = b"this is some test data\n";
        let built = PackageBuilder::simple_package(
            "Packager Shell Object",
            "Package",
            "Package",
            "test_data.txt",
            "Z:\\jackcess_test\\ole\\test_data.txt",
            "C:\\Users\\James\\AppData\\Local\\Temp\\test_data.txt",
            content,
        );
        let blob = OleBlob::from_internal_data(&built);
        assert_eq!(blob.length(), built.len());
        let parsed = blob.content().unwrap();
        assert_eq!(parsed.content_type(), ContentType::SimplePackage);
        assert_eq!(parsed.pretty_name(), Some("Packager Shell Object"));
        assert_eq!(parsed.class_name(), Some("Package"));
        assert_eq!(parsed.type_name(), Some("Package"));
        assert_eq!(parsed.file_name(), Some("test_data.txt"));
        assert_eq!(
            parsed.file_path(),
            Some("Z:\\jackcess_test\\ole\\test_data.txt")
        );
        assert_eq!(
            parsed.local_file_path(),
            Some("C:\\Users\\James\\AppData\\Local\\Temp\\test_data.txt")
        );
        assert_eq!(parsed.length(), content.len() as i64);
        assert_eq!(parsed.data().unwrap(), content);

        let simple = parsed.as_simple_package().unwrap();
        assert_eq!(simple.length(), content.len() as i64);
        assert_eq!(simple.data().unwrap(), content);
    }

    #[test]
    fn parses_link() {
        let blob = OleBlob::from_internal_data(&PackageBuilder::link(
            "test_data.txt",
            "Z:\\jackcess_test\\ole\\test_data.txt",
        ));
        let parsed = blob.content().unwrap();
        assert_eq!(parsed.content_type(), ContentType::Link);
        assert_eq!(parsed.file_name(), Some("test_data.txt"));
        assert_eq!(
            parsed.link_path(),
            Some("Z:\\jackcess_test\\ole\\test_data.txt")
        );
        assert_eq!(
            parsed.file_path(),
            Some("Z:\\jackcess_test\\ole\\test_data.txt")
        );
        assert_eq!(parsed.length(), -1);
    }

    #[test]
    fn parses_compound_storage() {
        let compound = sample_compound();
        let blob = OleBlob::from_internal_data(&PackageBuilder::compound(&compound));
        let parsed = blob.content().unwrap();
        assert_eq!(parsed.content_type(), ContentType::CompoundStorage);
        assert_eq!(parsed.pretty_name(), Some("Document"));
        assert_eq!(parsed.class_name(), Some("Word.Document.8"));
        let compound_content = parsed.as_compound().unwrap();
        assert_eq!(compound_content.length(), compound.len() as i64);
        assert_eq!(compound_content.data().unwrap(), &compound[..]);
        assert!(!compound_content.has_contents_entry().unwrap());
        assert!(compound_content.contents_entry().is_err());

        let entries = compound_content.entries().unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name()).collect();
        assert_eq!(names, vec!["//Nested", "/Big", "/Mini",]);
        let paths: Vec<&str> = entries.iter().map(|e| e.name()).collect();
        // `entry()` accepts the encoded names as well as simple paths.
        let big = compound_content.entry(paths[1]).unwrap();
        assert_eq!(big.length(), 5000);
        let expected: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        assert_eq!(big.data().unwrap(), expected);
        let nested = compound_content.entry("/Storage1/Nested").unwrap();
        assert_eq!(nested.data().unwrap(), (10..20).collect::<Vec<u8>>());
    }

    #[test]
    fn reports_compound_parse_errors_lazily() {
        let compound = sample_compound();
        let mut broken = compound.clone();
        // Keep the compound signature (so the wrapper still classifies it as
        // COMPOUND_STORAGE) but invalidate the FAT count.
        broken[44..48].copy_from_slice(&0u32.to_le_bytes());
        let blob = OleBlob::from_internal_data(&PackageBuilder::compound(&broken));
        let parsed = blob.content().unwrap();
        assert_eq!(parsed.content_type(), ContentType::CompoundStorage);
        let err = parsed.as_compound().unwrap().entries().unwrap_err();
        assert_eq!(err.to_string(), "Index 0 out of bounds for length 0");
    }

    #[test]
    fn clamps_embedded_data_beyond_the_blob() {
        // `EmbeddedContentImpl.getStream()` uses `ByteArrayInputStream`,
        // which clamps the requested range to the end of the blob instead of
        // throwing.  A truncated compound wrapper therefore yields the
        // remaining bytes while `length()` still reports the declared length.
        let compound = sample_compound();
        let wrapper = PackageBuilder::compound(&compound);
        let truncated = &wrapper[..wrapper.len() / 2];
        let blob = OleBlob::from_internal_data(truncated);
        let content = blob.content().unwrap();
        assert_eq!(content.content_type(), ContentType::CompoundStorage);
        assert_eq!(content.length(), compound.len() as i64);
        let data = content.data().unwrap();
        assert_eq!(data.len(), truncated.len() - 85);
        assert_eq!(data, &compound[..data.len()]);
        // The lazy compound parse still fails on the missing blocks.
        let err = content.as_compound().unwrap().entries().unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn rejects_truncated_simple_package() {
        let blob_bytes = PackageBuilder::simple_package(
            "Packager Shell Object",
            "Package",
            "Package",
            "test_data.txt",
            "Z:\\test_data.txt",
            "C:\\test_data.txt",
            b"hello",
        );
        let truncated = &blob_bytes[..blob_bytes.len() / 2];
        let blob = OleBlob::from_internal_data(truncated);
        let err = blob.content().unwrap_err();
        assert!(err.to_string().contains("newLimit > capacity"), "{err}");
    }

    #[test]
    fn parses_other_content() {
        // A well-formed package header whose type is not "Package" and whose
        // payload is not a compound file.
        let pretty = PackageBuilder::zero_term("Text");
        let class = PackageBuilder::zero_term("Text.File");
        let type_name = PackageBuilder::zero_term("TextFile");
        let payload = b"some embedded text";

        let package_header_len = 20 + pretty.len() + class.len();
        let mut blob = Vec::new();
        blob.extend_from_slice(&PACKAGE_SIGNATURE.to_le_bytes());
        blob.extend_from_slice(&(package_header_len as u16).to_le_bytes());
        blob.extend_from_slice(&2i32.to_le_bytes());
        blob.extend_from_slice(&(pretty.len() as u16).to_le_bytes());
        blob.extend_from_slice(&(class.len() as u16).to_le_bytes());
        let pretty_off = 20;
        blob.extend_from_slice(&(pretty_off as u16).to_le_bytes());
        blob.extend_from_slice(&((pretty_off + pretty.len()) as u16).to_le_bytes());
        blob.extend_from_slice(&(-1i32).to_le_bytes());
        blob.extend_from_slice(&pretty);
        blob.extend_from_slice(&class);
        blob.extend_from_slice(&OLE_VERSION.to_le_bytes());
        blob.extend_from_slice(&2i32.to_le_bytes());
        blob.extend_from_slice(&(type_name.len() as i32).to_le_bytes());
        blob.extend_from_slice(&type_name);
        blob.extend_from_slice(&0i64.to_le_bytes());
        blob.extend_from_slice(&(payload.len() as i32).to_le_bytes());
        blob.extend_from_slice(payload);

        let blob = OleBlob::from_internal_data(&blob);
        let parsed = blob.content().unwrap();
        assert_eq!(parsed.content_type(), ContentType::Other);
        assert_eq!(parsed.pretty_name(), Some("Text"));
        assert_eq!(parsed.class_name(), Some("Text.File"));
        assert_eq!(parsed.type_name(), Some("TextFile"));
        assert_eq!(parsed.length(), payload.len() as i64);
        assert_eq!(parsed.data().unwrap(), payload);
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    /// `OleBlob.Builder.setSimplePackageBytes` + `toBlob` produces the exact
    /// `OleUtil.createBlob` layout, and the result parses back to a simple
    /// package with the given content.
    #[test]
    fn simple_package_bytes_round_trip() {
        let content: &[u8] = b"hello ole";
        let blob = OleBlobBuilder::new()
            .set_simple_package_bytes(content.to_vec())
            .set_simple_package_file_name("c.txt")
            .set_simple_package_file_path("C:\\c.txt")
            .to_blob()
            .unwrap();
        let bytes = blob.bytes();
        assert_eq!(&bytes[0..2], PACKAGE_SIGNATURE.to_le_bytes().as_slice());
        assert_eq!(&bytes[bytes.len() - 12..], PACKAGE_FOOTER.as_slice());
        let content_at = bytes
            .windows(content.len())
            .position(|window| window == content)
            .expect("content in the blob");
        // after the content the simple package stream footer follows
        assert!(content_at > 0);

        let parsed = blob.content().unwrap();
        assert_eq!(parsed.content_type(), ContentType::SimplePackage);
        let simple = parsed.as_simple_package().unwrap();
        assert_eq!(simple.file_name(), "c.txt");
        assert_eq!(simple.file_path(), "C:\\c.txt");
        assert_eq!(simple.package().pretty_name(), "Packager Shell Object");
        assert_eq!(simple.package().class_name(), "Package");
        assert_eq!(simple.package().type_name(), "Package");
        assert_eq!(simple.length(), content.len() as i64);
        assert_eq!(simple.data().unwrap(), content);
    }

    /// A link's content is the zero terminated file path, and the parsed link
    /// keeps the file name / path.
    #[test]
    fn link_path_round_trip() {
        let blob = OleBlobBuilder::new()
            .set_link_path("C:\\data\\a.txt")
            .set_link_file_name("a.txt")
            .to_blob()
            .unwrap();
        let content = blob.content().unwrap();
        assert_eq!(content.content_type(), ContentType::Link);
        let link = content.as_link().unwrap();
        assert_eq!(link.file_name(), "a.txt");
        assert_eq!(link.link_path(), "C:\\data\\a.txt");
        assert_eq!(link.package().pretty_name(), "Packager Shell Object");
        assert_eq!(link.package().class_name(), "Package");
        assert_eq!(link.package().type_name(), "Package");
    }

    /// "Other" content is embedded unchanged.
    #[test]
    fn other_bytes_round_trip() {
        let blob = OleBlobBuilder::new()
            .set_other_bytes(vec![1u8, 2, 3, 4])
            .set_package_pretty_name("Packager Shell Object")
            .set_package_class_name("Package")
            .to_blob()
            .unwrap();
        let content = blob.content().unwrap();
        assert_eq!(content.content_type(), ContentType::Other);
        assert_eq!(content.data().unwrap(), &[1u8, 2, 3, 4]);
        assert_eq!(content.length(), 4);
    }

    /// The `OleUtil.getZeroTermStrBytes` normalization (NFD, strip the
    /// diacritical marks, US-ASCII with `?` replacement, NUL terminator).
    #[test]
    fn zero_term_bytes_normalize_like_java() {
        assert_eq!(
            zero_term_str_bytes(Some("caf\u{e9}.txt")).unwrap(),
            b"cafe.txt\0"
        );
        assert_eq!(
            zero_term_str_bytes(Some("M\u{fc}ller")).unwrap(),
            b"Muller\0"
        );
        // a spacing modifier letter (Lm) is stripped
        assert_eq!(zero_term_str_bytes(Some("a\u{02B0}b")).unwrap(), b"ab\0");
        // a character with no decomposition is unmappable for US-ASCII
        assert_eq!(
            zero_term_str_bytes(Some("\u{0141}\u{f3}d\u{17a}")).unwrap(),
            b"?odz\0"
        );
        assert_eq!(
            zero_term_str_bytes(Some("\u{65E5}\u{672C}\u{8A9E}")).unwrap(),
            b"???\0"
        );
    }

    /// The content length check mirrors Java's fixed size allocation: a
    /// declared length which does not match the bytes is refused.
    #[test]
    fn content_length_must_match() {
        struct Short(std::io::Cursor<Vec<u8>>);
        impl std::io::Read for Short {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf)
            }
        }
        let mut builder = OleBlobBuilder::new();
        builder
            .set_simple_package_stream(Short(std::io::Cursor::new(b"abc".to_vec())), 2)
            .unwrap()
            .set_simple_package_file_name("x.txt")
            .set_simple_package_file_path("C:\\x.txt");
        assert!(builder.to_blob().is_err());
    }
}
