//! Port of the Jackcess name map subsystem (`NameMap`, `TableNameMap`,
//! `QueryNameMap` and the `NameMapImpl` binary format).
//!
//! A name map is the data behind the ms access "Name AutoCorrect" option: a
//! record for each table and column an object depends on, tied to the guid in
//! the [`crate::property::GUID_PROP`] property of that table or column.  A
//! table stores its name map in the [`crate::property::NAME_MAP_PROP`]
//! property, a query in [`crate::property::QUERY_NAME_MAP_PROP`].
//!
//! The encoding, exactly as `NameMapImpl` parses it, is
//!
//! ```text
//! NameMap := magic [u32 reserved record*]
//! record  := guid:byte[16], type:u32, payload:byte[16], name:utf16, u32 zero
//! ```
//!
//! All integers are little endian, and the name is null terminated.  The
//! trailing zero is left off the last record, and there is no record count,
//! so a reader stops at the end of the data.  The payload is a date and eight
//! bytes of writer state for a table record, the parent table guid for a
//! column or reference record, and the writer tag for the end record.  A
//! query which tracks no names has the magic alone.
//!
//! The port covers the read API, the table level mismatch computation of
//! `TableNameMapImpl.findMismatches`, `NameMapImpl.toBytes`, and the mutating
//! operations of the Java `TableNameMap` (`addColumn`, `removeColumn` and
//! `save`, the last of which writes the encoded map back into the table's
//! property through the property-save plumbing).
//!
//! # Java name mapping (the `NameMap`/`TableNameMap` object API)
//!
//! | Java (`com.healthmarketscience.jackcess.NameMap`) | Rust |
//! |---|---|
//! | `iterator()` | [`NameMap::iter`] (`IntoIterator for &NameMap`) |
//! | `getTableRecords()` / `getColumnRecords()` | [`NameMap::get_table_records`] / [`NameMap::get_column_records`] |
//! | `getRecord(guid)` | [`NameMap::get_record`] |
//! | `getWriterTag()` | [`NameMap::get_writer_tag`] |
//! | `findMismatches()` | [`TableNameMap::find_mismatches`] |
//! | `Record.getType()` / `getGuid()` / `getParentGuid()` | [`Record::get_type`] / [`Record::get_guid`] / [`Record::get_parent_guid`] |
//! | `Record.getName()` / `setName(...)` / `getDate()` | [`Record::get_name`] / [`Record::set_name`] / [`Record::get_date`] |
//! | `Mismatch.getType()` / `getTable()` / `getQuery()` / `getColumn()` / `getRecord()` | [`Mismatch::get_type`] / [`Mismatch::get_table`] / [`Mismatch::get_query`] / [`Mismatch::get_column`] / [`Mismatch::get_record`] |
//!
//! | Java (`com.healthmarketscience.jackcess.TableNameMap`) | Rust |
//! |---|---|
//! | `getTableRecord()` / `getColumnRecord(name)` / `getReferences()` | [`TableNameMap::get_table_record`] / [`TableNameMap::get_column_record`] / [`TableNameMap::get_references`] |
//! | `addColumn(name, guid)` / `removeColumn(name)` | [`TableNameMap::add_column`] / [`TableNameMap::remove_column`] |
//! | `save()` | [`TableNameMap::save`] (or [`TableNameMap::save_for_table`]) |
//! | `isWritable()` (protected in `NameMapImpl`) | [`TableNameMap::is_writable`] / [`QueryNameMap::is_writable`] |

use std::fmt;

use crate::datenums::LocalDateTime;
use crate::error::{Error, Result};
use crate::property::{PropertyMap, PropertyMaps, GUID_PROP};
use crate::table::Table;

const MAGIC: i32 = 0x550E_CC0A;
const MAGIC_LENGTH: usize = 4;
const HEADER_LENGTH: usize = 8;
const GUID_LENGTH: usize = 16;
const PAYLOAD_LENGTH: usize = 16;
/// the length of a record without its name and trailing zero
const FIXED_LENGTH: usize = GUID_LENGTH + 4 + PAYLOAD_LENGTH;
const TRAILER_LENGTH: usize = 4;

const TABLE_TYPE: i32 = 0;
const REFERENCE_TYPE: i32 = 6;
const COLUMN_TYPE: i32 = 7;
const END_TYPE: i32 = 12;

/// The kinds of record in a name map (`NameMap.RecordType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecordType {
    /// a table.  A table name map has one, and it is the first record
    Table,
    /// a name which an expression in a table uses
    Reference,
    /// a column
    Column,
    /// the end of the name map.  Older builds of ms access did not write it
    End,
    /// a type jackcess does not know, which is kept as it was read
    Unknown,
}

impl RecordType {
    /// The Java enum constant name (`RecordType.name()`).
    pub fn name(self) -> &'static str {
        match self {
            RecordType::Table => "TABLE",
            RecordType::Reference => "REFERENCE",
            RecordType::Column => "COLUMN",
            RecordType::End => "END",
            RecordType::Unknown => "UNKNOWN",
        }
    }
}

impl fmt::Display for RecordType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One record of a name map (`NameMap.Record`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    type_code: i32,
    guid: [u8; GUID_LENGTH],
    payload: [u8; PAYLOAD_LENGTH],
    name: String,
    /// `NameMapImpl.isWritable()` for the map which owns this record; a query
    /// name map cannot be changed.
    writable: bool,
}

impl Record {
    fn new(
        type_code: i32,
        guid: [u8; GUID_LENGTH],
        payload: [u8; PAYLOAD_LENGTH],
        name: String,
        writable: bool,
    ) -> Record {
        Record {
            type_code,
            guid,
            payload,
            name,
            writable,
        }
    }

    /// `Record.getType()`.
    pub fn get_type(&self) -> RecordType {
        match self.type_code {
            TABLE_TYPE => RecordType::Table,
            REFERENCE_TYPE => RecordType::Reference,
            COLUMN_TYPE => RecordType::Column,
            END_TYPE => RecordType::End,
            _ => RecordType::Unknown,
        }
    }

