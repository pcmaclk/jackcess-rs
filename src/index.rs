//! Port of `IndexData`, `IndexImpl` and the index entry/page parts of the
//! Jackcess index B-tree.
//!
//! The in-memory page cache and the page-splitting logic live in
//! [`crate::indexpagecache`]; this module holds the logical index descriptor,
//! the index `Entry` model (ordering and encoding), the column descriptors
//! which encode index key bytes, and the index page read/write code.
//!
//! # Java name mapping (the `Index` object API)
//!
//! | Java (`com.healthmarketscience.jackcess.Index`) | Rust |
//! |---|---|
//! | `getTable()` | [`IndexImpl::get_table`] (the owning table is passed in) |
//! | `getName()` / `isPrimaryKey()` / `isForeignKey()` | field [`IndexImpl::name`] / [`IndexImpl::is_primary_key`] / [`IndexImpl::is_foreign_key`] |
//! | `getColumns()` / `getColumnCount()` | [`IndexImpl::get_columns`] / [`IndexImpl::get_column_count`] |
//! | `getReferencedIndex()` | [`IndexImpl::get_referenced_index`] |
//! | `shouldIgnoreNulls()` / `isUnique()` / `isRequired()` | [`IndexImpl::should_ignore_nulls`] / [`IndexImpl::is_unique`] / [`IndexImpl::is_required`] |
//! | `getProperties()` | [`IndexImpl::get_properties`] |
//! | `newCursor()` | [`IndexImpl::new_cursor`] (returns a [`crate::cursor::CursorBuilder`]) |
//! | `Index.Column.getColumn()` / `isAscending()` | [`IndexColumn::get_column`] / [`IndexColumn::is_ascending`] |
//! | `Index.Column.getColumnIndex()` / `getName()` | [`IndexColumn::get_column_index`] / [`IndexColumn::get_name`] |

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::bytebuf::{ByteBuf, ByteOrder};
use crate::column::{self, Column, DateContext};
use crate::database::DateTimeType;
use crate::datatype::DataType;
use crate::datenums::TimeZone;
use crate::error::{Error, Result};
use crate::format::{Charset, JetFormat};
use crate::page_types;
use crate::pagechannel::PageChannel;
use crate::table::Table;
use crate::usagemap::UsageMap;
use crate::value::Value;

pub const MAX_COLUMNS: usize = 10;
pub const COLUMN_UNUSED: i16 = -1;
pub const ASCENDING_COLUMN_FLAG: u8 = 0x01;
pub const UNIQUE_INDEX_FLAG: u8 = 0x01;
pub const IGNORE_NULLS_INDEX_FLAG: u8 = 0x02;
pub const REQUIRED_INDEX_FLAG: u8 = 0x08;
pub const UNKNOWN_INDEX_FLAG: u8 = 0x80;
pub const MAGIC_INDEX_NUMBER: i32 = 1923;

/// `IndexData.INVALID_INDEX_PAGE_NUMBER` (page 0 is never a valid index page)
pub const INVALID_INDEX_PAGE_NUMBER: i32 = 0;

/// `IndexCodes` flags which start an index key value
pub const ASC_START_FLAG: u8 = 0x7F;
pub const ASC_NULL_FLAG: u8 = 0x00;
pub const DESC_START_FLAG: u8 = 0x80;
pub const DESC_NULL_FLAG: u8 = 0xFF;

pub const ASC_BOOLEAN_TRUE: u8 = 0x00;
pub const ASC_BOOLEAN_FALSE: u8 = 0xFF;
pub const DESC_BOOLEAN_TRUE: u8 = ASC_BOOLEAN_FALSE;
pub const DESC_BOOLEAN_FALSE: u8 = ASC_BOOLEAN_TRUE;

/// maximum length of the key portion of an index entry, in the units a text
/// field is measured in
pub const MAX_KEY_UNITS: usize = 255;
/// length of the digest which ends a truncated key
pub const KEY_DIGEST_LENGTH: usize = 2;
/// the CRC-16/ARC polynomial in its MSB-first form
const KEY_DIGEST_POLY: u32 = 0x8005;

/// the byte order used for index entry data (the page header is little endian)
pub const ENTRY_BYTE_ORDER: ByteOrder = ByteOrder::BigEndian;

const ASC_EXT_DATE_TRAILER: [u8; 7] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02];

fn desc_ext_date_trailer() -> [u8; 7] {
    let mut t = ASC_EXT_DATE_TRAILER;
    for b in t.iter_mut() {
        *b = !*b;
    }
    t
}

static NEXT_ENTRY_ID: AtomicU64 = AtomicU64::new(1);

fn next_entry_id() -> u64 {
    NEXT_ENTRY_ID.fetch_add(1, AtomicOrdering::Relaxed)
}

/// `RowIdImpl.Type`
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowIdType {
    AlwaysFirst,
    Normal,
    AlwaysLast,
}

/// `RowIdImpl` - identifies the row of a table referenced by an index entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowId {
    pub page_number: i32,
    pub row_number: i32,
}

impl RowId {
    pub const FIRST_PAGE_NUMBER: i32 = -1;
    pub const LAST_PAGE_NUMBER: i32 = -2;
    pub const INVALID_ROW_NUMBER: i32 = -1;

    pub const FIRST: RowId = RowId {
        page_number: RowId::FIRST_PAGE_NUMBER,
        row_number: RowId::INVALID_ROW_NUMBER,
    };
    pub const LAST: RowId = RowId {
        page_number: RowId::LAST_PAGE_NUMBER,
        row_number: RowId::INVALID_ROW_NUMBER,
    };

    pub fn new(page_number: i32, row_number: i32) -> RowId {
        RowId {
            page_number,
            row_number,
        }
    }

    pub fn get_type(&self) -> RowIdType {
        if self.page_number == RowId::FIRST_PAGE_NUMBER {
            RowIdType::AlwaysFirst
        } else if self.page_number == RowId::LAST_PAGE_NUMBER {
            RowIdType::AlwaysLast
        } else {
            RowIdType::Normal
        }
    }

    pub fn is_valid(&self) -> bool {
        self.row_number >= 0 && self.page_number >= 0
    }

    /// `RowIdImpl.COMPARATOR`: type, then page number, then row number.
    pub fn compare_to(&self, other: &RowId) -> i32 {
        let t = (self.get_type() as i32).cmp(&(other.get_type() as i32));
        if t != std::cmp::Ordering::Equal {
            return if t == std::cmp::Ordering::Less { -1 } else { 1 };
        }
        let p = self.page_number.cmp(&other.page_number);
        if p != std::cmp::Ordering::Equal {
            return if p == std::cmp::Ordering::Less { -1 } else { 1 };
        }
        let r = self.row_number.cmp(&other.row_number);
        match r {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }
}

/// `IndexData.EntryType` - type attributes which simplify comparisons.  The
/// declaration order is significant (it is the Java enum ordinal order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryType {
    AlwaysFirst,
    FirstValid,
    Normal,
    LastValid,
    AlwaysLast,
}

/// `IndexData.BYTE_CODE_COMPARATOR`
pub fn compare_byte_codes(left: &Option<Vec<u8>>, right: &Option<Vec<u8>>) -> i32 {
    match (left, right) {
        (None, None) => 0,
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
        (Some(l), Some(r)) => {
            let len = l.len().min(r.len());
            let mut pos = 0;
            while pos < len && l[pos] == r[pos] {
                pos += 1;
            }
            if pos < len {
                if l[pos] < r[pos] {
                    -1
                } else {
                    1
                }
            } else if l.len() < r.len() {
                -1
            } else if l.len() > r.len() {
                1
            } else {
                0
            }
        }
    }
}

/// A single index entry (a leaf entry points at a row, a node entry points at
/// a sub-page).
#[derive(Clone, Debug)]
pub struct Entry {
    /// page/row on which the referenced row is stored
    row_id: RowId,
    /// the encoded key bytes (`None` for the special first/last entries)
    entry_bytes: Option<Vec<u8>>,
    /// comparable type for the entry
    entry_type: EntryType,
    /// index page number of the page to which a node entry refers
    sub_page_number: Option<i32>,
    /// object identity (Java compares entries by reference in a few places)
    id: u64,
}

fn determine_entry_type(entry_bytes: &Option<Vec<u8>>, row_id: &RowId) -> Result<EntryType> {
    if entry_bytes.is_some() {
        Ok(match row_id.get_type() {
            RowIdType::Normal => EntryType::Normal,
            RowIdType::AlwaysFirst => EntryType::FirstValid,
            RowIdType::AlwaysLast => EntryType::LastValid,
        })
    } else if !row_id.is_valid() {
        Ok(match row_id.get_type() {
            RowIdType::AlwaysFirst => EntryType::AlwaysFirst,
            _ => EntryType::AlwaysLast,
        })
    } else {
        Err(Error::IllegalArgument(
            "Values was null for valid entry".into(),
        ))
    }
}

impl Entry {
    pub fn new(entry_bytes: Option<Vec<u8>>, row_id: RowId) -> Result<Entry> {
        let entry_type = determine_entry_type(&entry_bytes, &row_id)?;
        Ok(Entry {
            row_id,
            entry_bytes,
            entry_type,
            sub_page_number: None,
            id: next_entry_id(),
        })
    }

    /// A leaf entry for the given encoded key.
    pub fn leaf(entry_bytes: Vec<u8>, row_id: RowId) -> Entry {
        Entry {
            row_id,
            entry_type: determine_entry_type(&Some(entry_bytes.clone()), &row_id)
                .unwrap_or(EntryType::Normal),
            entry_bytes: Some(entry_bytes),
            sub_page_number: None,
            id: next_entry_id(),
        }
    }

    /// One of the special `FIRST_ENTRY` / `LAST_ENTRY` sentinels.
    pub fn special(row_id: RowId) -> Entry {
        Entry {
            row_id,
            entry_type: determine_entry_type(&None, &row_id).unwrap_or(EntryType::Normal),
            entry_bytes: None,
            sub_page_number: None,
            id: next_entry_id(),
        }
    }

    pub fn first_entry() -> Entry {
        Entry::special(RowId::FIRST)
    }

    pub fn last_entry() -> Entry {
        Entry::special(RowId::LAST)
    }

    /// Reads an entry from the given bytes (`Entry` / `NodeEntry` constructors).
    pub fn read_from(data: &[u8], entry_len: usize, is_leaf: bool) -> Result<Entry> {
        let extra: usize = if is_leaf { 0 } else { 4 };
        if entry_len < 4 + extra || data.len() < entry_len {
            return Err(Error::Jackcess(format!(
                "Invalid index entry length {}",
                entry_len
            )));
        }
        let col_entry_len = entry_len - (4 + extra);
        let entry_bytes = data[..col_entry_len].to_vec();
        let page = ((data[col_entry_len] as i32) << 16)
            | ((data[col_entry_len + 1] as i32) << 8)
            | (data[col_entry_len + 2] as i32);
        let row = data[col_entry_len + 3] as i32;
        let row_id = RowId::new(page, row);
        let sub_page_number = if is_leaf {
            None
        } else {
            Some(i32::from_be_bytes([
                data[col_entry_len + 4],
                data[col_entry_len + 5],
                data[col_entry_len + 6],
                data[col_entry_len + 7],
            ]))
        };
        Ok(Entry {
            row_id,
            entry_bytes: Some(entry_bytes),
            entry_type: EntryType::Normal,
            sub_page_number,
            id: next_entry_id(),
        })
    }

    pub fn get_row_id(&self) -> &RowId {
        &self.row_id
    }

    pub fn get_type(&self) -> EntryType {
        self.entry_type
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn get_sub_page_number(&self) -> Option<i32> {
        self.sub_page_number
    }

    pub fn is_leaf_entry(&self) -> bool {
        self.sub_page_number.is_none()
    }

    pub fn is_valid(&self) -> bool {
        self.entry_bytes.is_some()
    }

    pub fn get_entry_bytes(&self) -> Option<&[u8]> {
        self.entry_bytes.as_deref()
    }

    /// Size of this entry in the db.
    pub fn size(&self) -> i32 {
        let len = self.entry_bytes.as_ref().map(|b| b.len()).unwrap_or(0) as i32;
        // need 4 trailing bytes for the rowId (and 4 more for a node entry)
        len + 4 + if self.sub_page_number.is_some() { 4 } else { 0 }
    }

    /// Writes this entry into a buffer (`Entry.write`).
    pub fn write(&self, buffer: &mut ByteBuf, prefix: &[u8]) -> Result<()> {
        let bytes = self
            .entry_bytes
            .as_ref()
            .ok_or_else(|| Error::IllegalState("cannot write an invalid index entry".into()))?;

        if prefix.len() <= bytes.len() {
            // write entry bytes, not including prefix
            buffer.put_bytes(&bytes[prefix.len()..])?;
            let page = self.row_id.page_number;
            buffer.put_u8(((page >> 16) & 0xFF) as u8)?;
            buffer.put_u8(((page >> 8) & 0xFF) as u8)?;
            buffer.put_u8((page & 0xFF) as u8)?;
        } else if prefix.len() <= bytes.len() + 3 {
            // the prefix includes part of the page number, write to temp buffer
            // and copy last bytes to output buffer
            let page = self.row_id.page_number;
            let tmp = [
                ((page >> 16) & 0xFF) as u8,
                ((page >> 8) & 0xFF) as u8,
                (page & 0xFF) as u8,
            ];
            let start = prefix.len() - bytes.len();
            buffer.put_bytes(&tmp[start..])?;
        } else {
            // since the row number would never be the same if the page number is
            // the same, nothing past the page number should ever be included in
            // the prefix.
            return Err(Error::IllegalState(
                "prefix should never be this long".into(),
            ));
        }

        buffer.put_u8((self.row_id.row_number & 0xFF) as u8)?;

        if let Some(sub) = self.sub_page_number {
            buffer.put_u8(((sub >> 24) & 0xFF) as u8)?;
            buffer.put_u8(((sub >> 16) & 0xFF) as u8)?;
            buffer.put_u8(((sub >> 8) & 0xFF) as u8)?;
            buffer.put_u8((sub & 0xFF) as u8)?;
        }

        Ok(())
    }

    /// `@return true iff the entryBytes are equal between this Entry and the
    /// given Entry`
    pub fn equals_entry_bytes(&self, other: &Entry) -> bool {
        compare_byte_codes(&self.entry_bytes, &other.entry_bytes) == 0
    }

    /// Returns a copy of this entry as a node entry with the given sub page.
    pub fn as_node_entry(&self, sub_page_number: i32) -> Entry {
        Entry {
            row_id: self.row_id,
            entry_bytes: self.entry_bytes.clone(),
            entry_type: self.entry_type,
            sub_page_number: Some(sub_page_number),
            id: next_entry_id(),
        }
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Entry) -> bool {
        if self.is_leaf_entry() != other.is_leaf_entry() {
            return false;
        }
        if !self.is_leaf_entry() && self.sub_page_number != other.sub_page_number {
            return false;
        }
        self.compare_to(other) == 0
    }
}

impl Eq for Entry {}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Entry) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Entry) -> std::cmp::Ordering {
        match self.compare_to(other) {
            i if i < 0 => std::cmp::Ordering::Less,
            0 => std::cmp::Ordering::Equal,
            _ => std::cmp::Ordering::Greater,
        }
    }
}