    /// The raw type value of the record, which a type jackcess does not know
    /// keeps as it was read (`NameMapImpl.RecordImpl._type`).
    pub fn type_code(&self) -> i32 {
        self.type_code
    }

    /// The 16 bytes of the record guid, exactly as stored.
    pub fn guid_bytes(&self) -> &[u8; GUID_LENGTH] {
        &self.guid
    }

    /// The 16 bytes of the record payload, exactly as stored.
    pub fn payload(&self) -> &[u8; PAYLOAD_LENGTH] {
        &self.payload
    }

    /// `Record.getGuid()`: the guid of the object, or `None` for a record
    /// which names no object (a reference or the end record) and for an
    /// all-zero guid.
    pub fn get_guid(&self) -> Option<String> {
        if is_zero(&self.guid) {
            None
        } else {
            Some(format_guid(&self.guid))
        }
    }

    /// `Record.getParentGuid()`: the guid of the table, for a column or a
    /// reference record, otherwise `None`.
    pub fn get_parent_guid(&self) -> Option<String> {
        if (self.type_code == COLUMN_TYPE) || (self.type_code == REFERENCE_TYPE) {
            if is_zero(&self.payload) {
                None
            } else {
                Some(format_guid(&self.payload))
            }
        } else {
            None
        }
    }

    /// `Record.getName()`.
    pub fn get_name(&self) -> &str {
        &self.name
    }

    /// `Record.setName(String)`.  The name map of a query cannot be changed,
    /// which Java reports as an `UnsupportedOperationException`; the port
    /// reports [`Error::Unsupported`].
    pub fn set_name(&mut self, name: impl Into<String>) -> Result<()> {
        if !self.writable {
            return Err(Error::Unsupported(
                "The name map of a query cannot be changed".to_string(),
            ));
        }
        self.name = name.into();
        Ok(())
    }

    /// `Record.getDate()`: the date in a table record, otherwise `None`.  It
    /// is the last modified date of the table when ms access wrote the name
    /// map.
    pub fn get_date(&self) -> Option<LocalDateTime> {
        if self.type_code == TABLE_TYPE {
            let value = f64::from_le_bytes(self.payload[0..8].try_into().unwrap());
            Some(crate::column::ldt_from_local_date_double(value))
        } else {
            None
        }
    }
}

fn is_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| *b == 0)
}

/// `ColumnImpl.readGUIDValue(buffer, DEFAULT_BYTE_ORDER)`: the first three
/// guid components are integer components, so they are stored little endian
/// and swapped before the canonical, braced, upper case form is built.
fn format_guid(bytes: &[u8; GUID_LENGTH]) -> String {
    let mut b = *bytes;
    b[0..4].reverse();
    b[4..6].reverse();
    b[6..8].reverse();
    let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!(
        "{{{}-{}-{}-{}-{}}}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `NameMapImpl.toGuidBytes`: the given guid encoded as a record stores it,
/// i.e. with the first three components in the name map's (little endian)
/// byte order.
///
/// `ColumnImpl.writeGUIDValue` rejects a value which is not a canonical guid
/// with `IllegalArgumentException("Invalid GUID: <guid>")`, which this mirrors.
fn to_guid_bytes(guid: &str) -> Result<[u8; GUID_LENGTH]> {
    if !crate::column::is_guid_string(guid) {
        return Err(Error::IllegalArgument(format!("Invalid GUID: {guid}")));
    }
    let canonical = crate::column::parse_guid(guid)?;
    Ok(crate::column::guid_stored_bytes(
        &canonical,
        crate::bytebuf::ByteOrder::LittleEndian,
    ))
}

/// The kinds of difference between a name map and the database
/// (`NameMap.MismatchType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MismatchType {
    /// a column of the table which holds the name map has no record
    MissingRecord,
    /// a record names no table or column, by guid or by name
    MissingObject,
    /// the object with the guid of a record has a different name
    NameDiffers,
    /// the object with the name of a record has a different guid
    GuidDiffers,
}

impl MismatchType {
    /// The Java enum constant name (`MismatchType.name()`).
    pub fn name(self) -> &'static str {
        match self {
            MismatchType::MissingRecord => "MISSING_RECORD",
            MismatchType::MissingObject => "MISSING_OBJECT",
            MismatchType::NameDiffers => "NAME_DIFFERS",
            MismatchType::GuidDiffers => "GUID_DIFFERS",
        }
    }
}

impl fmt::Display for MismatchType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One difference between a name map and the database (`NameMap.Mismatch`).
///
/// The Java interface returns live `Table`/`Column`/`Query` objects; the port
/// keeps their names (a query mismatch is not computed yet, see
/// [`QueryNameMap`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    mismatch_type: MismatchType,
    table: Option<String>,
    query: Option<String>,
    column: Option<String>,
    record: Option<Record>,
}

impl Mismatch {
    fn new(
        mismatch_type: MismatchType,
        table: Option<String>,
        query: Option<String>,
        column: Option<String>,
        record: Option<Record>,
    ) -> Mismatch {
        Mismatch {
            mismatch_type,
            table,
            query,
            column,
            record,
        }
    }

    /// `Mismatch.getType()`.
    pub fn get_type(&self) -> MismatchType {
        self.mismatch_type
    }

    /// `Mismatch.getTable()`: the table the difference is in, or `None`.
    pub fn get_table(&self) -> Option<&str> {
        self.table.as_deref()
    }

    /// `Mismatch.getQuery()`: the query the difference is in, or `None`.
    pub fn get_query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    /// `Mismatch.getColumn()`: the column, or `None` if the difference is in
    /// a table record or no column was found for the record.
    pub fn get_column(&self) -> Option<&str> {
        self.column.as_deref()
    }

    /// `Mismatch.getRecord()`: the record, or `None` if the column has no
    /// record.
    pub fn get_record(&self) -> Option<&Record> {
        self.record.as_ref()
    }
}

/// A name map, the shared record container of `NameMapImpl`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameMap {
    /// the word after the magic, `None` when the data is the magic alone
    reserved: Option<i32>,
    records: Vec<Record>,
}