impl Entry {
    /// `Entry.compareTo`
    pub fn compare_to(&self, other: &Entry) -> i32 {
        if std::ptr::eq(self, other) {
            return 0;
        }

        if self.is_valid() && other.is_valid() {
            // comparing two valid entries.  first compare by actual byte values
            let entry_cmp = compare_byte_codes(&self.entry_bytes, &other.entry_bytes);
            if entry_cmp != 0 {
                return entry_cmp;
            }
        } else {
            // mixed validity: defer to the EntryType
            let type_cmp = (self.entry_type as i32).cmp(&(other.entry_type as i32));
            if type_cmp != std::cmp::Ordering::Equal {
                return if type_cmp == std::cmp::Ordering::Less {
                    -1
                } else {
                    1
                };
            }
        }

        // at this point we let the RowId decide the final result
        self.row_id.compare_to(&other.row_id)
    }
}

/// Returns the valid insertion point for an index indicating a missing entry.
pub fn missing_index_to_insertion_point(idx: i32) -> i32 {
    -(idx + 1)
}

/// `Collections.binarySearch(List<Entry>, Entry)` over a slice.
pub fn binary_search(entries: &[Entry], entry: &Entry) -> i32 {
    let mut low: i64 = 0;
    let mut high: i64 = entries.len() as i64 - 1;
    while low <= high {
        let mid = (low + high) / 2;
        let cmp = entries[mid as usize].compare_to(entry);
        if cmp < 0 {
            low = mid + 1;
        } else if cmp > 0 {
            high = mid - 1;
        } else {
            return mid as i32;
        }
    }
    -(low as i32) - 1
}

/// Flips all the bits in the byte array (`IndexData.flipBytes`).
pub fn flip_bytes(value: &mut [u8]) {
    for b in value.iter_mut() {
        *b = !*b;
    }
}

/// Flips the first bit in the byte at the given index.
pub fn flip_first_bit_in_byte(value: &mut [u8], index: usize) {
    value[index] ^= 0x80;
}

/// Builds the transition table for the truncated key digest.
fn create_key_digest_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut reg: u32 = (i as u32) << 8;
        for _ in 0..8 {
            reg = ((reg << 1) & 0xFFFF)
                ^ if (reg & 0x8000) != 0 {
                    KEY_DIGEST_POLY
                } else {
                    0
                };
        }
        *slot = ((reg & 0xFF) << 8) | (reg >> 8);
    }
    table
}

/// `IndexData.truncateEntryBytes` - truncates a key which exceeds the maximum
/// key length, replacing the discarded bytes with a digest of them.
pub fn truncate_entry_bytes(entry_bytes: &[u8], max_length: usize) -> Vec<u8> {
    if entry_bytes.len() <= max_length {
        return entry_bytes.to_vec();
    }
    let kept = max_length - KEY_DIGEST_LENGTH;
    let table = create_key_digest_table();

    // every discarded byte feeds the digest, but the last byte of the entry is
    // xored in rather than clocked through the table
    let mut digest: u32 = 0;
    for &entry_byte in entry_bytes.iter().take(entry_bytes.len() - 1).skip(kept) {
        digest = (digest >> 8) ^ table[(digest & 0xFF) as usize] ^ (entry_byte as u32);
        digest &= 0xFFFF;
    }
    digest = (digest ^ ((entry_bytes[entry_bytes.len() - 1] as u32) << 8)) & 0xFFFF;

    let mut truncated = entry_bytes[..max_length].to_vec();
    truncated[kept] = (digest >> 8) as u8;
    truncated[kept + 1] = digest as u8;
    truncated
}

// ---------------------------------------------------------------------------
// column descriptors (index key encoding)
// ---------------------------------------------------------------------------

/// The kind of encoding used for one column of an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptorKind {
    /// INT, LONG, MONEY, COMPLEX_TYPE, BIG_INT
    Integer,
    /// FLOAT, DOUBLE, SHORT_DATE_TIME
    FloatingPoint,
    /// NUMERIC with the legacy sort order
    LegacyFixedPoint,
    /// NUMERIC with the new (Access 2007+) sort order
    FixedPoint,
    Byte,
    Boolean,
    /// TEXT, MEMO
    Text,
    Guid,
    Binary,
    ExtDate,
    /// a column type which cannot be written
    ReadOnly,
}

/// `IndexData.ColumnDescriptor` - information about a column in an index which
/// also encodes new index values.
#[derive(Clone, Debug)]
pub struct ColumnDescriptor {
    pub column: Column,
    pub flags: u8,
    pub kind: DescriptorKind,
    /// the database's date/time conversion context
    /// (`IndexData.this$0.getTable().getDatabase()` in Java, i.e. the
    /// `DateTimeFactory`/`TimeZone` a `SHORT_DATE_TIME` value is encoded with)
    pub date_context: DateContext,
}

fn null_entry_flag(is_ascending: bool) -> u8 {
    if is_ascending {
        ASC_NULL_FLAG
    } else {
        DESC_NULL_FLAG
    }
}

fn start_entry_flag(is_ascending: bool) -> u8 {
    if is_ascending {
        ASC_START_FLAG
    } else {
        DESC_START_FLAG
    }
}

impl ColumnDescriptor {
    pub fn new(column: Column, flags: u8, format: &JetFormat) -> ColumnDescriptor {
        ColumnDescriptor::with_date_context(column, flags, format, DateContext::default())
    }

    /// [`ColumnDescriptor::new`] with the database's date/time conversion
    /// context, which is what a `SHORT_DATE_TIME` key is encoded with
    /// (`IndexData.this$0.getTable().getDatabase().getDateTimeFactory()`).
    pub fn with_date_context(
        column: Column,
        flags: u8,
        format: &JetFormat,
        date_context: DateContext,
    ) -> ColumnDescriptor {
        let kind = match column.data_type {
            DataType::Text | DataType::Memo => DescriptorKind::Text,
            DataType::Int
            | DataType::Long
            | DataType::Money
            | DataType::ComplexType
            | DataType::BigInt => DescriptorKind::Integer,
            DataType::Float | DataType::Double | DataType::ShortDateTime => {
                DescriptorKind::FloatingPoint
            }
            DataType::Numeric => {
                if format.legacy_numeric_indexes {
                    DescriptorKind::LegacyFixedPoint
                } else {
                    DescriptorKind::FixedPoint
                }
            }
            DataType::Byte => DescriptorKind::Byte,
            DataType::Boolean => DescriptorKind::Boolean,
            DataType::Guid => DescriptorKind::Guid,
            DataType::Binary => DescriptorKind::Binary,
            DataType::ExtDateTime => DescriptorKind::ExtDate,
            _ => DescriptorKind::ReadOnly,
        };
        ColumnDescriptor {
            column,
            flags,
            kind,
            date_context,
        }
    }

    pub fn is_ascending(&self) -> bool {
        (self.flags & ASCENDING_COLUMN_FLAG) != 0
    }

    /// `ColumnDescriptor.isNullValue` (booleans are never null)
    pub fn is_null_value(&self, value: &Value) -> bool {
        if self.kind == DescriptorKind::Boolean {
            false
        } else {
            value.is_null()
        }
    }

    /// Writes the start flag and the rest of the value (`writeValue`).
    pub fn write_value(
        &self,
        value: &Value,
        bout: &mut Vec<u8>,
        format: &JetFormat,
        charset: Charset,
    ) -> Result<()> {
        if self.is_null_value(value) {
            bout.push(null_entry_flag(self.is_ascending()));
            return Ok(());
        }
        if self.kind == DescriptorKind::Text {
            // the text collation code writes the start flag itself (and applies
            // the descending transformation only to the weights)
            return self.write_non_null_value(value, bout, format, charset);
        }
        bout.push(start_entry_flag(self.is_ascending()));
        self.write_non_null_value(value, bout, format, charset)
    }

    fn write_non_null_value(
        &self,
        value: &Value,
        bout: &mut Vec<u8>,
        format: &JetFormat,
        charset: Charset,
    ) -> Result<()> {
        match self.kind {
            DescriptorKind::Integer => {
                let mut value_bytes =
                    encode_number_column_value(value, &self.column, format, self.date_context)?;
                // - isAsc  => flipFirstBit
                // - !isAsc => flipFirstBit, flipBytes
                flip_first_bit_in_byte(&mut value_bytes, 0);
                if !self.is_ascending() {
                    flip_bytes(&mut value_bytes);
                }
                bout.extend_from_slice(&value_bytes);
            }
            DescriptorKind::FloatingPoint => {
                let mut value_bytes =
                    encode_number_column_value(value, &self.column, format, self.date_context)?;
                // determine if the number is negative by testing the first bit
                let is_negative = (value_bytes[0] & 0x80) != 0;
                // isAsc && !isNeg => flipFirstBit
                // isAsc && isNeg => flipBytes
                // !isAsc && !isNeg => flipFirstBit, flipBytes
                // !isAsc && isNeg => nothing
                if !is_negative {
                    flip_first_bit_in_byte(&mut value_bytes, 0);
                }
                if is_negative == self.is_ascending() {
                    flip_bytes(&mut value_bytes);
                }
                bout.extend_from_slice(&value_bytes);
            }
            DescriptorKind::LegacyFixedPoint => {
                let mut value_bytes =
                    encode_number_column_value(value, &self.column, format, self.date_context)?;
                let is_negative = (value_bytes[0] & 0x80) != 0;
                if is_negative == self.is_ascending() {
                    flip_bytes(&mut value_bytes);
                }
                // reverse the sign byte (after any previous byte flipping)
                value_bytes[0] = if is_negative { 0x00 } else { 0xFF };
                bout.extend_from_slice(&value_bytes);
            }
            DescriptorKind::FixedPoint => {
                let mut value_bytes =
                    encode_number_column_value(value, &self.column, format, self.date_context)?;
                let is_negative = (value_bytes[0] & 0x80) != 0;
                // reverse the sign byte (before any byte flipping)
                value_bytes[0] = 0xFF;
                if is_negative == self.is_ascending() {
                    flip_bytes(&mut value_bytes);
                }
                bout.extend_from_slice(&value_bytes);
            }
            DescriptorKind::Byte => {
                let mut value_bytes =
                    encode_number_column_value(value, &self.column, format, self.date_context)?;
                if !self.is_ascending() {
                    flip_bytes(&mut value_bytes);
                }
                bout.extend_from_slice(&value_bytes);
            }
            DescriptorKind::Boolean => {
                let is_true = column::to_boolean_value(value);
                bout.push(if is_true {
                    if self.is_ascending() {
                        ASC_BOOLEAN_TRUE
                    } else {
                        DESC_BOOLEAN_TRUE
                    }
                } else if self.is_ascending() {
                    ASC_BOOLEAN_FALSE
                } else {
                    DESC_BOOLEAN_FALSE
                });
            }
            DescriptorKind::Text => {
                let text = column::to_char_sequence(value)?;
                let sort_order = self.column.sort_order.unwrap_or(format.default_sort_order);
                crate::indexcodes::write_index_text_value(
                    &text,
                    &sort_order,
                    charset,
                    bout,
                    self.is_ascending(),
                )?;
            }
            DescriptorKind::Guid => {
                let value_bytes =
                    encode_number_column_value(value, &self.column, format, self.date_context)?;
                write_general_binary_entry(&value_bytes, self.is_ascending(), bout);
            }
            DescriptorKind::Binary => {
                let value_bytes = column::to_byte_array(value)?;
                write_general_binary_entry(&value_bytes, self.is_ascending(), bout);
            }
            DescriptorKind::ExtDate => {
                let mut value_bytes =
                    encode_number_column_value(value, &self.column, format, self.date_context)?;
                let mut trailer = ASC_EXT_DATE_TRAILER;
                if !self.is_ascending() {
                    flip_bytes(&mut value_bytes);
                    trailer = desc_ext_date_trailer();
                }
                // first 5 blocks are all value data (42 bytes of data in blocks
                // of 8 bytes separated by '\t')
                let mut val_idx = 0usize;
                for _ in 0..5 {
                    bout.extend_from_slice(&value_bytes[val_idx..val_idx + 8]);
                    bout.push(0x09);
                    val_idx += 8;
                }
                // last two data bytes and then the trailer
                bout.extend_from_slice(&value_bytes[val_idx..val_idx + 2]);
                bout.extend_from_slice(&trailer);
            }
            DescriptorKind::ReadOnly => {
                return Err(Error::Unsupported(format!(
                    "unsupported data type {} for index",
                    self.column.data_type.get_name()
                )))
            }
        }
        Ok(())
    }
}

/// `IndexData.writeGeneralBinaryEntry`
fn write_general_binary_entry(value_bytes: &[u8], is_asc: bool, bout: &mut Vec<u8>) {
    // binary data is written in 8 byte segments with a trailing length byte.
    // The length byte is the amount of valid bytes in the segment (where 9
    // indicates that there is more data _after_ this segment).
    let data_len = value_bytes.len();
    let mut segment_len = data_len;
    let mut pos = 0usize;

    // first, write any intermediate segments
    while segment_len > 8 {
        let mut partial = [0u8; 9];
        partial[..8].copy_from_slice(&value_bytes[pos..pos + 8]);
        if !is_asc {
            // note, we do _not_ flip the length byte for intermediate segments
            flip_bytes(&mut partial[..8]);
        }
        partial[8] = 9;
        bout.extend_from_slice(&partial);
        pos += 8;
        segment_len -= 8;
    }

    // write the last segment (with slightly different rules)
    if segment_len > 0 {
        let mut partial = [0u8; 9];
        partial[..segment_len].copy_from_slice(&value_bytes[pos..pos + segment_len]);
        for b in partial.iter_mut().take(8).skip(segment_len) {
            *b = 0;
        }
        partial[8] = segment_len as u8;
        if !is_asc {
            // note, we _do_ flip the last length byte
            flip_bytes(&mut partial);
        }
        bout.extend_from_slice(&partial);
    }
}

/// Fixed-point conversion of a value: returns `(negative, unscaled)` where
/// `unscaled` is the absolute value scaled by `10^scale`.
fn to_unscaled(value: &Value, scale: i32) -> Result<(bool, i128)> {
    match value {
        Value::Decimal {
            unscaled,
            scale: value_scale,
        } => {
            let diff = scale - *value_scale;
            let mag = unscaled.abs();
            let scaled = if diff > 0 {
                mag.checked_mul(
                    10i128
                        .checked_pow(diff as u32)
                        .ok_or_else(|| Error::Jackcess("numeric value out of range".into()))?,
                )
                .ok_or_else(|| Error::Jackcess("numeric value out of range".into()))?
            } else if diff < 0 {
                let div = 10i128.pow((-diff) as u32);
                if mag % div != 0 {
                    return Err(Error::Jackcess(format!(
                        "Numeric value '{}' out of range",
                        unscaled
                    )));
                }
                mag / div
            } else {
                mag
            };
            Ok((*unscaled < 0, scaled))
        }
        Value::Byte(_)
        | Value::Int(_)
        | Value::Long(_)
        | Value::BigInt(_)
        | Value::Float(_)
        | Value::Double(_) => {
            let d = column::to_number(value)?;
            let scaled = (d * 10f64.powi(scale)).round();
            Ok((scaled < 0.0, scaled.abs() as i128))
        }
        Value::Bool(b) => Ok((*b, if *b { 1 } else { 0 })),
        Value::Text(s) => {
            let t = s.trim();
            let neg = t.starts_with('-');
            let t = t.trim_start_matches(['-', '+']);
            let (int_part, frac_part) = match t.split_once('.') {
                Some((a, b)) => (a, b),
                None => (t, ""),
            };
            let mut digits = String::with_capacity(int_part.len() + frac_part.len());
            digits.push_str(int_part);
            digits.push_str(frac_part);
            let parsed: i128 = digits
                .parse()
                .map_err(|_| Error::Jackcess(format!("invalid numeric value {s}")))?;
            let value_scale = frac_part.len() as i32;
            let diff = scale - value_scale;
            let scaled = if diff > 0 {
                parsed * 10i128.pow(diff as u32)
            } else if diff < 0 {
                parsed / 10i128.pow((-diff) as u32)
            } else {
                parsed
            };
            Ok((neg, scaled))
        }
        _ => Err(Error::Jackcess("cannot convert value to number".into())),
    }
}