impl NameMap {
    /// Decodes the value of a name map property (`NameMapImpl(byte[])`).
    ///
    /// The errors are the Java `IOException`s, message for message:
    /// `Unknown name map type <hex>`, `Name map ends inside a record`,
    /// `Name map ends inside a name` and
    /// `Name map record <name> has an unexpected end`.
    pub fn read(bytes: &[u8]) -> Result<NameMap> {
        if (bytes.len() < MAGIC_LENGTH) || (read_i32(bytes, 0) != MAGIC) {
            return Err(unknown_type(bytes));
        }
        // the magic was read, but the reserved word which follows it was cut
        // short
        if (bytes.len() > MAGIC_LENGTH) && (bytes.len() < HEADER_LENGTH) {
            return Err(unknown_type(bytes));
        }

        let (reserved, mut pos) = if bytes.len() > MAGIC_LENGTH {
            (Some(read_i32(bytes, MAGIC_LENGTH)), HEADER_LENGTH)
        } else {
            (None, MAGIC_LENGTH)
        };

        let mut records = Vec::new();
        while pos < bytes.len() {
            if bytes.len() - pos < FIXED_LENGTH {
                return Err(Error::Jackcess("Name map ends inside a record".to_string()));
            }
            let mut guid = [0u8; GUID_LENGTH];
            guid.copy_from_slice(&bytes[pos..pos + GUID_LENGTH]);
            let type_code = read_i32(bytes, pos + GUID_LENGTH);
            let mut payload = [0u8; PAYLOAD_LENGTH];
            payload.copy_from_slice(&bytes[pos + GUID_LENGTH + 4..pos + FIXED_LENGTH]);
            pos += FIXED_LENGTH;

            let (name, next) = read_name(bytes, pos)?;
            pos = next;
            if pos < bytes.len() {
                // every record but the last one ends with a zero, so a zero
                // which ends the data could not be written back
                if bytes.len() - pos < TRAILER_LENGTH {
                    return Err(record_end(&name));
                }
                let trailer = read_i32(bytes, pos);
                pos += TRAILER_LENGTH;
                if (trailer != 0) || (pos >= bytes.len()) {
                    return Err(record_end(&name));
                }
            }
            records.push(Record::new(type_code, guid, payload, name, true));
        }

        Ok(NameMap { reserved, records })
    }

    /// The word after the magic (`NameMapImpl._reserved`), `None` when the
    /// data is the magic alone.  Java has no public accessor for it.
    pub fn reserved(&self) -> Option<i32> {
        self.reserved
    }

    /// Every record, in the order of the name map.
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// `NameMap.iterator()`: every record, in the order of the name map.
    pub fn iter(&self) -> std::slice::Iter<'_, Record> {
        self.records.iter()
    }

    /// The table records, in the order of the name map
    /// (`NameMap.getTableRecords`).
    pub fn get_table_records(&self) -> Vec<&Record> {
        self.records
            .iter()
            .filter(|rec| rec.type_code == TABLE_TYPE)
            .collect()
    }

    /// The column records, in the order of the name map
    /// (`NameMap.getColumnRecords`).
    pub fn get_column_records(&self) -> Vec<&Record> {
        self.records
            .iter()
            .filter(|rec| rec.type_code == COLUMN_TYPE)
            .collect()
    }

    /// The first table or column record with the given guid, or `None`
    /// (`NameMap.getRecord`).  The comparison is case insensitive.
    pub fn get_record(&self, guid: &str) -> Option<&Record> {
        self.records.iter().find(|rec| {
            ((rec.type_code == TABLE_TYPE) || (rec.type_code == COLUMN_TYPE))
                && rec
                    .get_guid()
                    .is_some_and(|rec_guid| java_equals_ignore_case(&rec_guid, guid))
        })
    }

    /// `true` if a table or column record carries the given guid (the lookup
    /// `NameMap.getRecord(guid) != null`).
    pub fn contains(&self, guid: &str) -> bool {
        self.get_record(guid).is_some()
    }

    /// The tag in the end record (`NameMap.getWriterTag`), or `None` if the
    /// name map has no end record.
    pub fn get_writer_tag(&self) -> Option<i32> {
        self.records
            .iter()
            .find(|rec| rec.type_code == END_TYPE)
            .map(|rec| read_i32(&rec.payload, 0))
    }

    /// `NameMapImpl.toBytes()`: the name map encoded as it is stored in the
    /// property.  This is exposed for tests; the port has no API which writes
    /// a name map back into a database.
    pub fn to_bytes(&self) -> Vec<u8> {
        if self.reserved.is_none() && self.records.is_empty() {
            return MAGIC.to_le_bytes().to_vec();
        }

        let mut out = Vec::with_capacity(
            HEADER_LENGTH
                + self
                    .records
                    .iter()
                    .map(|rec| FIXED_LENGTH + (rec.name.encode_utf16().count() + 1) * 2 + 4)
                    .sum::<usize>(),
        );
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&self.reserved.unwrap_or(0).to_le_bytes());
        for (i, rec) in self.records.iter().enumerate() {
            out.extend_from_slice(&rec.guid);
            out.extend_from_slice(&rec.type_code.to_le_bytes());
            out.extend_from_slice(&rec.payload);
            for unit in rec.name.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            out.extend_from_slice(&0u16.to_le_bytes());
            if i + 1 < self.records.len() {
                out.extend_from_slice(&0u32.to_le_bytes());
            }
        }
        out
    }
}

impl<'a> IntoIterator for &'a NameMap {
    type Item = &'a Record;
    type IntoIter = std::slice::Iter<'a, Record>;

    fn into_iter(self) -> Self::IntoIter {
        self.records.iter()
    }
}

/// The name map of a table (`TableNameMapImpl`), stored in the table's
/// [`crate::property::NAME_MAP_PROP`] property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableNameMap {
    map: NameMap,
}