/// Converts a value to a number, tagging failures with the column name.
fn col_number(value: &Value, column: &Column) -> Result<f64> {
    column::to_number(value).map_err(|e| {
        Error::Jackcess(format!(
            "{}: column {} ({}) value {:?}",
            e,
            column.name,
            column.data_type.get_name(),
            value
        ))
    })
}

/// `IndexData.encodeNumberColumnValue` - writes the value of the given column
/// type to a byte array in big endian order (the index entry byte order).
fn encode_number_column_value(
    value: &Value,
    column: &Column,
    format: &JetFormat,
    date_context: DateContext,
) -> Result<Vec<u8>> {
    let size = column.fixed_data_size().max(0) as usize;
    let mut out = vec![0u8; size];
    match column.data_type {
        DataType::Byte => {
            let n = col_number(value, column)? as i64;
            out[0] = (n & 0xFF) as u8;
        }
        DataType::Int => {
            let n = col_number(value, column)?;
            let v = to_int_wrapping(n) as i16;
            out[..2].copy_from_slice(&v.to_be_bytes());
        }
        DataType::Long => {
            let n = col_number(value, column)?;
            let v = to_int_wrapping(n) as i32;
            out[..4].copy_from_slice(&v.to_be_bytes());
        }
        DataType::Money => {
            let (neg, unscaled) = to_unscaled(value, 4)?;
            let signed: i128 = if neg { -unscaled } else { unscaled };
            let v = signed as i64;
            out[..8].copy_from_slice(&v.to_be_bytes());
        }
        DataType::ComplexType => {
            let n = col_number(value, column)?;
            let v = to_int_wrapping(n) as i32;
            out[..4].copy_from_slice(&v.to_be_bytes());
        }
        DataType::BigInt => {
            let n = col_number(value, column)?;
            let v = n as i64;
            out[..8].copy_from_slice(&v.to_be_bytes());
        }
        DataType::Float => {
            let v = column::to_number(value)? as f32;
            out[..4].copy_from_slice(&v.to_bits().to_be_bytes());
        }
        DataType::Double => {
            let v = column::to_number(value)?;
            out[..8].copy_from_slice(&v.to_bits().to_be_bytes());
        }
        DataType::ShortDateTime => {
            // `ColumnDescriptor.writeNonNullValue` ->
            // `encodeNumberColumnValue` -> `ColumnImpl.write` ->
            // `writeDateValue` with the database's `DateTimeFactory`, i.e.
            // the same conversion the row writer uses
            let bits = column::date_value_bits_in(value, date_context)?;
            out[..8].copy_from_slice(&bits.to_be_bytes());
        }
        DataType::Numeric => {
            let scale = column.scale as i32;
            let (neg, unscaled) = to_unscaled(value, scale)?;
            out[0] = if neg { 0x80 } else { 0 };
            let mut mag = unscaled;
            for i in (1..size).rev() {
                out[i] = (mag & 0xFF) as u8;
                mag >>= 8;
            }
        }
        DataType::Guid => {
            let bytes = match value {
                Value::Uuid(b) => b.to_vec(),
                Value::Text(s) => column::parse_guid(s)?.to_vec(),
                _ => return Err(Error::Jackcess("invalid GUID value".into())),
            };
            if out.len() < bytes.len() {
                out.resize(bytes.len(), 0);
            }
            out[..bytes.len()].copy_from_slice(&bytes);
        }
        DataType::Binary | DataType::BigBinary => {
            let n = col_number(value, column)?;
            let v = to_int_wrapping(n) as i32;
            out[..4].copy_from_slice(&v.to_be_bytes());
        }
        DataType::Boolean => {
            // booleans are not written by this method
        }
        DataType::Text | DataType::Memo => {
            let num_chars = column.length_in_units(format);
            let bytes = column::encode_text_value(
                value,
                num_chars,
                num_chars,
                true,
                true,
                format.charset,
                format,
            )?;
            out.clear();
            out.extend_from_slice(&bytes);
        }
        _ => {
            return Err(Error::Unsupported(format!(
                "unsupported data type {} for index",
                column.data_type.get_name()
            )))
        }
    }
    Ok(out)
}

/// Java's narrowing `double -> int` conversion (saturating) followed by the
/// wrapping `int -> smaller` conversions.
fn to_int_wrapping(n: f64) -> i64 {
    if n.is_nan() {
        0
    } else if n >= i32::MAX as f64 {
        i32::MAX as i64
    } else if n <= i32::MIN as f64 {
        i32::MIN as i64
    } else {
        (n as i64) as i32 as i64
    }
}

// ---------------------------------------------------------------------------
// index pages
// ---------------------------------------------------------------------------

/// The state of an index page (`DataPageMain` + `DataPageExtra`).
#[derive(Clone, Debug)]
pub struct DataPage {
    pub page_number: i32,
    pub leaf: bool,
    /// depth below the leaves of the index tree, 0 for a leaf page
    pub level: i32,
    pub prev_page_number: i32,
    pub next_page_number: i32,
    pub child_tail_page_number: i32,
    pub total_entry_size: i32,
    pub entry_prefix: Vec<u8>,
    /// sorted collection of the page's (non-tail) index entries
    pub entries: Vec<Entry>,
    /// cached tail entry of the page's entry view (not stored on the page)
    pub child_tail_entry: Option<Entry>,
}

impl DataPage {
    pub fn new(page_number: i32, leaf: bool, level: i32) -> DataPage {
        DataPage {
            page_number,
            leaf,
            level,
            prev_page_number: INVALID_INDEX_PAGE_NUMBER,
            next_page_number: INVALID_INDEX_PAGE_NUMBER,
            child_tail_page_number: INVALID_INDEX_PAGE_NUMBER,
            total_entry_size: 0,
            entry_prefix: Vec::new(),
            entries: Vec::new(),
            child_tail_entry: None,
        }
    }

    /// `NEW_ROOT_DATA_PAGE`
    pub fn new_root() -> DataPage {
        DataPage::new(0, true, 0)
    }

    pub fn has_child_tail(&self) -> bool {
        self.child_tail_page_number != INVALID_INDEX_PAGE_NUMBER
    }

    pub fn is_child_tail_page_number(&self, page_number: i32) -> bool {
        self.child_tail_page_number == page_number
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_view_empty(&self) -> bool {
        self.entries.is_empty() && self.child_tail_entry.is_none()
    }

    pub fn view_len(&self) -> usize {
        self.entries.len()
            + if self.child_tail_entry.is_some() {
                1
            } else {
                0
            }
    }

    pub fn view_get(&self, idx: usize) -> Option<&Entry> {
        if idx < self.entries.len() {
            Some(&self.entries[idx])
        } else if idx == self.entries.len() {
            self.child_tail_entry.as_ref()
        } else {
            None
        }
    }

    pub fn view_last(&self) -> Option<&Entry> {
        match self.child_tail_entry.as_ref() {
            Some(e) => Some(e),
            None => self.entries.last(),
        }
    }

    pub fn find_entry(&self, entry: &Entry) -> i32 {
        binary_search(&self.entries, entry)
    }

    pub fn view_find(&self, entry: &Entry) -> i32 {
        let n = self.view_len();
        let mut low: i64 = 0;
        let mut high: i64 = n as i64 - 1;
        while low <= high {
            let mid = (low + high) / 2;
            let cmp = self.view_get(mid as usize).unwrap().compare_to(entry);
            if cmp < 0 {
                low = mid + 1;
            } else if cmp > 0 {
                high = mid - 1;
            } else {
                return mid as i32;
            }
        }
        -(low as i32) - 1
    }

    /// `EntryListView.add` - never adds the tail entry
    pub fn view_add(&mut self, idx: usize, entry: Entry) -> Result<()> {
        if idx > self.entries.len() {
            return Err(Error::IllegalState(format!(
                "invalid entry index {} (entries {})",
                idx,
                self.entries.len()
            )));
        }
        self.entries.insert(idx, entry);
        Ok(())
    }

    /// `EntryListView.set`
    pub fn view_set(&mut self, idx: usize, entry: Entry) -> Result<Entry> {
        if idx == self.entries.len() && self.child_tail_entry.is_some() {
            let old = self.child_tail_entry.replace(entry).unwrap();
            Ok(old)
        } else if idx < self.entries.len() {
            Ok(std::mem::replace(&mut self.entries[idx], entry))
        } else {
            Err(Error::IllegalState(format!(
                "invalid entry index {} (entries {})",
                idx,
                self.entries.len()
            )))
        }
    }

    /// `EntryListView.remove`
    pub fn view_remove(&mut self, idx: usize) -> Result<Entry> {
        if idx == self.entries.len() && self.child_tail_entry.is_some() {
            Ok(self.child_tail_entry.take().unwrap())
        } else if idx < self.entries.len() {
            Ok(self.entries.remove(idx))
        } else {
            Err(Error::IllegalState(format!(
                "invalid entry index {} (entries {})",
                idx,
                self.entries.len()
            )))
        }
    }

    /// `EntryListView.demoteTail`
    pub fn view_demote_tail(&mut self) -> Result<Entry> {
        match self.child_tail_entry.take() {
            Some(tail) => {
                self.entries.push(tail.clone());
                Ok(tail)
            }
            None => Err(Error::IllegalState("no tail entry to demote".into())),
        }
    }

    /// `EntryListView.promoteTail`
    pub fn view_promote_tail(&mut self) -> Result<Entry> {
        match self.entries.pop() {
            Some(last) => {
                self.child_tail_entry = Some(last.clone());
                Ok(last)
            }
            None => Err(Error::IllegalState("no entry to promote".into())),
        }
    }

    /// `DataPage.getCompressedEntrySize`
    pub fn compressed_entry_size(&self) -> i32 {
        self.total_entry_size - (self.entry_prefix.len() as i32 * (self.entries.len() as i32 - 1))
    }
}

/// `IndexData.getMaxPageEntrySize` / `calcMaxPageEntrySize`
pub fn calc_max_page_entry_size(format: &JetFormat) -> usize {
    let page_data_size =
        format.page_size - (format.offset_index_entry_mask + format.size_index_entry_mask);
    let entry_mask_size = format.size_index_entry_mask * 8;
    page_data_size.min(entry_mask_size)
}

/// `IndexData.writeDataPage(buffer, dataPage, tdefPageNumber, format)`
pub fn write_data_page(
    buffer: &mut ByteBuf,
    data_page: &DataPage,
    tdef_page_number: i32,
    format: &JetFormat,
) -> Result<()> {
    buffer.set_position(0)?;

    buffer.put_u8(if data_page.leaf {
        page_types::LEAF
    } else {
        page_types::INTERMEDIATE
    })?;
    buffer.put_u8(0x01)?; // constant 1 on every page type
    buffer.put_i16(0)?; // free space (updated below)
    buffer.put_i32(tdef_page_number)?;

    buffer.put_i32(0)?; // write stamp
    buffer.put_i32(data_page.prev_page_number)?;
    buffer.put_i32(data_page.next_page_number)?;
    buffer.put_i32(data_page.child_tail_page_number)?;

    let entry_prefix = &data_page.entry_prefix;
    buffer.put_i16(entry_prefix.len() as i16)?; // entry prefix byte count
    buffer.put_u8(data_page.level as u8)?; // level in the index tree

    let mut entry_mask = vec![0u8; format.size_index_entry_mask];
    // first entry includes the prefix
    let mut total_size = entry_prefix.len() as i32;
    for entry in &data_page.entries {
        total_size += entry.size() - entry_prefix.len() as i32;
        let idx = (total_size / 8) as usize;
        if idx >= entry_mask.len() {
            return Err(Error::IllegalState(format!(
                "index page entry mask overflow at entry size {}",
                total_size
            )));
        }
        entry_mask[idx] |= 1 << (total_size % 8);
    }
    buffer.put_bytes(&entry_mask)?;

    // first entry includes the prefix
    buffer.put_bytes(entry_prefix)?;

    for entry in &data_page.entries {
        entry.write(buffer, entry_prefix)?;
    }

    // update free space
    buffer.put_i16_at(
        2,
        (format.page_size as i32 - buffer.position() as i32) as i16,
    )?;
    Ok(())
}

/// Reads an unsigned variable length int at an absolute offset.
pub fn get_unsigned_var_int_at(buffer: &ByteBuf, offset: usize, num_bytes: usize) -> Result<i32> {
    match num_bytes {
        1 => Ok(buffer.get_u8_at(offset)? as i32),
        2 => Ok(buffer.get_u16_at(offset)? as i32),
        3 => crate::byteutil::get3_byte_int_at(buffer, offset, buffer.order()),
        4 => Ok(buffer.get_i32_at(offset)?),
        _ => Err(Error::IllegalArgument(format!(
            "Invalid num bytes {}",
            num_bytes
        ))),
    }
}

/// `IndexData.readDataPage`
pub fn read_data_page(
    buffer: &mut ByteBuf,
    page_number: i32,
    format: &JetFormat,
) -> Result<DataPage> {
    let page_type = buffer.get_u8_at(0)?;
    let is_leaf = match page_type {
        v if v == page_types::LEAF => true,
        v if v == page_types::INTERMEDIATE => false,
        _ => {
            return Err(Error::Jackcess(format!(
                "Unexpected page type {} in index page {}",
                page_type, page_number
            )))
        }
    };

    let level = if format.offset_index_level >= 0 {
        buffer.get_u8_at(format.offset_index_level as usize)? as i32
    } else {
        0
    };

    let entry_prefix_length = get_unsigned_var_int_at(
        buffer,
        format.offset_index_compressed_byte_count,
        format.size_index_compressed_byte_count,
    )?;
    let entry_mask_length = format.size_index_entry_mask;
    let entry_mask_pos = format.offset_index_entry_mask;
    let entry_pos = entry_mask_pos + entry_mask_length;
    let mut last_start = 0usize;
    let mut total_entry_size = 0i32;
    let mut entry_prefix: Option<Vec<u8>> = None;
    let mut entries: Vec<Entry> = Vec::new();

    let mut prev_entry = Entry::first_entry();
    for i in 0..entry_mask_length {
        let entry_mask = buffer.get_u8_at(entry_mask_pos + i)?;
        for j in 0..8 {
            if (entry_mask & (1 << j)) != 0 {
                let length = (i * 8) + j - last_start;
                let start = entry_pos + last_start;
                let (cur_entry_bytes, cur_entry_len) = match &entry_prefix {
                    Some(prefix) => {
                        let mut v = prefix.clone();
                        v.extend_from_slice(&buffer.get_bytes_at(start, length)?);
                        let len = length + prefix.len();
                        (v, len)
                    }
                    None => (buffer.get_bytes_at(start, length)?, length),
                };
                total_entry_size += cur_entry_len as i32;

                let entry = Entry::read_from(&cur_entry_bytes, cur_entry_len, is_leaf)?;
                if prev_entry.compare_to(&entry) >= 0 {
                    return Err(Error::Jackcess(format!(
                        "Unexpected order in index entries, {:?} >= {:?}",
                        prev_entry, entry
                    )));
                }
                entries.push(entry);

                if entries.len() == 1 && entry_prefix_length > 0 {
                    // read any shared entry prefix
                    entry_prefix = Some(buffer.get_bytes_at(start, entry_prefix_length as usize)?);
                }

                last_start += length;
                prev_entry = entries.last().unwrap().clone();
            }
        }
    }

    let mut page = DataPage::new(page_number, is_leaf, level);
    page.entry_prefix = entry_prefix.unwrap_or_default();
    page.total_entry_size = total_entry_size;
    page.entries = entries;
    page.prev_page_number = buffer.get_i32_at(format.offset_prev_index_page)?;
    page.next_page_number = buffer.get_i32_at(format.offset_next_index_page)?;
    page.child_tail_page_number = buffer.get_i32_at(format.offset_child_tail_index_page)?;
    Ok(page)
}

/// Mirrors `IndexData.createRootPageBuffer` / `writeDataPage(NEW_ROOT_DATA_PAGE)`:
/// returns a fresh page buffer holding an empty root index page for a new index.
pub fn create_root_page_buffer(
    ch: &mut PageChannel,
    tdef_page_number: i32,
    format: &JetFormat,
) -> Result<ByteBuf> {
    let mut buffer = ch.create_page_buffer();
    let root = DataPage::new_root();
    write_data_page(&mut buffer, &root, tdef_page_number, format)?;
    Ok(buffer)
}

// ---------------------------------------------------------------------------
// IndexImpl / IndexData
// ---------------------------------------------------------------------------

/// A column of an index plus its sort flags.
#[derive(Clone, Debug)]
pub struct IndexColumn {
    pub column: Column,
    pub flags: u8,
}

impl IndexColumn {
    pub fn is_ascending(&self) -> bool {
        (self.flags & ASCENDING_COLUMN_FLAG) != 0
    }

    /// `Index.Column.getColumn()`: the table column this index column sorts
    /// on.
    pub fn get_column(&self) -> &Column {
        &self.column
    }

    /// `Index.Column.getColumnIndex()`: the position of the column in the row
    /// value arrays (`Column.getColumnIndex` of the referenced column).
    pub fn get_column_index(&self) -> usize {
        self.column.column_index
    }

    /// `Index.Column.getName()`: the name of the referenced column.
    pub fn get_name(&self) -> &str {
        &self.column.name
    }
}

/// `IndexImpl.ForeignKeyReference`: the foreign key information stored in an
/// index definition when referential integrity is enforced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForeignKeyReference {
    pub table_type: u8,
    pub other_index_number: i32,
    pub other_table_page_number: i32,
    pub cascade_updates: bool,
    pub cascade_deletes: bool,
    pub cascade_null: bool,
}

impl ForeignKeyReference {
    pub fn new(
        table_type: u8,
        other_index_number: i32,
        other_table_page_number: i32,
        cascade_updates: bool,
        cascade_deletes: bool,
        cascade_null: bool,
    ) -> ForeignKeyReference {
        ForeignKeyReference {
            table_type,
            other_index_number,
            other_table_page_number,
            cascade_updates,
            cascade_deletes,
            cascade_null,
        }
    }

    pub fn is_primary_table(&self) -> bool {
        self.table_type == IndexImpl::FK_PRIMARY_TABLE_TYPE
    }
}

/// `ForeignKeyReference.toString()`: the `ToStringBuilder.builder()` layout
/// (`ForeignKeyReference[` then one indented `field: value,` line per field),
/// including the platform line separator Java's `System.lineSeparator()`
/// reports.  It appears in `getReferencedIndex`'s reciprocal-reference error
/// message.
impl fmt::Display for ForeignKeyReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let newline = if cfg!(windows) { "\r\n" } else { "\n" };
        write!(
            f,
            "ForeignKeyReference[{nl}  otherIndexNumber: {},{nl}  otherTablePageNum: {},{nl}  \
             isPrimaryTable: {},{nl}  isCascadeUpdates: {},{nl}  isCascadeDeletes: {},{nl}  \
             isCascadeNullOnDelete: {}{nl}]",
            self.other_index_number,
            self.other_table_page_number,
            self.is_primary_table(),
            self.cascade_updates,
            self.cascade_deletes,
            self.cascade_null,
            nl = newline
        )
    }
}

/// `IndexImpl` - the logical index.
#[derive(Clone, Debug)]
pub struct IndexImpl {
    pub index_number: i32,
    pub index_data_number: i32,
    pub name: String,
    pub index_type: u8,
    /// foreign key reference info, if any
    pub reference: Option<ForeignKeyReference>,
}

impl IndexImpl {
    pub const PRIMARY_KEY_INDEX_TYPE: u8 = 1;
    pub const FOREIGN_KEY_INDEX_TYPE: u8 = 2;
    pub const FK_PRIMARY_TABLE_TYPE: u8 = 1;
    pub const FK_SECONDARY_TABLE_TYPE: u8 = 2;
    pub const INVALID_INDEX_NUMBER: i32 = -1;

    pub fn is_primary_key(&self) -> bool {
        self.index_type == Self::PRIMARY_KEY_INDEX_TYPE
    }

    pub fn is_foreign_key(&self) -> bool {
        self.index_type == Self::FOREIGN_KEY_INDEX_TYPE
    }

    /// `Index.getTable()`: the table which owns this index.
    ///
    /// Java returns the reference stored on the index's
    /// [`IndexData`]; a Rust [`IndexImpl`] is a plain value copied out of
    /// [`crate::table::Table::indexes`], so the owning table is passed in and
    /// verified to hold this index (same index number and name).
    pub fn get_table<'a>(&self, table: &'a Table) -> Result<&'a Table> {
        let matches = table.indexes.iter().any(|index| {
            (index.index_number == self.index_number) && index.name.eq_ignore_ascii_case(&self.name)
        });
        if matches {
            Ok(table)
        } else {
            Err(Error::IllegalArgument(format!(
                "Index {} is not an index of table {}",
                self.name, table.name
            )))
        }
    }

    /// `IndexImpl.getIndexData()`: the index data backing this logical index.
    pub fn get_index_data<'a>(&self, table: &'a Table) -> Option<&'a IndexData> {
        table.index_data_for(self)
    }

    /// `Index.getColumns()`: the columns of this index, in index order, with
    /// their sort direction ([`IndexColumn`] is the port of
    /// `Index.Column`/`IndexData.ColumnDescriptor`).
    pub fn get_columns(&self, table: &Table) -> Vec<IndexColumn> {
        self.get_index_data(table)
            .map(|data| data.columns.clone())
            .unwrap_or_default()
    }

    /// `Index.getColumnCount()`.
    pub fn get_column_count(&self, table: &Table) -> usize {
        self.get_index_data(table)
            .map(|data| data.columns.len())
            .unwrap_or(0)
    }

    /// `Index.shouldIgnoreNulls()` (`IndexData.shouldIgnoreNulls`).
    pub fn should_ignore_nulls(&self, table: &Table) -> bool {
        self.get_index_data(table)
            .map(|data| data.should_ignore_nulls())
            .unwrap_or(false)
    }

    /// `Index.isUnique()` (`IndexData.isUnique`, which includes a primary
    /// key).
    pub fn is_unique(&self, table: &Table) -> bool {
        self.get_index_data(table)
            .map(|data| data.is_unique())
            .unwrap_or_else(|| self.is_primary_key())
    }

    /// `Index.isRequired()` (`IndexData.isRequired`).
    pub fn is_required(&self, table: &Table) -> bool {
        self.get_index_data(table)
            .map(|data| data.is_required())
            .unwrap_or(false)
    }

    /// `Index.getProperties()`: this index's property map
    /// (`PropertyMaps.getIndex` of the owning table).
    ///
    /// Java caches the map and creates it on demand; the port returns the map
    /// the database holds, or an empty, named map when there is none.  The
    /// table definition page identifies the table's property group.
    pub fn get_properties(
        &self,
        db: &mut crate::database::Database,
        table_page: i32,
    ) -> Result<crate::property::PropertyMap> {
        let maps = db.get_table_properties(table_page)?;
        Ok(maps.get_index(&self.name).cloned().unwrap_or_else(|| {
            crate::property::PropertyMap::with_name(
                self.name.clone(),
                crate::property::MAP_TYPE_INDEX,
            )
        }))
    }

    /// `Index.getReferencedIndex()`: the index on the other side of the
    /// foreign key reference this index carries, `None` for an index without
    /// one.
    ///
    /// The referenced index lives in another table, so Java reaches it through
    /// the owning table's database (`getTable().getDatabase()`); the port
    /// delegates to [`crate::database::Database::get_referenced_index`], which
    /// reproduces Java's `IOException`s for a missing table/index and for a
    /// broken reciprocal reference.  The owning table name is passed in.
    pub fn get_referenced_index(
        &self,
        db: &mut crate::database::Database,
        table_name: &str,
    ) -> Result<Option<IndexImpl>> {
        db.get_referenced_index(table_name, &self.name)
    }

    /// `Index.newCursor()`: a [`CursorBuilder`](crate::cursor::CursorBuilder)
    /// preset to the owning table and this index
    /// (`getTable().newCursor().setIndex(this)` in Java).
    pub fn new_cursor(&self, table: &Table) -> crate::cursor::CursorBuilder {
        let mut builder = crate::cursor::CursorBuilder::new(&table.name);
        builder.set_index(self);
        builder
    }

    /// Reads an index definition from the table definition buffer.
    pub fn read(buffer: &mut ByteBuf, format: &JetFormat) -> Result<IndexImpl> {
        buffer.advance(format.skip_before_index_slot as i64)?;
        let index_number = buffer.get_i32()?;
        let index_data_number = buffer.get_i32()?;
        let rel_index_type = buffer.get_u8()?;
        let rel_index_number = buffer.get_i32()?;
        let rel_table_page_number = buffer.get_i32()?;
        let cascade_updates_flag = buffer.get_u8()?;
        let cascade_deletes_flag = buffer.get_u8()?;
        let index_type = buffer.get_u8()?;
        buffer.advance(format.skip_after_index_slot as i64)?;
        let reference = if (index_type == Self::FOREIGN_KEY_INDEX_TYPE)
            && (rel_index_number != Self::INVALID_INDEX_NUMBER)
        {
            Some(ForeignKeyReference::new(
                rel_index_type,
                rel_index_number,
                rel_table_page_number,
                (cascade_updates_flag & 1) != 0,
                (cascade_deletes_flag & 1) != 0,
                (cascade_deletes_flag & 2) != 0,
            ))
        } else {
            None
        };
        Ok(IndexImpl {
            index_number,
            index_data_number,
            name: String::new(),
            index_type,
            reference,
        })
    }

    /// Writes a logical index definition (`IndexImpl.writeDefinition`).
    ///
    /// A foreign key index carries the reference of the *other* side of the
    /// relationship instead of the two zero flag bytes.
    pub fn write(
        buffer: &mut ByteBuf,
        index_number: i32,
        index_data_number: i32,
        index_type: u8,
        reference: Option<&ForeignKeyReference>,
    ) -> Result<()> {
        buffer.put_i32(crate::table::MAGIC_TABLE_NUMBER)?;
        buffer.put_i32(index_number)?;
        buffer.put_i32(index_data_number)?;
        if index_type != Self::FOREIGN_KEY_INDEX_TYPE {
            buffer.put_u8(0)?; // related table type
            buffer.put_i32(Self::INVALID_INDEX_NUMBER)?; // related index num
            buffer.put_i32(0)?; // related table definition page number
            buffer.put_u8(0)?; // cascade updates flag
            buffer.put_u8(0)?; // cascade deletes flag
        } else {
            let reference = reference.ok_or_else(|| {
                Error::IllegalArgument("foreign key index needs a foreign key reference".into())
            })?;
            buffer.put_u8(reference.table_type)?;
            buffer.put_i32(reference.other_index_number)?;
            buffer.put_i32(reference.other_table_page_number)?;
            let mut update_flags = 0u8;
            if reference.cascade_updates {
                update_flags |= 1;
            }
            let mut delete_flags = 0u8;
            if reference.cascade_deletes {
                delete_flags |= 1;
            }
            if reference.cascade_null {
                delete_flags |= 2;
            }
            buffer.put_u8(update_flags)?;
            buffer.put_u8(delete_flags)?;
        }
        buffer.put_u8(index_type)?;
        buffer.put_i32(0)?;
        Ok(())
    }
}

/// The state of a prepared (but not yet committed) index entry addition, the
/// port of `IndexData.AddRowPendingChange`.
pub struct PendingAdd {
    data_page: i32,
    idx: usize,
    entry: Entry,
    is_dupe_entry: bool,
}

/// Describes an index backed by index data, built while reading a tdef.
#[derive(Clone, Debug)]
pub struct IndexData {
    pub number: i32,
    pub unique_entry_count: i32,
    pub unique_entry_count_offset: usize,
    pub columns: Vec<IndexColumn>,
    pub owned_pages: Option<UsageMap>,
    pub root_page_number: i32,
    pub index_flags: u8,
    pub logical_index_numbers: Vec<i32>,
    pub primary_key: bool,
    pub is_complex: bool,
    /// initialized page cache (filled by the B-tree implementation)
    pub page_cache: Option<crate::indexpagecache::IndexPageCache>,
    /// the page number of the table definition which owns this index (stored
    /// in the header of every index page)
    pub tdef_page_number: i32,
    /// format of the database this index belongs to (set by `create`/`read`)
    pub format: Option<JetFormat>,
    /// `true` if this index cannot be written (unsupported column type)
    pub read_only: bool,
    /// reason for a non-VALID index status
    pub unsupported_reason: String,
    /// The database's date/time conversion context
    /// (`TableImpl.getDatabase().getDateTimeFactory()` + `getTimeZone()`
    /// through the owning table).  It selects how a caller supplied
    /// [`Value::DateTime`] becomes a `SHORT_DATE_TIME` index key: the index
    /// entry encoder is the same `ColumnImpl.write` the row writer uses, so
    /// under `DateTimeType.DATE` a sub-millisecond `LocalDateTime` key is
    /// truncated to the millisecond exactly like the row bytes.
    pub date_context: DateContext,
}

impl IndexData {
    pub fn new(
        number: i32,
        unique_entry_count: i32,
        unique_entry_count_offset: usize,
    ) -> IndexData {
        IndexData {
            number,
            unique_entry_count,
            unique_entry_count_offset,
            columns: Vec::new(),
            owned_pages: None,
            root_page_number: 0,
            index_flags: 0,
            logical_index_numbers: Vec::new(),
            primary_key: false,
            is_complex: false,
            page_cache: None,
            tdef_page_number: 0,
            format: None,
            read_only: false,
            unsupported_reason: String::new(),
            date_context: DateContext::default(),
        }
    }

    /// Sets the database's date/time conversion context (see
    /// [`IndexData::date_context`]).  The owning [`crate::table::Table`] is
    /// kept in sync by `Table::set_date_time_type`/`set_time_zone`.
    pub fn set_date_context(&mut self, date_context: DateContext) {
        self.date_context = date_context;
    }

    /// Updates the [`DateTimeType`] half of [`IndexData::date_context`].
    pub fn set_date_time_type(&mut self, date_time_type: DateTimeType) {
        self.date_context.date_time_type = date_time_type;
    }