impl TableNameMap {
    /// Decodes the value of the name map property of a table
    /// (`TableNameMapImpl.read`).
    pub fn read(bytes: &[u8]) -> Result<TableNameMap> {
        Ok(TableNameMap {
            map: NameMap::read(bytes)?,
        })
    }

    /// The underlying record container.
    pub fn name_map(&self) -> &NameMap {
        &self.map
    }

    /// The record for the table, or `None` if there is none
    /// (`TableNameMap.getTableRecord`).
    pub fn get_table_record(&self) -> Option<&Record> {
        self.map
            .records
            .iter()
            .find(|rec| rec.type_code == TABLE_TYPE)
    }

    /// The column record with the given name (case-insensitive), or `None`
    /// (`TableNameMap.getColumnRecord`).
    pub fn get_column_record(&self, name: &str) -> Option<&Record> {
        self.map
            .records
            .iter()
            .find(|rec| (rec.type_code == COLUMN_TYPE) && java_equals_ignore_case(&rec.name, name))
    }

    /// The records for the names which expressions in the table use, in the
    /// order of the name map (`TableNameMap.getReferences`).
    pub fn get_references(&self) -> Vec<&Record> {
        self.map
            .records
            .iter()
            .filter(|rec| rec.type_code == REFERENCE_TYPE)
            .collect()
    }

    /// `true` if the records of this name map can be changed
    /// (`NameMapImpl.isWritable`); always `true` for a table name map.
    pub fn is_writable(&self) -> bool {
        true
    }

    /// `TableNameMap.addColumn`: adds a column record for the given name and
    /// guid and returns it.
    ///
    /// Exactly like `TableNameMapImpl.addColumn`, the record is inserted just
    /// before the end record (at the end of a map without one), its parent guid
    /// is the table record's guid and the guid is stored in the name map's
    /// byte order.  A map without a table record is Java's
    /// `IllegalStateException` ("Name map has no table record"), an invalid
    /// guid is its `IllegalArgumentException` ("Invalid GUID: `<guid>`").
    pub fn add_column(&mut self, name: &str, guid: &str) -> Result<Record> {
        let table_guid = self
            .get_table_record()
            .map(|record| *record.guid_bytes())
            .ok_or_else(|| Error::IllegalState("Name map has no table record".to_string()))?;
        let column_guid = to_guid_bytes(guid)?;
        let record = Record::new(COLUMN_TYPE, column_guid, table_guid, name.to_string(), true);
        let index = self
            .map
            .records
            .iter()
            .position(|rec| rec.type_code == END_TYPE)
            .unwrap_or(self.map.records.len());
        self.map.records.insert(index, record.clone());
        Ok(record)
    }

    /// `TableNameMap.removeColumn`: removes and returns the column record with
    /// the given name (case insensitive), or `None` when there is none.
    pub fn remove_column(&mut self, name: &str) -> Option<Record> {
        let index = self.map.records.iter().position(|rec| {
            (rec.type_code == COLUMN_TYPE) && java_equals_ignore_case(&rec.name, name)
        })?;
        Some(self.map.records.remove(index))
    }

    /// `TableNameMap.save`: writes this name map back into the table's
    /// [`crate::property::NAME_MAP_PROP`] property and saves the table's
    /// property group (`TableNameMapImpl.save`: `getProperties().put(...)`
    /// then `save()`).
    ///
    /// The table definition page identifies the table whose property group is
    /// written; [`TableNameMap::save_for_table`] resolves it from a name.
    pub fn save(&self, db: &mut crate::database::Database, table_page: i32) -> Result<()> {
        let bytes = self.map.to_bytes();
        let mut maps = db.get_table_properties(table_page)?;
        maps.get_default().put_value(
            crate::property::NAME_MAP_PROP,
            crate::value::Value::Bytes(bytes.clone()),
        )?;
        db.save_properties(&mut maps)?;
        // Java's cached `TableNameMapImpl` *is* the object which was saved, so
        // `Table.getNameMap()` keeps returning the changed map.  The port caches
        // the encoded bytes on the table instead, so refresh them to keep the
        // same observable behaviour without re-reading the property.
        if let Some(table) = db.tables.get_mut(&table_page) {
            table.name_map_blob = Some(bytes);
        }
        Ok(())
    }

    /// [`TableNameMap::save`] for a table addressed by name.
    pub fn save_for_table(
        &self,
        db: &mut crate::database::Database,
        table_name: &str,
    ) -> Result<()> {
        let page = db.table_page(table_name)?;
        self.save(db, page)
    }

    /// Compares the table and column records with the table which holds this
    /// name map (`TableNameMapImpl.findMismatches`).
    ///
    /// `properties` must be the table's property map group (what
    /// [`crate::database::Database::get_table_properties_by_name`] returns):
    /// the table's own guid lives in the object level map and each column's
    /// guid in its column map.
    pub fn find_mismatches(
        &self,
        table: &Table,
        properties: &PropertyMaps,
    ) -> Result<Vec<Mismatch>> {
        let table_guid = properties.default_map().and_then(property_guid);
        let columns: Vec<(String, Option<String>)> = table
            .columns
            .iter()
            .map(|col| {
                let guid = properties.get(&col.name).and_then(property_guid);
                (col.name.clone(), guid)
            })
            .collect();
        Ok(self.find_mismatches_for(&table.name, table_guid.as_deref(), &columns))
    }