    /// Updates the time zone half of [`IndexData::date_context`].
    pub fn set_time_zone(&mut self, time_zone: TimeZone) {
        self.date_context.time_zone = time_zone;
    }

    /// `IndexData.create` - reads the unique entry count.
    pub fn create(table_buffer: &ByteBuf, number: i32, format: &JetFormat) -> Result<IndexData> {
        let offset =
            format.offset_index_def_block + number as usize * format.size_index_definition + 4;
        let unique_entry_count = table_buffer.get_i32_at(offset)?;
        let mut data = IndexData::new(number, unique_entry_count, offset);
        data.format = Some(format.clone());
        Ok(data)
    }

    /// `IndexData.read` - reads the column list, usage map, root page and flags.
    pub fn read(
        &mut self,
        ch: &mut PageChannel,
        table_buffer: &mut ByteBuf,
        available_columns: &[Column],
        format: &JetFormat,
    ) -> Result<()> {
        self.format = Some(format.clone());
        table_buffer.advance(format.skip_before_index as i64)?;
        for _ in 0..MAX_COLUMNS {
            let column_number = table_buffer.get_i16()?;
            let col_flags = table_buffer.get_u8()?;
            if column_number != COLUMN_UNUSED {
                let col = available_columns
                    .iter()
                    .find(|c| c.column_number == column_number)
                    .ok_or_else(|| {
                        Error::Jackcess(format!(
                            "Could not find column with number {} for index",
                            column_number
                        ))
                    })?;
                self.columns.push(IndexColumn {
                    column: col.clone(),
                    flags: col_flags,
                });
            }
        }
        self.owned_pages = Some(UsageMap::read_ptr(ch, table_buffer)?);
        self.root_page_number = table_buffer.get_i32()?;
        table_buffer.advance(format.skip_before_index_flags as i64)?;
        self.index_flags = table_buffer.get_u8()?;
        table_buffer.advance(format.skip_after_index_flags as i64)?;

        // determine whether any of the columns make the index read-only
        for col in &self.columns {
            let desc = ColumnDescriptor::with_date_context(
                col.column.clone(),
                col.flags,
                format,
                self.date_context,
            );
            if desc.kind == DescriptorKind::ReadOnly {
                self.read_only = true;
                self.unsupported_reason = format!(
                    "unsupported data type {} for index",
                    col.column.data_type.get_name()
                );
            }
        }
        Ok(())
    }

    pub fn should_ignore_nulls(&self) -> bool {
        (self.index_flags & IGNORE_NULLS_INDEX_FLAG) != 0
    }

    pub fn is_unique(&self) -> bool {
        self.primary_key || (self.index_flags & UNIQUE_INDEX_FLAG) != 0
    }

    pub fn is_required(&self) -> bool {
        (self.index_flags & REQUIRED_INDEX_FLAG) != 0
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn get_max_page_entry_size(format: &JetFormat) -> usize {
        calc_max_page_entry_size(format)
    }

    /// The page number of the table definition which owns this index.
    pub fn set_tdef_page_number(&mut self, page_number: i32) {
        self.tdef_page_number = page_number;
        if let Some(cache) = self.page_cache.as_mut() {
            cache.tdef_page_number = page_number;
        }
    }

    /// Initializes the page cache (parses the index pages).
    pub fn initialize(&mut self, ch: &mut PageChannel, format: &JetFormat) -> Result<()> {
        self.format = Some(format.clone());
        if self.page_cache.is_none() {
            let mut cache = crate::indexpagecache::IndexPageCache::new(
                self.root_page_number,
                self.owned_pages.clone(),
                format.clone(),
            );
            cache.tdef_page_number = self.tdef_page_number;
            cache.initialize(ch)?;
            self.page_cache = Some(cache);
        }
        Ok(())
    }

    /// The index of a column's value in a table row array.  A table's columns
    /// are sorted by persisted column number and re-indexed, so the persisted
    /// column number is the row array position (for the sequential column
    /// numbers Access writes they are the same thing).
    pub(crate) fn row_value_index(col: &Column) -> usize {
        col.column_index
    }

    /// `IndexData.countNullValues`
    fn count_null_values(&self, values: &[Value]) -> usize {
        let mut null_count = 0;
        for col in &self.columns {
            // booleans are never null values for index purposes
            if col.column.data_type == DataType::Boolean {
                continue;
            }
            let idx = IndexData::row_value_index(&col.column);
            if values.get(idx).map(|v| v.is_null()).unwrap_or(true) {
                null_count += 1;
            }
        }
        null_count
    }

    /// `IndexData.createEntryBytes`
    pub fn create_entry_bytes(
        &self,
        values: &[Value],
        format: &JetFormat,
        charset: Charset,
    ) -> Result<Vec<u8>> {
        let mut entry_buffer: Vec<u8> = Vec::new();
        for col in &self.columns {
            let idx = IndexData::row_value_index(&col.column);
            let value = values.get(idx).unwrap_or(&Value::Null);
            let desc = ColumnDescriptor::with_date_context(
                col.column.clone(),
                col.flags,
                format,
                self.date_context,
            );
            desc.write_value(value, &mut entry_buffer, format, charset)?;
        }
        let max_len = MAX_KEY_UNITS * format.size_text_field_unit;
        Ok(truncate_entry_bytes(&entry_buffer, max_len))
    }

    /// `IndexData.createEntryBytes` for values which are already in index
    /// column order.  Used to build the search key of an index lookup from the
    /// values of another table's row (the referential integrity check of
    /// `RelationshipCreator.validate`).
    pub fn create_entry_bytes_ordered(
        &self,
        values: &[Value],
        format: &JetFormat,
        charset: Charset,
    ) -> Result<Vec<u8>> {
        let mut entry_buffer: Vec<u8> = Vec::new();
        for (i, col) in self.columns.iter().enumerate() {
            let value = values.get(i).unwrap_or(&Value::Null);
            let desc = ColumnDescriptor::with_date_context(
                col.column.clone(),
                col.flags,
                format,
                self.date_context,
            );
            desc.write_value(value, &mut entry_buffer, format, charset)?;
        }
        let max_len = MAX_KEY_UNITS * format.size_text_field_unit;
        Ok(truncate_entry_bytes(&entry_buffer, max_len))
    }

    /// Prepares a row addition (`IndexData.prepareAddRow`): all constraints
    /// are checked and the insertion point reserved, but the entry is not yet
    /// written.  `None` means there is nothing to add (an ignored all-null
    /// entry or an entry which already exists).
    pub fn prepare_add_row(
        &mut self,
        ch: &mut PageChannel,
        row: &[Value],
        row_id: (i32, i32),
        format: &JetFormat,
    ) -> Result<Option<PendingAdd>> {
        let null_count = self.count_null_values(row);
        let is_null_entry = null_count == self.columns.len();
        if self.should_ignore_nulls() && is_null_entry {
            // nothing to do
            return Ok(None);
        }
        if null_count > 0 && (self.primary_key || self.is_required()) {
            return Err(Error::Jackcess(format!(
                "Null value found in row {:?} for primary key or required index",
                row
            )));
        }

        // make sure we've parsed the entries
        self.initialize(ch, format)?;

        let entry_bytes = self.create_entry_bytes(row, format, ch.charset)?;
        let new_entry = Entry::leaf(entry_bytes, RowId::new(row_id.0, row_id.1));

        let (data_page, idx) = {
            let cache = self
                .page_cache
                .as_mut()
                .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
            let data_page = cache.find_cache_data_page(ch, &new_entry)?;
            let idx = cache.find_entry(ch, data_page, &new_entry)?;
            (data_page, idx)
        };

        if idx >= 0 {
            // a duplicate entry: nothing to add
            return Ok(None);
        }

        let idx = missing_index_to_insertion_point(idx);
        let (next_entry, prev_entry) = {
            let cache = self
                .page_cache
                .as_mut()
                .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
            let next = cache.get_next_position(ch, data_page, idx as usize, true)?;
            let prev = cache.get_previous_position(ch, data_page, idx as usize)?;
            (next, prev)
        };

        let is_dupe_entry = next_entry
            .as_ref()
            .map(|e| new_entry.equals_entry_bytes(e))
            .unwrap_or(false)
            || prev_entry
                .as_ref()
                .map(|e| new_entry.equals_entry_bytes(e))
                .unwrap_or(false);

        if self.is_unique() && !is_null_entry && is_dupe_entry {
            return Err(Error::Jackcess(format!(
                "New row {:?} violates uniqueness constraint for index",
                row
            )));
        }

        Ok(Some(PendingAdd {
            data_page,
            idx: idx as usize,
            entry: new_entry,
            is_dupe_entry,
        }))
    }

    /// Completes a prepared row addition (`IndexData.commitAddRow`).
    ///
    /// `replaced_entry` mirrors the `oldEntry != null` condition of an update
    /// (which removed the previous entry): jackcess only increments the unique
    /// entry count for a brand new entry, never for one which replaces the
    /// row's old entry.
    pub fn commit_add_row(
        &mut self,
        ch: &mut PageChannel,
        pending: PendingAdd,
        replaced_entry: bool,
    ) -> Result<()> {
        {
            let cache = self
                .page_cache
                .as_mut()
                .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
            cache.add_entry(
                ch,
                &mut self.owned_pages,
                pending.data_page,
                pending.idx,
                pending.entry,
            )?;
        }
        if !pending.is_dupe_entry && !replaced_entry {
            self.unique_entry_count += 1;
        }
        Ok(())
    }

    /// Adds a row's index entry, checking constraints.
    pub fn add_row(
        &mut self,
        ch: &mut PageChannel,
        row: &[Value],
        row_id: (i32, i32),
        format: &JetFormat,
    ) -> Result<()> {
        if let Some(pending) = self.prepare_add_row(ch, row, row_id, format)? {
            self.commit_add_row(ch, pending, false)?;
        }
        Ok(())
    }

    /// Removes a row's index entry (`IndexData.removeEntry`): the entry is
    /// located by its key bytes, falling back to the RowId when only a partial
    /// entry is available.
    fn remove_entry(&mut self, ch: &mut PageChannel, old_entry: &Entry) -> Result<Option<Entry>> {
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        let data_page = cache.find_cache_data_page(ch, old_entry)?;
        let idx = cache.find_entry(ch, data_page, old_entry)?;
        if idx < 0 {
            // the caller may have only read some of the row data, search for
            // the page/row numbers instead
            let mut pn = data_page;
            loop {
                let n = cache.page_entries_len(pn)?;
                let mut found = None;
                for i in 0..n {
                    if cache
                        .page_entry(pn, i)?
                        .get_row_id()
                        .eq(old_entry.get_row_id())
                    {
                        found = Some((pn, i));
                        break;
                    }
                }
                if let Some((p, i)) = found {
                    return cache.remove_entry(ch, &mut self.owned_pages, p, i);
                }
                let next = cache.page_next(ch, pn)?;
                match next {
                    Some(p) => pn = p,
                    None => return Ok(None),
                }
            }
        }
        cache.remove_entry(ch, &mut self.owned_pages, data_page, idx as usize)
    }

    /// Removes a row's index entry, returning the removed entry so that it can
    /// be restored if the rest of an update fails
    /// (`IndexData.deleteRowImpl`).
    pub fn delete_row_entry(
        &mut self,
        ch: &mut PageChannel,
        row: &[Value],
        row_id: (i32, i32),
        format: &JetFormat,
    ) -> Result<Option<Entry>> {
        let null_count = self.count_null_values(row);
        if self.should_ignore_nulls() && null_count == self.columns.len() {
            // nothing to do
            return Ok(None);
        }
        // make sure we've parsed the entries
        self.initialize(ch, format)?;
        let entry_bytes = self.create_entry_bytes(row, format, ch.charset)?;
        let old_entry = Entry::leaf(entry_bytes, RowId::new(row_id.0, row_id.1));
        self.remove_entry(ch, &old_entry)
    }

    /// Removes a row's index entry (`IndexData.deleteRow`).
    pub fn delete_row(
        &mut self,
        ch: &mut PageChannel,
        row: &[Value],
        row_id: (i32, i32),
        format: &JetFormat,
    ) -> Result<bool> {
        Ok(self.delete_row_entry(ch, row, row_id, format)?.is_some())
    }

    /// Restores an entry removed by [`IndexData::delete_row_entry`]
    /// (`IndexData.rollbackDeletedRow`).  The page may have been restructured
    /// by the removal, so the entry is located again before it is re-inserted.
    pub fn rollback_deleted_row(&mut self, ch: &mut PageChannel, removed: Entry) -> Result<()> {
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        let data_page = cache.find_cache_data_page(ch, &removed)?;
        let idx = cache.find_entry(ch, data_page, &removed)?;
        if idx < 0 {
            cache.add_entry(
                ch,
                &mut self.owned_pages,
                data_page,
                missing_index_to_insertion_point(idx) as usize,
                removed,
            )?;
        }
        Ok(())
    }

    /// Writes pending index page changes to the database.
    pub fn update(&mut self, ch: &mut PageChannel) -> Result<()> {
        let format = ch.format.clone();
        self.initialize(ch, &format)?;

        if self.is_read_only() {
            return Err(Error::Unsupported(format!(
                "Cannot write indexes of this type due to {}",
                self.unsupported_reason
            )));
        }
        if let Some(cache) = self.page_cache.as_mut() {
            cache.write(ch, &mut self.owned_pages)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // index entry lookups (`IndexData.EntryCursor` / `IndexCursor`)
    // -----------------------------------------------------------------

    /// `IndexData.cursor()` with no bounds, positioned by
    /// `EntryCursor.beforeEntry(row)`: returns the first entry whose key is
    /// greater than or equal to the given values (which must be in index
    /// column order, like [`IndexData::create_entry_bytes_ordered`]), or
    /// `None` when the key sorts after every entry (the cursor's "end" entry,
    /// whose row id is invalid).
    pub fn find_first_entry(
        &mut self,
        ch: &mut PageChannel,
        values: &[Value],
    ) -> Result<Option<(EntryPos, Entry)>> {
        let format = ch.format.clone();
        let charset = ch.charset;
        self.initialize(ch, &format)?;
        let entry_bytes = self.create_entry_bytes_ordered(values, &format, charset)?;

        // the search entry is the "first row id" entry, so it always sorts
        // before an existing entry with the same bytes (`EntryCursor
        // .beforeEntry`)
        let search = Entry::leaf(entry_bytes, RowId::FIRST);
        let (data_page, idx) = {
            let cache = self
                .page_cache
                .as_mut()
                .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
            let data_page = cache.find_cache_data_page(ch, &search)?;
            let idx = cache.find_entry(ch, data_page, &search)?;
            (data_page, idx)
        };
        let (page, idx, between) = if idx < 0 {
            (
                data_page,
                missing_index_to_insertion_point(idx) as usize,
                true,
            )
        } else {
            (data_page, idx as usize, false)
        };

        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.get_next_entry_position(ch, page, idx, between)? {
            Some((page, idx, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// `EntryCursor.getNextEntry`: the entry after the given position, or
    /// `None` at the end of the index.
    pub fn next_entry(
        &mut self,
        ch: &mut PageChannel,
        pos: EntryPos,
    ) -> Result<Option<(EntryPos, Entry)>> {
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.get_next_entry_position(ch, pos.page_number, pos.idx, false)? {
            Some((page, idx, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// Builds the search key of a partial (prefix) lookup, where the trailing
    /// index columns use the `IndexData.MIN_VALUE`/`MAX_VALUE` sentinels
    /// (`IndexData.createEntryBytes`).  A `Min` writes the ascending null flag
    /// and a `Max` the descending null flag, which always sort before/after
    /// every real entry value, independent of the column's ascending flag.
    pub fn create_entry_bytes_partial(
        &self,
        values: &[EntryValue],
        format: &JetFormat,
        charset: Charset,
    ) -> Result<Vec<u8>> {
        let mut entry_buffer: Vec<u8> = Vec::new();
        for (i, col) in self.columns.iter().enumerate() {
            match values.get(i) {
                Some(EntryValue::Min) | None => entry_buffer.push(ASC_NULL_FLAG),
                Some(EntryValue::Max) => entry_buffer.push(DESC_NULL_FLAG),
                Some(EntryValue::Value(value)) => {
                    let desc = ColumnDescriptor::with_date_context(
                        col.column.clone(),
                        col.flags,
                        format,
                        self.date_context,
                    );
                    desc.write_value(value, &mut entry_buffer, format, charset)?;
                }
            }
        }
        let max_len = MAX_KEY_UNITS * format.size_text_field_unit;
        Ok(truncate_entry_bytes(&entry_buffer, max_len))
    }

    /// Finds the entry at or after the given search entry, returning its
    /// position and the entry (`EntryCursor.beforeEntry` + `getNextEntry`).
    ///
    /// This is also the movement a positioned `EntryCursor` performs
    /// (`getNextEntry`): for an entry which exists in the index it returns the
    /// next entry, and for one which does not (the range boundary entries with
    /// an invalid row id, or [`Entry::first_entry`]/[`Entry::last_entry`]) it
    /// returns the entry at the insertion point.  The index cursor uses it for
    /// both cases, which is why it is public.
    pub fn find_entry_at_or_after(
        &mut self,
        ch: &mut PageChannel,
        search: &Entry,
    ) -> Result<Option<(EntryPos, Entry)>> {
        let (data_page, idx) = self.locate_entry(ch, search)?;
        // `getNextEntryPosition` with `between` set to true returns the entry
        // at the insertion point (the first entry >= the search entry).
        let idx = if idx < 0 {
            missing_index_to_insertion_point(idx) as usize
        } else {
            idx as usize
        };
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.get_next_entry_position(ch, data_page, idx, true)? {
            Some((page, i, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx: i,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// Finds the entry at or before the given search entry, returning its
    /// position and the entry (`EntryCursor.afterEntry` + `getPreviousEntry`).
    ///
    /// Like [`IndexData::find_entry_at_or_after`] this is also the movement of
    /// a positioned `EntryCursor` (`getPreviousEntry`): the entry before the
    /// position of the given entry, or the last entry before the insertion
    /// point for an entry which does not exist.
    pub fn find_entry_at_or_before(
        &mut self,
        ch: &mut PageChannel,
        search: &Entry,
    ) -> Result<Option<(EntryPos, Entry)>> {
        let (data_page, idx) = self.locate_entry(ch, search)?;
        let idx = if idx < 0 {
            missing_index_to_insertion_point(idx) as usize
        } else {
            idx as usize
        };
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.get_previous_entry_position(ch, data_page, idx)? {
            Some((page, i, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx: i,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// `EntryCursor.getNextEntry` from the position of the given entry: the
    /// entry after it, or the entry at the insertion point when the given entry
    /// does not exist in the index.
    ///
    /// This is the movement of the positioned entry cursor
    /// (`IndexData.getNextPosition`) and differs from
    /// [`IndexData::find_entry_at_or_after`] only for an entry which does exist:
    /// it then returns the *next* entry instead of the entry itself.  The index
    /// cursor needs both, which is why this is public.
    pub fn get_next_entry_from(
        &mut self,
        ch: &mut PageChannel,
        entry: &Entry,
    ) -> Result<Option<(EntryPos, Entry)>> {
        let (data_page, idx) = self.locate_entry(ch, entry)?;
        let (idx, between) = if idx < 0 {
            (missing_index_to_insertion_point(idx) as usize, true)
        } else {
            (idx as usize, false)
        };
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.get_next_entry_position(ch, data_page, idx, between)? {
            Some((page, i, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx: i,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// `EntryCursor.getPreviousEntry` from the position of the given entry: the
    /// entry before it, or the entry before the insertion point when the given
    /// entry does not exist in the index.
    ///
    /// The backward counterpart of [`IndexData::get_next_entry_from`].
    pub fn get_previous_entry_from(
        &mut self,
        ch: &mut PageChannel,
        entry: &Entry,
    ) -> Result<Option<(EntryPos, Entry)>> {
        let (data_page, idx) = self.locate_entry(ch, entry)?;
        let idx = if idx < 0 {
            missing_index_to_insertion_point(idx) as usize
        } else {
            idx as usize
        };
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.get_previous_entry_position(ch, data_page, idx)? {
            Some((page, i, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx: i,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// The position of the given entry on its index page (which may be
    /// negative for a missing entry, in `DataPage.findEntry` form).
    fn locate_entry(&mut self, ch: &mut PageChannel, entry: &Entry) -> Result<(i32, i32)> {
        self.initialize(ch, &ch.format.clone())?;
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        let data_page = cache.find_cache_data_page(ch, entry)?;
        let idx = cache.find_entry(ch, data_page, entry)?;
        Ok((data_page, idx))
    }

    /// `EntryCursor.beforeEntry` + `getNextEntry` for a partial index row: the
    /// first entry whose key is greater than or equal to the given values.
    pub fn find_first_entry_partial(
        &mut self,
        ch: &mut PageChannel,
        values: &[EntryValue],
    ) -> Result<Option<(EntryPos, Entry)>> {
        let format = ch.format.clone();
        let charset = ch.charset;
        self.initialize(ch, &format)?;
        let entry_bytes = self.create_entry_bytes_partial(values, &format, charset)?;
        // the search entry is the first row id, so it always sorts before an
        // existing entry with the same bytes (`EntryCursor.beforeEntry`)
        let search = Entry::leaf(entry_bytes, RowId::FIRST);
        self.find_entry_at_or_after(ch, &search)
    }

    /// `EntryCursor.afterEntry` + `getPreviousEntry` for a partial index row:
    /// the last entry whose key is less than or equal to the given values.
    pub fn find_previous_entry_partial(
        &mut self,
        ch: &mut PageChannel,
        values: &[EntryValue],
    ) -> Result<Option<(EntryPos, Entry)>> {
        let format = ch.format.clone();
        let charset = ch.charset;
        self.initialize(ch, &format)?;
        let entry_bytes = self.create_entry_bytes_partial(values, &format, charset)?;
        // the search entry is the last row id, so it always sorts after an
        // existing entry with the same bytes (`EntryCursor.afterEntry`)
        let search = Entry::leaf(entry_bytes, RowId::LAST);
        self.find_entry_at_or_before(ch, &search)
    }

    /// The entry after the entry with the given key and row id
    /// (`EntryCursor.getNextEntry` from a saved position).
    pub fn find_entry_after(
        &mut self,
        ch: &mut PageChannel,
        entry_bytes: &[u8],
        row_id: RowId,
    ) -> Result<Option<(EntryPos, Entry)>> {
        let search = Entry::leaf(entry_bytes.to_vec(), row_id);
        let (data_page, idx) = self.locate_entry(ch, &search)?;
        let (page, idx, between) = if idx < 0 {
            (
                data_page,
                missing_index_to_insertion_point(idx) as usize,
                true,
            )
        } else {
            (data_page, idx as usize, false)
        };
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.get_next_entry_position(ch, page, idx, between)? {
            Some((page, i, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx: i,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// The entry before the entry with the given key and row id
    /// (`EntryCursor.getPreviousEntry` from a saved position).
    pub fn find_entry_before(
        &mut self,
        ch: &mut PageChannel,
        entry_bytes: &[u8],
        row_id: RowId,
    ) -> Result<Option<(EntryPos, Entry)>> {
        let search = Entry::leaf(entry_bytes.to_vec(), row_id);
        let (data_page, idx) = self.locate_entry(ch, &search)?;
        let idx = if idx < 0 {
            missing_index_to_insertion_point(idx) as usize
        } else {
            idx as usize
        };
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.get_previous_entry_position(ch, data_page, idx)? {
            Some((page, i, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx: i,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// The entry after the given position (`EntryCursor.getNextEntry`).
    pub fn entry_after(
        &mut self,
        ch: &mut PageChannel,
        pos: EntryPos,
    ) -> Result<Option<(EntryPos, Entry)>> {
        self.next_entry(ch, pos)
    }

    /// The entry before the given position (`EntryCursor.getPreviousEntry`).
    pub fn entry_before(
        &mut self,
        ch: &mut PageChannel,
        pos: EntryPos,
    ) -> Result<Option<(EntryPos, Entry)>> {
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.get_previous_entry_position(ch, pos.page_number, pos.idx)? {
            Some((page, i, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx: i,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// The first entry of the index (the beginning of a forward iteration),
    /// or `None` for an empty index.
    pub fn first_entry(&mut self, ch: &mut PageChannel) -> Result<Option<(EntryPos, Entry)>> {
        self.initialize(ch, &ch.format.clone())?;
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.find_first_entry_position(ch)? {
            Some((page, i, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx: i,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// The last entry of the index (the beginning of a reverse iteration), or
    /// `None` for an empty index.
    pub fn last_entry(&mut self, ch: &mut PageChannel) -> Result<Option<(EntryPos, Entry)>> {
        self.initialize(ch, &ch.format.clone())?;
        let cache = self
            .page_cache
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index page cache not initialized".into()))?;
        match cache.find_last_entry_position(ch)? {
            Some((page, i, entry)) => Ok(Some((
                EntryPos {
                    page_number: page,
                    idx: i,
                },
                entry,
            ))),
            None => Ok(None),
        }
    }

    /// The row ids of the entries which share the key of the first entry at or
    /// after the given values: the run of equal entry bytes
    /// `IndexCursorImpl.findMatchingRowInRun` walks.  The values still have to
    /// be compared against the rows themselves (the entry bytes of a text key
    /// fold case, and a long key is truncated), which is what the enforcer
    /// does.
    ///
    /// An empty result means the key sorts after every entry, or the index
    /// holds no entries.
    pub fn find_rows_by_entry(
        &mut self,
        ch: &mut PageChannel,
        values: &[Value],
    ) -> Result<Vec<RowId>> {
        let mut rows = Vec::new();
        let (mut pos, first) = match self.find_first_entry(ch, values)? {
            Some(found) => found,
            None => return Ok(rows),
        };
        let run_bytes = first.get_entry_bytes().map(|b| b.to_vec());
        rows.push(*first.get_row_id());
        while let Some((next_pos, entry)) = self.next_entry(ch, pos)? {
            if entry.get_entry_bytes() != run_bytes.as_deref() {
                break;
            }
            rows.push(*entry.get_row_id());
            pos = next_pos;
        }
        Ok(rows)
    }
}

/// The position of an index entry: the page holding it and its index within
/// that page's entry list (`IndexData.Position`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryPos {
    pub page_number: i32,
    pub idx: usize,
}

/// A value used to build an index lookup key: either a real column value or one
/// of the `IndexData.MIN_VALUE`/`IndexData.MAX_VALUE` sentinels jackcess uses
/// for partial (prefix) lookups.
///
/// `Min` sorts before every real entry value and `Max` sorts after every real
/// entry value, independent of the column's ascending/descending flag (the
/// entry bytes are always interpreted least to greatest).
#[derive(Clone, Debug, PartialEq)]
pub enum EntryValue {
    Value(Value),
    Min,
    Max,
}

impl EntryValue {
    /// Wraps a row value.
    pub fn from_value(value: &Value) -> EntryValue {
        EntryValue::Value(value.clone())
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datenums::LocalDateTime;
    use crate::format::JetFormat;

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    /// `ForeignKeyReference.toString()` is `ToStringBuilder.builder()`'s
    /// layout, field per indented line, and appears in the
    /// `getReferencedIndex` reciprocal-reference error message.
    #[test]
    fn foreign_key_reference_to_string_matches_java() {
        let reference = ForeignKeyReference::new(2, 3, 41, true, false, true);
        let newline = if cfg!(windows) { "\r\n" } else { "\n" };
        assert_eq!(
            reference.to_string(),
            format!(
                "ForeignKeyReference[{nl}  otherIndexNumber: 3,{nl}  \
                 otherTablePageNum: 41,{nl}  isPrimaryTable: false,{nl}  \
                 isCascadeUpdates: true,{nl}  isCascadeDeletes: false,{nl}  \
                 isCascadeNullOnDelete: true{nl}]",
                nl = newline
            )
        );
    }

    fn column(name: &str, data_type: DataType, column_number: i16, length: i16) -> Column {
        let mut c = Column {
            name: name.to_string(),
            data_type,
            column_number,
            column_id: column_number,
            column_length: length,
            variable_length: data_type.is_variable_length(),
            auto_number: false,
            calculated: false,
            complex_value_fk: false,
            hidden: false,
            security_identifier: false,
            var_len_table_index: 0,
            fixed_data_offset: 0,
            flags: 0,
            ext_flags: 0,
            precision: 18,
            scale: 2,
            sort_order: None,
            code_page: 0,
            compressed_unicode: false,
            display_index: column_number as usize,
            column_index: column_number as usize,
            umap_page_number: -1,
            umap_owned_row_number: 0,
            umap_free_row_number: 0,
            append_only: false,
            complex_id: 0,
            length_in_units: -1,
            default_value: None,
            required: false,
            allow_zero_length: true,
            column_validator: None,
        };
        if data_type == DataType::Text {
            c.sort_order = Some(SortOrder::GENERAL);
        }
        c
    }

    use crate::format::SortOrder;

    /// The rows used by the Java oracle (`IdxOracle.sampleRows`) for the V2003
    /// table: L, I, D, DT, B, M, BO, BY, N, G.
    fn oracle_rows() -> Vec<Vec<Value>> {
        vec![
            vec![
                Value::Long(1),
                Value::Int(2),
                Value::Double(1.5),
                Value::DateTime(LocalDateTime::new(2020, 1, 2, 3, 4, 5, 678_000_000)),
                Value::Bytes(vec![1, 2, 3, 4, 5, 6, 7, 8]),
                Value::Decimal {
                    unscaled: 123400,
                    scale: 4,
                },
                Value::Bool(true),
                Value::Byte(5),
                Value::Decimal {
                    unscaled: -123456,
                    scale: 2,
                },
                Value::Uuid([
                    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
                    0xdd, 0xee, 0xff,
                ]),
            ],
            vec![
                Value::Long(-1),
                Value::Int(-300),
                Value::Double(-2.5),
                Value::DateTime(LocalDateTime::new(1899, 12, 29, 23, 59, 58, 0)),
                Value::Bytes(vec![0xff, 0x00, 0x80, 0x7f, 9, 8, 7, 6]),
                Value::Decimal {
                    unscaled: -1,
                    scale: 4,
                },
                Value::Bool(false),
                Value::Byte(255),
                Value::Decimal {
                    unscaled: 0,
                    scale: 2,
                },
                Value::Uuid([0xff; 16]),
            ],
            vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::Long(0),
                Value::Int(0),
                Value::Double(0.0),
                Value::DateTime(LocalDateTime::new(1899, 12, 30, 0, 0, 0, 0)),
                Value::Bytes(vec![0; 8]),
                Value::Decimal {
                    unscaled: 0,
                    scale: 4,
                },
                Value::Bool(false),
                Value::Byte(0),
                Value::Decimal {
                    unscaled: 0,
                    scale: 2,
                },
                Value::Uuid([0; 16]),
            ],
            vec![
                Value::Long(2147483647),
                Value::Int(32767),
                Value::Double(f64::MAX),
                Value::DateTime(LocalDateTime::new(9999, 12, 31, 23, 59, 59, 999_000_000)),
                Value::Bytes(vec![1, 2, 3]),
                Value::Decimal {
                    unscaled: 9223372036854775807,
                    scale: 4,
                },
                Value::Bool(true),
                Value::Byte(127),
                Value::Decimal {
                    unscaled: 999999999999999999,
                    scale: 2,
                },
                Value::Uuid([
                    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89,
                    0xab, 0xcd, 0xef,
                ]),
            ],
            vec![
                Value::Long(-2147483648),
                Value::Int(-32768),
                Value::Double(-f64::MAX),
                Value::DateTime(LocalDateTime::new(100, 1, 1, 0, 0, 0, 0)),
                Value::Bytes(vec![0]),
                Value::Decimal {
                    unscaled: -9223372036854775808,
                    scale: 4,
                },
                Value::Bool(false),
                Value::Byte(128),
                Value::Decimal {
                    unscaled: -999999999999999999,
                    scale: 2,
                },
                Value::Uuid([
                    0xde, 0xad, 0xbe, 0xef, 0xde, 0xad, 0xbe, 0xef, 0xde, 0xad, 0xbe, 0xef, 0xde,
                    0xad, 0xbe, 0xef,
                ]),
            ],
        ]
    }

    /// Builds an IndexData over the given (column index, ascending) pairs.
    fn index_over(columns: &[Column], cols: &[(usize, bool)]) -> IndexData {
        let mut idx = IndexData::new(0, 0, 0);
        for (ci, asc) in cols {
            idx.columns.push(IndexColumn {
                column: columns[*ci].clone(),
                flags: if *asc { ASCENDING_COLUMN_FLAG } else { 0 },
            });
        }
        idx
    }

    fn v2003_columns() -> Vec<Column> {
        vec![
            column("L", DataType::Long, 0, 4),
            column("I", DataType::Int, 1, 2),
            column("D", DataType::Double, 2, 8),
            column("DT", DataType::ShortDateTime, 3, 8),
            column("B", DataType::Binary, 4, 8),
            column("M", DataType::Money, 5, 8),
            column("BO", DataType::Boolean, 6, 1),
            column("BY", DataType::Byte, 7, 1),
            column("N", DataType::Numeric, 8, 17),
            column("G", DataType::Guid, 9, 16),
        ]
    }

    /// Reference bytes produced by `IdxOracle entries` (jackcess 5.0.4) for the
    /// V2003 table.
    ///
    /// Each entry is the index name, its columns as `(column index, ascending)`
    /// pairs, and the reference encodings.
    type OracleEntries = (
        &'static str,
        &'static [(usize, bool)],
        &'static [&'static str],
    );

    const ORACLE_ENTRIES: &[OracleEntries] = &[
        (
            "L",
            &[(0, true)],
            &[
                "7f80000001",
                "7f7fffffff",
                "00",
                "7f80000000",
                "7fffffffff",
                "7f00000000",
            ],
        ),
        (
            "I",
            &[(1, true)],
            &["7f8002", "7f7ed4", "00", "7f8000", "7fffff", "7f0000"],
        ),
        (
            "D",
            &[(2, true)],
            &[
                "7fbff8000000000000",
                "7f3ffbffffffffffff",
                "00",
                "7f8000000000000000",
                "7fffefffffffffffff",
                "7f0010000000000000",
            ],
        ),
        (
            "DT",
            &[(3, true)],
            &[
                "7fc0e56704174b3df4",
                "7f4000001845c8a0cd",
                "00",
                "7f8000000000000000",
                "7fc1469240ffffffe7",
                "7f3edbefcbffffffff",
            ],
        ),
        (
            "B",
            &[(4, true)],
            &[
                "7f010203040506070808",
                "7fff00807f0908070608",
                "00",
                "7f000000000000000008",
                "7f010203000000000003",
                "7f000000000000000001",
            ],
        ),
        (
            "M",
            &[(5, true)],
            &[
                "7f800000000001e208",
                "7f7fffffffffffffff",
                "00",
                "7f8000000000000000",
                "7fffffffffffffffff",
                "7f0000000000000000",
            ],
        ),
        (
            "BO",
            &[(6, true)],
            &["7f00", "7fff", "7fff", "7fff", "7f00", "7fff"],
        ),
        (
            "BY",
            &[(7, true)],
            &["7f05", "7fff", "00", "7f00", "7f7f", "7f80"],
        ),
        (
            "N",
            &[(8, true)],
            &[
                "7f00fffffffffffffffffffffffffffe1dbf",
                "7fff00000000000000000000000000000000",
                "00",
                "7fff00000000000000000000000000000000",
                "7fff00000000000000000de0b6b3a763ffff",
                "7f00fffffffffffffffff21f494c589c0000",
            ],
        ),
        (
            "G",
            &[(9, true)],
            &[
                "7f0011223344556677098899aabbccddeeff08",
                "7fffffffffffffffff09ffffffffffffffff08",
                "00",
                "7f000000000000000009000000000000000008",
                "7f0123456789abcdef090123456789abcdef08",
                "7fdeadbeefdeadbeef09deadbeefdeadbeef08",
            ],
        ),
        (
            "LD",
            &[(0, true), (2, false)],
            &[
                "7f80000001804007ffffffffffff",
                "7f7fffffff80c004000000000000",
                "00ff",
                "7f80000000807fffffffffffffff",
                "7fffffffff800010000000000000",
                "7f0000000080ffefffffffffffff",
            ],
        ),
    ];

    /// Reference bytes produced by `IdxOracle text` (jackcess 5.0.4) for a
    /// single TEXT column, ascending and descending.
    const ORACLE_TEXT: &[(&str, &str, &str)] = &[
        ("Test", "7f6d516b6d0100", "8092ae9492feff00"),
        (
            "MSysAccessStorage",
            "7f606b766b4a4d4d516b6b6b6d64694a55510100",
            "809f948994b5b2b2ae949494929b96b5aaaefeff00",
        ),
        ("a", "7f4a0100", "80b5feff00"),
        ("ab", "7f4a4c0100", "80b5b3feff00"),
        ("abc", "7f4a4c4d0100", "80b5b3b2feff00"),
        ("abcd", "7f4a4c4d4f0100", "80b5b3b2b0feff00"),
        ("name0", "7f624a6051360100", "809db59faec9feff00"),
        ("name1", "7f624a6051380100", "809db59faec7feff00"),
        ("Test1", "7f6d516b6d380100", "8092ae9492c7feff00"),
        ("test", "7f6d516b6d0100", "8092ae9492feff00"),
        (" ", "7f0100", "80feff00"),
        (
            "MSysNavPaneGroupCategories",
            "7f606b766b624a71664a62515569646f664d4a6d5155646959516b0100",
            "809f9489949db58e99b59daeaa969b9099b2b592aeaa9b96a6ae94feff00",
        ),
        ("T", "7f6d0100", "8092feff00"),
        ("Te", "7f6d510100", "8092aefeff00"),
        ("Tes", "7f6d516b0100", "8092ae94feff00"),
        (
            "TestTest",
            "7f6d516b6d6d516b6d0100",
            "8092ae949292ae9492feff00",
        ),
        ("xyz", "7f7576780100", "808a8987feff00"),
        ("\u{00e9}", "7f51010e00", "80aefef1ff00"),
        (
            "\u{30ab}",
            "7f7f0a010101ff0280ff8000",
            "8080f5fefefe00fd7f007fff00",
        ),
    ];

    #[test]
    fn date_index_key_uses_the_configured_date_time_type() {
        let format = JetFormat::version_4();
        let cols = vec![column("DT", DataType::ShortDateTime, 0, 8)];
        let sub_ms = Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789));
        let millis = Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000));

        let ldt = index_over(&cols, &[(0, true)]);
        let mut date = ldt.clone();
        date.set_date_context(DateContext::new(
            DateTimeType::Date,
            TimeZone::from_offset_minutes(480),
        ));

        let ldt_sub = ldt
            .create_entry_bytes(std::slice::from_ref(&sub_ms), &format, Charset::Utf16Le)
            .unwrap();
        let ldt_ms = ldt
            .create_entry_bytes(std::slice::from_ref(&millis), &format, Charset::Utf16Le)
            .unwrap();
        let date_sub = date
            .create_entry_bytes(std::slice::from_ref(&sub_ms), &format, Charset::Utf16Le)
            .unwrap();
        let date_ms = date
            .create_entry_bytes(std::slice::from_ref(&millis), &format, Charset::Utf16Le)
            .unwrap();

        // LOCAL_DATE_TIME keeps the nanoseconds, DATE truncates the value to
        // the millisecond (`DefaultDateTimeFactory.toDateDouble`)
        assert_ne!(ldt_sub, ldt_ms);
        assert_eq!(date_sub, date_ms);
        // the DATE key is the key of the double the row writer stores
        let row_bits = column::date_value_bits_in(&sub_ms, date.date_context).unwrap();
        let row_entry = date
            .create_entry_bytes(
                &[Value::DateBits(f64::from_bits(row_bits))],
                &format,
                Charset::Utf16Le,
            )
            .unwrap();
        assert_eq!(date_sub, row_entry);

        // "noisy stored double": an index written under LOCAL_DATE_TIME keeps
        // sub-millisecond bits; a DATE-mode search key for the same
        // LocalDateTime is the millisecond double, so it does not match the
        // stored entry (Java behaves the same way)
        let stored_bits = column::date_value_bits_in(&sub_ms, ldt.date_context).unwrap();
        let stored = Value::DateBits(f64::from_bits(stored_bits));
        let stored_entry = ldt
            .create_entry_bytes(std::slice::from_ref(&stored), &format, Charset::Utf16Le)
            .unwrap();
        assert_eq!(stored_entry, ldt_sub);
        assert_ne!(date_sub, stored_entry);
        // rewriting the read value in DATE mode keeps its original bits, so
        // the row and its index entry still agree (the `DateExt` round trip)
        let reread_entry = date
            .create_entry_bytes(std::slice::from_ref(&stored), &format, Charset::Utf16Le)
            .unwrap();
        assert_eq!(reread_entry, stored_entry);
    }

    #[test]
    fn date_index_key_of_a_number_uses_the_database_zone() {
        let format = JetFormat::version_4();
        let cols = vec![column("DT", DataType::ShortDateTime, 0, 8)];
        let millis = 1_584_246_645_123i64;
        let mut date = index_over(&cols, &[(0, true)]);
        date.set_date_context(DateContext::new(
            DateTimeType::Date,
            TimeZone::from_offset_minutes(480),
        ));
        let mut ldt = index_over(&cols, &[(0, true)]);
        ldt.set_date_context(DateContext::new(
            DateTimeType::LocalDateTime,
            TimeZone::from_offset_minutes(480),
        ));

        // a numeric value is a `java.util.Date` instant: both factories first
        // convert it to the zone's wall clock time, so the keys agree (the
        // zone matters, the DateTimeType does not, for a numeric date)
        let local = LocalDateTime::from_epoch_millis(millis + 8 * 3_600_000);
        let number_key = date
            .create_entry_bytes(&[Value::BigInt(millis)], &format, Charset::Utf16Le)
            .unwrap();
        let local_key = date
            .create_entry_bytes(&[Value::DateTime(local)], &format, Charset::Utf16Le)
            .unwrap();
        assert_eq!(number_key, local_key);
        let ldt_number_key = ldt
            .create_entry_bytes(&[Value::BigInt(millis)], &format, Charset::Utf16Le)
            .unwrap();
        assert_eq!(number_key, ldt_number_key);
        // ... while a different zone builds a different key
        let mut other_zone = index_over(&cols, &[(0, true)]);
        other_zone.set_date_context(DateContext::new(DateTimeType::Date, TimeZone::utc()));
        assert_ne!(
            number_key,
            other_zone
                .create_entry_bytes(&[Value::BigInt(millis)], &format, Charset::Utf16Le)
                .unwrap()
        );
    }

    /// A `DATE`-mode index key built from a caller `LocalDateTime` uses the
    /// `DefaultDateTimeFactory` conversion (the zone-aware instant lookup), not
    /// the plain wall clock double: a local time inside the New York
    /// spring-forward gap keys as 03:30, exactly like the row bytes, so a
    /// lookup with the original value finds the row.
    #[test]
    fn date_index_key_uses_the_named_zone_conversion() {
        let format = JetFormat::version_4();
        let cols = vec![column("DT", DataType::ShortDateTime, 0, 8)];
        let ny = TimeZone::from_zone_id("America/New_York").unwrap();
        let mut date = index_over(&cols, &[(0, true)]);
        date.set_date_context(DateContext::new(DateTimeType::Date, ny));
        let mut ldt = index_over(&cols, &[(0, true)]);
        ldt.set_date_context(DateContext::new(DateTimeType::LocalDateTime, ny));

        let gap = Value::DateTime(LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0));
        let expected = crate::datenums::to_date_double(LocalDateTime::new(2020, 3, 8, 3, 30, 0, 0));
        let date_key = date
            .create_entry_bytes(std::slice::from_ref(&gap), &format, Charset::Utf16Le)
            .unwrap();
        let stored_key = date
            .create_entry_bytes(&[Value::DateBits(expected)], &format, Charset::Utf16Le)
            .unwrap();
        assert_eq!(date_key, stored_key);
        // LOCAL_DATE_TIME keeps the plain wall clock double
        let ldt_key = ldt
            .create_entry_bytes(std::slice::from_ref(&gap), &format, Charset::Utf16Le)
            .unwrap();
        assert_ne!(date_key, ldt_key);
        assert_eq!(
            ldt_key,
            ldt.create_entry_bytes(
                &[Value::DateTime(LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0))],
                &format,
                Charset::Utf16Le,
            )
            .unwrap()
        );
        // a search key whose value was read back from the table keeps its bits
        // (`DateExt`), so it still matches
        let reread = date
            .create_entry_bytes(&[Value::DateBits(expected)], &format, Charset::Utf16Le)
            .unwrap();
        assert_eq!(reread, date_key);
    }

    #[test]
    fn date_index_key_default_context_matches_the_old_local_date_time_path() {
        let format = JetFormat::version_4();
        let cols = vec![column("DT", DataType::ShortDateTime, 0, 8)];
        let date = Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000));
        let idx = index_over(&cols, &[(0, true)]);
        let got = idx
            .create_entry_bytes(std::slice::from_ref(&date), &format, Charset::Utf16Le)
            .unwrap();
        // whole millisecond values are byte identical in both modes
        let mut date_mode = idx.clone();
        date_mode.set_date_time_type(DateTimeType::Date);
        assert_eq!(
            got,
            date_mode
                .create_entry_bytes(&[date], &format, Charset::Utf16Le)
                .unwrap()
        );
    }

    #[test]
    fn text_entry_bytes_match_java() {
        let format = JetFormat::version_4();
        for (value, asc_expected, desc_expected) in ORACLE_TEXT {
            let mut col = column("N", DataType::Text, 0, 50);
            col.sort_order = Some(SortOrder::GENERAL_LEGACY);
            let idx_asc = index_over(std::slice::from_ref(&col), &[(0, true)]);
            let got = idx_asc
                .create_entry_bytes(
                    &[Value::Text((*value).to_string())],
                    &format,
                    crate::format::Charset::Utf16Le,
                )
                .unwrap();
            assert_eq!(hex(&got), *asc_expected, "ascending text {:?}", value);

            let idx_desc = index_over(std::slice::from_ref(&col), &[(0, false)]);
            let got = idx_desc
                .create_entry_bytes(
                    &[Value::Text((*value).to_string())],
                    &format,
                    crate::format::Charset::Utf16Le,
                )
                .unwrap();
            assert_eq!(hex(&got), *desc_expected, "descending text {:?}", value);
        }
    }

    /// Reference bytes for a descending NUMERIC index: the legacy (Jet 4) and
    /// new (ACE) sort orders encode the sign byte differently.  From
    /// `IdxOracle entries` (V2003 and V2016 tables).
    const ORACLE_NUMERIC_DESC_LEGACY: &[&str] = &[
        "80000000000000000000000000000001e240",
        "80ffffffffffffffffffffffffffffffffff",
        "ff",
        "80ffffffffffffffffffffffffffffffffff",
        "80fffffffffffffffffff21f494c589c0000",
        "800000000000000000000de0b6b3a763ffff",
    ];
    const ORACLE_NUMERIC_DESC_NEW: &[&str] = &[
        "80ff0000000000000000000000000001e240",
        "8000ffffffffffffffffffffffffffffffff",
        "ff",
        "8000ffffffffffffffffffffffffffffffff",
        "8000fffffffffffffffff21f494c589c0000",
        "80ff00000000000000000de0b6b3a763ffff",
    ];
    const ORACLE_BIG_INT: &[&str] = &[
        "7f8000011f71fb04cb",
        "7f7ffffee08e04fb35",
        "00",
        "7f8000000000000000",
        "7fffffffffffffffff",
        "7f0000000000000000",
    ];

    #[test]
    fn numeric_and_bigint_entry_bytes_match_java() {
        let rows = oracle_rows();
        let columns = v2003_columns();

        // legacy numeric (Jet 4), descending
        let legacy = JetFormat::version_4();
        assert!(legacy.legacy_numeric_indexes);
        let idx = index_over(&columns, &[(8, false)]);
        for (ri, row) in rows.iter().enumerate() {
            let got = idx
                .create_entry_bytes(row, &legacy, crate::format::Charset::Utf16Le)
                .unwrap();
            assert_eq!(
                hex(&got),
                ORACLE_NUMERIC_DESC_LEGACY[ri],
                "legacy numeric desc row {}",
                ri
            );
        }

        // new numeric (ACE 2007+), descending
        let ace = JetFormat::version_2007();
        assert!(!ace.legacy_numeric_indexes);
        let idx = index_over(&columns, &[(8, false)]);
        for (ri, row) in rows.iter().enumerate() {
            let got = idx
                .create_entry_bytes(row, &ace, crate::format::Charset::Utf16Le)
                .unwrap();
            assert_eq!(
                hex(&got),
                ORACLE_NUMERIC_DESC_NEW[ri],
                "new numeric desc row {}",
                ri
            );
        }

        // BIG_INT (ACE), ascending
        let big_ints = [
            Value::BigInt(1234567890123),
            Value::BigInt(-1234567890123),
            Value::Null,
            Value::BigInt(0),
            Value::BigInt(i64::MAX),
            Value::BigInt(i64::MIN),
        ];
        let bi_col = column("BI", DataType::BigInt, 0, 8);
        let idx = index_over(std::slice::from_ref(&bi_col), &[(0, true)]);
        for (ri, value) in big_ints.iter().enumerate() {
            let got = idx
                .create_entry_bytes(
                    std::slice::from_ref(value),
                    &ace,
                    crate::format::Charset::Utf16Le,
                )
                .unwrap();
            assert_eq!(hex(&got), ORACLE_BIG_INT[ri], "bigint row {}", ri);
        }
    }

    #[test]
    fn entry_bytes_match_java() {
        let format = JetFormat::version_4();
        let columns = v2003_columns();
        let rows = oracle_rows();
        for (name, cols, expected) in ORACLE_ENTRIES {
            let idx = index_over(&columns, cols);
            for (ri, row) in rows.iter().enumerate() {
                let got = idx
                    .create_entry_bytes(row, &format, crate::format::Charset::Utf16Le)
                    .unwrap_or_else(|e| panic!("{} row {}: {}", name, ri, e));
                assert_eq!(
                    hex(&got),
                    expected[ri],
                    "entry bytes mismatch for index {} row {}",
                    name,
                    ri
                );
            }
        }
    }

    #[test]
    fn descending_entry_bytes_match_java() {
        // desc flags: the oracle did not include a desc single-column index, so
        // verify the desc transformation on the integer column against the
        // multi-column index bytes (L asc, D desc) above.
        let format = JetFormat::version_4();
        let columns = v2003_columns();
        let idx = index_over(&columns, &[(0, true), (2, false)]);
        let row = oracle_rows();
        let got = idx
            .create_entry_bytes(&row[0], &format, crate::format::Charset::Utf16Le)
            .unwrap();
        assert_eq!(hex(&got), "7f80000001804007ffffffffffff");
    }

    #[test]
    fn truncate_entry_bytes_matches_java() {
        // IndexData.truncateEntryBytes reference values from the oracle
        let mut long_asc = vec![0u8; 600];
        for (i, b) in long_asc.iter_mut().enumerate() {
            *b = (i * 7 + 1) as u8;
        }
        let trunc = truncate_entry_bytes(&long_asc, 510);
        assert_eq!(trunc.len(), 510);
        assert_eq!(&trunc[510 - KEY_DIGEST_LENGTH..], &[0xc2, 0x00]);

        let mut long_desc = long_asc.clone();
        flip_bytes(&mut long_desc);
        let trunc = truncate_entry_bytes(&long_desc, 510);
        assert_eq!(&trunc[510 - KEY_DIGEST_LENGTH..], &[0xe9, 0xff]);

        let exact: Vec<u8> = (0..510).map(|i| (i * 3) as u8).collect();
        let trunc = truncate_entry_bytes(&exact, 510);
        assert_eq!(trunc, exact);

        let over: Vec<u8> = (0..511).map(|i| (255 - i) as u8).collect();
        let trunc = truncate_entry_bytes(&over, 510);
        assert_eq!(trunc.len(), 510);
        assert_eq!(&trunc[510 - KEY_DIGEST_LENGTH..], &[0x0b, 0x02]);

        // the kept prefix is a straight copy of the input
        assert_eq!(&trunc[..508], &over[..508]);
    }

    #[test]
    fn root_page_buffer_matches_java() {
        // ROOT tdef=50: the Java dump of a fresh root page is 4096 bytes whose
        // only non-zero bytes are the 8 byte header (page type 0x04, constant
        // 0x01, free space 0x0e20 = 3616, tdef page 50).
        let format = JetFormat::version_4();
        let mut ch = PageChannel::new(
            Box::new(crate::pagechannel::MemChannel::new()),
            format.clone(),
            false,
        );
        let buf = create_root_page_buffer(&mut ch, 50, &format).unwrap();
        let bytes = buf.array();
        assert_eq!(bytes.len(), 4096);
        let mut expected = vec![0u8; 4096];
        expected[0] = 0x04; // INDEX_LEAF
        expected[1] = 0x01;
        expected[2..4].copy_from_slice(&(3616i16).to_le_bytes());
        expected[4..8].copy_from_slice(&50i32.to_le_bytes());
        assert_eq!(hex(&bytes[0..8]), "0401200e32000000");
        assert_eq!(bytes, &expected[..]);
    }

    #[test]
    fn page_round_trip() {
        let format = JetFormat::version_4();
        let mut page = DataPage::new(7, true, 0);
        page.entry_prefix = vec![0x7f, 0x80];
        page.prev_page_number = 3;
        page.next_page_number = 9;
        page.child_tail_page_number = 0;
        // three leaf entries with a shared prefix
        page.entries
            .push(Entry::leaf(vec![0x7f, 0x80, 0x00, 0x01], RowId::new(12, 0)));
        page.entries
            .push(Entry::leaf(vec![0x7f, 0x80, 0x00, 0x02], RowId::new(12, 1)));
        page.entries
            .push(Entry::leaf(vec![0x7f, 0x80, 0x00, 0x03], RowId::new(13, 0)));
        page.total_entry_size = page.entries.iter().map(|e| e.size()).sum();

        let mut buffer = ByteBuf::allocate(format.page_size);
        write_data_page(&mut buffer, &page, 50, &format).unwrap();

        // header sanity: free space, prefix length, level, entry mask
        let used = 27 + format.size_index_entry_mask + 2 + 3 * (8 - 2);
        assert_eq!(
            buffer.get_i16_at(2).unwrap() as i32,
            format.page_size as i32 - used as i32
        );
        assert_eq!(buffer.get_i16_at(24).unwrap(), 2);
        assert_eq!(buffer.get_u8_at(26).unwrap(), 0);
        // three mask bits set, at the cumulative entry ends
        let mask = buffer
            .get_bytes_at(27, format.size_index_entry_mask)
            .unwrap();
        let mut bits = Vec::new();
        for (i, b) in mask.iter().enumerate() {
            for j in 0..8 {
                if b & (1 << j) != 0 {
                    bits.push(i * 8 + j);
                }
            }
        }
        // first entry size 8, then each subsequent adds 6 (prefix is 2 bytes)
        assert_eq!(bits, vec![8, 14, 20]);

        let read = read_data_page(&mut buffer, 7, &format).unwrap();
        assert!(read.leaf);
        assert_eq!(read.level, 0);
        assert_eq!(read.prev_page_number, 3);
        assert_eq!(read.next_page_number, 9);
        assert_eq!(read.entry_prefix, vec![0x7f, 0x80]);
        assert_eq!(read.entries.len(), 3);
        assert_eq!(read.total_entry_size, page.total_entry_size);
        for (a, b) in read.entries.iter().zip(page.entries.iter()) {
            assert_eq!(
                hex(a.get_entry_bytes().unwrap()),
                hex(b.get_entry_bytes().unwrap())
            );
            assert_eq!(a.get_row_id(), b.get_row_id());
        }
    }

    #[test]
    fn node_entry_round_trip() {
        let format = JetFormat::version_4();
        let mut page = DataPage::new(8, false, 1);
        page.child_tail_page_number = 11;
        page.entries
            .push(Entry::leaf(vec![0x7f, 0x01], RowId::new(4, 0)).as_node_entry(10));
        page.total_entry_size = page.entries.iter().map(|e| e.size()).sum();
        let mut buffer = ByteBuf::allocate(format.page_size);
        write_data_page(&mut buffer, &page, 50, &format).unwrap();
        let read = read_data_page(&mut buffer, 8, &format).unwrap();
        assert!(!read.leaf);
        assert_eq!(read.level, 1);
        assert_eq!(read.child_tail_page_number, 11);
        assert_eq!(read.entries.len(), 1);
        let e = &read.entries[0];
        assert!(!e.is_leaf_entry());
        assert_eq!(e.get_sub_page_number(), Some(10));
        assert_eq!(e.get_row_id().page_number, 4);
        assert_eq!(e.get_row_id().row_number, 0);
        assert_eq!(e.size(), 2 + 4 + 4);
    }

    #[test]
    fn entry_ordering() {
        let a = Entry::leaf(vec![0x01], RowId::new(2, 0));
        let b = Entry::leaf(vec![0x01], RowId::new(2, 1));
        let c = Entry::leaf(vec![0x02], RowId::new(1, 0));
        assert!(a.compare_to(&b) < 0);
        assert!(b.compare_to(&c) < 0);
        // byte values dominate the rowId
        let d = Entry::leaf(vec![0x01], RowId::new(99, 9));
        assert!(a.compare_to(&d) < 0);

        // special entries sort outside the valid range
        let first = Entry::first_entry();
        let last = Entry::last_entry();
        assert!(first.compare_to(&a) < 0);
        assert!(a.compare_to(&last) < 0);

        // equal bytes with different rowIds
        assert!(a.equals_entry_bytes(&d));
        assert!(!a.equals_entry_bytes(&c));

        // node entries compare by bytes then rowId, and equality also considers
        // the sub page number
        let n1 = a.as_node_entry(5);
        let n2 = a.as_node_entry(6);
        assert_eq!(n1.compare_to(&n2), 0);
        assert_ne!(n1, n2);
        assert_ne!(n1, a);
        assert!(!n1.is_leaf_entry());
    }

    // -----------------------------------------------------------------
    // the Java `Index` object API (getTable/getProperties/newCursor/...)
    // -----------------------------------------------------------------

    fn objapi_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn java_index_accessors_match_the_table() {
        use crate::column::ColumnSpec;
        use crate::database::Database;
        use crate::format::FileFormat;
        use crate::tablecreator::{IndexSpec, TableSpec};

        let path =
            objapi_tmp_dir().join(format!("jackcess_index_objapi_{}.mdb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(20),
                ColumnSpec::new("Num", DataType::Long),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ))
        .with_index(
            IndexSpec::new("ByName", vec![("Name".to_string(), true)])
                .unique()
                .ignore_nulls()
                .required(),
        )
        .with_index(IndexSpec::new(
            "ByNumDesc",
            vec![("Num".to_string(), false)],
        ));
        db.create_table(&spec).expect("create table");

        let page;
        let pk;
        let builder;
        {
            let table = db.get_table("T").unwrap();
            page = table.table_def_page_number;
            pk = table.get_index("PrimaryKey").unwrap().clone();
            // `Index.getTable` returns the owning table
            assert_eq!(pk.get_table(table).unwrap().get_name(), "T");
            assert_eq!(pk.get_column_count(table), 1);
            assert!(pk.is_unique(table));
            assert!(pk.is_required(table));
            assert!(!pk.should_ignore_nulls(table));
            let columns = pk.get_columns(table);
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].get_name(), "ID");
            assert_eq!(columns[0].get_column().name, "ID");
            assert_eq!(columns[0].get_column_index(), 0);
            assert!(columns[0].is_ascending());

            let by_name = table.get_index("ByName").unwrap();
            assert!(by_name.is_unique(table));
            assert!(by_name.is_required(table));
            assert!(by_name.should_ignore_nulls(table));
            let by_num = table.get_index("ByNumDesc").unwrap();
            assert!(!by_num.is_unique(table));
            let desc = by_num.get_columns(table);
            assert_eq!(desc.len(), 1);
            assert!(!desc[0].is_ascending());
            assert_eq!(desc[0].get_column_index(), 2);

            // `Index.newCursor()` presets the table and the index
            builder = pk.new_cursor(table);
        }
        let resolved = builder.resolve_index(&mut db).expect("resolve index");
        assert_eq!(resolved.name, "PrimaryKey");

        // `Index.getProperties` returns the (empty, named) index map
        let props = pk.get_properties(&mut db, page).expect("properties");
        assert_eq!(props.name(), "PrimaryKey");
        assert!(props.is_empty());

        // a primary key index has no foreign key reference to resolve
        assert!(pk
            .get_referenced_index(&mut db, "T")
            .expect("referenced index")
            .is_none());
        // an index of another table is rejected
        let other = TableSpec::new("U", vec![ColumnSpec::new("ID", DataType::Long)]);
        db.create_table(&other).expect("create U");
        assert!(matches!(
            pk.get_table(db.get_table("U").unwrap()),
            Err(Error::IllegalArgument(_))
        ));
        let _ = std::fs::remove_file(&path);
    }
}