    /// The mismatch computation over a table name and its columns, which
    /// [`Self::find_mismatches`] feeds from a [`Table`].
    pub(crate) fn find_mismatches_for(
        &self,
        table_name: &str,
        table_guid: Option<&str>,
        columns: &[(String, Option<String>)],
    ) -> Vec<Mismatch> {
        let mut mismatches = Vec::new();

        let table_rec_index = self
            .map
            .records
            .iter()
            .position(|rec| rec.type_code == TABLE_TYPE);
        if let Some(idx) = table_rec_index {
            let table_rec = &self.map.records[idx];
            let rec_guid = table_rec.get_guid();
            if let (Some(guid), Some(rec_guid)) = (table_guid, rec_guid.as_deref()) {
                if !java_equals_ignore_case(guid, rec_guid) {
                    mismatches.push(Mismatch::new(
                        MismatchType::GuidDiffers,
                        Some(table_name.to_string()),
                        None,
                        None,
                        Some(table_rec.clone()),
                    ));
                }
            }
            if table_rec.name != table_name {
                mismatches.push(Mismatch::new(
                    MismatchType::NameDiffers,
                    Some(table_name.to_string()),
                    None,
                    None,
                    Some(table_rec.clone()),
                ));
            }
        }

        // every guid is matched before any name, so a record which a column
        // claims by guid is not given to another column of the same name
        let mut matched = vec![false; self.map.records.len()];
        let mut unmatched_cols: Vec<&(String, Option<String>)> = Vec::new();
        for col in columns {
            let rec_index = col
                .1
                .as_deref()
                .and_then(|guid| self.find_record_index(guid));
            let rec_index = rec_index.filter(|idx| self.map.records[*idx].type_code == COLUMN_TYPE);
            let Some(idx) = rec_index else {
                unmatched_cols.push(col);
                continue;
            };
            matched[idx] = true;
            let rec = &self.map.records[idx];
            if rec.name != col.0 {
                mismatches.push(Mismatch::new(
                    MismatchType::NameDiffers,
                    Some(table_name.to_string()),
                    None,
                    Some(col.0.clone()),
                    Some(rec.clone()),
                ));
            }
        }

        for col in unmatched_cols {
            let rec_index = self.find_unmatched_column_index(&col.0, &matched);
            let Some(idx) = rec_index else {
                mismatches.push(Mismatch::new(
                    MismatchType::MissingRecord,
                    Some(table_name.to_string()),
                    None,
                    Some(col.0.clone()),
                    None,
                ));
                continue;
            };
            matched[idx] = true;
            if col.1.is_some() {
                mismatches.push(Mismatch::new(
                    MismatchType::GuidDiffers,
                    Some(table_name.to_string()),
                    None,
                    Some(col.0.clone()),
                    Some(self.map.records[idx].clone()),
                ));
            }
        }

        for (idx, rec) in self.map.records.iter().enumerate() {
            if (rec.type_code == COLUMN_TYPE) && !matched[idx] {
                mismatches.push(Mismatch::new(
                    MismatchType::MissingObject,
                    Some(table_name.to_string()),
                    None,
                    None,
                    Some(rec.clone()),
                ));
            }
        }

        mismatches
    }

    fn find_record_index(&self, guid: &str) -> Option<usize> {
        self.map.records.iter().position(|rec| {
            ((rec.type_code == TABLE_TYPE) || (rec.type_code == COLUMN_TYPE))
                && rec
                    .get_guid()
                    .is_some_and(|rec_guid| java_equals_ignore_case(&rec_guid, guid))
        })
    }

    fn find_unmatched_column_index(&self, name: &str, matched: &[bool]) -> Option<usize> {
        self.map.records.iter().enumerate().position(|(idx, rec)| {
            (rec.type_code == COLUMN_TYPE)
                && !matched[idx]
                && java_equals_ignore_case(&rec.name, name)
        })
    }
}

impl std::ops::Deref for TableNameMap {
    type Target = NameMap;

    fn deref(&self) -> &NameMap {
        &self.map
    }
}

/// The name map of a query (`QueryNameMapImpl`), stored in the query's
/// [`crate::property::QUERY_NAME_MAP_PROP`] property.  Its records cannot be
/// changed (see [`Record::set_name`]).
///
/// The Java class also resolves its records against the database
/// (`findMismatches`); the Rust crate has no query property integration yet,
/// so this port exposes the decoded records only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryNameMap {
    map: NameMap,
}

impl QueryNameMap {
    /// Decodes the value of the name map property of a query
    /// (`QueryNameMapImpl.read`).
    pub fn read(bytes: &[u8]) -> Result<QueryNameMap> {
        let mut map = NameMap::read(bytes)?;
        for rec in &mut map.records {
            rec.writable = false;
        }
        Ok(QueryNameMap { map })
    }

    /// The underlying record container.
    pub fn name_map(&self) -> &NameMap {
        &self.map
    }

    /// `true` if the records of this name map can be changed
    /// (`NameMapImpl.isWritable`); always `false` for a query name map.
    pub fn is_writable(&self) -> bool {
        false
    }
}

impl std::ops::Deref for QueryNameMap {
    type Target = NameMap;

    fn deref(&self) -> &NameMap {
        &self.map
    }
}

/// `NameMapImpl.getGuid(PropertyMap)`: the guid in the given properties, or
/// `None` if there is none.  Java only accepts a `String` value.
fn property_guid(map: &PropertyMap) -> Option<String> {
    match map.get_value(GUID_PROP) {
        Some(crate::value::Value::Text(guid)) => Some(guid.clone()),
        _ => None,
    }
}

/// `String.equalsIgnoreCase` (approximately: the port uses the simple Unicode
/// lower case mapping, which agrees with Java for the names in a database).
pub(crate) fn java_equals_ignore_case(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let mut a_chars = a.chars();
    let mut b_chars = b.chars();
    loop {
        match (a_chars.next(), b_chars.next()) {
            (None, None) => return true,
            (Some(c1), Some(c2)) => {
                if c1 == c2 {
                    continue;
                }
                let mut c1_upper = c1.to_uppercase();
                let mut c2_upper = c2.to_uppercase();
                let (u1, u2) = (c1_upper.next(), c2_upper.next());
                if (u1 == u2) && c1_upper.next().is_none() && c2_upper.next().is_none() {
                    continue;
                }
                let l1 = c1.to_lowercase().next();
                let l2 = c2.to_lowercase().next();
                if l1 == l2 {
                    continue;
                }
                return false;
            }
            _ => return false,
        }
    }
}

fn read_i32(bytes: &[u8], pos: usize) -> i32 {
    i32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap())
}

fn read_name(bytes: &[u8], mut pos: usize) -> Result<(String, usize)> {
    let mut units: Vec<u16> = Vec::new();
    while bytes.len() - pos >= 2 {
        let unit = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
        pos += 2;
        if unit == 0 {
            return Ok((String::from_utf16_lossy(&units), pos));
        }
        units.push(unit);
    }
    Err(Error::Jackcess("Name map ends inside a name".to_string()))
}

fn unknown_type(bytes: &[u8]) -> Error {
    let size = bytes.len().min(MAGIC_LENGTH);
    let hex: Vec<String> = bytes[..size].iter().map(|b| format!("{b:02X}")).collect();
    Error::Jackcess(format!("Unknown name map type {}", hex.join(" ")))
}

fn record_end(name: &str) -> Error {
    Error::Jackcess(format!("Name map record {name} has an unexpected end"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    fn put_name(out: &mut Vec<u8>, name: &str) {
        for unit in name.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }

    fn guid_bytes(guid: &str) -> [u8; 16] {
        let hex: String = guid.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        let mut b = [0u8; 16];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        b[0..4].reverse();
        b[4..6].reverse();
        b[6..8].reverse();
        b
    }

    fn push_record(
        out: &mut Vec<u8>,
        type_code: i32,
        guid: [u8; 16],
        payload: [u8; 16],
        name: &str,
    ) {
        out.extend_from_slice(&guid);
        out.extend_from_slice(&type_code.to_le_bytes());
        out.extend_from_slice(&payload);
        put_name(out, name);
    }

    fn header(reserved: Option<i32>) -> Vec<u8> {
        let mut out = MAGIC.to_le_bytes().to_vec();
        if let Some(reserved) = reserved {
            out.extend_from_slice(&reserved.to_le_bytes());
        }
        out
    }

    const TABLE_GUID: &str = "{11111111-2222-3333-4444-555555555555}";
    const COL_GUID: &str = "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}";

    fn sample_map() -> Vec<u8> {
        let mut out = header(Some(0x0A0B_0C0D));
        let table_guid = guid_bytes(TABLE_GUID);
        let mut table_payload = [0u8; 16];
        table_payload[0..8].copy_from_slice(&0f64.to_le_bytes());
        push_record(&mut out, TABLE_TYPE, table_guid, table_payload, "Full");
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut ref_payload = [0u8; 16];
        ref_payload.copy_from_slice(&guid_bytes("{99999999-8888-7777-6666-555555555555}"));
        push_record(&mut out, REFERENCE_TYPE, [0u8; 16], ref_payload, "Lookup😀");
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut col_payload = [0u8; 16];
        col_payload.copy_from_slice(&table_guid);
        push_record(
            &mut out,
            COLUMN_TYPE,
            guid_bytes(COL_GUID),
            col_payload,
            "Näme",
        );
        out.extend_from_slice(&0u32.to_le_bytes());
        push_record(&mut out, 9, [0u8; 16], [0u8; 16], "Mystery");
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut end_payload = [0u8; 16];
        end_payload[0..4].copy_from_slice(&0x1234_5678i32.to_le_bytes());
        push_record(&mut out, END_TYPE, [0u8; 16], end_payload, "");
        out
    }

    fn sample_table_map() -> TableNameMap {
        // two records need a separator zero between them
        let mut out = header(Some(0));
        let table_guid = guid_bytes(TABLE_GUID);
        let mut table_payload = [0u8; 16];
        table_payload[0..8].copy_from_slice(&0f64.to_le_bytes());
        push_record(&mut out, TABLE_TYPE, table_guid, table_payload, "Full");
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut col_payload = [0u8; 16];
        col_payload.copy_from_slice(&table_guid);
        push_record(
            &mut out,
            COLUMN_TYPE,
            guid_bytes(COL_GUID),
            col_payload,
            "Näme",
        );
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut end_payload = [0u8; 16];
        end_payload[0..4].copy_from_slice(&7i32.to_le_bytes());
        push_record(&mut out, END_TYPE, [0u8; 16], end_payload, "");
        TableNameMap::read(&out).unwrap()
    }

    #[test]
    fn parses_records_and_lookups() {
        let map = sample_table_map();
        assert_eq!(map.name_map().reserved(), Some(0));
        assert_eq!(map.records().len(), 3);
        assert_eq!(map.get_table_records().len(), 1);
        assert_eq!(map.get_column_records().len(), 1);
        assert_eq!(map.get_writer_tag(), Some(7));
        assert_eq!(
            map.get_table_record().unwrap().get_guid().as_deref(),
            Some(TABLE_GUID)
        );
        assert_eq!(map.get_table_record().unwrap().get_name(), "Full");
        assert_eq!(
            map.get_column_record("nÄME").unwrap().get_guid().as_deref(),
            Some(COL_GUID)
        );
        assert_eq!(
            map.get_column_record("Näme")
                .unwrap()
                .get_parent_guid()
                .as_deref(),
            Some(TABLE_GUID)
        );
        assert!(map.get_column_record("missing").is_none());
        assert!(map.contains(COL_GUID));
        assert!(map.contains(&COL_GUID.to_lowercase()));
        assert!(!map.contains("{00000000-0000-0000-0000-000000000000}"));
    }

    #[test]
    fn parses_unicode_and_unknown_records() {
        let bytes = sample_map();
        let map = TableNameMap::read(&bytes).unwrap();
        assert_eq!(map.name_map().reserved(), Some(0x0A0B_0C0D));
        assert_eq!(map.records().len(), 5);
        let refs = map.get_references();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].get_name(), "Lookup😀");
        assert_eq!(
            refs[0].get_parent_guid().as_deref(),
            Some("{99999999-8888-7777-6666-555555555555}")
        );
        let unknown = &map.records()[3];
        assert_eq!(unknown.get_type(), RecordType::Unknown);
        assert_eq!(unknown.type_code(), 9);
        assert_eq!(unknown.get_guid(), None);
        let end = &map.records()[4];
        assert_eq!(end.get_type(), RecordType::End);
        assert_eq!(end.get_guid(), None);
        assert_eq!(map.get_writer_tag(), Some(0x1234_5678));
    }

    #[test]
    fn round_trips_bytes() {
        let bytes = sample_map();
        let map = NameMap::read(&bytes).unwrap();
        assert_eq!(map.to_bytes(), bytes);
        let magic_only = NameMap::read(&MAGIC.to_le_bytes()).unwrap();
        assert_eq!(magic_only.to_bytes(), MAGIC.to_le_bytes());
    }

    #[test]
    fn table_record_date_is_decoded() {
        let mut out = header(Some(0));
        let mut payload = [0u8; 16];
        let date = LocalDateTime::new(2024, 5, 6, 7, 8, 9, 500_000_000);
        payload[0..8].copy_from_slice(&crate::datenums::to_date_double(date).to_le_bytes());
        push_record(&mut out, TABLE_TYPE, [1u8; 16], payload, "Dated");
        let map = NameMap::read(&out).unwrap();
        assert_eq!(
            map.records()[0].get_date(),
            Some(LocalDateTime::new(2024, 5, 6, 7, 8, 9, 500_000_000))
        );
    }

    #[test]
    fn malformed_errors_match_java() {
        // shorter than the magic
        assert_eq!(
            NameMap::read(&[]).unwrap_err().to_string(),
            "Unknown name map type "
        );
        assert_eq!(
            NameMap::read(&[0x01, 0x02]).unwrap_err().to_string(),
            "Unknown name map type 01 02"
        );
        // wrong magic
        assert_eq!(
            NameMap::read(&[0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0])
                .unwrap_err()
                .to_string(),
            "Unknown name map type 11 22 33 44"
        );
        // magic followed by a cut short reserved word
        let mut short = MAGIC.to_le_bytes().to_vec();
        short.extend_from_slice(&[0xAA, 0xBB]);
        assert_eq!(
            NameMap::read(&short).unwrap_err().to_string(),
            "Unknown name map type 0A CC 0E 55"
        );
        // magic + reserved + part of a record
        let mut out = header(Some(0));
        out.extend_from_slice(&[0u8; 10]);
        assert_eq!(
            NameMap::read(&out).unwrap_err().to_string(),
            "Name map ends inside a record"
        );
        // fixed record without any name
        let mut out = header(Some(0));
        push_record(&mut out, TABLE_TYPE, [0u8; 16], [0u8; 16], "");
        let no_name = &out[..out.len() - 2];
        assert_eq!(
            NameMap::read(no_name).unwrap_err().to_string(),
            "Name map ends inside a name"
        );
        // odd number of name bytes left
        let mut out = header(Some(0));
        out.extend_from_slice(&[0u8; 36]);
        out.extend_from_slice(&[0x41, 0x00, 0x42]);
        assert_eq!(
            NameMap::read(&out).unwrap_err().to_string(),
            "Name map ends inside a name"
        );
        // name without terminator
        let mut out = header(Some(0));
        out.extend_from_slice(&[0u8; 36]);
        out.extend_from_slice(&[0x41, 0x00, 0x42, 0x00]);
        assert_eq!(
            NameMap::read(&out).unwrap_err().to_string(),
            "Name map ends inside a name"
        );
        // a zero which ends the data could not be written back
        let mut out = header(Some(0));
        push_record(&mut out, TABLE_TYPE, [0u8; 16], [0u8; 16], "A");
        out.extend_from_slice(&[0u8; 4]);
        assert_eq!(
            NameMap::read(&out).unwrap_err().to_string(),
            "Name map record A has an unexpected end"
        );
        // a cut short trailer
        let mut out = header(Some(0));
        push_record(&mut out, TABLE_TYPE, [0u8; 16], [0u8; 16], "A");
        out.extend_from_slice(&[0u8; 2]);
        assert_eq!(
            NameMap::read(&out).unwrap_err().to_string(),
            "Name map record A has an unexpected end"
        );
        // a non zero trailer
        let mut out = header(Some(0));
        push_record(&mut out, TABLE_TYPE, [0u8; 16], [0u8; 16], "A");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        assert_eq!(
            NameMap::read(&out).unwrap_err().to_string(),
            "Name map record A has an unexpected end"
        );
    }

    #[test]
    fn query_name_map_is_read_only() {
        let mut out = header(Some(0));
        push_record(&mut out, TABLE_TYPE, [0u8; 16], [0u8; 16], "Q");
        let map = QueryNameMap::read(&out).unwrap();
        assert!(!map.is_writable());
        assert_eq!(map.records().len(), 1);
        let mut rec = map.records()[0].clone();
        assert!(matches!(
            rec.set_name("x").unwrap_err(),
            Error::Unsupported(_)
        ));
        assert_eq!(rec.get_name(), "Q");
        let table_map = sample_table_map();
        let mut rec = table_map.records()[1].clone();
        rec.set_name("oke").unwrap();
        assert_eq!(rec.get_name(), "oke");
    }

    #[test]
    fn table_mismatches_are_ordered_like_java() {
        let mut out = header(Some(0));
        let table_guid = guid_bytes("{11111111-1111-1111-1111-111111111111}");
        push_record(&mut out, TABLE_TYPE, table_guid, [0u8; 16], "OldName");
        out.extend_from_slice(&0u32.to_le_bytes());
        let parent = [0u8; 16];
        push_record(
            &mut out,
            COLUMN_TYPE,
            guid_bytes("{AAAAAAAA-0000-0000-0000-000000000001}"),
            parent,
            "KeptOld",
        );
        out.extend_from_slice(&0u32.to_le_bytes());
        push_record(
            &mut out,
            COLUMN_TYPE,
            guid_bytes("{AAAAAAAA-0000-0000-0000-000000000002}"),
            parent,
            "Moved",
        );
        out.extend_from_slice(&0u32.to_le_bytes());
        push_record(
            &mut out,
            COLUMN_TYPE,
            guid_bytes("{AAAAAAAA-0000-0000-0000-000000000003}"),
            parent,
            "Gone",
        );
        let map = TableNameMap::read(&out).unwrap();

        let columns = vec![
            (
                "Kept".to_string(),
                Some("{AAAAAAAA-0000-0000-0000-000000000001}".to_string()),
            ),
            (
                "Moved".to_string(),
                Some("{AAAAAAAA-0000-0000-0000-000000000009}".to_string()),
            ),
            (
                "Dropped".to_string(),
                Some("{AAAAAAAA-0000-0000-0000-00000000000A}".to_string()),
            ),
        ];
        let mismatches = map.find_mismatches_for(
            "NewName",
            Some("{11111111-1111-1111-1111-111111111119}"),
            &columns,
        );
        let got: Vec<(&str, Option<&str>, Option<&str>)> = mismatches
            .iter()
            .map(|m| (m.get_type().name(), m.get_table(), m.get_column()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("GUID_DIFFERS", Some("NewName"), None),
                ("NAME_DIFFERS", Some("NewName"), None),
                ("NAME_DIFFERS", Some("NewName"), Some("Kept")),
                ("GUID_DIFFERS", Some("NewName"), Some("Moved")),
                ("MISSING_RECORD", Some("NewName"), Some("Dropped")),
                ("MISSING_OBJECT", Some("NewName"), None),
            ]
        );
    }

    #[test]
    fn empty_and_end_only_maps() {
        let empty = TableNameMap::read(&MAGIC.to_le_bytes()).unwrap();
        assert_eq!(empty.records().len(), 0);
        assert!(empty.get_table_record().is_none());
        assert_eq!(empty.get_writer_tag(), None);
        let mismatches = empty.find_mismatches_for(
            "T",
            None,
            &[("A".to_string(), None), ("B".to_string(), None)],
        );
        assert_eq!(mismatches.len(), 2);
        assert!(mismatches
            .iter()
            .all(|m| m.get_type() == MismatchType::MissingRecord));

        let mut out = header(Some(0));
        let mut payload = [0u8; 16];
        payload[0..4].copy_from_slice(&42i32.to_le_bytes());
        push_record(&mut out, END_TYPE, [0u8; 16], payload, "");
        let end_only = TableNameMap::read(&out).unwrap();
        assert_eq!(end_only.get_writer_tag(), Some(42));
        assert_eq!(
            end_only.find_mismatches_for("T", None, &[("A".to_string(), None)])[0].get_type(),
            MismatchType::MissingRecord
        );
    }

    fn test_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn add_remove_and_save_follow_table_name_map_impl() {
        use crate::column::ColumnSpec;
        use crate::datatype::DataType;
        use crate::format::FileFormat;
        use crate::tablecreator::TableSpec;

        let path = test_tmp_dir().join(format!(
            "jackcess_namemap_objapi_{}.accdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(20),
            ],
        );
        db.create_table(&spec).expect("create table");
        let page = db.get_table("T").unwrap().table_def_page_number;

        // `TableNameMapImpl.save`: the encoded map becomes the table's
        // `NameMap` property, and `Table.getNameMap` returns it
        let map = sample_table_map();
        map.save(&mut db, page).expect("save name map");
        let read_back = db
            .get_table("T")
            .unwrap()
            .get_name_map()
            .expect("read name map")
            .expect("name map present");
        assert_eq!(read_back.to_bytes(), map.to_bytes());
        assert!(read_back.is_writable());

        // addColumn inserts before the end record and uses the table record's
        // guid as the parent guid
        let mut map = read_back;
        let added_guid = "{AAAAAAAA-0000-0000-0000-00000000000A}";
        let added = map.add_column("Added", added_guid).expect("add column");
        assert_eq!(added.get_type(), RecordType::Column);
        assert_eq!(added.get_guid().as_deref(), Some(added_guid));
        assert_eq!(added.get_parent_guid().as_deref(), Some(TABLE_GUID));
        assert_eq!(added.get_name(), "Added");
        let types: Vec<RecordType> = map.records().iter().map(|r| r.get_type()).collect();
        assert_eq!(
            types,
            vec![
                RecordType::Table,
                RecordType::Column,
                RecordType::Column,
                RecordType::End
            ]
        );

        // removeColumn matches the name case insensitively
        let removed = map.remove_column("nÄME").expect("remove column");
        assert_eq!(removed.get_name(), "Näme");
        assert!(map.remove_column("Näme").is_none());
        assert!(map.get_column_record("Näme").is_none());

        map.save(&mut db, page).expect("save changed name map");
        let read_back = db.get_table("T").unwrap().get_name_map().unwrap().unwrap();
        assert_eq!(read_back.records().len(), 3);
        assert!(read_back.get_column_record("Added").is_some());
        assert!(read_back.get_column_record("Name").is_none());
        // the property value is the encoded map
        let props = db.get_table_properties(page).expect("properties");
        match props
            .default_map()
            .and_then(|m| m.get_value(crate::property::NAME_MAP_PROP))
        {
            Some(crate::value::Value::Bytes(bytes)) => assert_eq!(bytes, &map.to_bytes()),
            other => panic!("unexpected NameMap property: {other:?}"),
        }

        // a map without a table record cannot take a column record
        let mut no_table = TableNameMap::read(&header(Some(0))).unwrap();
        assert!(matches!(
            no_table.add_column("X", "{AAAAAAAA-0000-0000-0000-00000000000A}"),
            Err(Error::IllegalState(message)) if message == "Name map has no table record"
        ));
        // an invalid guid is Java's IllegalArgumentException
        assert!(matches!(
            map.add_column("X", "not-a-guid"),
            Err(Error::IllegalArgument(message)) if message == "Invalid GUID: not-a-guid"
        ));
        // the guid is stored in the name map byte order (first three
        // components little endian)
        let record = map
            .add_column("GuidCheck", "{01020304-0506-0708-090A-0B0C0D0E0F10}")
            .expect("add guid column");
        assert_eq!(
            record.guid_bytes(),
            &[
                0x04, 0x03, 0x02, 0x01, 0x06, 0x05, 0x08, 0x07, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
                0x0F, 0x10
            ]
        );

        let _ = std::fs::remove_file(&path);
    }
}
