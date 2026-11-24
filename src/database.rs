//! Port of `com.healthmarketscience.jackcess.impl.DatabaseImpl`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::column::ColumnSpec;
use crate::datatype::DataType;
use crate::error::{Error, Result};
use crate::format::{Charset, FileFormat, JetFormat, SortOrder};
use crate::linked::{LinkResolver, LinkedTableInfo};
use crate::pagechannel::{FileChannel, PageChannel};
use crate::table::{ErrorHandler, Table};
use crate::value::Value;

/// System catalog always lives on page 2
pub const PAGE_SYSTEM_CATALOG: i32 = 2;
pub const TABLE_SYSTEM_CATALOG: &str = "MSysObjects";
pub const TABLE_SYSTEM_ACES: &str = "MSysACEs";
pub const TABLE_SYSTEM_RELATIONSHIPS: &str = "MSysRelationships";
pub const TABLE_SYSTEM_QUERIES: &str = "MSysQueries";
pub const TABLE_SYSTEM_COMPLEX_COLS: &str = "MSysComplexColumns";

pub const SYSTEM_OBJECT_FLAG: i32 = 0x80000000u32 as i32;
pub const ALT_SYSTEM_OBJECT_FLAG: i32 = 0x02;
pub const HIDDEN_OBJECT_FLAG: i32 = 0x08;
pub const SYSTEM_OBJECT_FLAGS: i32 = SYSTEM_OBJECT_FLAG | ALT_SYSTEM_OBJECT_FLAG;

pub const TYPE_TABLE: i16 = 1;
pub const TYPE_LINKED_ODBC_TABLE: i16 = 4;
pub const TYPE_QUERY: i16 = 5;
pub const TYPE_LINKED_TABLE: i16 = 6;
pub const TYPE_RELATIONSHIP: i16 = 8;

/// top-level parentid for a database
pub const DB_PARENT_ID: i32 = 0xF000000;

const SYS_DEFAULT_SID: [u8; 2] = [0xA6, 0x33];
#[expect(
    dead_code,
    reason = "documents the full access mask (0xFFFFF) the Java library writes into MSysACEs; the ACE writer lives in tablecreator"
)]
const SYS_FULL_ACCESS_ACM: i32 = 1048575;

const CAT_COL_ID: &str = "Id";
const CAT_COL_NAME: &str = "Name";
const CAT_COL_OWNER: &str = "Owner";
const CAT_COL_PARENT_ID: &str = "ParentId";
const CAT_COL_TYPE: &str = "Type";
const CAT_COL_DATE_CREATE: &str = "DateCreate";
const CAT_COL_DATE_UPDATE: &str = "DateUpdate";
const CAT_COL_FLAGS: &str = "Flags";
const CAT_COL_DATABASE: &str = "Database";
const CAT_COL_FOREIGN_NAME: &str = "ForeignName";
const CAT_COL_CONNECT: &str = "Connect";
/// `DatabaseImpl.CAT_COL_PROPS`: the property blob column of `MSysObjects`.
const CAT_COL_PROPS: &str = "LvProp";

/// The names of the database level catalog objects which hold property groups
/// (`DatabaseImpl.OBJECT_NAME_DB_PROPS` and friends).
const OBJECT_NAME_DB_PROPS: &str = "MSysDb";
const OBJECT_NAME_SUMMARY_PROPS: &str = "SummaryInfo";
const OBJECT_NAME_USERDEF_PROPS: &str = "UserDefined";

const ACE_COL_ACM: &str = "ACM";
const ACE_COL_F_INHERITABLE: &str = "FInheritable";
const ACE_COL_OBJECT_ID: &str = "ObjectId";
const ACE_COL_SID: &str = "SID";

/// `MSysRelationships` column name of the column count
pub(crate) const REL_COL_COLUMN_COUNT: &str = "ccolumn";
/// `MSysRelationships` column name of the flags
pub(crate) const REL_COL_FLAGS: &str = "grbit";
/// `MSysRelationships` column name of the index of the columns
pub(crate) const REL_COL_COLUMN_INDEX: &str = "icolumn";
/// `MSysRelationships` column name of the "to" column name
pub(crate) const REL_COL_TO_COLUMN: &str = "szColumn";
/// `MSysRelationships` column name of the "to" table name
pub(crate) const REL_COL_TO_TABLE: &str = "szObject";
/// `MSysRelationships` column name of the "from" column name
pub(crate) const REL_COL_FROM_COLUMN: &str = "szReferencedColumn";
/// `MSysRelationships` column name of the "from" table name
pub(crate) const REL_COL_FROM_TABLE: &str = "szReferencedObject";
/// `MSysRelationships` column name of the relationship
pub(crate) const REL_COL_NAME: &str = "szRelationship";

/// The properties which are applied while a database is created, the port of
/// `DatabaseBuilder.putDatabaseProperty` / `putSummaryProperty` /
/// `putUserDefinedProperty` (whose `create()` puts the properties into the
/// corresponding property map and saves it).
///
/// Each list is applied by [`Database::create_with_properties`] in the Java
/// order (database properties, then summary properties, then user defined
/// properties).  The Java builder collects the properties in a `HashMap`, so
/// each group is written in that map's iteration order; see
/// [`crate::property::java_hashmap_order`].
#[derive(Clone, Debug, Default)]
pub struct DatabaseProperties {
    pub database: Vec<crate::property::Property>,
    pub summary: Vec<crate::property::Property>,
    pub user_defined: Vec<crate::property::Property>,
}

impl DatabaseProperties {
    pub fn new() -> DatabaseProperties {
        DatabaseProperties::default()
    }

    /// `DatabaseBuilder.putDatabaseProperty(name, value)`.
    pub fn put_database_property(
        mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<DatabaseProperties> {
        Self::put(&mut self.database, name.into(), None, value)?;
        Ok(self)
    }

    /// `DatabaseBuilder.putDatabaseProperty(name, type, value)`.
    pub fn put_database_property_typed(
        mut self,
        name: impl Into<String>,
        data_type: DataType,
        value: Value,
    ) -> DatabaseProperties {
        Self::put_typed(&mut self.database, name.into(), data_type, value);
        self
    }

    /// `DatabaseBuilder.putSummaryProperty(name, value)`.
    pub fn put_summary_property(
        mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<DatabaseProperties> {
        Self::put(&mut self.summary, name.into(), None, value)?;
        Ok(self)
    }

    /// `DatabaseBuilder.putSummaryProperty(name, type, value)`.
    pub fn put_summary_property_typed(
        mut self,
        name: impl Into<String>,
        data_type: DataType,
        value: Value,
    ) -> DatabaseProperties {
        Self::put_typed(&mut self.summary, name.into(), data_type, value);
        self
    }

    /// `DatabaseBuilder.putUserDefinedProperty(name, value)`.
    pub fn put_user_defined_property(
        mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<DatabaseProperties> {
        Self::put(&mut self.user_defined, name.into(), None, value)?;
        Ok(self)
    }

    /// `DatabaseBuilder.putUserDefinedProperty(name, type, value)`.
    pub fn put_user_defined_property_typed(
        mut self,
        name: impl Into<String>,
        data_type: DataType,
        value: Value,
    ) -> DatabaseProperties {
        Self::put_typed(&mut self.user_defined, name.into(), data_type, value);
        self
    }

    fn put(
        props: &mut Vec<crate::property::Property>,
        name: String,
        data_type: Option<DataType>,
        value: Value,
    ) -> Result<()> {
        let prop = crate::property::create_property_typed(&name, data_type, value)?;
        crate::property::put_builder_property(props, prop);
        Ok(())
    }

    fn put_typed(
        props: &mut Vec<crate::property::Property>,
        name: String,
        data_type: DataType,
        value: Value,
    ) {
        let prop = crate::property::Property::new(name, data_type, value);
        crate::property::put_builder_property(props, prop);
    }
}

/// `Table.ColumnOrder`: the order in which `Table.getColumns` hands the
/// columns out.  jackcess' default is [`ColumnOrder::Data`], which keeps the
/// order the columns have in the table definition.  The order is only a view
/// of the columns; it never changes the layout of a stored row.
///
/// Java applies the database's order once, while constructing the `TableImpl`
/// (`TableImpl`'s constructor sorts its `_columns` list with
/// `ColumnImpl.DISPLAY_ORDER_COMPARATOR` when the order is not `DATA`), so
/// changing [`Database::set_column_order`] does not re-order a table which is
/// already loaded.  [`Table::get_columns`] reproduces that with
/// [`Table::column_order`].
///
/// Note that jackcess 5.0.4 assigns `ColumnImpl.getDisplayIndex()` from the
/// order in which the column definitions are read
/// (`TableImpl.readColumnDefinitions` passes `dispIndex++`), and never reads
/// the `ColumnOrder` *column* property (`PropertyMap.COLUMN_ORDER_PROP`) for
/// it, so `DISPLAY` yields the same order as `DATA` for every database.  The
/// port keeps that assignment and view, so both settings match Java (the
/// `ColumnOrder` property is still written and read as an ordinary property).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColumnOrder {
    /// columns ordered by their position in the table data (`DATA`)
    #[default]
    Data,
    /// columns ordered by their "display" index (`DISPLAY`)
    Display,
}

impl ColumnOrder {
    /// The Java enum constant name (`ColumnOrder.name()`).
    pub fn name(self) -> &'static str {
        match self {
            ColumnOrder::Data => "DATA",
            ColumnOrder::Display => "DISPLAY",
        }
    }

    /// `ColumnOrder.valueOf(name)`, case insensitively; anything else is
    /// `DATA` (the default).
    pub fn from_name(name: &str) -> ColumnOrder {
        if name.trim().eq_ignore_ascii_case("DISPLAY") {
            ColumnOrder::Display
        } else {
            ColumnOrder::Data
        }
    }
}

/// `TableMetaData.Type`: the catalog object type of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TableMetaDataType {
    /// a normal (local) table, catalog `Type == 1`
    Local,
    /// a linked table, catalog `Type == 6`
    Linked,
    /// a linked ODBC table, catalog `Type == 4`
    LinkedOdbc,
}

impl TableMetaDataType {
    /// The Java enum constant name (`LOCAL`, `LINKED`, `LINKED_ODBC`).
    pub fn name(self) -> &'static str {
        match self {
            TableMetaDataType::Local => "LOCAL",
            TableMetaDataType::Linked => "LINKED",
            TableMetaDataType::LinkedOdbc => "LINKED_ODBC",
        }
    }
}

impl std::fmt::Display for TableMetaDataType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// `com.healthmarketscience.jackcess.TableMetaData`: the catalog information
/// of a table, available without loading the table definition.
///
/// The port also carries the catalog `DateCreate`/`DateUpdate` values and the
/// object flags, which Java only exposes through the opened `Table` (and which
/// its `TableMetaData` interface does not expose at all).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableMetaData {
    /// `TableMetaData.getType()`.
    pub table_type: TableMetaDataType,
    /// `TableMetaData.getName()`: the name as stored in the catalog.
    pub name: String,
    /// the catalog `Flags` value (`DatabaseImpl.TableInfo.flags`).
    pub flags: i32,
    /// the catalog `Id`: a table definition page for a local/ODBC table, a
    /// synthetic object id for a linked table.
    pub page_number: Option<i32>,
    /// `TableMetaData.getLinkedTableName()` (`ForeignName`).
    pub linked_table_name: Option<String>,
    /// `TableMetaData.getLinkedDbName()` (`Database`).
    pub linked_db_name: Option<String>,
    /// `TableMetaData.getConnectionName()` (`Connect`), ODBC only.
    pub connection_name: Option<String>,
    /// the catalog `DateCreate` value.
    pub created_date: Option<crate::datenums::LocalDateTime>,
    /// the catalog `DateUpdate` value.
    pub updated_date: Option<crate::datenums::LocalDateTime>,
}

impl TableMetaData {
    /// `TableMetaData.isLinked()`: `true` for both linked types.
    pub fn is_linked(&self) -> bool {
        matches!(
            self.table_type,
            TableMetaDataType::Linked | TableMetaDataType::LinkedOdbc
        )
    }

    /// `TableMetaData.isSystem()`: whether the catalog flags mark a system
    /// object.
    pub fn is_system(&self) -> bool {
        (self.flags & SYSTEM_OBJECT_FLAGS) != 0
    }

    /// `TableMetaData.open(Database)`: opens this table from the given
    /// database instance.
    ///
    /// Java's implementations:
    ///
    /// * `LOCAL` (`DatabaseImpl.TableInfo.open`): loads the local table
    ///   definition page, system tables included;
    /// * `LINKED` (`TableInfo.open` plus the `getTable` linked branch): resolves
    ///   the linkee database and opens the table there (the linkee's instance is
    ///   returned, like Java);
    /// * `LINKED_ODBC` (`LinkedODBCTableInfo.open`): returns `null`, because the
    ///   rows of an ODBC table live in the remote database.  The port returns
    ///   `None` for Java's `null`.
    pub fn open<'a>(&self, db: &'a mut Database) -> Result<Option<&'a mut Table>> {
        match self.table_type {
            TableMetaDataType::LinkedOdbc => Ok(None),
            TableMetaDataType::Linked => {
                let linked_db_name = match self.linked_db_name.as_deref() {
                    Some(name) => name.to_string(),
                    None => return Ok(None),
                };
                let linked_table_name = match self.linked_table_name.as_deref() {
                    Some(name) => name.to_string(),
                    None => return Ok(None),
                };
                let linked_db = db.get_or_open_linked_database(&linked_db_name)?;
                Ok(Some(linked_db.get_table(&linked_table_name)?))
            }
            TableMetaDataType::Local => {
                let page = match self.page_number {
                    Some(page) if page > 0 => page,
                    _ => return Ok(None),
                };
                Ok(Some(db.load_table(page, &self.name, self.flags)?))
            }
        }
    }

    /// `TableMetaData.getTableDefinition(Database)`: the local table definition
    /// of this table if available.
    ///
    /// Java's implementations only return a definition for `LINKED_ODBC` tables
    /// (their local definition page), and `null` for `LOCAL` and `LINKED`
    /// tables; the port returns the local [`Table`] for an ODBC row whose
    /// catalog `Id` is a table definition page (Java's
    /// `(pageNumber != null) && (pageNumber > 0)` test) and `None` otherwise.
    /// Java models a table definition with the `TableDefinition` interface,
    /// which `TableImpl` implements; the port's [`Table`] is the equivalent, so
    /// this returns the local definition as a [`Table`].
    pub fn get_table_definition<'a>(&self, db: &'a mut Database) -> Result<Option<&'a mut Table>> {
        match self.table_type {
            TableMetaDataType::LinkedOdbc => match self.page_number {
                Some(page) if page > 0 => Ok(Some(db.load_table(page, &self.name, self.flags)?)),
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }
}

/// `Database.getTimeZone()` / `Database.getZoneId()` as one value: the zone
/// used by every date conversion of the database.
///
/// Java keeps a `java.util.TimeZone` and a `java.time.ZoneId` which are kept
/// aligned; this port keeps both in [`crate::datenums::TimeZone`], so
/// [`Database::get_zone_info`] reports the id ([`Database::get_zone_id`]) and
/// the offsets together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneInfo {
    /// `ZoneId.getId()` (`"UTC"`, `"GMT+05:30"`, `"America/New_York"`).
    pub id: String,
    /// `TimeZone.getRawOffset()` in minutes (the standard offset, without
    /// daylight saving time).
    pub offset_minutes: i32,
    /// `TimeZone.getRawOffset()` in milliseconds.
    pub raw_offset_ms: i64,
    /// `true` for an IANA zone with a transition table, `false` for a fixed
    /// offset zone.
    pub is_named: bool,
}

impl ZoneInfo {
    /// `Database.getZoneId().getId()`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// `TimeZone.getRawOffset() / 60000` (the Java `TimeZone` reports
    /// milliseconds).
    pub fn offset_minutes(&self) -> i32 {
        self.offset_minutes
    }
}

/// `Database.newTableMetaDataIterable()`: the Rust shape of Java's
/// `Iterable<TableMetaData>`.
///
/// The iterable owns the metadata of every table (the port materializes the
/// catalog scan, see [`Database::new_table_meta_data_iterable`]), so it can be
/// iterated without holding a `&mut Database`.  Iteration order is the index
/// order of the `(ParentId, Name)` `MSysObjects` index, exactly like the Java
/// cursor.
#[derive(Clone, Debug)]
pub struct TableMetaDataIterable {
    items: std::vec::IntoIter<TableMetaData>,
}

impl TableMetaDataIterable {
    fn new(items: Vec<TableMetaData>) -> TableMetaDataIterable {
        TableMetaDataIterable {
            items: items.into_iter(),
        }
    }

    /// The number of remaining entries (`Iterable` has no size in Java; this is
    /// the Rust convenience).
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// `true` when no entry remains.
    pub fn is_empty(&self) -> bool {
        self.items.len() == 0
    }
}

impl Iterator for TableMetaDataIterable {
    type Item = TableMetaData;

    fn next(&mut self) -> Option<TableMetaData> {
        self.items.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.items.size_hint()
    }
}

impl std::iter::FusedIterator for TableMetaDataIterable {}

impl<'a> IntoIterator for &'a TableMetaDataIterable {
    type Item = &'a TableMetaData;
    type IntoIter = std::slice::Iter<'a, TableMetaData>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.as_slice().iter()
    }
}

/// One row of `MSysACEs`, as `Database.getAccessControlEntries()`'s cursor
/// yields it: Java's `Row.get("ObjectId")` (`Integer`), `getBytes("SID")`
/// (`byte[]`), `getInt("ACM")` (`Integer`) and `get("FInheritable")`
/// (`Boolean`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AceEntry {
    pub object_id: i32,
    pub sid: Vec<u8>,
    pub acm: i32,
    pub f_inheritable: bool,
}

/// The catalog information of a type 4 (linked ODBC) table, mirroring
/// `DatabaseImpl.LinkedODBCTableInfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedOdbcTableInfo {
    /// the local table name
    pub name: String,
    /// the catalog `Id`: the table definition page of the local definition
    pub object_id: i32,
    /// the catalog `Flags`
    pub flags: i32,
    /// the catalog `Connect` value
    pub connection_name: String,
    /// the catalog `ForeignName` value: the remote table name
    pub linked_table_name: String,
}

/// The column indices of the `MSysObjects` columns the metadata APIs read.
#[derive(Clone, Copy, Debug, Default)]
struct CatalogColumns {
    id: Option<usize>,
    parent: Option<usize>,
    name: Option<usize>,
    typ: Option<usize>,
    flags: Option<usize>,
    database: Option<usize>,
    connect: Option<usize>,
    foreign_name: Option<usize>,
    date_create: Option<usize>,
    date_update: Option<usize>,
}

impl CatalogColumns {
    fn from_table(table: &Table) -> CatalogColumns {
        CatalogColumns {
            id: table.column_index(CAT_COL_ID),
            parent: table.column_index(CAT_COL_PARENT_ID),
            name: table.column_index(CAT_COL_NAME),
            typ: table.column_index(CAT_COL_TYPE),
            flags: table.column_index(CAT_COL_FLAGS),
            database: table.column_index(CAT_COL_DATABASE),
            connect: table.column_index(CAT_COL_CONNECT),
            foreign_name: table.column_index(CAT_COL_FOREIGN_NAME),
            date_create: table.column_index(CAT_COL_DATE_CREATE),
            date_update: table.column_index(CAT_COL_DATE_UPDATE),
        }
    }

    fn text(&self, row: &[Value], idx: Option<usize>) -> Option<String> {
        idx.and_then(|i| row.get(i))
            .and_then(|v| v.as_text())
            .map(|s| s.to_string())
    }

    fn i64(&self, row: &[Value], idx: Option<usize>) -> Option<i64> {
        idx.and_then(|i| row.get(i)).and_then(|v| v.as_i64())
    }

    fn date(
        &self,
        row: &[Value],
        idx: Option<usize>,
        tz: &crate::datenums::TimeZone,
    ) -> Option<crate::datenums::LocalDateTime> {
        match idx.and_then(|i| row.get(i)) {
            Some(Value::DateTime(value)) => Some(*value),
            // DATE mode reads the catalog date as the `DateExt` analogue;
            // Java's `DatabaseImpl.getDateForObject` converts it with
            // `ColumnImpl.toLocalDateTime(DateExt, db)`, which rounds the
            // `DateExt.getTime()` milliseconds and converts that instant to the
            // database zone
            Some(Value::DateBits(bits)) => Some(crate::column::date_ext_local_date_time(*bits, tz)),
            _ => None,
        }
    }
}

/// `MSysQueries.Attribute` is a `BYTE` in the table definition; be liberal
/// about the numeric value the reader hands back.
fn value_as_u8(value: Option<&Value>) -> Option<u8> {
    match value {
        Some(Value::Byte(v)) => Some(*v),
        Some(Value::Int(v)) => Some(*v as u8),
        Some(Value::Long(v)) => Some(*v as u8),
        Some(Value::BigInt(v)) => Some(*v as u8),
        _ => None,
    }
}

/// `MSysQueries.Flag` is an `INT` (`short` in Java).
fn value_as_i16(value: Option<&Value>) -> Option<i16> {
    match value {
        Some(Value::Int(v)) => Some(*v),
        Some(Value::Byte(v)) => Some(*v as i16),
        Some(Value::Long(v)) => Some(*v as i16),
        Some(Value::BigInt(v)) => Some(*v as i16),
        _ => None,
    }
}

/// `MSysQueries.ObjectId`/`LvExtra` are `LONG` (`int` in Java).
fn value_as_i32(value: Option<&Value>) -> Option<i32> {
    match value {
        Some(Value::Long(v)) => Some(*v),
        Some(Value::Int(v)) => Some(*v as i32),
        Some(Value::Byte(v)) => Some(*v as i32),
        Some(Value::BigInt(v)) => Some(*v as i32),
        _ => None,
    }
}

fn value_as_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|value| value.as_text())
        .map(|text| text.to_string())
}

/// `String.CASE_INSENSITIVE_ORDER.compare`: Java compares the upper case forms
/// first, and when those differ it compares their lower case forms (so a
/// non-letter such as `_` keeps its code point order against letters).
fn compare_names_case_insensitive(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    use std::iter::Iterator;
    let mut left = left.chars();
    let mut right = right.chars();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) => {
                if a == b {
                    continue;
                }
                let upper_a = a.to_uppercase().next().unwrap_or(a);
                let upper_b = b.to_uppercase().next().unwrap_or(b);
                if upper_a != upper_b {
                    let lower_a = upper_a.to_lowercase().next().unwrap_or(upper_a);
                    let lower_b = upper_b.to_lowercase().next().unwrap_or(upper_b);
                    if lower_a != lower_b {
                        return lower_a.cmp(&lower_b);
                    }
                }
            }
        }
    }
}

/// Sorts names like Java's `TreeSet<>(String.CASE_INSENSITIVE_ORDER)`, keeping
/// the first spelling of a name which differs from another only by case.
fn sort_case_insensitive(names: &mut Vec<String>) {
    names.sort_by(|left, right| compare_names_case_insensitive(left, right));
    names.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
}

/// `DateTimeType`: the in-memory representation of a date column, mirroring
/// `com.healthmarketscience.jackcess.DateTimeType`.
///
/// The 5.0.4 enum has exactly two constants, `DATE` and `LOCAL_DATE_TIME` (the
/// default since Jackcess 3.5); there is no formatted string mode (the
/// `javap` dump of `DateTimeType.class` in `jackcess-5.0.4.jar` lists only
/// those two constants).  The mode selects the conversion of every
/// `SHORT_DATE_TIME` column read and write
/// (`ColumnImpl.DateTimeFactory.fromDateBits` / `toDateDouble`):
///
/// * [`DateTimeType::Date`] (`DefaultDateTimeFactory`): a value is a
///   `java.util.Date` epoch instant.  A read value keeps its original stored
///   bits so a round trip is lossless, which the port represents as
///   [`crate::value::Value::DateBits`]; use
///   [`crate::column::date_value_millis`] for the `Date.getTime()` the Java
///   value reports.  Writes accept a [`crate::value::Value::DateTime`] (a
///   `java.time.LocalDateTime`, converted to the database zone's instant, so
///   its sub-millisecond part is dropped), a numeric epoch millisecond
///   ([`crate::value::Value::BigInt`] etc.) and a
///   [`crate::value::Value::DateBits`] (written with its original bits).
/// * [`DateTimeType::LocalDateTime`] (the default,
///   `LDTDateTimeFactory`): a value is a
///   [`crate::datenums::LocalDateTime`]
///   ([`crate::value::Value::DateTime`]), which keeps its full nanosecond
///   precision; a numeric value is an epoch instant converted to the database
///   zone.
///
/// The expression engine always works in `LocalDateTime`
/// (`BaseEvalContext.toValue` forces `LDT_DATE_TIME_FACTORY`), so default and
/// calculated date values are `LocalDateTime` objects in both modes, exactly
/// like Java.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateTimeType {
    /// legacy `java.util.Date` handling (`DateTimeType.DATE`)
    Date,
    /// `java.time.LocalDateTime` handling (the default,
    /// `DateTimeType.LOCAL_DATE_TIME`)
    #[default]
    LocalDateTime,
}

impl DateTimeType {
    /// The Java enum constant name (`DateTimeType.name()`).
    pub fn name(self) -> &'static str {
        match self {
            DateTimeType::Date => "DATE",
            DateTimeType::LocalDateTime => "LOCAL_DATE_TIME",
        }
    }

    /// `DateTimeType.valueOf(name)`, case insensitively; anything else is
    /// `LOCAL_DATE_TIME` (the default).
    pub fn from_name(name: &str) -> DateTimeType {
        if name.trim().eq_ignore_ascii_case("DATE") {
            DateTimeType::Date
        } else {
            DateTimeType::LocalDateTime
        }
    }
}

/// The error `PageChannel.flush` reports for a closed database: a
/// `java.nio.channels.ClosedChannelException` (an `IOException` with no
/// message).  The port models it as an [`Error::Io`] whose diagnostic text is
/// the Java class name, like the `NonWritableChannelException` case.
fn closed_channel_error() -> Error {
    Error::Io(std::io::Error::other("ClosedChannelException"))
}

/// Port of `com.healthmarketscience.jackcess.util.ReadOnlyFileChannel`: a
/// [`crate::pagechannel::FileIo`] which passes reads through and refuses every
/// write with the exception a read-only `FileChannel` throws
/// (`NonWritableChannelException`).
struct ReadOnlyIo(Box<dyn crate::pagechannel::FileIo>);

impl crate::pagechannel::FileIo for ReadOnlyIo {
    fn size(&mut self) -> Result<u64> {
        self.0.size()
    }

    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.0.read_at(buf, offset)
    }

    fn write_at(&mut self, _buf: &[u8], _offset: u64) -> Result<usize> {
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "NonWritableChannelException",
        )))
    }

    fn truncate(&mut self, _size: u64) -> Result<()> {
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "NonWritableChannelException",
        )))
    }

    fn flush(&mut self) -> Result<()> {
        self.0.flush()
    }
}

/// Port of `DatabaseImpl.getDefaultEvaluateExpressions`: jackcess evaluates the
/// property based expressions (calculated columns, default values and the
/// validation rules) unless the caller turns the policy off.
pub const DEFAULT_EVALUATE_EXPRESSIONS: bool = true;

/// `DatabaseImpl.getDefaultWriteBrokenIndex`: an index write failure does not
/// leave a broken index behind unless the caller allows it.
pub const DEFAULT_WRITE_BROKEN_INDEX: bool = false;

/// `Database.DEFAULT_COLUMN_ORDER`.
pub const DEFAULT_COLUMN_ORDER: ColumnOrder = ColumnOrder::Data;

/// `DatabaseImpl.getDefaultDateTimeType`.
pub const DEFAULT_DATE_TIME_TYPE: DateTimeType = DateTimeType::LocalDateTime;

/// `Database.DEFAULT_AUTO_SYNC`: jackcess flushes every write to the file system
/// unless the caller turns it off.
pub const DEFAULT_AUTO_SYNC: bool = true;

/// The open/create options of [`crate::builder::DatabaseBuilder`], i.e. the
/// argument list of `DatabaseImpl.open` / `DatabaseImpl.create`.
///
/// This is the Rust shape of the Java
/// `DatabaseImpl.open(Path, boolean readOnly, FileChannel channel,
/// boolean autoSync, Charset charset, TimeZone timeZone, CodecProvider,
/// boolean ignoreSystemCatalogIndex)` and
/// `DatabaseImpl.create(FileFormat, Path, FileChannel, boolean autoSync,
/// Charset, TimeZone)`.  Every field maps to one of the Java builder options:
///
/// | field | `DatabaseBuilder` method |
/// |---|---|
/// | [`read_only`](Self::read_only) | `setReadOnly` |
/// | [`auto_sync`](Self::auto_sync) | `setAutoSync` |
/// | [`charset`](Self::charset) | `setCharset` |
/// | [`time_zone`](Self::time_zone) | `setTimeZone` |
/// | [`codec_provider`](Self::codec_provider) | `setCodecProvider` |
/// | [`file_format`](Self::file_format) | `setFileFormat` (create only) |
/// | [`ignore_broken_system_catalog_index`](Self::ignore_broken_system_catalog_index) | `setIgnoreBrokenSystemCatalogIndex` |
///
/// Unlike the Java builder the struct is public, so callers which cannot use
/// [`crate::builder::DatabaseBuilder`] (for instance code which already owns a
/// channel) can still reach the same constructor.
#[derive(Clone)]
pub struct OpenSettings<'a> {
    /// `DatabaseBuilder.setReadOnly`: open the existing file read-only.
    pub read_only: bool,
    /// `DatabaseBuilder.setAutoSync`: flush every write; `None` keeps the
    /// setting of the channel the caller supplied.
    pub auto_sync: Option<bool>,
    /// `DatabaseBuilder.setCharset`: the charset for the text of a database
    /// whose format/header does not name one.
    ///
    /// A charset which cannot decode the unicode system catalog names is
    /// refused by Java while opening (`Did not find required parent table id`,
    /// because the `MSysObjects` rows become unreadable); the port reads the
    /// catalog by page, so the database opens with the mangled names and the
    /// table lookups report the missing tables instead.  Use a charset the
    /// format can decode (the format's own, or the header's code page for
    /// Jet 3).
    pub charset: Option<Charset>,
    /// `DatabaseBuilder.setTimeZone`.
    pub time_zone: Option<crate::datenums::TimeZone>,
    /// `DatabaseBuilder.setCodecProvider`.
    pub codec_provider: Option<&'a dyn crate::crypt::CodecProvider>,
    /// `DatabaseBuilder.setFileFormat`: the format of a database which is
    /// created (ignored by the open entry points, like Java).
    pub file_format: Option<FileFormat>,
    /// `DatabaseBuilder.setIgnoreBrokenSystemCatalogIndex`.
    pub ignore_broken_system_catalog_index: bool,
    /// `DatabaseImpl`'s `closeChannel` constructor argument: whether
    /// [`Database::close`] owns (and therefore closes) the channel.  `true` for
    /// the path-based opens (the database opened the file itself), `false` for
    /// a channel supplied through [`crate::builder::DatabaseBuilder::set_channel`]
    /// (Java documents that the caller keeps its handle).  This is not a
    /// `DatabaseBuilder` option; the builder derives it from whether a channel
    /// was configured.
    pub close_channel: bool,
}

impl std::fmt::Debug for OpenSettings<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenSettings")
            .field("read_only", &self.read_only)
            .field("auto_sync", &self.auto_sync)
            .field("charset", &self.charset)
            .field("time_zone", &self.time_zone)
            .field("has_codec_provider", &self.codec_provider.is_some())
            .field("file_format", &self.file_format)
            .field(
                "ignore_broken_system_catalog_index",
                &self.ignore_broken_system_catalog_index,
            )
            .finish()
    }
}

impl<'a> OpenSettings<'a> {
    /// The Java defaults: writable, auto-sync on, no charset/zone/codec/format
    /// override, the system catalog index used when available.
    pub fn new() -> OpenSettings<'a> {
        OpenSettings {
            read_only: false,
            auto_sync: Some(DEFAULT_AUTO_SYNC),
            charset: None,
            time_zone: None,
            codec_provider: None,
            file_format: None,
            ignore_broken_system_catalog_index: false,
            close_channel: true,
        }
    }
}

impl Default for OpenSettings<'_> {
    fn default() -> Self {
        OpenSettings::new()
    }
}

pub struct Database {
    pub format: JetFormat,
    pub channel: PageChannel,
    pub charset: Charset,
    pub tz: crate::datenums::TimeZone,
    pub file_format: Option<FileFormat>,
    pub default_sort_order: SortOrder,
    /// `DatabaseImpl._defaultCodePage`: the code page from page 0, read when a
    /// format stores text in the file code page (Jet 3) and `None` for the
    /// unicode formats.
    pub default_code_page: Option<i16>,
    /// table definition page -> loaded table
    pub tables: HashMap<i32, Table>,
    /// uppercase table name -> (tdef page, flags)
    pub table_lookup: HashMap<String, (i32, i32)>,
    pub table_parent_id: i32,
    pub db_parent_id: i32,
    pub rel_parent_id: i32,
    pub mod_count: i32,
    pub now_ms: Option<i64>,
    /// queue of timestamps consumed by each catalog object creation
    pub now_queue: Vec<i64>,
    /// queue of *local* catalog timestamps, consumed before [`Database::now_queue`].
    ///
    /// Java's catalog rows hold a `java.util.Date` which is converted with the
    /// database zone, so a harness which wants byte identical file output hands
    /// the exact `LocalDateTime` the reference file got to the port; converting
    /// epoch milliseconds back to a local time would not always reproduce it (a
    /// local time repeated by a DST fall-back is ambiguous).
    pub now_ldt_queue: Vec<crate::datenums::LocalDateTime>,
    /// the most recent timestamp consumed from `now_queue`
    pub last_now_ms: Option<i64>,
    /// the simple file name of the database (`DatabaseImpl.getName`), used in
    /// the foreign key constraint messages
    pub name: String,
    /// `DatabaseImpl._file`: the path the database was opened from (or created
    /// at), if it came from a file.  A [`crate::mem::MemDatabase`] has none.
    pub path: Option<std::path::PathBuf>,
    /// `DatabaseImpl._enforceForeignKeys`: whether foreign key relationships
    /// are enforced on row modifications (default `true`, like jackcess)
    pub enforce_foreign_keys: bool,
    /// `FKEnforcer.SharedState._updateDepth`: the depth of cascading update
    /// calls across one or more tables
    pub fk_update_depth: i32,
    /// the names of the flat tables the next complex columns create.  Access
    /// puts a random guid in these names; the verification harness reads them
    /// back from the Java file so the two files are comparable.
    pub complex_names: Vec<String>,
    /// counter used to build a deterministic flat table name when
    /// `complex_names` is exhausted
    pub complex_name_counter: u32,
    /// `DatabaseImpl._linkResolver` / `getDefaultLinkResolver`: the policy used
    /// to open linkee files.  Defaults to [`crate::linked::DEFAULT`], which
    /// refuses every linked database.
    pub link_resolver: Box<dyn LinkResolver>,
    /// `DatabaseImpl._linkedDbs`: linkee catalog name -> opened database.  The
    /// linkee databases are owned by the linker so
    /// [`Database::get_table`] can hand out a table reference which still
    /// belongs to the linkee file.  See the [`crate::linked`] module for the
    /// design.
    pub linked_databases: HashMap<String, Box<Database>>,
    /// The type 6 (`Database`/`ForeignName`) catalog rows, keyed by the upper
    /// case local table name (`DatabaseImpl.LinkedTableInfo`).
    pub linked_tables: HashMap<String, LinkedTableInfo>,
    /// The type 4 (`Connect`/`ForeignName`) catalog rows, keyed by the upper
    /// case local table name (`DatabaseImpl.LinkedODBCTableInfo`).  These
    /// tables have a local table definition (a `TableDefinition` in Java) but
    /// their rows live in the remote ODBC database.
    pub linked_odbc_tables: HashMap<String, LinkedOdbcTableInfo>,
    /// Whether this database was opened read-only (`DatabaseImpl.isReadOnly`).
    /// [`crate::linked::UNRESTRICTED`] opens the linkee read-only when the
    /// linker is read-only.
    pub read_only: bool,
    /// `DatabaseImpl._evaluateExpressions`: whether the property based
    /// expressions (calculated columns, default values and the expression
    /// validation rules) are evaluated.  Defaults to `true`
    /// ([`getDefaultEvaluateExpressions`](Database::is_evaluate_expressions)).
    /// Column `Required` and `AllowZeroLength` validators are *not* gated by
    /// it (Java adds them before it checks the flag).
    pub evaluate_expressions: bool,
    /// `DatabaseImpl._writeBrokenIndex`: whether an index write may leave a
    /// broken index behind instead of failing the whole row write.  Defaults to
    /// `false` ([`getDefaultWriteBrokenIndex`](Database::is_write_broken_index)).
    pub write_broken_index: bool,
    /// `DatabaseImpl._evalConfig` (`Database.getEvalConfig`): the expression
    /// evaluation configuration.  The zone is kept aligned with the database
    /// zone; [`crate::expr::EvalConfig`] documents that the evaluator still uses
    /// its built-in US formatting/clock paths.
    pub eval_config: crate::expr::EvalConfig,
    /// `DatabaseImpl._columnValidatorFactory` (`Database
    /// .getColumnValidatorFactory`): the factory which creates the column
    /// validators of the user tables loaded from this database.  `None` means
    /// the default [`crate::validator::SimpleColumnValidatorFactory`], like
    /// Java's `null` handling.
    ///
    /// The port's [`crate::table::Table`] has no database handle and builds its
    /// validators directly (the simple factory's set), so a factory installed
    /// here is recorded for the API and is not consulted by the table loader
    /// yet.
    pub column_validator_factory: Option<crate::validator::ColumnValidatorFactoryHandle>,
    /// `DatabaseImpl._columnOrder`: the order [`crate::table::Table::get_columns`]
    /// uses for tables loaded from now on.  It never affects the row layout.
    pub column_order: ColumnOrder,
    /// `DatabaseImpl._dtf.getType()`: the date/time representation of every
    /// `SHORT_DATE_TIME` column of this database.
    pub date_time_type: DateTimeType,
    /// `DatabaseImpl._dbErrorHandler`: the database level [`ErrorHandler`]
    /// (`None` means [`crate::table::DEFAULT_ERROR_HANDLER`], which rethrows).
    pub error_handler: Option<Arc<dyn ErrorHandler>>,
    /// Tables created by this instance, with the column names in column order.
    ///
    /// Java keeps one [`crate::property::PropertyMaps`] instance per table for
    /// as long as the table is loaded, and a table created in the process
    /// anchors the property block order at its columns (the `TableImpl`
    /// constructor allocates the map of every column before the table's own map
    /// is touched).  The port rebuilds the group from the stored blob on every
    /// property access, so the anchor is recorded here for the tables this
    /// process created; a table loaded from a file keeps the blob's order.
    pub(crate) created_table_column_maps: HashMap<i32, Vec<String>>,
    /// `DatabaseBuilder.setIgnoreBrokenSystemCatalogIndex`: whether the system
    /// catalog index may be skipped when looking up tables.
    ///
    /// The port resolves table metadata by scanning the `MSysObjects` rows
    /// ([`Database::catalog_entries`]) instead of probing the system catalog
    /// index, so it behaves like Java's "no index" table finder for every
    /// lookup: the flag is accepted (and reported) for API parity but changes
    /// nothing.
    pub ignore_broken_system_catalog_index: bool,
    /// `true` once [`Database::close`] has run.  Java marks a closed database
    /// through its closed `FileChannel` (`PageChannel.isOpen()` turns `false`,
    /// and a later `flush`/write throws `ClosedChannelException`); the port has
    /// no owned, closable channel, so the state is tracked here.  A database
    /// whose channel was supplied by the caller (see
    /// [`OpenSettings::close_channel`]) is not marked closed, like Java's
    /// `closeChannel=false` case.
    pub closed: bool,
    /// `DatabaseImpl`'s `closeChannel` flag: whether [`Database::close`] owns
    /// the channel (see [`OpenSettings::close_channel`]).
    pub close_channel: bool,
}

/// `DatabaseImpl.getDefaultEnforceForeignKeys` - foreign keys are enforced
/// unless the caller turns the policy off.
pub const DEFAULT_ENFORCE_FOREIGN_KEYS: bool = true;

impl Database {
    fn open_channel(mut channel: PageChannel, settings: &OpenSettings) -> Result<Database> {
        // `DatabaseImpl`'s constructor takes the charset from the format
        // before the page channel is initialized, and the codec provider is
        // asked for its handler before the global usage map is read (both
        // page 1 and the usage map may be encoded)
        if let Some(auto_sync) = settings.auto_sync {
            channel.auto_sync = auto_sync;
        }
        channel.charset = channel.format.charset;
        let codec_provider: &dyn crate::crypt::CodecProvider = settings
            .codec_provider
            .unwrap_or(&crate::crypt::DefaultCodecProvider::INSTANCE);
        channel.install_codec(codec_provider)?;
        channel.initialize()?;
        let format = channel.format.clone();
        channel.charset = format.charset;
        let time_zone = settings.time_zone.unwrap_or_default();
        // `DBEvalContext` answers `getZoneId()` from the owning database; the
        // port's `EvalConfig` carries the zone, so keep it aligned from the
        // start (and in `set_time_zone`)
        let mut eval_config = crate::expr::EvalConfig::default();
        eval_config.set_zone(Some(time_zone));
        let mut db = Database {
            format,
            channel,
            charset: Charset::Utf16Le,
            tz: time_zone,
            file_format: settings.file_format,
            default_sort_order: SortOrder::GENERAL_LEGACY,
            default_code_page: None,
            tables: HashMap::new(),
            table_lookup: HashMap::new(),
            table_parent_id: 0,
            db_parent_id: 0,
            rel_parent_id: 0,
            mod_count: 0,
            now_ms: None,
            now_queue: Vec::new(),
            now_ldt_queue: Vec::new(),
            last_now_ms: None,
            name: "<UNKNOWN.DB>".to_string(),
            path: None,
            enforce_foreign_keys: DEFAULT_ENFORCE_FOREIGN_KEYS,
            fk_update_depth: 0,
            complex_names: Vec::new(),
            complex_name_counter: 0,
            link_resolver: Box::new(crate::linked::DEFAULT),
            linked_databases: HashMap::new(),
            linked_tables: HashMap::new(),
            linked_odbc_tables: HashMap::new(),
            read_only: settings.read_only,
            evaluate_expressions: DEFAULT_EVALUATE_EXPRESSIONS,
            write_broken_index: DEFAULT_WRITE_BROKEN_INDEX,
            eval_config,
            column_validator_factory: None,
            column_order: DEFAULT_COLUMN_ORDER,
            date_time_type: DEFAULT_DATE_TIME_TYPE,
            error_handler: None,
            created_table_column_maps: HashMap::new(),
            ignore_broken_system_catalog_index: settings.ignore_broken_system_catalog_index,
            closed: false,
            close_channel: settings.close_channel,
        };
        db.init_root_page_info()?;
        // `DatabaseImpl`'s constructor: the configured charset wins, otherwise
        // the charset named by the format/code page does
        db.charset = match settings.charset {
            Some(charset) => charset,
            None => db.default_charset()?,
        };
        // the channel is what a table uses to decode text and names, so it
        // carries the same charset (`DatabaseImpl.getCharset` is the only
        // source of it in Java)
        db.channel.charset = db.charset;
        db.read_system_catalog()?;
        Ok(db)
    }

    /// `DatabaseImpl.getDefaultCharset`: the charset of the database text.
    ///
    /// For a format which stores text as unicode this is the format charset.
    /// For Jet 3 (`CODE_PAGE_CHARSET`) it is the charset named by the code
    /// page in page 0, falling back to the format charset when the code page
    /// is unset or this port cannot decode it.
    pub fn default_charset(&self) -> Result<Charset> {
        if self.format.code_page_charset {
            if let Some(code_page) = self.default_code_page {
                if let Some(charset) = Charset::for_code_page(code_page) {
                    return Ok(charset);
                }
                // Java's `getCodePageCharset` returns null for a page the JVM
                // cannot name, and the caller falls back to the format charset
                // (UTF-8 for Jet 3) - do the same for the IBM DBCS pages this
                // port has no tables for.
            }
        }
        Ok(self.format.charset)
    }

    /// `Database.getCharset()`: the charset used for the text of this database.
    ///
    /// This is the charset a [`DatabaseBuilder`](crate::database::Database)
    /// would use by default (see [`Database::default_charset`]) unless
    /// [`Database::set_charset`] replaced it.
    pub fn get_charset(&self) -> Charset {
        self.charset
    }

    /// `Database.setCharset(Charset)`: `None` restores the default charset
    /// (the code page of the file for Jet 3, the format charset otherwise).
    ///
    /// The page channel, which is what decodes text and names while rows are
    /// read, is kept in sync (`DatabaseImpl.getCharset` is the only source of
    /// the charset in Java).
    pub fn set_charset(&mut self, charset: Option<Charset>) {
        self.charset = charset.unwrap_or_else(|| {
            // `defaultCharset()` was already validated while this database was
            // opened, so the fallback is only defensive
            self.default_charset().unwrap_or(self.format.charset)
        });
        self.channel.charset = self.charset;
    }

    /// `DatabaseImpl.getName()`: the simple file name of the database (Java's
    /// `DatabaseImpl` only exposes it as a non-interface method).
    pub fn get_name(&self) -> &str {
        &self.name
    }

    /// `Database.getPath()`: the path this database was opened from (or
    /// created at).  `None` for an in-memory database
    /// ([`crate::mem::MemDatabase`]).
    pub fn get_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Opens an existing database (`new DatabaseBuilder(path).open()`).
    ///
    /// A MSISAM (Microsoft Money `.mny`) database is opened read-only with the
    /// MSISAM page codec, even without a password: the pages of a `.mny` are
    /// always encoded (the old encoding derives its key from the file, the
    /// modern one from the password), so the plain [`DefaultCodecProvider`]
    /// could not read one.  Use [`Database::open_path_with_password`] for a
    /// password protected Money file.  Every other format uses the Java
    /// `DefaultCodecProvider`.
    ///
    /// [`DefaultCodecProvider`]: crate::crypt::DefaultCodecProvider
    pub fn open_path(path: &Path) -> Result<Database> {
        Self::open_path_with_mode(path, false, None)
    }

    /// Opens an existing database with the given codec provider, which decodes
    /// every page transparently (`new DatabaseBuilder(path)
    /// .setCodecProvider(...).open()`).  Use
    /// [`crate::crypt::CryptCodecProvider`] to open an encoded database.
    pub fn open_path_with_provider(
        path: &Path,
        codec_provider: &dyn crate::crypt::CodecProvider,
    ) -> Result<Database> {
        Self::open_path_with_mode(path, false, Some(codec_provider))
    }

    /// Opens an existing database with a jet database password (`new
    /// DatabaseBuilder(path).setCodecProvider(new CryptCodecProvider(password))
    /// .open()`).
    ///
    /// The jet database password does not encrypt the pages, so this opens the
    /// same way with or without the password; the parameter exists for parity
    /// with the Java API.  The MSISAM (Money 2008+) and Office codecs do need
    /// it: a wrong or missing password fails with
    /// `InvalidCredentials("Incorrect password provided")`, like Java's
    /// `InvalidCredentialsException`.  Use
    /// [`Database::verify_database_password`] to check a jet password.
    pub fn open_path_with_password(path: &Path, password: Option<&str>) -> Result<Database> {
        let provider = crate::crypt::CryptCodecProvider::from_password(password);
        Self::open_path_with_mode(path, false, Some(&provider))
    }

    /// Opens an existing database read-only (`DatabaseBuilder.setReadOnly(true)`
    /// / `DatabaseImpl.isReadOnly`).
    ///
    /// The port has no `readOnly` flag on [`PageChannel`], so the flag lives on
    /// the database.  Read operations work normally; writes fail in the
    /// underlying [`crate::pagechannel::FileChannel`].  This is the mode
    /// [`crate::linked::UNRESTRICTED`] uses for the linkee when the linker
    /// itself is read-only.  A MSISAM database is always opened this way (see
    /// [`Database::open_path`]).
    pub fn open_path_read_only(path: &Path) -> Result<Database> {
        Self::open_path_with_mode(path, true, None)
    }

    /// Opens an existing database read-only with the given codec provider.
    pub fn open_path_read_only_with_provider(
        path: &Path,
        codec_provider: &dyn crate::crypt::CodecProvider,
    ) -> Result<Database> {
        Self::open_path_with_mode(path, true, Some(codec_provider))
    }

    /// Opens a database, choosing the codec provider for the convenience entry
    /// points: a MSISAM (`.mny`) database always needs the MSISAM page
    /// encoding, even when it has no password (the old encoding's key is
    /// derived from the file, the modern one from the password), so those
    /// opens provide [`crate::crypt::CryptCodecProvider`] themselves.  Every
    /// other format gets the Java `DefaultCodecProvider` (which refuses an
    /// encoded database exactly like Java).  An explicit provider, given
    /// through [`Database::open_path_with_provider`], is always used as is.
    fn open_path_with_mode(
        path: &Path,
        read_only: bool,
        codec_provider: Option<&dyn crate::crypt::CodecProvider>,
    ) -> Result<Database> {
        let mut settings = OpenSettings::new();
        settings.read_only = read_only;
        settings.codec_provider = codec_provider;
        // the convenience entry points always auto-sync, like the Java
        // `DatabaseBuilder.open` they mirror
        settings.auto_sync = Some(DEFAULT_AUTO_SYNC);
        Self::open_path_with_settings(path, &settings)
    }

    /// Opens an existing database with the given [`OpenSettings`]
    /// (`DatabaseImpl.open` / `DatabaseBuilder.open`).
    ///
    /// This is the full entry point the [`crate::builder::DatabaseBuilder`]
    /// uses: the charset and time zone are installed before the system catalog
    /// is read, exactly like the `DatabaseImpl` constructor, so the catalog
    /// names, the decoded text and the catalog dates already follow them.
    /// A format jackcess cannot write (Jet 3) and a MSISAM database force
    /// read-only mode, like `DatabaseBuilder.open`.
    pub fn open_path_with_settings(path: &Path, settings: &OpenSettings) -> Result<Database> {
        let file = if settings.read_only {
            FileChannel::open_read_only(path)?
        } else {
            FileChannel::open(path)?
        };
        let mut header = [0u8; 21];
        {
            use crate::pagechannel::FileIo;
            let mut ch = file;
            ch.read_at(&mut header, 0)?;
            let format = JetFormat::get_format(&header)?;
            // `DatabaseBuilder.open`: a format jackcess cannot write (Jet 3)
            // forces read-only mode, and a MSISAM database is read-only in this
            // port (jackcess has no MSISAM creation template; the task of the
            // port is reading Money files).  Java also wraps the channel in a
            // `ReadOnlyFileChannel` so a write fails with
            // `NonWritableChannelException`.
            let read_only = settings.read_only
                || format.read_only
                || format.codec_type == crate::format::CodecType::Msisam;
            let io: Box<dyn crate::pagechannel::FileIo> = if read_only {
                Box::new(ReadOnlyIo(Box::new(ch)))
            } else {
                Box::new(ch)
            };
            let auto_provider;
            let codec_provider: &dyn crate::crypt::CodecProvider = match settings.codec_provider {
                Some(provider) => provider,
                None if format.codec_type == crate::format::CodecType::Msisam => {
                    auto_provider = crate::crypt::CryptCodecProvider::new();
                    &auto_provider
                }
                None => &crate::crypt::DefaultCodecProvider::INSTANCE,
            };
            let channel =
                PageChannel::new(io, format, settings.auto_sync.unwrap_or(DEFAULT_AUTO_SYNC));
            let mut effective = settings.clone();
            effective.read_only = read_only;
            effective.codec_provider = Some(codec_provider);
            // the open entry points ignore `setFileFormat`, like Java
            effective.file_format = None;
            let db = Database::open_channel(channel, &effective)?;
            let mut db = db;
            db.format = db.channel.format.clone();
            // `DatabaseImpl.getName(Path)`: the simple file name
            db.name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<UNKNOWN.DB>".to_string());
            // the tables loaded while opening (the system catalog) copied the
            // placeholder name; give them the real one, which their error
            // messages (`(Db=<name>;Table=...)`) report
            let name = db.name.clone();
            for table in db.tables.values_mut() {
                table.db_name = name.clone();
            }
            // `DatabaseImpl._file`
            db.path = Some(path.to_path_buf());
            Ok(db)
        }
    }

    /// Creates a new database from the embedded template (`DatabaseBuilder.create`).
    pub fn create(file_format: FileFormat, path: &Path) -> Result<Database> {
        Self::create_with_settings(file_format, path, &OpenSettings::new())
    }

    /// Creates a new database with the given [`OpenSettings`]
    /// (`DatabaseBuilder.setFileFormat(...).create()`, with the charset and
    /// time zone of the builder installed before the system catalog is read).
    ///
    /// The embedded empty template is written to the path (an existing file is
    /// overwritten, like the Java `DatabaseImpl.create`), then the database is
    /// opened over it with the settings (except `read_only`, which the Java
    /// builder ignores for creation).
    pub fn create_with_settings(
        file_format: FileFormat,
        path: &Path,
        settings: &OpenSettings,
    ) -> Result<Database> {
        let template: &[u8] = match file_format {
            FileFormat::V2000 => include_bytes!("../resources/empty.mdb"),
            FileFormat::V2003 => include_bytes!("../resources/empty2003.mdb"),
            FileFormat::V2007 => include_bytes!("../resources/empty2007.accdb"),
            FileFormat::V2010 => include_bytes!("../resources/empty2010.accdb"),
            FileFormat::V2016 => include_bytes!("../resources/empty2016.accdb"),
            FileFormat::V2019 => include_bytes!("../resources/empty2019.accdb"),
            // `DatabaseImpl.create`: a format without an empty template cannot
            // be created.  Java's `FileFormat.toString()` is
            // `name() + " [" + JetFormat + "]"`, so the MSISAM message reads
            // `File format MSISAM [MSISAM] ...`; the text is reproduced here
            // exactly.
            FileFormat::Msisam => {
                return Err(Error::Jackcess(format!(
                    "File format MSISAM [MSISAM] does not support file creation for {}",
                    path.display()
                )))
            }
            other => {
                return Err(Error::Jackcess(format!(
                    "File format {:?} does not support file creation",
                    other
                )))
            }
        };
        std::fs::write(path, template)?;
        let mut effective = settings.clone();
        effective.read_only = false;
        let mut db = Database::open_path_with_settings(path, &effective)?;
        db.file_format = Some(file_format);
        Ok(db)
    }

    /// Creates a new database and applies the given database, summary and user
    /// defined properties, exactly like `DatabaseBuilder.create()` does with
    /// `putDatabaseProperty` / `putSummaryProperty` / `putUserDefinedProperty`.
    pub fn create_with_properties(
        file_format: FileFormat,
        path: &Path,
        props: &DatabaseProperties,
    ) -> Result<Database> {
        Self::create_with_settings_and_properties(file_format, path, &OpenSettings::new(), props)
    }

    /// [`Database::create_with_settings`] plus the property groups of
    /// `DatabaseBuilder.create` ([`Database::apply_create_properties`]).
    pub fn create_with_settings_and_properties(
        file_format: FileFormat,
        path: &Path,
        settings: &OpenSettings,
        props: &DatabaseProperties,
    ) -> Result<Database> {
        let mut db = Database::create_with_settings(file_format, path, settings)?;
        db.apply_create_properties(props)?;
        Ok(db)
    }

    /// `DatabaseBuilder.create`: puts the configured properties into the
    /// database, summary and user defined property maps and saves each group
    /// (in that order).
    pub(crate) fn apply_create_properties(&mut self, props: &DatabaseProperties) -> Result<()> {
        if !props.database.is_empty() {
            let mut maps = self.get_database_properties()?;
            crate::property::put_builder_properties(maps.get_default(), &props.database);
            self.write_property_maps(&mut maps)?;
        }
        if !props.summary.is_empty() {
            let mut maps = self.get_summary_properties()?;
            crate::property::put_builder_properties(maps.get_default(), &props.summary);
            self.write_property_maps(&mut maps)?;
        }
        if !props.user_defined.is_empty() {
            let mut maps = self.get_user_defined_properties()?;
            crate::property::put_builder_properties(maps.get_default(), &props.user_defined);
            self.write_property_maps(&mut maps)?;
        }
        Ok(())
    }

    /// Opens a database whose pages come from the given channel instead of a
    /// file (`new DatabaseBuilder().setChannel(channel).open()`).
    ///
    /// The channel is read to determine the file format (like
    /// `DatabaseImpl.open` with an external channel); a format jackcess cannot
    /// write (Jet 3) or a MSISAM database forces read-only mode, and a null
    /// `path`/`file` means [`Database::get_path`] returns `None` and
    /// [`Database::name`] stays `"<UNKNOWN.DB>"`.  The channel is not owned by
    /// the database in Java either (`DatabaseBuilder.setChannel` documents that
    /// `Database.close` does not close it), so the caller keeps its handle.
    ///
    /// This is the constructor [`crate::mem::MemDatabase`] uses; it needs no
    /// scratch file.
    pub fn open_with_channel(
        io: Box<dyn crate::pagechannel::FileIo>,
        read_only: bool,
    ) -> Result<Database> {
        let mut settings = OpenSettings::new();
        settings.read_only = read_only;
        Self::open_channel_io(io, &settings)
    }

    /// [`Database::open_with_channel`] with an explicit codec provider
    /// (`DatabaseBuilder.setCodecProvider` over an external channel).
    pub fn open_with_channel_with_provider(
        io: Box<dyn crate::pagechannel::FileIo>,
        read_only: bool,
        codec_provider: &dyn crate::crypt::CodecProvider,
    ) -> Result<Database> {
        let mut settings = OpenSettings::new();
        settings.read_only = read_only;
        settings.codec_provider = Some(codec_provider);
        Self::open_channel_io(io, &settings)
    }

    /// Creates a new database directly in the given channel
    /// (`new DatabaseBuilder().setFileFormat(fmt).setChannel(channel).create()`).
    ///
    /// The embedded empty template is written to the channel (`FileChannel
    /// .truncate(0)` + the resource transfer of `DatabaseImpl.create`) and the
    /// database is opened over it, so no file is ever created.
    pub fn create_with_channel(
        file_format: FileFormat,
        io: Box<dyn crate::pagechannel::FileIo>,
    ) -> Result<Database> {
        Self::create_with_channel_settings(file_format, io, &OpenSettings::new())
    }

    /// [`Database::create_with_channel`] with the [`OpenSettings`] of a
    /// [`crate::builder::DatabaseBuilder`] (`setFileFormat` + `setChannel` +
    /// `create`, with the charset and time zone installed before the system
    /// catalog is read).
    pub fn create_with_channel_settings(
        file_format: FileFormat,
        mut io: Box<dyn crate::pagechannel::FileIo>,
        settings: &OpenSettings,
    ) -> Result<Database> {
        let template: &[u8] = match file_format {
            FileFormat::V2000 => include_bytes!("../resources/empty.mdb"),
            FileFormat::V2003 => include_bytes!("../resources/empty2003.mdb"),
            FileFormat::V2007 => include_bytes!("../resources/empty2007.accdb"),
            FileFormat::V2010 => include_bytes!("../resources/empty2010.accdb"),
            FileFormat::V2016 => include_bytes!("../resources/empty2016.accdb"),
            FileFormat::V2019 => include_bytes!("../resources/empty2019.accdb"),
            // `DatabaseImpl.create` formats the message with its `mdbFile`
            // argument, which `DatabaseBuilder.create` leaves null when only a
            // channel is configured.
            FileFormat::Msisam => {
                return Err(Error::Jackcess(
                    "File format MSISAM [MSISAM] does not support file creation for null"
                        .to_string(),
                ))
            }
            other => {
                return Err(Error::Jackcess(format!(
                    "File format {:?} does not support file creation",
                    other
                )))
            }
        };
        io.truncate(0)?;
        io.write_at(template, 0)?;
        io.flush()?;
        let mut effective = settings.clone();
        effective.read_only = false;
        effective.file_format = Some(file_format);
        Self::open_channel_io(io, &effective)
    }

    /// Opens a database whose pages come from the given channel with the given
    /// [`OpenSettings`] (`new DatabaseBuilder().setChannel(channel)...open()`).
    pub fn open_with_channel_settings(
        io: Box<dyn crate::pagechannel::FileIo>,
        settings: &OpenSettings,
    ) -> Result<Database> {
        Self::open_channel_io(io, settings)
    }

    /// [`Database::create_with_channel_settings`] plus the property groups of
    /// `DatabaseBuilder.create`.
    pub fn create_with_channel_settings_and_properties(
        file_format: FileFormat,
        io: Box<dyn crate::pagechannel::FileIo>,
        settings: &OpenSettings,
        props: &DatabaseProperties,
    ) -> Result<Database> {
        let mut db = Self::create_with_channel_settings(file_format, io, settings)?;
        db.apply_create_properties(props)?;
        Ok(db)
    }

    /// Opens a database over an already created [`PageChannel`]
    /// (`DatabaseImpl`'s constructor): the channel's format and codec are the
    /// caller's choice, only the read-only flag is applied.
    ///
    /// Prefer [`Database::open_with_channel`] for a `FileChannel` like channel,
    /// which determines the format and the read-only mode the way
    /// `DatabaseBuilder.open` does.
    pub fn from_channel(
        channel: PageChannel,
        file_format: Option<FileFormat>,
        read_only: bool,
    ) -> Result<Database> {
        let mut settings = OpenSettings::new();
        settings.file_format = file_format;
        settings.read_only = read_only;
        // the caller already chose the channel (and its auto-sync setting)
        settings.auto_sync = None;
        settings.close_channel = false;
        Database::open_channel(channel, &settings)
    }

    /// The shared body of [`Database::open_with_channel`] and
    /// [`Database::create_with_channel`]: determines the format from the
    /// channel header and applies the `DatabaseBuilder.open` policies
    /// (read-only formats, the MSISAM codec).
    fn open_channel_io(
        mut io: Box<dyn crate::pagechannel::FileIo>,
        settings: &OpenSettings,
    ) -> Result<Database> {
        let mut header = [0u8; 21];
        io.read_at(&mut header, 0)?;
        let format = JetFormat::get_format(&header)?;
        let read_only = settings.read_only
            || format.read_only
            || format.codec_type == crate::format::CodecType::Msisam;
        let io: Box<dyn crate::pagechannel::FileIo> = if read_only {
            Box::new(ReadOnlyIo(io))
        } else {
            io
        };
        let auto_provider;
        let codec_provider: &dyn crate::crypt::CodecProvider = match settings.codec_provider {
            Some(provider) => provider,
            None if format.codec_type == crate::format::CodecType::Msisam => {
                auto_provider = crate::crypt::CryptCodecProvider::new();
                &auto_provider
            }
            None => &crate::crypt::DefaultCodecProvider::INSTANCE,
        };
        let channel = PageChannel::new(io, format, settings.auto_sync.unwrap_or(DEFAULT_AUTO_SYNC));
        let mut effective = settings.clone();
        effective.read_only = read_only;
        effective.codec_provider = Some(codec_provider);
        // an external channel is never owned by the database
        effective.close_channel = false;
        Database::open_channel(channel, &effective)
    }

    fn init_root_page_info(&mut self) -> Result<()> {
        let format = self.format.clone();
        let mut buffer = self.channel.create_page_buffer();
        self.channel.read_root_page(&mut buffer)?;
        self.default_sort_order =
            crate::column::read_sort_order(&buffer, format.offset_sort_order, &format)?;
        self.default_code_page = Some(buffer.get_i16_at(format.offset_code_page)?);
        Ok(())
    }

    fn read_system_catalog(&mut self) -> Result<()> {
        let format = self.format.clone();
        // load MSysObjects directly from its known page
        let mut table = Table::read(
            &mut self.channel,
            PAGE_SYSTEM_CATALOG,
            TABLE_SYSTEM_CATALOG,
            SYSTEM_OBJECT_FLAGS,
            &format,
        )?;
        table.set_database_error_handler(self.error_handler.clone());
        self.apply_table_settings(&mut table);
        self.tables.insert(PAGE_SYSTEM_CATALOG, table);
        self.table_lookup.insert(
            "MSYSOBJECTS".to_string(),
            (PAGE_SYSTEM_CATALOG, SYSTEM_OBJECT_FLAGS),
        );
        self.index_catalog()?;
        // `TableImpl.getCreatedDate`/`getUpdatedDate` of the system catalog read
        // its own catalog row (`Id == PAGE_SYSTEM_CATALOG`), so the catalog gets
        // its dates like every other table
        self.apply_catalog_table_info(PAGE_SYSTEM_CATALOG)?;
        // `TableImpl.getNameMap` reads the `NameMap` property of any table,
        // the system catalog included; capture the raw value so the catalog's
        // `Table.get_name_map` works like every other table's
        let name_map = self.name_map_blob_for(PAGE_SYSTEM_CATALOG)?;
        if let Some(table) = self.tables.get_mut(&PAGE_SYSTEM_CATALOG) {
            table.name_map_blob = name_map;
        }
        Ok(())
    }

    fn index_catalog(&mut self) -> Result<()> {
        let rows = {
            let table = self.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
            table.read_all_rows(&mut self.channel)?
        };
        let table = self.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
        let idx_id = table.column_index(CAT_COL_ID);
        let idx_name = table.column_index(CAT_COL_NAME);
        let idx_type = table.column_index(CAT_COL_TYPE);
        let idx_flags = table.column_index(CAT_COL_FLAGS);
        let _idx_parent = table.column_index(CAT_COL_PARENT_ID);
        let idx_db = table.column_index(CAT_COL_DATABASE);
        let idx_connect = table.column_index(CAT_COL_CONNECT);
        let idx_foreign = table.column_index(CAT_COL_FOREIGN_NAME);

        for row in rows.into_iter().flatten() {
            let name = idx_name
                .and_then(|i| row.get(i))
                .and_then(|v| v.as_text())
                .map(|s| s.to_string());
            let id = idx_id.and_then(|i| row.get(i)).and_then(|v| v.as_i64());
            let typ = idx_type.and_then(|i| row.get(i)).and_then(|v| v.as_i64());
            let flags = idx_flags.and_then(|i| row.get(i)).and_then(|v| v.as_i64());
            if name.as_deref() == Some("Tables") {
                if let Some(id) = id {
                    self.table_parent_id = id as i32;
                }
            }
            if name.as_deref() == Some("Databases") {
                if let Some(id) = id {
                    self.db_parent_id = id as i32;
                }
            }
            if name.as_deref() == Some("Relationships") {
                if let Some(id) = id {
                    self.rel_parent_id = id as i32;
                }
            }
            if let (Some(name), Some(id), Some(typ)) = (name, id, typ) {
                let page = id as i32;
                // user tables (type 1), linked ODBC tables (type 4, which still
                // have a local definition page) and linked tables (type 6,
                // whose id is a synthetic object id instead of a page number)
                // are found by name; the other object types are not tables
                if typ == TYPE_TABLE as i64
                    || typ == TYPE_LINKED_ODBC_TABLE as i64
                    || typ == TYPE_LINKED_TABLE as i64
                {
                    self.table_lookup
                        .insert(name.to_uppercase(), (page, flags.unwrap_or(0) as i32));
                }
                let linked_table_name = || {
                    idx_foreign
                        .and_then(|i| row.get(i))
                        .and_then(|v| v.as_text())
                        .unwrap_or_default()
                        .to_string()
                };
                if typ == TYPE_LINKED_TABLE as i64 {
                    // `DatabaseImpl.createTableInfo`: a linked table keeps the
                    // linkee file name (`Database`) and remote table name
                    // (`ForeignName`) of its catalog row
                    let linked_db_name = idx_db
                        .and_then(|i| row.get(i))
                        .and_then(|v| v.as_text())
                        .unwrap_or_default()
                        .to_string();
                    self.linked_tables.insert(
                        name.to_uppercase(),
                        LinkedTableInfo {
                            name: name.to_string(),
                            object_id: page,
                            flags: flags.unwrap_or(0) as i32,
                            linked_db_name,
                            linked_table_name: linked_table_name(),
                        },
                    );
                } else if typ == TYPE_LINKED_ODBC_TABLE as i64 {
                    // `DatabaseImpl.LinkedODBCTableInfo`: an ODBC link keeps
                    // the connect string (`Connect`) and the remote table name
                    // (`ForeignName`); it has no linkee database name
                    let connection_name = idx_connect
                        .and_then(|i| row.get(i))
                        .and_then(|v| v.as_text())
                        .unwrap_or_default()
                        .to_string();
                    self.linked_odbc_tables.insert(
                        name.to_uppercase(),
                        LinkedOdbcTableInfo {
                            name: name.to_string(),
                            object_id: page,
                            flags: flags.unwrap_or(0) as i32,
                            connection_name,
                            linked_table_name: linked_table_name(),
                        },
                    );
                }
            }
        }
        if self.table_parent_id == 0 {
            self.table_parent_id = DB_PARENT_ID;
        }
        if self.db_parent_id == 0 {
            self.db_parent_id = DB_PARENT_ID;
        }
        Ok(())
    }

    pub fn get_table(&mut self, name: &str) -> Result<&mut Table> {
        // `DatabaseImpl.getTable(TableInfo, ...)`: a linked table is looked up
        // in (or opened from) the linkee database, so the returned table reads
        // from the linkee file.  The linkee is owned by this database (see the
        // `linked` module), which is why the returned reference can stay tied
        // to `&mut self`.
        if let Some(info) = self.linked_table_info(name).cloned() {
            let linked_db = self.get_or_open_linked_database(&info.linked_db_name)?;
            return linked_db.get_table(&info.linked_table_name);
        }
        // `Database.getTable(String)` never returns a system table ("or null
        // if it doesn't exist (or is a system table)").  The Result-based port
        // keeps a single table lookup which covers system tables as well (the
        // internal callers need it), and offers
        // [`Database::get_system_table`] for Java's `getSystemTable`; this is
        // the one documented semantic difference of the lookup APIs.
        let (page, flags) = match self.table_lookup.get(&name.to_uppercase()) {
            Some(v) => *v,
            None => return Err(Error::Jackcess(format!("Table {} does not exist", name))),
        };
        self.load_table(page, name, flags)
    }

    /// `Database.getSystemTable(String)`: the table of any type (system
    /// included), or `None` if it does not exist.
    ///
    /// Java's method returns `null` in that case; the port returns `Option`.
    /// Unlike [`Database::get_table`] this includes system tables, exactly like
    /// `DatabaseImpl.getSystemTable` (which passes `includeSystemTables =
    /// true`).
    pub fn get_system_table(&mut self, name: &str) -> Result<Option<&mut Table>> {
        self.find_table(name)
    }

    pub fn find_table(&mut self, name: &str) -> Result<Option<&mut Table>> {
        // the linked table branch of `DatabaseImpl.getTable(name, ...)`
        if let Some(info) = self.linked_table_info(name).cloned() {
            let linked_db = self.get_or_open_linked_database(&info.linked_db_name)?;
            return linked_db.find_table(&info.linked_table_name);
        }
        let entry = self.table_lookup.get(&name.to_uppercase()).copied();
        match entry {
            Some((page, flags)) => Ok(Some(self.load_table(page, name, flags)?)),
            None => Ok(None),
        }
    }

    pub fn load_table(&mut self, page: i32, name: &str, flags: i32) -> Result<&mut Table> {
        if page <= 0 {
            // a linked table has no table definition page (its catalog id is a
            // synthetic object id); the table-name based APIs resolve the link
            // to the linkee database
            return Err(Error::Unsupported(format!(
                "table '{}' has no table definition page (it is a linked table); \
                 use the table name based API",
                name
            )));
        }
        if !self.tables.contains_key(&page) {
            let mut table = Table::read(&mut self.channel, page, name, flags, &self.format)?;
            table.set_database_error_handler(self.error_handler.clone());
            self.apply_table_settings(&mut table);
            self.tables.insert(page, table);
            self.apply_catalog_table_info(page)?;
            self.init_table_column_properties(page)?;
        }
        Ok(self.tables.get_mut(&page).unwrap())
    }

    pub fn table_by_page(&mut self, page: i32) -> Result<&mut Table> {
        // a linked table's catalog id is a synthetic (negative) object id, not
        // a table definition page.  Resolve it through the linkee like
        // `DatabaseImpl.getTable(TableInfo, ...)` does for type 6 rows.
        if page < 0 {
            if let Some(info) = self
                .linked_tables
                .values()
                .find(|info| info.object_id == page)
                .cloned()
            {
                let linked_db = self.get_or_open_linked_database(&info.linked_db_name)?;
                return linked_db.get_table(&info.linked_table_name);
            }
            return Err(Error::Jackcess(format!(
                "invalid table definition page number {}",
                page
            )));
        }
        if !self.tables.contains_key(&page) {
            // the catalog spelling of the name is the table name the Java
            // library reports (the lookup key is upper case)
            let name = match self.catalog_table_name(page)? {
                Some(name) => name,
                None => self
                    .table_lookup
                    .iter()
                    .find(|(_, (p, _))| *p == page)
                    .map(|(n, _)| n.clone())
                    .unwrap_or_default(),
            };
            let flags = self
                .table_lookup
                .get(&name.to_uppercase())
                .map(|(_, f)| *f)
                .unwrap_or(0);
            let mut table = Table::read(&mut self.channel, page, &name, flags, &self.format)?;
            table.set_database_error_handler(self.error_handler.clone());
            self.apply_table_settings(&mut table);
            self.tables.insert(page, table);
            self.apply_catalog_table_info(page)?;
            self.init_table_column_properties(page)?;
        }
        Ok(self.tables.get_mut(&page).unwrap())
    }

    /// Alias of [`Database::table_by_page`] (`DatabaseImpl.getTable(int
    /// tableDefPageNumber)`).  A linked table's synthetic object id resolves to
    /// the table in the linkee database.
    pub fn get_table_by_page(&mut self, page: i32) -> Result<&mut Table> {
        self.table_by_page(page)
    }

    // ------------------------------------------------------------------
    // system catalog access
    // ------------------------------------------------------------------

    /// All readable rows of `MSysObjects`, together with the column indices
    /// the metadata APIs need.  Deleted catalog rows are skipped.
    fn catalog_entries(&mut self) -> Result<(CatalogColumns, Vec<Vec<Value>>)> {
        let rows = self.read_table_rows(PAGE_SYSTEM_CATALOG)?;
        let cols = {
            let table = self.tables.get(&PAGE_SYSTEM_CATALOG).ok_or_else(|| {
                Error::IllegalState("the system catalog is not loaded".to_string())
            })?;
            CatalogColumns::from_table(table)
        };
        Ok((cols, rows.into_iter().flatten().collect()))
    }

    /// The `(Type, DateCreate, DateUpdate)` catalog values of one object.
    fn catalog_object_info(
        &mut self,
        object_id: i32,
    ) -> Result<(
        Option<i16>,
        Option<crate::datenums::LocalDateTime>,
        Option<crate::datenums::LocalDateTime>,
    )> {
        let (cols, rows) = self.catalog_entries()?;
        let tz = self.tz;
        for row in &rows {
            if cols.i64(row, cols.id) == Some(object_id as i64) {
                return Ok((
                    cols.i64(row, cols.typ).map(|v| v as i16),
                    cols.date(row, cols.date_create, &tz),
                    cols.date(row, cols.date_update, &tz),
                ));
            }
        }
        Ok((None, None, None))
    }

    /// Applies the catalog information of a freshly loaded table: the catalog
    /// type (`Type == 4` makes it a definition-only table) and the creation /
    /// update dates (`Table.getCreatedDate` / `getUpdatedDate`).
    ///
    /// This includes the system catalog itself, exactly like Java:
    /// `TableImpl.getCreatedDate`/`getUpdatedDate` ask the database for the
    /// catalog row of the table definition page, which works for `MSysObjects`
    /// too (its own row is the one with `Id == PAGE_SYSTEM_CATALOG`).  Reading
    /// the catalog rows here cannot recurse: the table is inserted into
    /// [`Database::tables`] before this is called.
    fn apply_catalog_table_info(&mut self, page: i32) -> Result<()> {
        let (typ, created, updated) = self.catalog_object_info(page)?;
        if let Some(table) = self.tables.get_mut(&page) {
            table.definition_only = typ == Some(TYPE_LINKED_ODBC_TABLE);
            table.created_date = created;
            table.updated_date = updated;
        }
        Ok(())
    }

    /// The definition page of the named system table (`DatabaseImpl
    /// .getRequiredSystemTable` finds it by name in the catalog).
    fn system_table_page(&mut self, name: &str) -> Result<Option<i32>> {
        let (cols, rows) = self.catalog_entries()?;
        for row in &rows {
            let row_name = match cols.text(row, cols.name) {
                Some(name) => name,
                None => continue,
            };
            if row_name.eq_ignore_ascii_case(name)
                && cols.i64(row, cols.typ).map(|typ| typ as i16) == Some(TYPE_TABLE)
            {
                if let Some(id) = cols.i64(row, cols.id) {
                    return Ok(Some(id as i32));
                }
            }
        }
        Ok(None)
    }

    /// `DatabaseImpl.getRequiredSystemTable`: a required system table, or the
    /// `IOException` Java raises when it is missing.
    fn required_system_table_page(&mut self, name: &str) -> Result<i32> {
        match self.system_table_page(name)? {
            Some(page) => Ok(page),
            None => Err(Error::Jackcess(format!(
                "Could not find system table {} (Db={})",
                name, self.name
            ))),
        }
    }

    /// `Database.getAccessControlEntries()`: the system Access Control Entries
    /// table (`MSysACEs`), loaded on demand.
    ///
    /// This is Java's table-level return value (the caller then reads the
    /// `ObjectId`/`SID`/`ACM`/`FInheritable` rows in cursor order); the typed
    /// rows are also available through
    /// [`Database::get_access_control_entries_rows`].
    pub fn get_access_control_entries(&mut self) -> Result<&mut Table> {
        let page = self.required_system_table_page(TABLE_SYSTEM_ACES)?;
        self.table_by_page(page)
    }

    /// The rows of `Database.getAccessControlEntries()` as Java's `Row` API
    /// returns them: in the table (cursor) order, with the stored types
    /// (`ObjectId` and `ACM` as Java `Integer`, `SID` as `byte[]`,
    /// `FInheritable` as `Boolean`).
    ///
    /// A deleted row is skipped exactly like the cursor iteration skips it.
    pub fn get_access_control_entries_rows(&mut self) -> Result<Vec<AceEntry>> {
        let page = {
            let table = self.get_access_control_entries()?;
            table.get_table_def_page_number()
        };
        let (object_id, sid, acm, inheritable) = {
            let table = self.tables.get(&page).unwrap();
            let column = |name: &str| -> Result<usize> {
                table.column_index(name).ok_or_else(|| {
                    Error::IllegalArgument(format!(
                        "Column with name {} does not exist in this table",
                        name
                    ))
                })
            };
            (
                column(ACE_COL_OBJECT_ID)?,
                column(ACE_COL_SID)?,
                column(ACE_COL_ACM)?,
                column(ACE_COL_F_INHERITABLE)?,
            )
        };
        let rows = self.read_table_rows(page)?;
        let mut entries = Vec::new();
        for row in rows.into_iter().flatten() {
            entries.push(AceEntry {
                object_id: row.get(object_id).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                sid: row
                    .get(sid)
                    .and_then(|v| v.as_bytes())
                    .unwrap_or_default()
                    .to_vec(),
                acm: row.get(acm).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                f_inheritable: matches!(row.get(inheritable), Some(Value::Bool(true))),
            });
        }
        Ok(entries)
    }

    /// `Table.getNameMap()` for any table, system tables and the system catalog
    /// included: the `NameMap` property of the table's catalog row, parsed.
    ///
    /// Java loads the property maps of any table (`TableImpl.getPropertyMaps`)
    /// and reads the `NameMap` value from the default map; the port caches the
    /// raw value on the table (see [`Table::name_map_blob`]), so a later
    /// [`Table::get_name_map`] returns the same map.  `None` when the table has
    /// no `NameMap` property.
    pub fn get_table_name_map(
        &mut self,
        table_name: &str,
    ) -> Result<Option<crate::namemap::TableNameMap>> {
        let page = self.table_page(table_name)?;
        self.get_table_name_map_by_page(page)
    }

    /// [`Database::get_table_name_map`] for a table definition page.
    pub fn get_table_name_map_by_page(
        &mut self,
        page: i32,
    ) -> Result<Option<crate::namemap::TableNameMap>> {
        let blob = self.name_map_blob_for(page)?;
        if let Some(table) = self.tables.get_mut(&page) {
            table.name_map_blob = blob.clone();
        }
        match blob {
            Some(bytes) => Ok(Some(crate::namemap::TableNameMap::read(&bytes)?)),
            None => Ok(None),
        }
    }

    /// The raw `NameMap` property value of the catalog row of the table on the
    /// given definition page (`TableImpl.getNameMap`'s property lookup).
    fn name_map_blob_for(&mut self, page: i32) -> Result<Option<Vec<u8>>> {
        let maps = self.get_object_properties(page)?;
        Ok(maps
            .default_map()
            .and_then(|map| map.get_value(crate::property::NAME_MAP_PROP))
            .and_then(|value| match value {
                Value::Bytes(bytes) => Some(bytes.clone()),
                _ => None,
            }))
    }

    /// `Index.getReferencedIndex()`: the index on the other side of this
    /// index's foreign key reference, or `None` when the index has no
    /// reference.
    ///
    /// Java's method lives on `Index`, which knows its table; the port's
    /// [`crate::index::IndexImpl`] has no back reference to the database, so
    /// the table and index are named instead.  The resolution and every
    /// validation are Java's: a reference to a missing table or index is an
    /// `IOException`, and the found index must reference *this* index back
    /// (the reciprocal-reference validation), otherwise the failure names the
    /// unexpected index and its reference.  The messages carry Java's
    /// `(Db=<name>;Index=<name>)` context.
    pub fn get_referenced_index(
        &mut self,
        table_name: &str,
        index_name: &str,
    ) -> Result<Option<crate::index::IndexImpl>> {
        let table_page = self.table_page(table_name)?;
        let (reference, index_number, stored_index_name) = {
            let table = self.tables.get(&table_page).unwrap();
            // `Table.getIndex(name)`: the caller of the Java method already has
            // the index; an unknown name is Java's IllegalArgumentException
            let index = table.get_index(index_name)?;
            (index.reference, index.index_number, index.name.clone())
        };
        let reference = match reference {
            Some(reference) => reference,
            None => return Ok(None),
        };
        let db_name = self.name.clone();
        let context =
            |message: String| format!("{} (Db={};Index={})", message, db_name, stored_index_name);

        // `getTable().getDatabase().getTable(otherTablePageNumber)`; Java's
        // `getTable(int)` returns null for a page without a catalog row, which
        // is the "missing table" case
        let ref_table = match self.table_by_page(reference.other_table_page_number) {
            Ok(table) => table,
            Err(_) => {
                return Err(Error::Jackcess(context(format!(
                    "Reference to missing table {}",
                    reference.other_table_page_number
                ))))
            }
        };
        let (ref_index, ref_table_name) = {
            let index_number = reference.other_index_number;
            match ref_table
                .indexes
                .iter()
                .find(|index| index.index_number == index_number)
            {
                Some(index) => (index.clone(), ref_table.name.clone()),
                None => {
                    return Err(Error::Jackcess(context(format!(
                        "Reference to missing index {} on table {}",
                        index_number, ref_table.name
                    ))))
                }
            }
        };

        // finally verify that we found the expected index (it must reference
        // this index)
        let other_reference = ref_index.reference;
        let reciprocal = other_reference.is_some_and(|other| {
            other.other_table_page_number == table_page && other.other_index_number == index_number
        });
        if !reciprocal {
            return Err(Error::Jackcess(context(format!(
                "Found unexpected index {} on table {} with reference {}",
                ref_index.name,
                ref_table_name,
                match other_reference {
                    Some(other) => other.to_string(),
                    None => "null".to_string(),
                }
            ))));
        }
        Ok(Some(ref_index))
    }

    // ------------------------------------------------------------------
    // table metadata (`TableMetaData`)
    // ------------------------------------------------------------------

    fn table_meta_data_from_row(
        cols: &CatalogColumns,
        row: &[Value],
        tz: &crate::datenums::TimeZone,
    ) -> Option<TableMetaData> {
        let name = cols.text(row, cols.name)?;
        let object_id = cols.i64(row, cols.id)? as i32;
        let typ = cols.i64(row, cols.typ)? as i16;
        let flags = cols.i64(row, cols.flags).unwrap_or(0) as i32;
        let linked_table_name = cols.text(row, cols.foreign_name);
        let (table_type, linked_db_name, connection_name) = match typ {
            TYPE_LINKED_TABLE => (
                TableMetaDataType::Linked,
                cols.text(row, cols.database),
                None,
            ),
            TYPE_LINKED_ODBC_TABLE => (
                TableMetaDataType::LinkedOdbc,
                None,
                cols.text(row, cols.connect),
            ),
            _ => (TableMetaDataType::Local, None, None),
        };
        Some(TableMetaData {
            table_type,
            name,
            flags,
            page_number: Some(object_id),
            linked_table_name,
            linked_db_name,
            connection_name,
            created_date: cols.date(row, cols.date_create, tz),
            updated_date: cols.date(row, cols.date_update, tz),
        })
    }

    /// `Database.getTableMetaData(String)`: the catalog metadata of the named
    /// table (case-insensitive), or `None` if there is no such table.  System
    /// tables are found too, exactly like the Java method.
    pub fn get_table_meta_data(&mut self, name: &str) -> Result<Option<TableMetaData>> {
        let parent_id = self.table_parent_id;
        let (cols, rows) = self.catalog_entries()?;
        let tz = self.tz;
        for row in &rows {
            let row_name = match cols.text(row, cols.name) {
                Some(row_name) => row_name,
                None => continue,
            };
            if !row_name.eq_ignore_ascii_case(name) {
                continue;
            }
            let typ = match cols.i64(row, cols.typ) {
                Some(typ) => typ as i16,
                None => continue,
            };
            if !matches!(typ, TYPE_TABLE | TYPE_LINKED_ODBC_TABLE | TYPE_LINKED_TABLE) {
                continue;
            }
            if cols.i64(row, cols.parent) != Some(parent_id as i64) {
                continue;
            }
            if let Some(meta) = Self::table_meta_data_from_row(&cols, row, &tz) {
                return Ok(Some(meta));
            }
        }
        Ok(None)
    }

    /// The [`crate::table::ColumnMetaData`] of one column of the named table,
    /// with the version-history link resolved: Java's `Column.isAppendOnly()`
    /// asks the column for its version history column, which is linked through
    /// `MSysComplexColumns` and therefore needs the database.  The
    /// [`crate::table::Table`] method alone can only report the raw column
    /// definition flag and leaves `version_history_column` empty.
    pub fn get_column_meta_data(
        &mut self,
        table_name: &str,
        column_name: &str,
    ) -> Result<Option<crate::table::ColumnMetaData>> {
        // a type 6 linked table is opened from the linkee database
        if let Some(info) = self.linked_table_info(table_name).cloned() {
            let linked_db = self.get_or_open_linked_database(&info.linked_db_name)?;
            return linked_db.get_column_meta_data(&info.linked_table_name, column_name);
        }
        let (page, catalog_name) = match self.find_table(table_name)? {
            Some(table) => (table.table_def_page_number, table.name.clone()),
            None => return Ok(None),
        };
        let mut meta = match self.tables.get(&page) {
            Some(table) => table.get_column_meta_data(column_name),
            None => None,
        };
        if let Some(meta) = &mut meta {
            if meta.data_type == DataType::Memo {
                // `Column.isAppendOnly`/`getVersionHistoryColumn`: the memo
                // column is append-only when a version history complex column
                // in `MSysComplexColumns` points back at it.  A read failure
                // leaves it plain, like Java's `postTableLoadInit`.
                let version_history = self
                    .version_history_column_name(&catalog_name, &meta.name)
                    .unwrap_or(None);
                meta.append_only = version_history.is_some();
                meta.version_history_column = version_history;
            }
        }
        Ok(meta)
    }

    /// The [`crate::table::ColumnMetaData`] of every column of the named table,
    /// in definition order (with the version-history link resolved like
    /// [`Database::get_column_meta_data`]).
    pub fn get_column_meta_data_all(
        &mut self,
        table_name: &str,
    ) -> Result<Vec<crate::table::ColumnMetaData>> {
        // a type 6 linked table is opened from the linkee database
        if let Some(info) = self.linked_table_info(table_name).cloned() {
            let linked_db = self.get_or_open_linked_database(&info.linked_db_name)?;
            return linked_db.get_column_meta_data_all(&info.linked_table_name);
        }
        let (page, catalog_name) = match self.find_table(table_name)? {
            Some(table) => (table.table_def_page_number, table.name.clone()),
            None => return Ok(Vec::new()),
        };
        let mut columns = match self.tables.get(&page) {
            Some(table) => table.get_column_meta_data_all(),
            None => return Ok(Vec::new()),
        };
        for column in &mut columns {
            if column.data_type == DataType::Memo {
                let version_history = self
                    .version_history_column_name(&catalog_name, &column.name)
                    .unwrap_or(None);
                column.append_only = version_history.is_some();
                column.version_history_column = version_history;
            }
        }
        Ok(columns)
    }

    /// All table metadata catalog rows in the order of Java's
    /// `Database.newTableMetaDataIterable`.
    ///
    /// Java's default table finder iterates the `MSysObjects` index on
    /// `(ParentId, Name)` (`DefaultTableFinder.getTableNamesCursor` returns
    /// `_systemCatalogCursor.getIndex().newCursor()`), so the metadata comes
    /// back in index entry order: parent id first, then the table name in
    /// Access' text sort order.  The port scans the catalog rows like the Java
    /// `NoIndexTableFinder` and therefore sorts the collected rows the same way
    /// (parent id, then the case insensitive name order the general sort order
    /// yields for these ASCII names).
    pub fn table_meta_data_all(&mut self) -> Result<Vec<TableMetaData>> {
        let parent_id = self.table_parent_id;
        let (cols, rows) = self.catalog_entries()?;
        let tz = self.tz;
        let mut out: Vec<(i64, TableMetaData)> = Vec::new();
        for row in &rows {
            let typ = match cols.i64(row, cols.typ) {
                Some(typ) => typ as i16,
                None => continue,
            };
            if !matches!(typ, TYPE_TABLE | TYPE_LINKED_ODBC_TABLE | TYPE_LINKED_TABLE) {
                continue;
            }
            let row_parent = match cols.i64(row, cols.parent) {
                Some(parent) => parent,
                None => continue,
            };
            if row_parent != parent_id as i64 {
                continue;
            }
            if let Some(meta) = Self::table_meta_data_from_row(&cols, row, &tz) {
                out.push((row_parent, meta));
            }
        }
        out.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| compare_names_case_insensitive(&left.1.name, &right.1.name))
        });
        Ok(out.into_iter().map(|(_, meta)| meta).collect())
    }

    /// `DatabaseImpl.isSystemObject(flags)`.
    fn is_system_object(flags: i32) -> bool {
        (flags & SYSTEM_OBJECT_FLAGS) != 0
    }

    /// `Database.getTableNames()`: the names of the normal (non-system) tables
    /// and of every linked table, sorted like Java's
    /// `TreeSet(String.CASE_INSENSITIVE_ORDER)`.
    pub fn get_table_names(&mut self) -> Result<Vec<String>> {
        self.get_table_names_filtered(true, false, true)
    }

    /// `Database.getSystemTableNames()`: the names of the system tables (catalog
    /// `Type == 1` with a system object flag), sorted like the Java method.
    pub fn get_system_table_names(&mut self) -> Result<Vec<String>> {
        self.get_table_names_filtered(false, true, false)
    }

    /// `DatabaseImpl.getTableNames(boolean normalTables, boolean systemTables,
    /// boolean linkedTables)`: the catalog table names selected by the three
    /// flags, sorted like Java's `TreeSet(String.CASE_INSENSITIVE_ORDER)`.
    ///
    /// This is the name set [`crate::builder::TableIterableBuilder`] iterates
    /// (`newIterable()` in Java).  The `MSysObjects` rows are scanned in
    /// catalog order and the names are then sorted case insensitively (Java
    /// fills a `TreeSet`), so a name which appears twice with different casing
    /// keeps its first catalog spelling.
    pub fn get_table_names_filtered(
        &mut self,
        normal_tables: bool,
        system_tables: bool,
        linked_tables: bool,
    ) -> Result<Vec<String>> {
        let parent_id = self.table_parent_id;
        let (cols, rows) = self.catalog_entries()?;
        let mut names = Vec::new();
        for row in &rows {
            // `DatabaseImpl.TableFinder.getTableNames`: a damage tolerant scan
            // which skips a row without a name, flags or parent
            let name = match cols.text(row, cols.name) {
                Some(name) => name,
                None => continue,
            };
            let flags = match cols.i64(row, cols.flags) {
                Some(flags) => flags as i32,
                None => continue,
            };
            let parent = match cols.i64(row, cols.parent) {
                Some(parent) => parent,
                None => continue,
            };
            if parent != parent_id as i64 {
                continue;
            }
            match cols.i64(row, cols.typ).map(|v| v as i16) {
                Some(TYPE_TABLE) => {
                    if Self::is_system_object(flags) {
                        if system_tables {
                            names.push(name);
                        }
                    } else if normal_tables {
                        names.push(name);
                    }
                }
                Some(TYPE_LINKED_TABLE) | Some(TYPE_LINKED_ODBC_TABLE) if linked_tables => {
                    names.push(name);
                }
                _ => {}
            }
        }
        sort_case_insensitive(&mut names);
        Ok(names)
    }

    /// `Database.newIterable()`: a [`crate::builder::TableIterableBuilder`] for
    /// this database.
    ///
    /// Java's builder keeps the database handle and its `iterator()` asks the
    /// database for the selected names; the port's builder is a plain filter
    /// description which is handed the database when it is used
    /// ([`crate::builder::TableIterableBuilder::table_names`],
    /// [`crate::builder::TableIterableBuilder::for_each_table`]), because the
    /// tables of a [`Database`] are reached through `&mut Database`.
    pub fn new_iterable(&self) -> crate::builder::TableIterableBuilder {
        crate::builder::TableIterableBuilder::new()
    }

    /// `Database.newTableMetaDataIterable()`: an iterable over the metadata of
    /// every table of the database.
    ///
    /// Java's iterable walks the `MSysObjects` index in `(ParentId, Name)`
    /// order and yields the `TableMetaData` of every table type (system tables
    /// and linked tables included); the port materializes that same list
    /// through [`Database::table_meta_data_all`] (which sorts the scanned rows
    /// into that order), so the iterator can be handed out without borrowing
    /// the database.
    pub fn new_table_meta_data_iterable(&mut self) -> Result<TableMetaDataIterable> {
        Ok(TableMetaDataIterable::new(self.table_meta_data_all()?))
    }

    /// `Database.newTableMetaDataStream()`: a stream over
    /// [`Database::new_table_meta_data_iterable`].
    ///
    /// Rust's `Iterator` is that stream; this returns the owning iterator
    /// (`Vec::into_iter`) a `Stream` would wrap.
    pub fn new_table_meta_data_stream(&mut self) -> Result<std::vec::IntoIter<TableMetaData>> {
        Ok(self.table_meta_data_all()?.into_iter())
    }

    /// The `Table` values of Java's `Database.iterator()` which this database
    /// owns: the normal (non-system) *local* tables, in Java's iteration order
    /// (case-insensitive name order).
    ///
    /// Java's iterator yields the linkee's `Table` instance for a linked table;
    /// a `Vec<&mut Table>` cannot hold references owned by two different
    /// `Database` instances, so the linked tables are left out here and are
    /// reachable through [`Database::table_names`] + [`Database::get_table`] or
    /// [`crate::builder::TableIterableBuilder::for_each_table`] (which visits
    /// every selected table in Java order).  Use
    /// [`Database::tables_with_system`] to include the system tables.
    pub fn tables(&mut self) -> Result<Vec<&mut Table>> {
        self.tables_filtered(false)
    }

    /// [`Database::tables`] with the system tables included (Java's
    /// `newIterable().setIncludeSystemTables(true)` local subset).
    pub fn tables_with_system(&mut self) -> Result<Vec<&mut Table>> {
        self.tables_filtered(true)
    }

    /// The local tables selected by the name order of
    /// `DatabaseImpl.getTableNames(normal=true, system=<include_system>,
    /// linked=false)`.
    fn tables_filtered(&mut self, include_system: bool) -> Result<Vec<&mut Table>> {
        let names = self.get_table_names_filtered(true, include_system, false)?;
        let mut pages = Vec::with_capacity(names.len());
        for name in &names {
            if let Some((page, flags)) = self.table_lookup.get(&name.to_uppercase()).copied() {
                // load (or reuse) the table so the reference below exists
                self.load_table(page, name, flags)?;
                pages.push(page);
            }
        }
        let mut tables: Vec<&mut Table> = self
            .tables
            .iter_mut()
            .filter(|(page, _)| pages.contains(page))
            .map(|(_, table)| table)
            .collect();
        tables.sort_by(|left, right| compare_names_case_insensitive(&left.name, &right.name));
        Ok(tables)
    }

    // ------------------------------------------------------------------
    // queries (`Database.getQueries`)
    // ------------------------------------------------------------------

    /// `Database.getQueries()`: builds every query from the catalog (`Type ==
    /// 5`) rows and the `MSysQueries` rows which share their object id.
    pub fn get_queries(&mut self) -> Result<Vec<crate::query::Query>> {
        let (cols, catalog_rows) = self.catalog_entries()?;

        /** `(name, flags, id)` of every query catalog row, in catalog order */
        struct QueryInfo {
            name: String,
            flags: Option<i32>,
            object_id: i32,
        }
        let mut query_info: Vec<QueryInfo> = Vec::new();
        let mut query_rows: HashMap<i32, Vec<crate::query::QueryRow>> = HashMap::new();
        for row in &catalog_rows {
            let name = match cols.text(row, cols.name) {
                Some(name) => name,
                None => continue,
            };
            if cols.i64(row, cols.typ) != Some(TYPE_QUERY as i64) {
                continue;
            }
            let object_id = match cols.i64(row, cols.id) {
                Some(id) => id as i32,
                None => continue,
            };
            let flags = cols.i64(row, cols.flags).map(|flags| flags as i32);
            query_rows.entry(object_id).or_default();
            query_info.push(QueryInfo {
                name,
                flags,
                object_id,
            });
        }

        let query_page = self
            .system_table_page(TABLE_SYSTEM_QUERIES)?
            .ok_or_else(|| {
                Error::Jackcess(format!(
                    "Could not find system table {} (Db={})",
                    TABLE_SYSTEM_QUERIES, self.name
                ))
            })?;
        let system_rows = self.read_table_rows(query_page)?;
        let system_table = self.tables.get(&query_page).unwrap();
        let idx_attribute = system_table.column_index(crate::query::COL_ATTRIBUTE);
        let idx_expression = system_table.column_index(crate::query::COL_EXPRESSION);
        let idx_flag = system_table.column_index(crate::query::COL_FLAG);
        let idx_extra = system_table.column_index(crate::query::COL_EXTRA);
        let idx_name1 = system_table.column_index(crate::query::COL_NAME1);
        let idx_name2 = system_table.column_index(crate::query::COL_NAME2);
        let idx_object_id = system_table.column_index(crate::query::COL_OBJECTID);
        let idx_order = system_table.column_index(crate::query::COL_ORDER);

        for row in system_rows.into_iter().flatten() {
            let query_row = crate::query::QueryRow {
                attribute: value_as_u8(row.get(idx_attribute.unwrap_or(usize::MAX))),
                expression: value_as_text(row.get(idx_expression.unwrap_or(usize::MAX))),
                flag: value_as_i16(row.get(idx_flag.unwrap_or(usize::MAX))),
                lv_extra: value_as_i32(row.get(idx_extra.unwrap_or(usize::MAX))),
                name1: value_as_text(row.get(idx_name1.unwrap_or(usize::MAX))),
                name2: value_as_text(row.get(idx_name2.unwrap_or(usize::MAX))),
                object_id: value_as_i32(row.get(idx_object_id.unwrap_or(usize::MAX))),
                order: row
                    .get(idx_order.unwrap_or(usize::MAX))
                    .and_then(|value| value.as_bytes())
                    .map(|bytes| bytes.to_vec()),
            };
            // a `MSysQueries` row whose id has no catalog row is ignored (Java
            // logs a warning and continues)
            if let Some(object_id) = query_row.object_id {
                if let Some(rows) = query_rows.get_mut(&object_id) {
                    rows.push(query_row);
                }
            }
        }

        let mut queries = Vec::with_capacity(query_info.len());
        for info in query_info {
            let flags = match info.flags {
                Some(flags) => flags,
                // the flags carry the query type, so a damaged row without
                // them describes no query
                None => continue,
            };
            let rows = query_rows.remove(&info.object_id).unwrap_or_default();
            queries.push(crate::query::create_query(
                flags,
                &info.name,
                &rows,
                info.object_id,
            )?);
        }
        // Java binds the query's property maps lazily; loading them here keeps
        // `Query::get_properties()` working without a second database walk, and
        // a bad blob stays on the query and is reported by the getter.
        for query in &mut queries {
            let _ = self.load_query_properties(query);
        }
        Ok(queries)
    }

    /// The queries whose catalog flags mark them as system objects
    /// (`DatabaseImpl.isSystemObject`).  jackcess 5.0.4 itself only exposes
    /// `getQueries`; this is the query counterpart of
    /// [`Database::get_system_table_names`].
    pub fn get_system_queries(&mut self) -> Result<Vec<crate::query::Query>> {
        Ok(self
            .get_queries()?
            .into_iter()
            .filter(|query| Self::is_system_object(query.object_flag()))
            .collect())
    }

    /// The hidden queries (`Query.isHidden()`).
    pub fn get_hidden_queries(&mut self) -> Result<Vec<crate::query::Query>> {
        Ok(self
            .get_queries()?
            .into_iter()
            .filter(|query| query.is_hidden())
            .collect())
    }

    /// The query with the given name (case-insensitive), if any.  jackcess
    /// 5.0.4 has no such convenience method; it is built on
    /// [`Database::get_queries`].
    pub fn get_query(&mut self, name: &str) -> Result<Option<crate::query::Query>> {
        Ok(self
            .get_queries()?
            .into_iter()
            .find(|query| query.name().eq_ignore_ascii_case(name)))
    }

    /// The parameters of the named query (`Query.getParameters()`).
    pub fn get_query_parameters(&mut self, name: &str) -> Result<Option<Vec<String>>> {
        match self.get_query(name)? {
            Some(query) => Ok(Some(query.get_parameters()?)),
            None => Ok(None),
        }
    }

    /// `Database.flush()`: flushes any pending changes to the database file (and
    /// any linked databases) to disk.
    ///
    /// Java flushes the linked databases and then its page channel; a closed
    /// database reports the `ClosedChannelException` a flush of the closed
    /// `FileChannel` throws.  The port keeps the closed state on the database
    /// (see [`Database::close`]), so it reproduces that error here.
    pub fn flush(&mut self) -> Result<()> {
        if self.closed {
            return Err(closed_channel_error());
        }
        for linked_db in self.linked_databases.values_mut() {
            linked_db.flush()?;
        }
        self.channel.flush()
    }

    /// `Database.close()`: flushes the linked databases and this database and
    /// marks it closed.
    ///
    /// Java's `DatabaseImpl.close` flushes every linkee and then closes its
    /// page channel (`PageChannel.close` flushes and closes the channel unless
    /// the channel was supplied by the caller through
    /// `DatabaseBuilder.setChannel`, in which case it is only flushed).  A
    /// closed `Database` reports `false` from `PageChannel.isOpen`, and a
    /// second `close` (or a `flush`) throws the `ClosedChannelException` the
    /// closed channel's `force` throws; the port models all of that with
    /// [`Database::is_open`] / [`Database::is_closed`] and the same error.
    ///
    /// The port has no closable channel, so operations other than `close` and
    /// `flush` do not check the flag: Java's later reads would fail with
    /// `ClosedChannelException` while the port's still read the file it was
    /// opened from (documented gap, kept so the existing single-owner read
    /// paths do not grow a state check).
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Err(closed_channel_error());
        }
        for linked_db in self.linked_databases.values_mut() {
            linked_db.close()?;
        }
        self.channel.flush()?;
        // `PageChannel.close` only closes the channel when the database owns
        // it; with a caller supplied channel Java leaves it open (and
        // `PageChannel.isOpen` stays true), so the port does not mark the
        // database closed either
        if self.close_channel {
            self.closed = true;
        }
        Ok(())
    }

    /// `PageChannel.isOpen()` of the owning channel: `false` after
    /// [`Database::close`].  (Java's `Database` interface does not expose the
    /// state; `DatabaseImpl.getPageChannel().isOpen()` does.)
    pub fn is_open(&self) -> bool {
        !self.closed
    }

    /// `!PageChannel.isOpen()`: whether [`Database::close`] has run.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// `Database.getFile()`: the file underlying this database.
    ///
    /// Java returns a `java.io.File` (and `getPath` the `Path`); the port
    /// represents both with the same [`Path`], so this is the alias of
    /// [`Database::get_path`] (`None` for an in-memory database, exactly like
    /// Java's `getFile` returning `null` when the builder was given a channel
    /// and no file).
    pub fn get_file(&self) -> Option<&Path> {
        self.get_path()
    }

    /// `DatabaseBuilder.setIgnoreBrokenSystemCatalogIndex`: whether the system
    /// catalog index is ignored when looking up tables.
    ///
    /// The port scans the `MSysObjects` rows for every metadata lookup, which
    /// is what Java's "no index" table finder does, so the flag never changes a
    /// result; it is recorded here so the API round-trips it.
    pub fn is_ignore_broken_system_catalog_index(&self) -> bool {
        self.ignore_broken_system_catalog_index
    }

    /// `Database.getErrorHandler`: the configured database level handler, or
    /// [`crate::table::DEFAULT_ERROR_HANDLER`] (always non-`None`, like Java).
    pub fn get_error_handler(&self) -> &dyn ErrorHandler {
        self.error_handler
            .as_deref()
            .unwrap_or(&crate::table::DEFAULT_ERROR_HANDLER)
    }

    /// `Database.setErrorHandler`: sets the database level handler, which is
    /// used by every table (and cursor) which does not have its own handler.
    /// `None` resets to the default handler.  The handler is copied into the
    /// already loaded tables because the port's [`Table`] does not keep a
    /// reference to its owning database.
    pub fn set_error_handler(&mut self, new_error_handler: Option<Arc<dyn ErrorHandler>>) {
        self.error_handler = new_error_handler;
        let handler = self.error_handler.clone();
        for table in self.tables.values_mut() {
            table.set_database_error_handler(handler.clone());
        }
    }

    /// Copies the database level settings a `TableImpl` asks its database for
    /// (`getDateTimeType` for every date conversion and `getColumnOrder` for
    /// the `getColumns` view) into a freshly read table.  The port's tables
    /// have no reference back to the database, so the value is copied when the
    /// table is loaded; [`Database::set_date_time_type`] refreshes the loaded
    /// tables and [`Database::set_column_order`] deliberately does not (Java
    /// applies the column order once, while constructing the table).
    fn apply_table_settings(&self, table: &mut Table) {
        table.set_date_time_type(self.date_time_type);
        table.set_time_zone(self.tz);
        table.set_column_order(self.column_order);
        // Java's `ColumnImpl.getColumnValidator` asks the database factory for
        // the external validator; the port's Table has no database back
        // reference, so the factory is applied while the table is loaded.
        let factory = self.get_column_validator_factory();
        for idx in 0..table.columns.len() {
            let handle = factory.create_validator(&table.columns[idx]);
            table.columns[idx].set_column_validator(handle);
        }
        // Java's `ColumnImpl` asks its database for the `DateTimeFactory` and
        // the time zone on every conversion (and `IndexData.ColumnDescriptor`
        // goes through the column), so the loaded indexes take the same
        // context.  The port's `Table` has no database back reference, which
        // is why the database copies both into the loaded index datas.
        for data in &mut table.index_datas {
            data.set_date_context(crate::column::DateContext::new(
                self.date_time_type,
                self.tz,
            ));
        }
    }

    // ------------------------------------------------------------------
    // database level policies
    // ------------------------------------------------------------------

    /// `Database.isEnforceForeignKeys`: whether foreign key relationships are
    /// enforced when rows are added, updated or deleted.  jackcess defaults to
    /// `true` (`getDefaultEnforceForeignKeys`).
    pub fn is_enforce_foreign_keys(&self) -> bool {
        self.enforce_foreign_keys
    }

    /// `Database.setEnforceForeignKeys`: turns foreign key enforcement on or
    /// off for the whole database.  jackcess accepts `null` for "the default
    /// policy"; the Rust port takes a plain `bool` and offers
    /// [`Database::reset_enforce_foreign_keys`] for that.
    pub fn set_enforce_foreign_keys(&mut self, enforce: bool) {
        self.enforce_foreign_keys = enforce;
    }

    /// `Database.setEnforceForeignKeys(null)`: restores the default policy.
    pub fn reset_enforce_foreign_keys(&mut self) {
        self.enforce_foreign_keys = DEFAULT_ENFORCE_FOREIGN_KEYS;
    }

    /// jackcess has no per-table foreign key setting, every table follows the
    /// database policy (`FKEnforcer.enforcing`).
    pub fn table_enforces_foreign_keys(&self, _table_name: &str) -> bool {
        self.enforce_foreign_keys
    }

    /// `Database.isReuseFreePages`: whether newly allocated pages are taken
    /// from the free space map before the file is grown.
    pub fn is_reuse_free_pages(&self) -> bool {
        self.channel.is_reuse_free_pages()
    }

    /// `Database.setReuseFreePages` (`null` means the default, `false`).
    pub fn set_reuse_free_pages(&mut self, reuse: bool) {
        self.channel.set_reuse_free_pages(reuse);
    }

    /// `Database.isAllowAutoNumberInsert`: whether an explicit value may be
    /// given for an auto-number column.
    pub fn is_allow_auto_number_insert(&self) -> bool {
        self.channel.allow_auto_number_insert
    }

    /// `Database.setAllowAutoNumberInsert` (`null` means the default, `false`).
    pub fn set_allow_auto_number_insert(&mut self, allow: bool) {
        self.channel.allow_auto_number_insert = allow;
    }

    /// `Table.setAllowAutoNumberInsert`: a per-table override of the database
    /// policy (`None` removes the override).
    pub fn set_table_allow_auto_number_insert(
        &mut self,
        table_name: &str,
        allow: Option<bool>,
    ) -> Result<()> {
        // `Table.setAllowAutoNumberInsert` on a linked table sets the flag on
        // the linkee's table instance
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.set_table_allow_auto_number_insert(&linked_name, allow);
        }
        let page = self.table_page(table_name)?;
        if let Some(table) = self.tables.get_mut(&page) {
            table.set_allow_auto_number_insert(allow);
        }
        Ok(())
    }

    /// `Database.isEvaluateExpressions`: whether the property based
    /// expressions are evaluated.
    ///
    /// When it is `false` (Java `Database.setEvaluateExpressions(false)`),
    /// jackcess:
    /// - does not build a calculated column evaluator, so a calculated column
    ///   keeps the value the row holds (explicit values are still written, in
    ///   the calculated value wrapper) and `CalcColEvaluator.add` ignores it;
    /// - does not generate column `DefaultValue` values on insert (`ColumnImpl
    ///   .reloadPropertiesValidators` returns before the default value is
    ///   built);
    /// - does not build the expression based column validation rule validator
    ///   nor the table row validator;
    /// - still enforces the `Required` and `AllowZeroLength` column validators,
    ///   because Java adds those before it checks the flag.
    ///
    /// The flag is a runtime setting: it is not stored in the database file and
    /// a reopened database starts from the default (`true`).
    pub fn is_evaluate_expressions(&self) -> bool {
        self.evaluate_expressions
    }

    /// `Database.setEvaluateExpressions(Boolean)`; `None` restores the default
    /// policy (`true`, [`DEFAULT_EVALUATE_EXPRESSIONS`]).
    pub fn set_evaluate_expressions(&mut self, evaluate: bool) {
        self.evaluate_expressions = evaluate;
    }

    /// `Database.setEvaluateExpressions(null)`.
    pub fn reset_evaluate_expressions(&mut self) {
        self.set_evaluate_expressions(DEFAULT_EVALUATE_EXPRESSIONS);
    }

    /// `Database.isWriteBrokenIndex`: whether an index write may leave a broken
    /// index behind instead of failing the row write.  This is a runtime policy
    /// (not stored in the file) which defaults to `false`
    /// ([`DEFAULT_WRITE_BROKEN_INDEX`]).
    pub fn is_write_broken_index(&self) -> bool {
        self.write_broken_index
    }

    /// `Database.setWriteBrokenIndex(Boolean)`; `None` restores the default
    /// policy (`false`).
    pub fn set_write_broken_index(&mut self, write_broken_index: Option<bool>) {
        self.write_broken_index = write_broken_index.unwrap_or(DEFAULT_WRITE_BROKEN_INDEX);
    }

    /// `Database.getEvalConfig()`: the expression evaluation configuration of
    /// this database.
    ///
    /// Java returns the `DBEvalContext` of the database; the port keeps an
    /// [`crate::expr::EvalConfig`] whose zone is the database zone
    /// (`DatabaseImpl.getZoneId`, which `DBEvalContext` reads from its owning
    /// database).  Like Java's object, the config is live: mutate it through
    /// [`Database::get_eval_config_mut`].
    pub fn get_eval_config(&self) -> &crate::expr::EvalConfig {
        &self.eval_config
    }

    /// Mutable access to [`Database::get_eval_config`] (Java's
    /// `getEvalConfig()` returns the live `EvalConfig` instance, whose setters
    /// mutate it in place).
    pub fn get_eval_config_mut(&mut self) -> &mut crate::expr::EvalConfig {
        &mut self.eval_config
    }

    /// `Database.getColumnValidatorFactory()`: the factory which creates the
    /// column validators of the user tables loaded from this database.
    ///
    /// Java returns the `SimpleColumnValidatorFactory` when none was installed;
    /// the port does the same.  See the `column_validator_factory` field for the
    /// current limitation (the table loader builds the simple validators
    /// itself).
    pub fn get_column_validator_factory(&self) -> crate::validator::ColumnValidatorFactoryHandle {
        self.column_validator_factory.clone().unwrap_or_default()
    }

    /// `Database.setColumnValidatorFactory(ColumnValidatorFactory)`: `None`
    /// restores the default factory.  The configured factory is applied to the
    /// columns of every loaded table immediately and to tables loaded later
    /// (Java's `ColumnImpl.getColumnValidator` asks the database factory the
    /// first time a validator is needed).
    pub fn set_column_validator_factory(
        &mut self,
        new_factory: Option<crate::validator::ColumnValidatorFactoryHandle>,
    ) {
        self.column_validator_factory = new_factory;
        let factory = self.get_column_validator_factory();
        for table in self.tables.values_mut() {
            for idx in 0..table.columns.len() {
                let handle = factory.create_validator(&table.columns[idx]);
                table.columns[idx].set_column_validator(handle);
            }
        }
    }

    /// `Database.getColumnOrder`: the order [`crate::table::Table::get_columns`]
    /// uses for the tables loaded after this call.
    ///
    /// Like Java, the order is applied while a table is constructed, so an
    /// already loaded table keeps the order it was opened with
    /// (`TableImpl`'s constructor sorts its column list once).
    pub fn get_column_order(&self) -> ColumnOrder {
        self.column_order
    }

    /// `Database.setColumnOrder`; `None` restores the default
    /// ([`ColumnOrder::Data`]).  Only tables loaded afterwards are affected
    /// (see [`Database::get_column_order`]).
    pub fn set_column_order(&mut self, order: ColumnOrder) {
        self.column_order = order;
    }

    /// `Database.setColumnOrder(null)`.
    pub fn reset_column_order(&mut self) {
        self.column_order = DEFAULT_COLUMN_ORDER;
    }

    /// `Database.getDateTimeType`.
    pub fn get_date_time_type(&self) -> DateTimeType {
        self.date_time_type
    }

    /// `Database.setDateTimeType`: switches the conversion of every
    /// `SHORT_DATE_TIME` column of this database (see [`DateTimeType`]).  Like
    /// Java's `DatabaseImpl.setDateTimeType` (where each column asks the
    /// database for its `DateTimeFactory`), this also applies to the tables
    /// and index datas which are already loaded.
    pub fn set_date_time_type(&mut self, date_time_type: DateTimeType) {
        self.date_time_type = date_time_type;
        for table in self.tables.values_mut() {
            table.set_date_time_type(date_time_type);
            // the index entry encoder converts a caller supplied date with the
            // database's DateTimeType, like `ColumnImpl.write` does
            for data in &mut table.index_datas {
                data.set_date_time_type(date_time_type);
            }
        }
    }

    /// The database `TimeZone` (`Database.getTimeZone`).
    pub fn get_time_zone(&self) -> &crate::datenums::TimeZone {
        &self.tz
    }

    /// `Database.setTimeZone`: replaces the zone used by every date
    /// conversion, by the index entry encoder and by the system catalog
    /// timestamps (`DatabaseImpl.setZoneInfo` sets the `ZoneId` and the
    /// `TimeZone` together, which [`crate::datenums::TimeZone`] keeps as one
    /// value).
    pub fn set_time_zone(&mut self, time_zone: crate::datenums::TimeZone) {
        self.tz = time_zone;
        // `DBEvalContext.getZoneId` answers from the owning database, so the
        // port's standalone EvalConfig follows the zone
        self.eval_config.set_zone(Some(time_zone));
        // the property driven expressions convert a `DATE` value with the zone
        // too (`BaseEvalContext.toValue`), so the loaded tables follow
        for table in self.tables.values_mut() {
            table.set_time_zone(time_zone);
        }
        // the index entry encoder uses the database zone for numeric date
        // values (`DefaultDateTimeFactory.toDateDouble`), so the loaded index
        // datas have to see the new zone as well
        for table in self.tables.values_mut() {
            for data in &mut table.index_datas {
                data.set_time_zone(time_zone);
            }
        }
    }

    /// The current offset east of UTC in minutes (`TimeZone.getRawOffset()`
    /// expressed in minutes).
    pub fn time_zone_offset_minutes(&self) -> i32 {
        self.tz.offset_minutes()
    }

    /// `Database.getZoneId()`: the id of the zone used by the date conversions,
    /// i.e. the `java.time.ZoneId` of
    /// [`Database::get_time_zone`]`().toZoneId()`.
    ///
    /// A database switched with [`Database::set_zone_id`] reports the IANA id
    /// (`"Asia/Shanghai"`, `"America/New_York"`), and one switched with the
    /// fixed offset APIs reports the `"UTC"` / `"GMT+05:30"` form Java produces
    /// for those zones.
    pub fn get_zone_id(&self) -> String {
        self.tz.zone_id()
    }

    /// `Database.getTimeZone()` plus `Database.getZoneId()` as one value (see
    /// [`ZoneInfo`]).
    pub fn get_zone_info(&self) -> ZoneInfo {
        ZoneInfo {
            id: self.tz.zone_id(),
            offset_minutes: self.tz.offset_minutes(),
            raw_offset_ms: self.tz.get_raw_offset(),
            is_named: self.tz.is_named(),
        }
    }

    /// `Database.setZoneId(ZoneId)`: switches the database to a named IANA zone
    /// (`"America/New_York"`) or to one of the fixed offset forms Java's
    /// `ZoneId.of` accepts (`"UTC"`, `"GMT+05:30"`, `"+05:30"`).
    ///
    /// The zone's historical and daylight saving rules are then used by every
    /// date conversion: the `SHORT_DATE_TIME` row values in both
    /// `DateTimeType` modes, the `DATE`-mode millisecond conversions, the
    /// catalog `DateCreate`/`DateUpdate` timestamps and the default/calculated
    /// expressions, date valued properties and index keys.  Like Java's
    /// `setZoneInfo`, this also replaces the database `TimeZone` with the one
    /// Java derives from the id (`TimeZone.getTimeZone(zoneId)`).
    pub fn set_zone_id(&mut self, zone_id: &str) -> Result<()> {
        let tz = crate::datenums::TimeZone::from_zone_id(zone_id)
            .ok_or_else(|| Error::IllegalArgument(format!("Unknown time-zone ID: {}", zone_id)))?;
        self.set_time_zone(tz);
        Ok(())
    }

    /// `Database.setZoneId(ZoneId)` under the name the time-zone harnesses use:
    /// identical to [`Database::set_zone_id`].
    ///
    /// The accepted ids are the ones `java.time.ZoneId.of` understands: every
    /// IANA id the bundled tz database knows (`America/New_York`,
    /// `Europe/Berlin`, `Asia/Shanghai`, `Australia/Lord_Howe`, `Pacific/Apia`,
    /// `UTC`, ...) plus the fixed offset forms (`GMT-05:00`, `GMT+05:30`,
    /// `+05:30`, `Z`).  The zone supplies the historical offsets and the
    /// daylight saving rules for every date conversion, the catalog
    /// `DateCreate`/`DateUpdate` timestamps included.
    pub fn set_time_zone_id(&mut self, zone_id: &str) -> Result<()> {
        self.set_zone_id(zone_id)
    }

    /// `Database.getZoneId().getId()`: the id of the current zone
    /// ([`Database::get_zone_id`] under its explicit name).  A named zone
    /// reports its IANA id, a fixed offset zone the `UTC` / `GMT±HH:MM` form.
    pub fn get_time_zone_id(&self) -> String {
        self.get_zone_id()
    }

    /// Convenience equivalent of Java's
    /// `db.setTimeZone(TimeZone.getTimeZone("GMT" + off))` for a fixed offset
    /// zone such as `"UTC"` (0) or `"GMT-05:00"` (-300).  Those zones observe
    /// no daylight saving time, so the offset is constant.
    pub fn set_time_zone_offset_minutes(&mut self, minutes: i32) {
        self.set_time_zone(crate::datenums::TimeZone::from_offset_minutes(minutes));
    }

    /// `DatabaseImpl.getDefaultSortOrder`: the database default language sort
    /// order for textual columns, read from page 0 when the database was
    /// opened.
    pub fn get_default_sort_order(&self) -> SortOrder {
        self.default_sort_order
    }

    /// `DatabaseImpl.getDefaultCodePage`: the default code page for textual
    /// data read from page 0 (only relevant for Jet 3).
    pub fn get_default_code_page(&self) -> i16 {
        self.default_code_page.unwrap_or(0)
    }

    /// The local wall clock time of the given epoch milliseconds in this
    /// database's zone (`LocalDateTime.ofInstant(Instant.ofEpochMilli(millis),
    /// getZoneId())`).  This is the conversion a `java.util.Date` value (or a
    /// numeric date value) is written with.
    pub fn local_date_time_from_millis(&self, millis: i64) -> crate::datenums::LocalDateTime {
        self.tz.local_date_time_from_millis(millis)
    }

    /// `Database.reclaimFreeSpace`: truncates the run of free pages at the end
    /// of the file, returning the number of pages given back.
    ///
    /// A read-only database cannot be shortened, so nothing is given back
    /// (Java returns 0 before touching the channel, which avoids the
    /// `NonWritableChannelException` a truncate would throw).
    pub fn reclaim_free_space(&mut self) -> Result<i32> {
        if self.read_only {
            return Ok(0);
        }
        self.channel.start_exclusive_write()?;
        let result = self.channel.truncate_free_space();
        self.channel.finish_write()?;
        result
    }

    /// Loads the table stored at the given table definition page, using the
    /// name stored in the system catalog (so the table keeps its on-disk
    /// spelling, which the foreign key constraint messages use).
    pub(crate) fn load_table_by_page(&mut self, page: i32) -> Result<()> {
        if self.tables.contains_key(&page) {
            return Ok(());
        }
        let name = self.catalog_table_name(page)?.unwrap_or_default();
        let flags = self
            .table_lookup
            .get(&name.to_uppercase())
            .map(|(_, f)| *f)
            .unwrap_or(0);
        let mut table = Table::read(&mut self.channel, page, &name, flags, &self.format)?;
        table.set_database_error_handler(self.error_handler.clone());
        self.apply_table_settings(&mut table);
        self.tables.insert(page, table);
        self.apply_catalog_table_info(page)?;
        self.init_table_column_properties(page)?;
        Ok(())
    }

    /// The catalog name of the table whose definition lives on the given page.
    fn catalog_table_name(&mut self, page: i32) -> Result<Option<String>> {
        let rows = self.read_table_rows(PAGE_SYSTEM_CATALOG)?;
        let table = self.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
        let idx_id = table.column_index(CAT_COL_ID);
        let idx_name = table.column_index(CAT_COL_NAME);
        Ok(rows.into_iter().flatten().find_map(|row| {
            let id = idx_id.and_then(|i| row.get(i)).and_then(|v| v.as_i64());
            if id == Some(page as i64) {
                idx_name
                    .and_then(|i| row.get(i))
                    .and_then(|v| v.as_text())
                    .map(|s| s.to_string())
            } else {
                None
            }
        }))
    }

    // ------------------------------------------------------------------
    // linked tables
    // ------------------------------------------------------------------

    /// `Database.createLinkedTable`: adds a linked table catalog object (and
    /// its access control entries) without a table definition page.  The
    /// linked table's synthetic object id is its "table definition page".
    pub fn create_linked_table(
        &mut self,
        name: &str,
        linked_db_name: &str,
        linked_table_name: &str,
    ) -> Result<()> {
        if self.table_lookup_entry(name).is_some() {
            return Err(Error::IllegalArgument(format!(
                "Cannot create linked table with name of existing table '{}' (Db={})",
                name, self.name
            )));
        }

        validate_identifier_name(name, self.format.max_table_name_length, "table")?;
        validate_name(
            linked_db_name,
            DataType::Memo.get_max_size() as usize,
            "linked database",
        )?;
        validate_identifier_name(
            linked_table_name,
            self.format.max_table_name_length,
            "linked table",
        )?;

        self.channel.start_write();
        let result = (|| -> Result<i32> {
            let linked_table_id = self.next_free_synthetic_id()?;
            crate::tablecreator::add_new_table(
                self,
                name,
                linked_table_id,
                TYPE_LINKED_TABLE,
                0,
                Some(linked_db_name),
                Some(linked_table_name),
            )?;
            Ok(linked_table_id)
        })();
        let finish = self.channel.finish_write();
        match result {
            Ok(linked_table_id) => {
                finish?;
                // keep the in-memory linked metadata in sync with the new
                // catalog row (`DatabaseImpl.addTable` + `createTableInfo`)
                self.linked_tables.insert(
                    name.to_uppercase(),
                    LinkedTableInfo {
                        name: name.to_string(),
                        object_id: linked_table_id,
                        flags: 0,
                        linked_db_name: linked_db_name.to_string(),
                        linked_table_name: linked_table_name.to_string(),
                    },
                );
                Ok(())
            }
            Err(e) => {
                let _ = finish;
                Err(e)
            }
        }
    }

    // ------------------------------------------------------------------
    // linked database resolution (`DatabaseImpl` linked table support)
    // ------------------------------------------------------------------

    /// `Database.setLinkResolver`: sets the policy used to open linkee files.
    ///
    /// `None` restores the default via [`Database::reset_link_resolver`]; this
    /// method takes any [`LinkResolver`], including a closure
    /// `Fn(&Database, &str) -> Result<Database>`.
    pub fn set_link_resolver<R>(&mut self, resolver: R)
    where
        R: LinkResolver + 'static,
    {
        self.link_resolver = Box::new(resolver);
    }

    /// `Database.getLinkResolver`: the configured policy (defaults to
    /// [`crate::linked::DEFAULT`], which denies every link).
    pub fn link_resolver(&self) -> &dyn LinkResolver {
        self.link_resolver.as_ref()
    }

    /// `Database.setLinkResolver(null)` / `getDefaultLinkResolver`: restores
    /// the default denying policy.
    pub fn reset_link_resolver(&mut self) {
        self.link_resolver = Box::new(crate::linked::DEFAULT);
    }

    /// `DatabaseImpl.isReadOnly`: whether this database was opened read-only.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// `Database.getFileFormat()`: the Access version of this database.
    ///
    /// The format is derived from the page format (`JetFormat`) and, for a
    /// generic Jet 4 file, from the `AccessVersion` database property, exactly
    /// like `DatabaseImpl.getFileFormat`:
    ///
    /// * `VERSION_3` -> `V1997`, `MSISAM` -> `MSISAM`, and the ACE page formats
    ///   map to their own constant;
    /// * a Jet 4 file is `V2000` for `AccessVersion = "08.50"`, `V2003` for
    ///   `"09.50"`, and `GENERIC_JET4` when the property is missing or blank;
    /// * any other `AccessVersion` is an `IllegalStateException`
    ///   (`"Could not determine FileFormat (Db=<name>)"`), like Java.
    ///
    /// The result is cached, and a database created by this port reports the
    /// requested format directly.
    pub fn get_file_format(&mut self) -> Result<FileFormat> {
        if let Some(file_format) = self.file_format {
            return Ok(file_format);
        }
        let file_format = match self.format.name {
            "VERSION_3" => FileFormat::V1997,
            "MSISAM" => FileFormat::Msisam,
            "VERSION_2007" => FileFormat::V2007,
            "VERSION_2010" => FileFormat::V2010,
            "VERSION_2016" => FileFormat::V2016,
            "VERSION_2019" => FileFormat::V2019,
            _ => {
                // `PossibleFileFormats.POSSIBLE_VERSION_4` is keyed by the
                // `AccessVersion` property, with a null key for the generic
                // file (a missing or blank value)
                let access_version = {
                    let maps = self.get_database_properties()?;
                    maps.default_map()
                        .and_then(|map| map.get_trimmed_string("AccessVersion"))
                        .map(|version| version.to_string())
                };
                match access_version.as_deref() {
                    Some("08.50") => FileFormat::V2000,
                    Some("09.50") => FileFormat::V2003,
                    None => FileFormat::GenericJet4,
                    Some(_) => {
                        return Err(Error::IllegalState(format!(
                            "Could not determine FileFormat (Db={})",
                            self.name
                        )))
                    }
                }
            }
        };
        self.file_format = Some(file_format);
        Ok(file_format)
    }

    /// `Database.getLinkedDatabases`: the linkee databases opened so far,
    /// keyed by the linkee file name stored in the linker catalog.
    ///
    /// Java returns an unmodifiable view of its map; this port returns the
    /// map of owned linkees.  Use [`Database::get_linked_database_mut`] to read
    /// rows from a linkee (the linker methods which need `&mut self` cannot be
    /// used through this shared view).
    pub fn get_linked_databases(&self) -> &HashMap<String, Box<Database>> {
        &self.linked_databases
    }

    /// The opened linkee database of the given catalog file name, if it has
    /// been resolved already (`getLinkedDatabases().get(name)`).
    pub fn get_linked_database(&self, linkee_name: &str) -> Option<&Database> {
        self.linked_databases.get(linkee_name).map(|db| db.as_ref())
    }

    /// The opened linkee database of the given catalog file name, if it has
    /// been resolved already, mutably (so its tables can be read).
    pub fn get_linked_database_mut(&mut self, linkee_name: &str) -> Option<&mut Database> {
        self.linked_databases
            .get_mut(linkee_name)
            .map(|db| db.as_mut())
    }

    /// The linked table metadata of a local table name (`lookupTable` +
    /// `TableMetaData` of a type 6 catalog row), if it is a linked table.
    pub fn linked_table_info(&self, table_name: &str) -> Option<&LinkedTableInfo> {
        self.linked_tables.get(&table_name.to_uppercase())
    }

    /// The local names of all linked tables, in catalog spelling.
    pub fn linked_table_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .linked_tables
            .values()
            .map(|info| info.name.clone())
            .collect();
        names.sort();
        names
    }

    /// `Database.isLinkedTable` (the common case): whether the named catalog
    /// object is a local link to another database (a type 6 row).
    ///
    /// Java's overload takes a `Table` and also checks that the table belongs
    /// to another `DatabaseImpl`; the port's [`Table`] carries no database
    /// handle, so the catalog name is used.  A table obtained from a linkee
    /// through [`Database::get_table`] has the linkee's name and is not
    /// reported as linked by the linker under that name; query the linker with
    /// the local link name instead.
    pub fn is_linked_table(&self, table_name: &str) -> bool {
        self.linked_tables.contains_key(&table_name.to_uppercase())
    }

    /// If `table_name` names a linked table, resolves it to the (cached or
    /// newly opened) linkee database and the table name inside the linkee,
    /// like `DatabaseImpl.getTable(TableInfo, ...)` does for a type 6 catalog
    /// row.  Returns `None` for a local table.
    ///
    /// Every table-name based operation (row reads and writes, schema changes,
    /// complex column access) goes through this helper, so a linked table is
    /// operated on exactly like the linkee's own `Table` instance: the work
    /// uses the linkee's table definition page, channel, catalog, validators,
    /// defaults, indexes and foreign keys, and the linker file itself is never
    /// written.
    fn linked_table_target(&mut self, table_name: &str) -> Result<Option<(&mut Database, String)>> {
        let info = match self.linked_table_info(table_name) {
            Some(info) => info.clone(),
            None => return Ok(None),
        };
        let linked_db = self.get_or_open_linked_database(&info.linked_db_name)?;
        Ok(Some((linked_db, info.linked_table_name)))
    }

    /// Returns the opened linkee database, opening (and caching) it through the
    /// configured [`LinkResolver`] the first time it is needed
    /// (`DatabaseImpl.getTable(TableInfo, ...)`).
    fn get_or_open_linked_database(&mut self, linkee_name: &str) -> Result<&mut Database> {
        if !self.linked_databases.contains_key(linkee_name) {
            // the resolver gets a shared view of the linker (Java passes the
            // linker itself); the new database is stored only after resolution
            // succeeds, so a denied/failed link leaves no cache entry
            let linkee = self
                .link_resolver
                .resolve_linked_database(self, linkee_name)?;
            self.linked_databases
                .insert(linkee_name.to_string(), Box::new(linkee));
        }
        Ok(self.linked_databases.get_mut(linkee_name).unwrap().as_mut())
    }

    /// The local date/time used for catalog timestamps (Java's `new Date()`
    /// converted through the database time zone,
    /// `LocalDateTime.ofInstant(instant, getZoneId())`).
    pub fn now_local_datetime(&mut self) -> crate::datenums::LocalDateTime {
        if !self.now_ldt_queue.is_empty() {
            return self.now_ldt_queue.remove(0);
        }
        if !self.now_queue.is_empty() {
            let ms = self.now_queue.remove(0);
            self.last_now_ms = Some(ms);
            return self.tz.local_date_time_from_millis(ms);
        }
        let millis = match self.now_ms {
            Some(ms) => ms,
            None => self.last_now_ms.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            }),
        };
        self.tz.local_date_time_from_millis(millis)
    }

    /// Creates a new table (`TableBuilder.toTable`).
    pub fn create_table(&mut self, spec: &crate::tablecreator::TableSpec) -> Result<i32> {
        // `TableCreator.validate` -> `TableMutator.validateAutoNumberColumn`:
        // at most one auto number column of each type per table (COMPLEX_TYPE
        // is exempt: all complex columns of a row share one value)
        let auto_columns: Vec<&ColumnSpec> =
            spec.columns.iter().filter(|c| c.auto_number).collect();
        if auto_columns.len() > 1 {
            let mut auto_types: Vec<crate::datatype::DataType> = Vec::new();
            for column in auto_columns {
                if column.data_type == crate::datatype::DataType::ComplexType {
                    continue;
                }
                if auto_types.contains(&column.data_type) {
                    return Err(Error::IllegalArgument(format!(
                        "Can have at most one AutoNumber column of type {} per table(Table={})",
                        column.data_type.get_name(),
                        spec.name
                    )));
                }
                auto_types.push(column.data_type);
            }
        }
        let page = crate::tablecreator::create_table(self, spec)?;
        // `TableImpl`'s constructor (which `TableCreator` runs to apply the
        // table and column properties) allocates the property map of every
        // column, in column order, before the table's own map, so the property
        // blob this process writes keeps that order.  The port rebuilds the
        // group from the blob on every access, so the anchor is recorded here.
        // Loading the table now is the same `getTable` the Java creator runs.
        let _ = self.table_page(&spec.name);
        if let Some(table) = self.tables.get(&page) {
            // `TableImpl`'s constructor runs `ColumnImpl.postTableLoadInit` for
            // every column *before* `initColumnValidator`: a calculated column's
            // post-load work reads its `Expression`/`ResultType` properties, so
            // the calculated columns' maps are created first (in column order),
            // followed by the remaining columns' maps
            let mut names: Vec<String> = table
                .columns
                .iter()
                .filter(|c| c.calculated)
                .map(|c| c.name.clone())
                .collect();
            names.extend(
                table
                    .columns
                    .iter()
                    .filter(|c| !c.calculated)
                    .map(|c| c.name.clone()),
            );
            self.created_table_column_maps.insert(page, names);
        }
        Ok(page)
    }

    /// Adds a column to an existing table (`ColumnBuilder.addToTable`).
    ///
    /// jackcess 5.0.4 has no drop-column API, so this is the only column
    /// change supported.  A complex column gets its supporting tables first
    /// (`TableUpdater.addColumn` calls `ComplexColumnCreator.create` before it
    /// mutates the table definition), and the table's catalog flags are marked
    /// as holding a complex column.
    pub fn add_column(
        &mut self,
        table_name: &str,
        spec: ColumnSpec,
    ) -> Result<crate::column::Column> {
        // `ColumnBuilder.addToTable` on a linked `Table` adds the column to the
        // linkee's table definition
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.add_column(&linked_name, spec);
        }
        let page = self.table_page(table_name)?;
        let format = self.format.clone();
        let parent_name = self
            .tables
            .get(&page)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| table_name.to_string());
        // `TableUpdater.addColumn` applies the new column's properties before
        // it adds the version history column (and before it gives the existing
        // rows their complex ids), so they are saved here at that point
        let column_props = spec.properties.clone();
        let append_only = spec.append_only;
        let memo_column_name = spec.name.clone();
        // a table holds one `PropertyMaps` instance for its whole life, and the
        // map blocks are written in the order the maps were created: the
        // columns the table already had, then the table's own map, then the
        // map of the column being added (`TableImpl` creates them when it is
        // loaded, `ColumnImpl.getProperties` adds the new one).  The group is
        // read before the column is added so the order matches.
        let mut maps = if !column_props.is_empty() || append_only {
            let mut maps = self.get_table_properties(page)?;
            let _ = maps.get_default();
            Some(maps)
        } else {
            None
        };
        let mut complex_id = None;
        if spec.complex_desc.is_some() {
            // `TableUpdater.addColumn` creates the supporting tables inside its
            // write operation, before it mutates the table definition
            self.channel.start_write();
            let created = crate::tablecreator::create_complex_column(
                self,
                &spec,
                &parent_name,
                page,
                &format,
            );
            let finish = self.channel.finish_write();
            let id = match created {
                Ok(id) => id,
                Err(e) => {
                    let _ = finish;
                    return Err(e);
                }
            };
            finish?;
            complex_id = Some(id);
        }
        let sort_order = self.default_sort_order;
        let column = {
            let t = self.tables.get_mut(&page).unwrap();
            // `TableUpdater.addColumn` does not wrap the failures of the
            // mutation: the `IllegalArgumentException`s of the table definition
            // buffer (`Buffer.position`) and the I/O failures propagate as they
            // are, so the caller sees the same exception class/message as Java
            crate::tableupdater::add_column_with_complex(
                t,
                &mut self.channel,
                spec,
                sort_order,
                complex_id,
            )?
        };
        if !column_props.is_empty() {
            // `TableImpl.mutateAddColumn`: `newCol.getProperties().putAll(...)`
            // then `getProperties().save()`
            if let Some(maps) = maps.as_mut() {
                crate::property::put_builder_properties(maps.get_mut(&column.name), &column_props);
                maps.save(self)?;
            }
        }
        if complex_id.is_some() {
            crate::tablecreator::add_to_system_catalog_flags(
                self,
                page,
                crate::complex::CATALOG_FLAGS_HAS_COMPLEX,
            )?;
            self.fill_complex_column_values(page, &column.name)?;
        }
        if append_only {
            // the versions of a memo column live in a complex column beside it
            // (`TableUpdater.addColumn` adds it after the memo column)
            self.add_version_history_column(
                page,
                &parent_name,
                &memo_column_name,
                &format,
                maps.as_mut(),
            )?;
        }
        Ok(column)
    }

    /// The `TableUpdater.addColumn` branch which adds the complex column that
    /// holds the versions of a new append-only memo column, then stores the
    /// table's `AppendOnly` property.
    fn add_version_history_column(
        &mut self,
        page: i32,
        parent_name: &str,
        memo_column_name: &str,
        format: &crate::format::JetFormat,
        mut maps: Option<&mut crate::property::PropertyMaps>,
    ) -> Result<()> {
        let vh_spec = crate::complex::version_history_column_spec(memo_column_name);
        // `ComplexColumnCreator.create` runs before the definition is mutated
        self.channel.start_write();
        let created =
            crate::tablecreator::create_complex_column(self, &vh_spec, parent_name, page, format);
        let finish = self.channel.finish_write();
        let complex_id = match created {
            Ok(id) => id,
            Err(e) => {
                let _ = finish;
                return Err(e);
            }
        };
        finish?;
        let sort_order = self.default_sort_order;
        {
            let t = self.tables.get_mut(&page).unwrap();
            crate::tableupdater::add_column_with_complex(
                t,
                &mut self.channel,
                vh_spec,
                sort_order,
                Some(complex_id),
            )?;
        }
        crate::tablecreator::add_to_system_catalog_flags(
            self,
            page,
            crate::complex::CATALOG_FLAGS_HAS_COMPLEX,
        )?;
        self.fill_complex_column_values(page, crate::complex::VERSION_HISTORY_COL_NAME)?;
        // `PropertyMap tableProps = _table.getProperties();
        //  tableProps.put(APPEND_ONLY_PROP, BOOLEAN, TRUE); tableProps.save()`
        if let Some(maps) = maps.as_mut() {
            maps.get_default().put(crate::property::Property::new(
                crate::property::APPEND_ONLY_PROP,
                DataType::Boolean,
                Value::Bool(true),
            ));
            maps.save(self)?;
        }
        Ok(())
    }

    /// `TableImpl.fillComplexColumnValues`: gives every row which was written
    /// before the complex column was added an id, as access does.  A row
    /// shares the id of its other complex columns, or gets the next complex
    /// auto number.
    fn fill_complex_column_values(&mut self, page: i32, column_name: &str) -> Result<()> {
        let rows = self.table_rows_with_ids(page)?;
        let (col_index, other_complex) = {
            let table = self.tables.get(&page).unwrap();
            let col_index = table.column_index(column_name);
            let others: Vec<usize> = table
                .columns
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.data_type == DataType::ComplexType
                        && !c.name.eq_ignore_ascii_case(column_name)
                })
                .map(|(i, _)| i)
                .collect();
            (col_index, others)
        };
        let col_index = match col_index {
            Some(i) => i,
            None => return Ok(()),
        };
        let old_override = self
            .tables
            .get(&page)
            .and_then(|t| t.allow_auto_number_insert);
        if let Some(t) = self.tables.get_mut(&page) {
            t.set_allow_auto_number_insert(Some(true));
        }
        let result = (|| -> Result<()> {
            for (row_id, values) in rows {
                let mut values = match values {
                    Some(v) => v,
                    None => continue,
                };
                if values.len() <= col_index {
                    values.resize(col_index + 1, Value::Null);
                }
                let shared = other_complex
                    .iter()
                    .filter_map(|i| values.get(*i))
                    .filter_map(|v| v.as_i64())
                    .find(|v| *v > 0);
                let id = match shared {
                    Some(id) => id as i32,
                    None => {
                        let t = self.tables.get_mut(&page).unwrap();
                        t.last_complex_auto_number += 1;
                        t.last_complex_auto_number
                    }
                };
                values[col_index] = Value::Long(id);
                let tz = self.tz;
                let t = self.tables.get_mut(&page).unwrap();
                t.update_row(&mut self.channel, row_id, &values, &tz)?;
            }
            Ok(())
        })();
        if let Some(t) = self.tables.get_mut(&page) {
            t.set_allow_auto_number_insert(old_override);
        }
        result
    }

    fn table_rows_with_ids(
        &mut self,
        page: i32,
    ) -> Result<Vec<(crate::table::RowId, Option<Vec<Value>>)>> {
        self.load_table_by_page(page)?;
        let t = self.tables.get_mut(&page).unwrap();
        t.read_all_rows_with_ids(&mut self.channel)
    }

    /// Adds an index to an existing table (`IndexBuilder.addToTable`).
    ///
    /// jackcess 5.0.4 has no drop-index API, so this is the only index change
    /// supported.
    pub fn add_index(
        &mut self,
        table_name: &str,
        spec: crate::tablecreator::IndexSpec,
    ) -> Result<crate::index::IndexImpl> {
        // `IndexBuilder.addToTable` on a linked `Table` indexes the linkee's
        // table
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.add_index(&linked_name, spec);
        }
        let page = self.table_page(table_name)?;
        let t = self.tables.get_mut(&page).unwrap();
        // `IndexBuilder.addToTable` -> `TableUpdater.addIndex` propagates the
        // failure unchanged (the `IllegalArgumentException` of the definition
        // buffer's `position` included), like `add_column`
        crate::tableupdater::add_index(t, &mut self.channel, spec)
    }

    /// Creates a relationship between two existing tables
    /// (`RelationshipBuilder.toRelationship` / `DatabaseImpl.writeRelationship`).
    ///
    /// When `spec.referential_integrity` is set, the supporting unique index on
    /// the primary side and the foreign key index on the secondary side are
    /// created as well, exactly like jackcess does.
    pub fn add_relationship(
        &mut self,
        spec: crate::relationship::RelationshipSpec,
    ) -> Result<crate::relationship::Relationship> {
        crate::relationship::create_relationship(self, spec)
    }

    /// The relationships of the given table (`Database.getRelationships(Table)`).
    pub fn relationships(
        &mut self,
        table_name: &str,
    ) -> Result<Vec<crate::relationship::Relationship>> {
        crate::relationship::read_relationships(self, Some(table_name), None, true)
    }

    /// The relationships between the two given tables
    /// (`Database.getRelationships(Table, Table)`).
    pub fn relationships_between(
        &mut self,
        table1: &str,
        table2: &str,
    ) -> Result<Vec<crate::relationship::Relationship>> {
        // jackcess "orders" the two tables so that both argument orders return
        // the same collection (`String.compareTo`, so case sensitively)
        let (table1, table2) = if table1 > table2 {
            (table2, table1)
        } else {
            (table1, table2)
        };
        crate::relationship::read_relationships(self, Some(table1), Some(table2), true)
    }

    /// All relationships of the database (`Database.getRelationships()`).
    pub fn relationships_all(&mut self) -> Result<Vec<crate::relationship::Relationship>> {
        crate::relationship::read_relationships(self, None, None, false)
    }

    /// `Database.getRelationships()`: Java-named alias of
    /// [`Database::relationships_all`].
    pub fn get_relationships(&mut self) -> Result<Vec<crate::relationship::Relationship>> {
        self.relationships_all()
    }

    /// All relationships including the ones between system tables
    /// (`Database.getSystemRelationships()`).
    pub fn system_relationships(&mut self) -> Result<Vec<crate::relationship::Relationship>> {
        crate::relationship::read_relationships(self, None, None, true)
    }

    /// `Database.getSystemRelationships()`: Java-named alias of
    /// [`Database::system_relationships`].
    pub fn get_system_relationships(&mut self) -> Result<Vec<crate::relationship::Relationship>> {
        self.system_relationships()
    }

    /// Finds the table definition page and flags of a named table without
    /// loading it.
    pub(crate) fn table_lookup_entry(&self, table_name: &str) -> Option<(i32, i32)> {
        self.table_lookup.get(&table_name.to_uppercase()).copied()
    }

    /// `TableFinder.getNextFreeSyntheticId`: synthetic object ids count up
    /// from the smallest integer, so the next one is one past the highest
    /// in-use negative id.
    pub(crate) fn next_free_synthetic_id(&mut self) -> Result<i32> {
        let rows = self.read_table_rows(PAGE_SYSTEM_CATALOG)?;
        let table = self.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
        let idx_id = table.column_index(CAT_COL_ID);
        let mut max_synth_id = i32::MIN;
        for row in rows.into_iter().flatten() {
            if let Some(id) = idx_id.and_then(|i| row.get(i)).and_then(|v| v.as_i64()) {
                let id = id as i32;
                if id > max_synth_id && id < 0 {
                    max_synth_id = id;
                }
            }
        }
        if max_synth_id >= -1 {
            return Err(Error::IllegalState("Too many database objects".into()));
        }
        Ok(max_synth_id + 1)
    }

    /// The names of the catalog objects of the given type, in catalog row
    /// order (`DatabaseImpl.createRelationshipName` collects them this way).
    pub(crate) fn catalog_object_names(&mut self, obj_type: i16) -> Result<Vec<String>> {
        let rows = self.read_table_rows(PAGE_SYSTEM_CATALOG)?;
        let table = self.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
        let idx_name = table.column_index(CAT_COL_NAME);
        let idx_type = table.column_index(CAT_COL_TYPE);
        let mut names = Vec::new();
        for row in rows.into_iter().flatten() {
            let typ = idx_type.and_then(|i| row.get(i)).and_then(|v| v.as_i64());
            if typ != Some(obj_type as i64) {
                continue;
            }
            if let Some(name) = idx_name.and_then(|i| row.get(i)).and_then(|v| v.as_text()) {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    /// Reads the group of property maps of one catalog row matching either an
    /// object id or a (parent id, name) pair (`DatabaseImpl.readProperties`).
    /// The returned group is bound to the row, so it can be saved again.
    fn read_catalog_properties(
        &mut self,
        want_id: Option<i32>,
        parent_and_name: Option<(i32, &str)>,
    ) -> Result<crate::property::PropertyMaps> {
        let rows = self.read_rows_with_ids(TABLE_SYSTEM_CATALOG)?;
        let (idx_id, idx_name, idx_parent, idx_props) = {
            let table = self.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
            (
                table.column_index(CAT_COL_ID),
                table.column_index(CAT_COL_NAME),
                table.column_index(CAT_COL_PARENT_ID),
                table.column_index(CAT_COL_PROPS),
            )
        };
        for (row_id, values) in rows {
            let values = match values {
                Some(values) => values,
                None => continue,
            };
            let id = idx_id.and_then(|i| values.get(i)).and_then(|v| v.as_i64());
            match want_id {
                Some(want_id) => {
                    if id != Some(want_id as i64) {
                        continue;
                    }
                }
                None => {
                    let (want_parent, want_name) = match parent_and_name {
                        Some(pair) => pair,
                        None => continue,
                    };
                    let name = idx_name
                        .and_then(|i| values.get(i))
                        .and_then(|v| v.as_text());
                    let parent = idx_parent
                        .and_then(|i| values.get(i))
                        .and_then(|v| v.as_i64());
                    if name != Some(want_name) || parent != Some(want_parent as i64) {
                        continue;
                    }
                }
            }
            let object_id = id.unwrap_or(-1) as i32;
            let blob = idx_props
                .and_then(|i| values.get(i))
                .and_then(|v| v.as_bytes());
            let mut maps = match blob {
                Some(bytes) => crate::property::PropertyMaps::read_with_context(
                    bytes,
                    &self.format,
                    self.charset,
                    crate::column::DateContext::new(self.date_time_type, self.tz),
                )?,
                None => crate::property::PropertyMaps::new(object_id),
            };
            maps.set_object_id(object_id);
            maps.set_row_id(Some(row_id));
            return Ok(maps);
        }
        Ok(crate::property::PropertyMaps::new(want_id.unwrap_or(-1)))
    }

    /// Reads the property maps stored in the catalog row of the given object
    /// id (`DatabaseImpl.getPropertiesForObject` / the `Table.getPropertyMaps`
    /// plumbing).
    pub fn get_object_properties(
        &mut self,
        object_id: i32,
    ) -> Result<crate::property::PropertyMaps> {
        self.read_catalog_properties(Some(object_id), None)
    }

    /// The property maps of a database level catalog object.
    fn get_db_object_properties(&mut self, name: &str) -> Result<crate::property::PropertyMaps> {
        let parent = self.db_parent_id;
        let mut maps = self.read_catalog_properties(None, Some((parent, name)))?;
        if maps.row_id().is_none() {
            // no row (yet); saving will create one
            maps.set_db_object_name(Some(name));
            maps.set_object_id(-1);
        }
        Ok(maps)
    }

    /// The database level properties (`Database.getDatabaseProperties`).
    pub fn get_database_properties(&mut self) -> Result<crate::property::PropertyMaps> {
        self.get_db_object_properties(OBJECT_NAME_DB_PROPS)
    }

    /// The summary properties (`Database.getSummaryProperties`).
    pub fn get_summary_properties(&mut self) -> Result<crate::property::PropertyMaps> {
        self.get_db_object_properties(OBJECT_NAME_SUMMARY_PROPS)
    }

    /// The user defined properties (`Database.getUserDefinedProperties`).
    pub fn get_user_defined_properties(&mut self) -> Result<crate::property::PropertyMaps> {
        self.get_db_object_properties(OBJECT_NAME_USERDEF_PROPS)
    }

    /// The properties of the table stored at the given definition page
    /// (`Table.getPropertyMaps`: the object level map is the table's own
    /// property map, `get(name)` is a column's map and `get_index(name)` an
    /// index's map).
    pub fn get_table_properties(&mut self, page: i32) -> Result<crate::property::PropertyMaps> {
        let _ = self.table_by_page(page)?;
        let mut maps = self.get_object_properties(page)?;
        self.preload_table_column_maps(page, &mut maps);
        maps.set_table_owner_page(Some(page));
        Ok(maps)
    }

    /// The property maps of the named table (`Table.getPropertyMaps`).
    pub fn get_table_properties_by_name(
        &mut self,
        table_name: &str,
    ) -> Result<crate::property::PropertyMaps> {
        let page = self.table_page(table_name)?;
        let mut maps = self.get_object_properties(page)?;
        self.preload_table_column_maps(page, &mut maps);
        maps.set_table_owner_page(Some(page));
        Ok(maps)
    }

    /// `ColumnImpl.initColumnValidator` for a freshly loaded table: remembers
    /// the database name (used in the validator messages) and, for user
    /// tables, applies the table's property maps to its columns so that the
    /// `DefaultValue`, `Required` and `AllowZeroLength` properties take effect
    /// on row add/update (jackcess only runs the internal validators for user
    /// tables).
    fn init_table_column_properties(&mut self, page: i32) -> Result<()> {
        let is_user = self
            .tables
            .get(&page)
            .map(|t| !t.is_system())
            .unwrap_or(false);
        let db_name = self.name.clone();
        if let Some(table) = self.tables.get_mut(&page) {
            table.db_name = db_name;
        }
        if !is_user {
            // `TableImpl.getNameMap` reads the `NameMap` property of *any*
            // table (`getProperties()` -> `getPropertyMaps()`), system tables
            // included, so the raw value is captured here as well; the column
            // defaults and validators are still only initialised for user
            // tables, exactly like Java's `if(!isSystem())` guard.
            let blob = self.name_map_blob_for(page)?;
            if let Some(table) = self.tables.get_mut(&page) {
                table.name_map_blob = blob;
            }
            return Ok(());
        }
        let maps = self.get_object_properties(page)?;
        let evaluate_expressions = self.evaluate_expressions;
        if let Some(table) = self.tables.get_mut(&page) {
            table.apply_property_maps(&maps, evaluate_expressions);
        }
        Ok(())
    }

    /// Loading a user table creates the property map of every one of its
    /// columns (`ColumnImpl.initColumnValidator` -> `ColumnImpl.getProperties`,
    /// which `TableImpl` only runs for user tables) before the table's own map
    /// is touched by the caller.  The maps are written in that creation order,
    /// so the maps which a property blob does not hold yet are created here, in
    /// column order, to reproduce the Java block order.
    fn preload_table_column_maps(&mut self, page: i32, maps: &mut crate::property::PropertyMaps) {
        let column_names: Vec<String> = match self.tables.get(&page) {
            Some(table) if !table.is_system() => {
                table.columns.iter().map(|col| col.name.clone()).collect()
            }
            _ => Vec::new(),
        };
        if let Some(anchor) = self.created_table_column_maps.get(&page) {
            // a table this process created: the block order is anchored at its
            // columns (the `TableImpl` constructor runs before the table's own
            // map is touched), so restore that order instead of appending
            maps.reorder_column_maps(anchor);
            return;
        }
        for name in column_names {
            let _ = maps.get_mut(&name);
        }
    }

    /// The property block order a table's maps get when the group is created
    /// from nothing (`TableImpl`'s constructor: `postTableLoadInit` of the
    /// calculated columns first, then `initColumnValidator` of every column, so
    /// the calculated columns' maps come first, each group in column order).
    fn table_property_map_anchor(&self, page: i32) -> Option<Vec<String>> {
        if let Some(anchor) = self.created_table_column_maps.get(&page) {
            return Some(anchor.clone());
        }
        let table = self.tables.get(&page)?;
        if table.is_system() {
            return None;
        }
        let mut names: Vec<String> = table
            .columns
            .iter()
            .filter(|c| c.calculated)
            .map(|c| c.name.clone())
            .collect();
        names.extend(
            table
                .columns
                .iter()
                .filter(|c| !c.calculated)
                .map(|c| c.name.clone()),
        );
        Some(names)
    }

    /// Saves a property group back to the database (`PropertyMaps.save`;
    /// `PropertyMaps.Handler.save` in the Java library).
    pub fn save_properties(&mut self, maps: &mut crate::property::PropertyMaps) -> Result<()> {
        self.write_property_maps(maps)
    }

    /// Writes the serialized property group into the `LvProp` value of the
    /// catalog row the group belongs to.
    ///
    /// An empty group is stored as a null value (the Java `write` returns
    /// `null` for a group without property names, which the row update stores
    /// as a null long value).  A database level group whose catalog row does
    /// not exist has its row created first (and the group is bound to it).
    pub(crate) fn write_property_maps(
        &mut self,
        maps: &mut crate::property::PropertyMaps,
    ) -> Result<()> {
        // the first write of a table's group (the properties applied while the
        // table is created) uses the `TableImpl` column creation order, which
        // is what Java's in-memory group holds at that point
        if maps.read_from_empty() {
            if let Some(page) = maps.table_owner_page() {
                if let Some(anchor) = self.table_property_map_anchor(page) {
                    maps.reorder_column_maps(&anchor);
                }
            }
        }
        let blob = maps.write_with_context(
            &self.format,
            self.charset,
            crate::column::DateContext::new(self.date_time_type, self.tz),
        )?;
        let value = if blob.is_empty() {
            Value::Null
        } else {
            Value::Bytes(blob)
        };
        let result = match maps.row_id() {
            Some(row_id) => self.update_catalog_row_value(row_id, CAT_COL_PROPS, value),
            None => {
                let db_object_name = maps.db_object_name().map(|name| name.to_string());
                match db_object_name {
                    Some(name) => {
                        let (object_id, row_id) = self.create_db_properties_row(&name, value)?;
                        maps.set_object_id(object_id);
                        maps.set_row_id(Some(row_id));
                        maps.set_db_object_name(None::<String>);
                        Ok(())
                    }
                    None => Err(Error::IllegalState(
                        "PropertyMaps cannot be saved without a row id".into(),
                    )),
                }
            }
        };
        result?;
        // `PropertyMaps.save` -> owner.propertiesUpdated -> `TableImpl
        // .propertiesUpdated`, which reloads every column's internal
        // validators and default value from the (new) maps
        if let Some(page) = maps.table_owner_page() {
            let evaluate_expressions = self.evaluate_expressions;
            if let Some(table) = self.tables.get_mut(&page) {
                table.apply_property_maps(maps, evaluate_expressions);
            }
        }
        Ok(())
    }

    /// Sets one column value of an existing `MSysObjects` row and writes the
    /// row back, the port of `TableImpl.updateValue` (the other columns keep
    /// the values the row holds).
    fn update_catalog_row_value(
        &mut self,
        row_id: crate::table::RowId,
        column: &str,
        value: Value,
    ) -> Result<()> {
        let rows = self.read_rows_with_ids(TABLE_SYSTEM_CATALOG)?;
        let mut values = rows
            .into_iter()
            .find(|(rid, _)| *rid == row_id)
            .and_then(|(_, values)| values)
            .ok_or_else(|| {
                Error::IllegalState(format!(
                    "PropertyMaps row {:?} is not in the system catalog",
                    row_id
                ))
            })?;
        let column_index = {
            let table = self.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
            table.column_index(column).ok_or_else(|| {
                Error::Jackcess(format!("The system catalog has no {} column", column))
            })?
        };
        if column_index >= values.len() {
            values.resize(column_index + 1, Value::Null);
        }
        values[column_index] = value;
        let tz = self.tz;
        let table = self.tables.get_mut(&PAGE_SYSTEM_CATALOG).unwrap();
        table.update_row(&mut self.channel, row_id, &values, &tz)
    }

    /// Creates the catalog row of a database level property group which does
    /// not exist yet (a `SummaryInfo`/`UserDefined` row in a database which
    /// Access never added one to), then stores the property blob in it and
    /// returns the new object id and row id.
    ///
    /// The Java library has no equivalent (`PropertyMaps.save` fails without a
    /// row id), so this is an extension of this port.  The row gets a free
    /// synthetic object id and the shape Access gives these rows.
    fn create_db_properties_row(
        &mut self,
        name: &str,
        value: Value,
    ) -> Result<(i32, crate::table::RowId)> {
        // adding the catalog row writes pages through several steps (the row
        // itself and the table definition), so the whole creation is one write
        self.channel.start_write();
        let result = (|| -> Result<(i32, crate::table::RowId)> {
            let object_id = self.next_free_synthetic_id()?;
            // Access stores the database properties object with type 2 and the
            // "system object" flag, the summary/user defined objects with type
            // 0x800B and no flag
            let (obj_type, flags) = match name {
                OBJECT_NAME_DB_PROPS => (2i16, SYSTEM_OBJECT_FLAG),
                _ => (-32757i16, 0), // 0x800B
            };
            let parent = self.db_parent_id;
            crate::tablecreator::add_to_system_catalog(
                self, name, object_id, obj_type, flags, None, None, parent,
            )?;
            let rows = self.read_rows_with_ids(TABLE_SYSTEM_CATALOG)?;
            let idx_id = self
                .tables
                .get(&PAGE_SYSTEM_CATALOG)
                .and_then(|t| t.column_index(CAT_COL_ID));
            let row_id = rows
                .into_iter()
                .find_map(|(row_id, values)| {
                    let values = values?;
                    let id = idx_id.and_then(|i| values.get(i)).and_then(|v| v.as_i64());
                    if id == Some(object_id as i64) {
                        Some(row_id)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    Error::IllegalState(format!(
                        "The {} property row was not added to the catalog",
                        name
                    ))
                })?;
            self.update_catalog_row_value(row_id, CAT_COL_PROPS, value)?;
            Ok((object_id, row_id))
        })();
        self.channel.finish_write()?;
        result
    }

    /// Adds rows to the named table, checking the foreign keys of every row
    /// before any of them is written (`TableImpl.addRows` calls
    /// `FKEnforcer.addRow` for each row), then flushes its table definition.
    ///
    /// The column defaults and the property driven column validators run
    /// before the foreign key checks, like `TableImpl.addRows` does, so a
    /// rejected row leaves the file (and the auto number counters) untouched.
    ///
    /// This is jackcess' *batch* insertion: like `TableImpl.addRows`, the index
    /// page changes are kept in memory and only written (and split) once, when
    /// the batch is flushed.  Adding the rows one call at a time instead
    /// reproduces `TableImpl.addRow`/`addRowFromMap`, which runs
    /// `IndexData.update` -> `IndexPageCache.write` after every row; the two
    /// histories are observably different when an index page crosses
    /// `maxPageEntrySize` (entry prefix compression leaves different bytes in
    /// the reused page buffer, and the split leaf is allocated at a different
    /// point in the file), so callers which must match a jackcess per-row loop
    /// byte for byte have to add the rows one at a time.
    ///
    /// A failed write is reported the way Java's batched `addRows` reports it:
    /// as `com.healthmarketscience.jackcess.BatchUpdateException`
    /// ([`Error::BatchUpdate`], message `Failed adding rows (Db=...;Table=...):
    /// <cause>`, update count 0 because a write failure leaves the status of
    /// every row unknown) with the original failure as the cause.  Java wraps
    /// the constraint/validator failures of a batch the same way; this port
    /// validates those before it writes anything (the batch is transactional)
    /// and reports the rejection unchanged, so the port's row count is always
    /// 0 for them and the existing `ConstraintViolation`/
    /// `InvalidValue` error contract is preserved.
    pub fn add_rows(&mut self, table_name: &str, rows: &[Vec<Value>]) -> Result<()> {
        // a linked table name is the linkee's table (`DatabaseImpl.getTable` on
        // a type 6 catalog row), so the rows are inserted with the linkee's
        // definition, indexes, validators and channel
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.add_rows(&linked_name, rows);
        }
        let page = self.table_page(table_name)?;
        let prepared = {
            let t = self.tables.get(&page).unwrap();
            let mut prepared = Vec::with_capacity(rows.len());
            for row in rows {
                let mut row = row.clone();
                row.resize(t.columns.len(), Value::Null);
                t.prepare_add_row(&mut row)?;
                prepared.push(row);
            }
            prepared
        };
        self.check_add_rows(page, &prepared)?;
        let tz = self.tz;
        let result = {
            let t = self.tables.get_mut(&page).unwrap();
            t.add_rows_tracked(&mut self.channel, &prepared, &tz)
        };
        match result {
            Ok(_) => Ok(()),
            // `TableImpl.addRows`: a write failure is wrapped in a
            // `BatchUpdateException` (`isWriteFailure` -> update count 0)
            Err((update_count, e)) if e.is_write_failure() => {
                let table = self.tables.get(&page).unwrap();
                Err(table.batch_update_error(update_count, e))
            }
            Err((_, e)) => Err(e),
        }
    }

    /// Runs the foreign key checks for a batch of rows about to be added.
    ///
    /// jackcess checks each row right before writing it (after the auto
    /// numbers are assigned); the port checks the whole batch up front, which
    /// sees the same row values (the auto numbers are previewed) and leaves a
    /// rejected batch untouched.  The only observable difference is a row
    /// naming another row of the same batch, which jackcess cannot resolve
    /// either (the earlier row's data page is not written yet).
    fn check_add_rows(&mut self, table_page: i32, rows: &[Vec<Value>]) -> Result<()> {
        if rows.is_empty() || !self.enforce_foreign_keys {
            return Ok(());
        }
        let has_indexes = self
            .tables
            .get(&table_page)
            .map(|t| !t.index_datas.is_empty())
            .unwrap_or(false);
        if !has_indexes {
            return Ok(());
        }
        let context = crate::fkenforcer::FkContext::for_table(self, table_page)?;
        if context.is_empty() {
            return Ok(());
        }
        let preview = {
            let table = self.tables.get(&table_page).unwrap();
            let allow_insert = table.is_allow_auto_number_insert(&self.channel);
            table.preview_add_rows(rows, allow_insert)?
        };
        for row in &preview {
            context.add_row(self, row)?;
        }
        Ok(())
    }

    /// The table definition page of a named table, loading it if necessary.
    pub(crate) fn table_page(&mut self, table_name: &str) -> Result<i32> {
        let page = match self.table_lookup.get(&table_name.to_uppercase()) {
            Some((p, _)) => *p,
            None => {
                return Err(Error::Jackcess(format!(
                    "Table {} does not exist",
                    table_name
                )))
            }
        };
        if page <= 0 {
            return Err(Error::Unsupported(format!(
                "table '{}' has no table definition page (it is a linked table); \
                 use the table name based API",
                table_name
            )));
        }
        if !self.tables.contains_key(&page) {
            let flags = self
                .table_lookup
                .get(&table_name.to_uppercase())
                .map(|(_, f)| *f)
                .unwrap_or(0);
            let mut t = Table::read(&mut self.channel, page, table_name, flags, &self.format)?;
            t.set_database_error_handler(self.error_handler.clone());
            self.apply_table_settings(&mut t);
            self.tables.insert(page, t);
            // `DatabaseImpl.getTable` always applies the catalog metadata (the
            // linked ODBC flag, the property driven validators and the database
            // name), also when the table is first touched through a row API
            self.apply_catalog_table_info(page)?;
            self.init_table_column_properties(page)?;
        }
        Ok(page)
    }

    /// Deletes the row with the given id from the named table
    /// (`Table.deleteRow`), after the foreign key checks and cascades of
    /// `FKEnforcer.deleteRow` (which jackcess runs before the row is marked
    /// deleted).
    pub fn delete_row(&mut self, table_name: &str, row_id: crate::table::RowId) -> Result<()> {
        // the row id comes from (and is deleted with) the linkee's table
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.delete_row(&linked_name, row_id);
        }
        let page = self.table_page(table_name)?;
        self.check_delete_row(page, row_id)?;
        let t = self.tables.get_mut(&page).unwrap();
        t.delete_row(&mut self.channel, row_id)
    }

    fn check_delete_row(&mut self, table_page: i32, row_id: crate::table::RowId) -> Result<()> {
        if !self.enforce_foreign_keys {
            return Ok(());
        }
        let has_indexes = self
            .tables
            .get(&table_page)
            .map(|t| !t.index_datas.is_empty())
            .unwrap_or(false);
        if !has_indexes {
            return Ok(());
        }
        let context = crate::fkenforcer::FkContext::for_table(self, table_page)?;
        if context.is_empty() {
            return Ok(());
        }
        // jackcess reads the row values first, and ignores a duplicate deletion
        let deleted = {
            let table = self.tables.get(&table_page).unwrap();
            table
                .position_at_row_header(&mut self.channel, row_id)?
                .deleted
        };
        if deleted {
            return Ok(());
        }
        let row = {
            let table = self.tables.get(&table_page).unwrap();
            table.read_row_at(&mut self.channel, row_id)?
        };
        context.delete_row(self, &row)
    }

    /// Updates the row with the given id in the named table
    /// (`Table.updateRow`), after the foreign key checks and cascades of
    /// `FKEnforcer.updateRow` (which jackcess runs before the row or its index
    /// entries are written).
    ///
    /// Any column value may be [`Value::Keep`] (`Column.KEEP_VALUE`), which
    /// keeps the value the row already holds (see [`Database::update_row_partial`]).
    pub fn update_row(
        &mut self,
        table_name: &str,
        row_id: crate::table::RowId,
        row: &[Value],
    ) -> Result<()> {
        self.update_row_partial(table_name, row_id, row)
    }

    /// Java `Table.updateRow(RowId, Object...)`: updates the row with the given
    /// id, where any column value may be [`Value::Keep`] (`Column.KEEP_VALUE`)
    /// to keep the value the row already holds.
    ///
    /// A kept fixed length column keeps its old decoded value, while a kept
    /// variable length column also keeps the raw on-page bytes of the old
    /// value (Java's `keepRawVarValues`): a kept `MEMO`/`OLE` value therefore
    /// keeps its existing long value definition (the same inline/other page
    /// pointer) instead of being written to new long value pages.  Kept values
    /// are the old values for the index updates, the foreign key checks and the
    /// validators, and an update never generates a column default value.
    pub fn update_row_partial(
        &mut self,
        table_name: &str,
        row_id: crate::table::RowId,
        row: &[Value],
    ) -> Result<()> {
        // a linked table updates the linkee's row (and its index entries) using
        // the linkee's definition and channel; `Value::Keep` entries keep the
        // linkee row's values, and the linkee's validators/foreign keys run
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.update_row_partial(&linked_name, row_id, row);
        }
        let page = self.table_page(table_name)?;
        self.check_update_row(page, row_id, row)?;
        let tz = self.tz;
        let t = self.tables.get_mut(&page).unwrap();
        t.update_row(&mut self.channel, row_id, row, &tz)
    }

    /// Java `Table.updateValue(Column, RowId, Object)`: updates a single
    /// column of the row and keeps every other column (`Arrays.fill(row,
    /// Column.KEEP_VALUE)`).
    ///
    /// The column is matched by name, case insensitively.  The value may be
    /// [`Value::Keep`] as well, which makes the call a no-op for the column.
    pub fn update_value(
        &mut self,
        table_name: &str,
        row_id: crate::table::RowId,
        column: &str,
        value: Value,
    ) -> Result<()> {
        // the column is looked up in the linkee's table (`Table.updateValue`)
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.update_value(&linked_name, row_id, column, value);
        }
        let page = self.table_page(table_name)?;
        let (ci, num_columns) = {
            let t = self.tables.get(&page).unwrap();
            let ci = t.column_index(column).ok_or_else(|| {
                Error::IllegalArgument(format!(
                    "Column {} does not exist in table {}",
                    column, table_name
                ))
            })?;
            (ci, t.columns.len())
        };
        let mut row = vec![Value::Keep; num_columns];
        row[ci] = value;
        self.update_row_partial(table_name, row_id, &row)
    }

    /// Java `Table.asUpdateRow` followed by `Table.updateRow(RowId, Object...)`:
    /// builds a row where every column the map does not name is kept
    /// (`Column.KEEP_VALUE`), updates the row, and returns the row values as
    /// they are now stored (the equivalent of Java's `returnRowValues`).
    ///
    /// Column names are matched case insensitively; a key which names no column
    /// of the table is ignored (Java's `asRow` only visits the table's
    /// columns).  A key mapped to [`Value::Keep`] is a no-op for that column,
    /// and a key mapped to a null value sets the column to null (the map
    /// `containsKey` semantics).
    pub fn update_row_from_map(
        &mut self,
        table_name: &str,
        row_id: crate::table::RowId,
        row: &std::collections::BTreeMap<String, Value>,
    ) -> Result<Vec<Value>> {
        // a linked table's row is built from the linkee's columns and updated
        // (and read back) through the linkee's channel
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.update_row_from_map(&linked_name, row_id, row);
        }
        let page = self.table_page(table_name)?;
        let num_columns = {
            let t = self.tables.get(&page).unwrap();
            t.columns.len()
        };
        let mut values = vec![Value::Keep; num_columns];
        {
            let t = self.tables.get(&page).unwrap();
            for (name, value) in row {
                if let Some(ci) = t.column_index(name) {
                    values[ci] = value.clone();
                }
            }
        }
        self.update_row_partial(table_name, row_id, &values)?;
        let t = self.tables.get(&page).unwrap();
        t.read_row_at(&mut self.channel, row_id)
    }

    fn check_update_row(
        &mut self,
        table_page: i32,
        row_id: crate::table::RowId,
        row: &[Value],
    ) -> Result<()> {
        if !self.enforce_foreign_keys {
            return Ok(());
        }
        let has_indexes = self
            .tables
            .get(&table_page)
            .map(|t| !t.index_datas.is_empty())
            .unwrap_or(false);
        if !has_indexes {
            return Ok(());
        }
        let context = crate::fkenforcer::FkContext::for_table(self, table_page)?;
        if context.is_empty() {
            return Ok(());
        }
        let old_row = {
            let table = self.tables.get(&table_page).unwrap();
            table.read_row_at(&mut self.channel, row_id)?
        };
        // the row the enforcer sees is the one `Table::update_row` will write:
        // padded to the column count, with the KEEP_VALUE entries resolved to
        // the old values and the auto numbers of the old row (unless an
        // explicit auto number insert is allowed)
        let mut new_row = row.to_vec();
        let table = self.tables.get(&table_page).unwrap();
        let num_columns = table.columns.len();
        new_row.resize(num_columns, Value::Null);
        let auto_columns = table.auto_num_columns.clone();
        let allow_auto_number_insert = table.is_allow_auto_number_insert(&self.channel);
        for ci in auto_columns {
            let keep = match new_row.get(ci) {
                // a KEEP_VALUE on an auto number behaves like nothing given
                Some(v) if !v.is_keep() && allow_auto_number_insert && !v.is_null() => false,
                _ => true,
            };
            if keep {
                new_row[ci] = old_row.get(ci).cloned().unwrap_or(Value::Null);
            }
        }
        // the property driven column validators run before the foreign key
        // checks (which may cascade into other tables), like `TableImpl
        // .updateRow`; a kept column is not validated again
        {
            let table = self.tables.get(&table_page).unwrap();
            table.prepare_update_row(&old_row, &mut new_row)?;
        }
        context.update_row(self, &old_row, &new_row)
    }

    /// Reads all rows of the named table together with their header row ids
    /// (`Table.readAllRowsWithIds` equivalent).
    ///
    /// A linked table name is resolved through the linker's link resolver and
    /// the rows are read from the linkee database with the linkee's channel
    /// (`DatabaseImpl.getTable(TableInfo, ...)` + `Table.readAllRowsWithIds`).
    /// The returned rows are owned, so the linker can be used again
    /// immediately.
    ///
    /// Row errors are passed to the handler configured at the cursor, table or
    /// database level (in that order); the default handler rethrows them.
    pub fn read_rows_with_ids(
        &mut self,
        table_name: &str,
    ) -> Result<Vec<(crate::table::RowId, Option<Vec<Value>>)>> {
        self.read_rows_with_ids_using(table_name, None)
    }

    /// [`Database::read_rows_with_ids`] with an explicit cursor level
    /// [`ErrorHandler`] override (`None` uses the table/database handler).
    pub fn read_rows_with_ids_using(
        &mut self,
        table_name: &str,
        handler_override: Option<Arc<dyn ErrorHandler>>,
    ) -> Result<Vec<(crate::table::RowId, Option<Vec<Value>>)>> {
        if let Some(info) = self.linked_table_info(table_name).cloned() {
            let linked_db = self.get_or_open_linked_database(&info.linked_db_name)?;
            return linked_db.read_rows_with_ids_using(&info.linked_table_name, handler_override);
        }
        let page = self.table_page(table_name)?;
        let t = self.tables.get_mut(&page).unwrap();
        let handler = handler_override.or_else(|| t.error_handler_override());
        t.read_all_rows_with_ids_using(&mut self.channel, handler.as_deref())
    }

    /// The row slots of the named table with their deleted flags, without
    /// decoding column values (the position scan a cursor uses to move between
    /// rows; no [`ErrorHandler`] is invoked).
    pub fn row_id_statuses(
        &mut self,
        table_name: &str,
    ) -> Result<Vec<(crate::table::RowId, bool)>> {
        if let Some(info) = self.linked_table_info(table_name).cloned() {
            let linked_db = self.get_or_open_linked_database(&info.linked_db_name)?;
            return linked_db.row_id_statuses(&info.linked_table_name);
        }
        let page = self.table_page(table_name)?;
        let table = self.tables.get(&page).unwrap();
        table.row_id_statuses(&mut self.channel)
    }

    /// Reads one row of the named table by id
    /// (`CursorImpl.getCurrentRow`'s row read), resolving a linked table like
    /// [`Database::read_rows_with_ids`] and applying the cursor level error
    /// handler override when one is given.
    pub fn read_row_by_id_using(
        &mut self,
        table_name: &str,
        row_id: crate::table::RowId,
        handler_override: Option<Arc<dyn ErrorHandler>>,
    ) -> Result<Option<Vec<Value>>> {
        if let Some(info) = self.linked_table_info(table_name).cloned() {
            let linked_db = self.get_or_open_linked_database(&info.linked_db_name)?;
            return linked_db.read_row_by_id_using(
                &info.linked_table_name,
                row_id,
                handler_override,
            );
        }
        let page = self.table_page(table_name)?;
        let table = self.tables.get(&page).unwrap();
        if table
            .position_at_row_header(&mut self.channel, row_id)?
            .deleted
        {
            return Ok(None);
        }
        let handler = handler_override.or_else(|| table.error_handler_override());
        Ok(Some(table.read_row_at_using(
            &mut self.channel,
            row_id,
            handler.as_deref(),
        )?))
    }

    // ------------------------------------------------------------------
    // complex (multi-value / attachment) columns
    // ------------------------------------------------------------------

    /// `DatabaseImpl.getSystemComplexColumns` exists; the port resolves the
    /// `MSysComplexColumns` table by name where needed.
    ///
    /// Sets the flat table names the next complex columns create.  Access puts
    /// a random guid in these names; the verification harness passes the names
    /// the Java library generated so the two files can be compared.
    pub fn set_complex_names(&mut self, names: Vec<String>) {
        self.complex_names = names;
    }

    /// The complex column info of a column of the named table
    /// (`Column.getComplexInfo`).
    pub fn complex_info(
        &mut self,
        table_name: &str,
        column_name: &str,
    ) -> Result<crate::complex::ComplexColumnInfo> {
        // a complex column of a linked table belongs to the linkee's table
        // definition (its supporting tables live in the linkee file)
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.complex_info(&linked_name, column_name);
        }
        let page = self.table_page(table_name)?;
        self.load_table_by_page(page)?;
        let column = {
            let table = self.tables.get(&page).unwrap();
            table
                .get_column(column_name)
                .cloned()
                .ok_or_else(|| Error::Jackcess(format!("Column {} not found", column_name)))?
        };
        crate::complex::load_complex_info(self, page, &column)
    }

    /// The handle for the complex values of one row (`Column.getRowValue`
    /// returns a `ComplexValueForeignKey` for a complex column).
    pub fn complex_value_fk(
        &mut self,
        table_name: &str,
        row_id: crate::table::RowId,
        column_name: &str,
    ) -> Result<crate::complex::ComplexValueForeignKey<'_>> {
        // the returned handle is bound to the linkee database, so its writes
        // go to the linkee's complex value tables
        if self.is_linked_table(table_name) {
            let (linked_db, linked_name) = self
                .linked_table_target(table_name)?
                .expect("the table name was just found to be linked");
            return linked_db.complex_value_fk(&linked_name, row_id, column_name);
        }
        self.local_complex_value_fk(table_name, row_id, column_name)
    }

    /// The non-linked branch of [`Database::complex_value_fk`].
    fn local_complex_value_fk(
        &mut self,
        table_name: &str,
        row_id: crate::table::RowId,
        column_name: &str,
    ) -> Result<crate::complex::ComplexValueForeignKey<'_>> {
        let page = self.table_page(table_name)?;
        self.load_table_by_page(page)?;
        let column = {
            let table = self.tables.get(&page).unwrap();
            table
                .get_column(column_name)
                .cloned()
                .ok_or_else(|| Error::Jackcess(format!("Column {} not found", column_name)))?
        };
        let info = crate::complex::load_complex_info(self, page, &column)?;
        let column_index = {
            let table = self.tables.get(&page).unwrap();
            table
                .get_column(column_name)
                .map(|c| c.column_index)
                .ok_or_else(|| Error::Jackcess(format!("Column {} not found", column_name)))?
        };
        let row = {
            let table = self.tables.get(&page).unwrap();
            table.read_row_at(&mut self.channel, row_id)?
        };
        let value = row.get(column_index).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        Ok(crate::complex::ComplexValueForeignKey::new(
            self, page, row_id, info, value,
        ))
    }

    /// The complex values of one row, without keeping a handle
    /// (`ComplexValueForeignKey.getValues`).
    pub fn complex_values(
        &mut self,
        table_name: &str,
        row_id: crate::table::RowId,
        column_name: &str,
    ) -> Result<Vec<crate::complex::ComplexValue>> {
        let mut fk = self.complex_value_fk(table_name, row_id, column_name)?;
        fk.values()
    }

    /// The name of the version history complex column which tracks the given
    /// memo column (`Column.getVersionHistoryColumn`, which jackcess links to
    /// the memo column in `VersionHistoryColumnInfoImpl.postTableLoadInit`).
    pub fn version_history_column_name(
        &mut self,
        table_name: &str,
        memo_column_name: &str,
    ) -> Result<Option<String>> {
        // the memo column and its version history column are the linkee's
        // columns (`Column.getVersionHistoryColumn`)
        if let Some((linked_db, linked_name)) = self.linked_table_target(table_name)? {
            return linked_db.version_history_column_name(&linked_name, memo_column_name);
        }
        let page = self.table_page(table_name)?;
        self.load_table_by_page(page)?;
        let complex_columns: Vec<crate::column::Column> = {
            let table = self.tables.get(&page).unwrap();
            table
                .columns
                .iter()
                .filter(|c| c.data_type == DataType::ComplexType)
                .cloned()
                .collect()
        };
        for column in complex_columns {
            let info = crate::complex::load_complex_info(self, page, &column)?;
            if info.kind != crate::complex::ComplexDataType::VersionHistory {
                continue;
            }
            if info
                .value_column
                .as_deref()
                .map(|name| name.eq_ignore_ascii_case(memo_column_name))
                .unwrap_or(false)
            {
                return Ok(Some(column.name));
            }
        }
        Ok(None)
    }

    /// Whether a memo column keeps a version history (`Column.isAppendOnly`,
    /// which jackcess answers by looking for the linked version history
    /// column).
    pub fn is_append_only(&mut self, table_name: &str, memo_column_name: &str) -> Result<bool> {
        Ok(self
            .version_history_column_name(table_name, memo_column_name)?
            .is_some())
    }

    /// The complex info of the version history column of a memo column
    /// (`Column.getComplexInfo` of the version history column).
    pub fn version_history_info(
        &mut self,
        table_name: &str,
        memo_column_name: &str,
    ) -> Result<Option<crate::complex::ComplexColumnInfo>> {
        match self.version_history_column_name(table_name, memo_column_name)? {
            Some(name) => Ok(Some(self.complex_info(table_name, &name)?)),
            None => Ok(None),
        }
    }

    /// The version history handle of one row (`ComplexValue` of the version
    /// history column, addressed through the memo column it tracks).
    pub fn version_history_fk(
        &mut self,
        table_name: &str,
        row_id: crate::table::RowId,
        memo_column_name: &str,
    ) -> Result<crate::complex::ComplexValueForeignKey<'_>> {
        let name = self
            .version_history_column_name(table_name, memo_column_name)?
            .ok_or_else(|| {
                Error::Jackcess(format!(
                    "Column {} of table {} does not keep a version history",
                    memo_column_name, table_name
                ))
            })?;
        self.complex_value_fk(table_name, row_id, &name)
    }

    pub fn table_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.table_lookup.keys().cloned().collect();
        names.sort();
        names
    }

    /// Returns the owner SID used for newly created objects.
    pub fn get_new_object_owner(&mut self) -> Result<Vec<u8>> {
        let db_parent = self.db_parent_id;
        let owner = {
            let rows = {
                let t = self.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
                t.read_all_rows(&mut self.channel)?
            };
            let t = self.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
            let idx_name = t.column_index(CAT_COL_NAME);
            let idx_parent = t.column_index(CAT_COL_PARENT_ID);
            let idx_owner = t.column_index(CAT_COL_OWNER);
            rows.into_iter().flatten().find_map(|row| {
                let name = idx_name.and_then(|i| row.get(i)).and_then(|v| v.as_text());
                let parent = idx_parent.and_then(|i| row.get(i)).and_then(|v| v.as_i64());
                if name == Some("MSysDb") && parent == Some(db_parent as i64) {
                    idx_owner
                        .and_then(|i| row.get(i))
                        .and_then(|v| v.as_bytes())
                        .map(|b| b.to_vec())
                } else {
                    None
                }
            })
        };
        Ok(match owner {
            Some(o) if !o.is_empty() => o,
            _ => SYS_DEFAULT_SID.to_vec(),
        })
    }

    /// Adds a row to a system table by name.
    pub fn add_system_row(&mut self, table_name: &str, values: Vec<Value>) -> Result<()> {
        // `Table.addRow` writes the row in its own write operation, while the
        // table definition update needs one to be in progress; the outer
        // operation keeps the whole insert (row + definition) together and is
        // a no-op for callers which already hold one.
        self.channel.start_write();
        let result = (|| -> Result<()> {
            let tz = self.tz;
            let page = match self.table_lookup.get(&table_name.to_uppercase()) {
                Some((p, _)) => *p,
                None => {
                    return Err(Error::Jackcess(format!(
                        "missing system table {}",
                        table_name
                    )))
                }
            };
            if !self.tables.contains_key(&page) {
                let mut t = Table::read(&mut self.channel, page, table_name, 0, &self.format)?;
                t.set_database_error_handler(self.error_handler.clone());
                self.apply_table_settings(&mut t);
                self.tables.insert(page, t);
                self.apply_catalog_table_info(page)?;
            }
            {
                let table = self.tables.get_mut(&page).unwrap();
                table.add_row(&mut self.channel, &values, &tz)?;
                table.update_table_definition_ch(&mut self.channel)?;
            }
            Ok(())
        })();
        let finish = self.channel.finish_write();
        match result {
            Ok(()) => finish,
            Err(e) => {
                let _ = finish;
                Err(e)
            }
        }
    }

    /// Reads all rows of the table on the given definition page.
    pub fn read_table_rows(&mut self, page: i32) -> Result<Vec<Option<Vec<Value>>>> {
        if !self.tables.contains_key(&page) {
            // the catalog spelling is the table name the Java library reports
            // (the lookup key is upper case)
            let name = match self.catalog_table_name(page)? {
                Some(name) => name,
                None => self
                    .table_lookup
                    .iter()
                    .find(|(_, (p, _))| *p == page)
                    .map(|(n, _)| n.clone())
                    .unwrap_or_default(),
            };
            let flags = self.table_lookup.get(&name).map(|(_, f)| *f).unwrap_or(0);
            let mut table = Table::read(&mut self.channel, page, &name, flags, &self.format)?;
            table.set_database_error_handler(self.error_handler.clone());
            self.apply_table_settings(&mut table);
            self.tables.insert(page, table);
            self.apply_catalog_table_info(page)?;
            self.init_table_column_properties(page)?;
        }
        let table = self.tables.get(&page).unwrap();
        table.read_all_rows(&mut self.channel)
    }
}

/// `DatabaseImpl.validateIdentifierName`: a name which is valid for an Access
/// identifier (relationship, table, ...).
pub(crate) fn validate_identifier_name(
    name: &str,
    max_length: usize,
    identifier_type: &str,
) -> Result<()> {
    validate_name(name, max_length, identifier_type)?;

    // `DatabaseImpl.INVALID_IDENTIFIER_CHARS`
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '.' | '!' | '`' | ']' | '['))
    {
        return Err(Error::IllegalArgument(format!(
            "{} name '{}' contains invalid characters",
            identifier_type, name
        )));
    }
    if name.starts_with(' ') {
        return Err(Error::IllegalArgument(format!(
            "{} name '{}' cannot start with a space character",
            identifier_type, name
        )));
    }
    Ok(())
}

/// `DatabaseImpl.validateName`
fn validate_name(name: &str, max_length: usize, name_type: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::IllegalArgument(format!(
            "{} must have non-blank name",
            name_type
        )));
    }
    // Java's String.length counts UTF-16 code units
    if name.encode_utf16().count() > max_length {
        return Err(Error::IllegalArgument(format!(
            "{} name is longer than max length of {}: {}",
            name_type, max_length, name
        )));
    }
    Ok(())
}

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    //! Tests of the property persistence plumbing against fixtures the Java
    //! library wrote (`java-src/PropFixtures.java`) and of the catalog row
    //! creation fallback.

    use super::*;
    use crate::column::ColumnSpec;
    use crate::property::{
        DEFAULT_VALUE_PROP, DESCRIPTION_PROP, FORMAT_PROP, ROW_SOURCE_PROP, TITLE_PROP,
    };
    use crate::tablecreator::{IndexSpec, TableSpec};

    /// Test scratch directory inside the workspace (the sandbox does not allow
    /// writes outside it for spawned test processes).
    fn test_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn blob_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("property_ref")
            .join("blobs")
    }

    fn java_blob(name: &str) -> Vec<u8> {
        std::fs::read(blob_dir().join(name))
            .unwrap_or_else(|e| panic!("missing java fixture {}: {}", name, e))
    }

    /// The properties of the `dbprops` scenario (`Gen.java`).
    fn db_properties() -> DatabaseProperties {
        DatabaseProperties::new()
            .put_database_property_typed(
                TITLE_PROP,
                DataType::Text,
                Value::Text("Property DB".to_string()),
            )
            .put_database_property_typed("CustomNum", DataType::Long, Value::Long(12345))
            .put_summary_property_typed(
                TITLE_PROP,
                DataType::Text,
                Value::Text("Summary title".to_string()),
            )
            .put_user_defined_property_typed("MyBool", DataType::Boolean, Value::Bool(true))
    }

    /// The properties of the `tableprops` scenario (`Gen.java`).
    fn props_table_spec() -> TableSpec {
        TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text)
                    .with_length(50)
                    .put_property(DESCRIPTION_PROP, Value::Text("the name column".to_string()))
                    .unwrap()
                    .put_property(FORMAT_PROP, Value::Text("@".to_string()))
                    .unwrap(),
                ColumnSpec::new("Value", DataType::Double)
                    .put_property(DEFAULT_VALUE_PROP, Value::Text("1.5".to_string()))
                    .unwrap(),
                ColumnSpec::new("Note", DataType::Text)
                    .with_length(30)
                    .put_property(DESCRIPTION_PROP, Value::Text("the note column".to_string()))
                    .unwrap(),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ))
        .put_property(DESCRIPTION_PROP, Value::Text("the test table".to_string()))
        .unwrap()
        // a MEMO property whose value does not fit into one long value row
        .put_property_typed(
            ROW_SOURCE_PROP,
            DataType::Memo,
            Value::Text("abcdefghij".repeat(600)),
        )
    }

    /// The current `LvProp` value of a catalog row, read back from the file.
    fn raw_lvprop(db: &mut Database, object_id: i32) -> Option<Value> {
        let rows = db.read_rows_with_ids(TABLE_SYSTEM_CATALOG).unwrap();
        let table = db.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
        let idx_id = table.column_index(CAT_COL_ID);
        let idx_props = table.column_index(CAT_COL_PROPS);
        for (_, values) in rows {
            let values = match values {
                Some(values) => values,
                None => continue,
            };
            let id = idx_id.and_then(|i| values.get(i)).and_then(|v| v.as_i64());
            if id == Some(object_id as i64) {
                return idx_props.and_then(|i| values.get(i)).cloned();
            }
        }
        None
    }

    #[test]
    fn database_property_groups_match_java_fixtures() {
        let path = test_tmp_dir().join("propsave_dbprops.mdb");
        let _ = std::fs::remove_file(&path);
        {
            let mut db =
                Database::create_with_properties(FileFormat::V2003, &path, &db_properties())
                    .unwrap();
            db.flush().unwrap();
        }

        let mut db = Database::open_path(&path).unwrap();
        let format = db.format.clone();

        let mut maps = db.get_database_properties().unwrap();
        assert!(maps.row_id().is_some(), "the MSysDb row is bound");
        assert_eq!(
            maps.write(&format).unwrap(),
            java_blob("dbprops_jet4.bin"),
            "database property group bytes"
        );
        let default = maps.get_default();
        assert_eq!(
            default.get_value(TITLE_PROP),
            Some(&Value::Text("Property DB".to_string()))
        );
        assert_eq!(default.get_value("CustomNum"), Some(&Value::Long(12345)));
        assert_eq!(default.get("CustomNum").unwrap().data_type, DataType::Long);
        // Title is not a DDL property, CustomNum is an unknown property
        assert!(!default.is_ddl(TITLE_PROP));
        assert!(!default.is_ddl("CustomNum"));

        let mut maps = db.get_summary_properties().unwrap();
        assert!(maps.row_id().is_some());
        assert_eq!(
            maps.write(&format).unwrap(),
            java_blob("dbprops_summary_jet4.bin"),
            "summary property group bytes"
        );
        assert_eq!(
            maps.get_default().get_value(TITLE_PROP),
            Some(&Value::Text("Summary title".to_string()))
        );

        let mut maps = db.get_user_defined_properties().unwrap();
        assert!(maps.row_id().is_some());
        assert_eq!(
            maps.write(&format).unwrap(),
            java_blob("dbprops_userdef_jet4.bin"),
            "user defined property group bytes"
        );
        let prop = maps.get_default().get("MyBool").unwrap();
        assert_eq!(prop.data_type, DataType::Boolean);
        assert_eq!(prop.value, Value::Bool(true));
    }

    #[test]
    fn table_creation_properties_match_java_fixtures() {
        let path = test_tmp_dir().join("propsave_tableprops.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let page = db.create_table(&props_table_spec()).unwrap();
        let format = db.format.clone();

        // the group the table creator saved: column maps first, then the table
        let created = java_blob("tableprops_created_jet4.bin");
        assert!(
            created.len() > 4096,
            "the creation blob is stored across several long value pages"
        );
        {
            let mut maps = db.get_table_properties(page).unwrap();
            assert_eq!(maps.write(&format).unwrap(), created);
            assert_eq!(
                maps.get_default().get_value(DESCRIPTION_PROP),
                Some(&Value::Text("the test table".to_string()))
            );
            assert_eq!(
                maps.get_default().get_value(ROW_SOURCE_PROP),
                Some(&Value::Text("abcdefghij".repeat(600)))
            );
            assert_eq!(
                maps.get("Name").unwrap().get_value(DESCRIPTION_PROP),
                Some(&Value::Text("the name column".to_string()))
            );
            assert_eq!(
                maps.get("Name").unwrap().get_value(FORMAT_PROP),
                Some(&Value::Text("@".to_string()))
            );
            assert_eq!(
                maps.get("Value").unwrap().get_value(DEFAULT_VALUE_PROP),
                Some(&Value::Text("1.5".to_string()))
            );
            assert_eq!(
                maps.get("Note").unwrap().get_value(DESCRIPTION_PROP),
                Some(&Value::Text("the note column".to_string()))
            );
            // a column's Description is not the table's Description
            let column_description = maps
                .get("Name")
                .unwrap()
                .get_value(DESCRIPTION_PROP)
                .cloned();
            let table_description = maps.get_default().get_value(DESCRIPTION_PROP).cloned();
            assert_ne!(column_description, table_description);
        }
        // what the table creator wrote is what the file holds
        match raw_lvprop(&mut db, page) {
            Some(Value::Bytes(bytes)) => assert_eq!(bytes, created, "LvProp value"),
            other => panic!("unexpected LvProp value: {:?}", other),
        }

        let rows: Vec<Vec<Value>> = (0..3i64)
            .map(|i| {
                vec![
                    Value::Null,
                    Value::Text(format!("name{}", i)),
                    Value::Double(1.5 + i as f64),
                    Value::Text(format!("note {}", i)),
                ]
            })
            .collect();
        db.add_rows("Test", &rows).unwrap();

        // change a property value, remove another property and save
        let mut maps = db.get_table_properties_by_name("Test").unwrap();
        maps.get_default()
            .put_value(
                DESCRIPTION_PROP,
                Value::Text("the changed table".to_string()),
            )
            .unwrap();
        assert!(maps.get_mut("Name").remove(FORMAT_PROP).is_some());
        maps.get_mut("Note")
            .put_value(DESCRIPTION_PROP, Value::Text("note changed".to_string()))
            .unwrap();
        maps.save(&mut db).unwrap();
        db.flush().unwrap();
        drop(db);

        // the updated group matches the java bytes after a reopen as well
        let mut db = Database::open_path(&path).unwrap();
        let format = db.format.clone();
        let mut maps = db.get_table_properties_by_name("Test").unwrap();
        assert_eq!(
            maps.write(&format).unwrap(),
            java_blob("tableprops_updated_jet4.bin"),
            "updated table property group bytes"
        );
        assert_eq!(
            maps.get_default().get_value(DESCRIPTION_PROP),
            Some(&Value::Text("the changed table".to_string()))
        );
        // the multi page MEMO property survived the update unchanged
        assert_eq!(
            maps.get_default().get_value(ROW_SOURCE_PROP),
            Some(&Value::Text("abcdefghij".repeat(600)))
        );
        assert!(maps.get("Name").unwrap().get(FORMAT_PROP).is_none());
        assert_eq!(
            maps.get("Name").unwrap().get_value(DESCRIPTION_PROP),
            Some(&Value::Text("the name column".to_string()))
        );
        assert_eq!(
            maps.get("Note").unwrap().get_value(DESCRIPTION_PROP),
            Some(&Value::Text("note changed".to_string()))
        );
        // the untouched column property survived
        assert_eq!(
            maps.get("Value").unwrap().get_value(DEFAULT_VALUE_PROP),
            Some(&Value::Text("1.5".to_string()))
        );
    }

    /// Removing every property of a group stores a null `LvProp`, exactly like
    /// the Java `PropertyMaps.Handler.write` returning `null` for an empty
    /// group (and the row update storing it as a null long value).
    #[test]
    fn empty_property_group_is_saved_as_a_null_value() {
        let path = test_tmp_dir().join("propsave_empty.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(20),
            ],
        )
        .put_property(DESCRIPTION_PROP, Value::Text("a table".to_string()))
        .unwrap();
        let page = db.create_table(&spec).unwrap();
        assert!(matches!(raw_lvprop(&mut db, page), Some(Value::Bytes(_))));

        let mut maps = db.get_table_properties(page).unwrap();
        maps.get_default().clear();
        maps.save(&mut db).unwrap();
        assert_eq!(raw_lvprop(&mut db, page), Some(Value::Null));

        db.flush().unwrap();
        drop(db);
        let mut db = Database::open_path(&path).unwrap();
        let mut maps = db.get_table_properties_by_name("T").unwrap();
        assert!(maps.default_map().map(|m| m.is_empty()).unwrap_or(true));
        // saving the empty group again keeps the value null
        maps.save(&mut db).unwrap();
        assert_eq!(raw_lvprop(&mut db, page), Some(Value::Null));
    }

    /// A database level group whose catalog row does not exist (Access never
    /// added the `SummaryInfo` row) gets its row created on save.  The Java
    /// library has no such fallback.
    #[test]
    fn missing_summary_row_is_created_on_save() {
        let path = test_tmp_dir().join("propsave_missing_row.mdb");
        let _ = std::fs::remove_file(&path);
        {
            let mut db = Database::create(FileFormat::V2003, &path).unwrap();
            // find and delete the SummaryInfo row
            let parent = db.db_parent_id;
            let rows = db.read_rows_with_ids(TABLE_SYSTEM_CATALOG).unwrap();
            let table = db.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
            let idx_name = table.column_index(CAT_COL_NAME);
            let idx_parent = table.column_index(CAT_COL_PARENT_ID);
            let mut summary_row = None;
            for (row_id, values) in rows {
                let values = match values {
                    Some(values) => values,
                    None => continue,
                };
                let name = idx_name
                    .and_then(|i| values.get(i))
                    .and_then(|v| v.as_text());
                let row_parent = idx_parent
                    .and_then(|i| values.get(i))
                    .and_then(|v| v.as_i64());
                if name == Some("SummaryInfo") && row_parent == Some(parent as i64) {
                    summary_row = Some(row_id);
                }
            }
            let row_id = summary_row.expect("the template has a SummaryInfo row");
            db.delete_row(TABLE_SYSTEM_CATALOG, row_id).unwrap();
            db.flush().unwrap();
        }

        let mut db = Database::open_path(&path).unwrap();
        let mut maps = db.get_summary_properties().unwrap();
        assert!(maps.row_id().is_none(), "the SummaryInfo row is gone");
        assert_eq!(maps.db_object_name(), Some("SummaryInfo"));
        maps.get_default()
            .put_value(TITLE_PROP, Value::Text("created row".to_string()))
            .unwrap();
        maps.save(&mut db).unwrap();
        // the group is bound to the created row, so a second save updates it
        assert!(maps.row_id().is_some(), "the group is bound to the new row");
        assert_eq!(maps.db_object_name(), None);
        maps.get_default()
            .put_value(TITLE_PROP, Value::Text("second value".to_string()))
            .unwrap();
        maps.save(&mut db).unwrap();
        db.flush().unwrap();
        drop(db);

        let mut db = Database::open_path(&path).unwrap();
        let mut maps = db.get_summary_properties().unwrap();
        assert!(maps.row_id().is_some(), "the row was created");
        assert_eq!(
            maps.get_default().get_value(TITLE_PROP),
            Some(&Value::Text("second value".to_string()))
        );
        assert!(
            !maps.write(&db.format).unwrap().is_empty(),
            "the created row holds the property blob"
        );
        // exactly one SummaryInfo row was created
        let parent = db.db_parent_id;
        let rows = db.read_rows_with_ids(TABLE_SYSTEM_CATALOG).unwrap();
        let table = db.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
        let idx_name = table.column_index(CAT_COL_NAME);
        let idx_parent = table.column_index(CAT_COL_PARENT_ID);
        let mut count = 0;
        for (_, values) in rows {
            let values = match values {
                Some(values) => values,
                None => continue,
            };
            let name = idx_name
                .and_then(|i| values.get(i))
                .and_then(|v| v.as_text());
            let row_parent = idx_parent
                .and_then(|i| values.get(i))
                .and_then(|v| v.as_i64());
            if name == Some("SummaryInfo") && row_parent == Some(parent as i64) {
                count += 1;
            }
        }
        assert_eq!(count, 1, "only one SummaryInfo row");
    }

    /// The `summaryprops` scenario without the byte comparison: change/remove
    /// properties of the three database level groups of an existing database
    /// and read the result back after a reopen.
    #[test]
    fn database_level_properties_can_be_changed_and_removed() {
        let path = test_tmp_dir().join("propsave_summaryprops.mdb");
        let _ = std::fs::remove_file(&path);
        {
            let mut db = Database::create(FileFormat::V2003, &path).unwrap();

            let mut props = db.get_database_properties().unwrap();
            props
                .get_default()
                .put_value(TITLE_PROP, Value::Text("Changed DB title".to_string()))
                .unwrap();
            assert!(props.get_default().remove("NavPane Width").is_some());
            props.save(&mut db).unwrap();

            let mut props = db.get_summary_properties().unwrap();
            props
                .get_default()
                .put_value(TITLE_PROP, Value::Text("The summary title".to_string()))
                .unwrap();
            assert!(props
                .get_default()
                .remove(crate::property::AUTHOR_PROP)
                .is_some());
            props.save(&mut db).unwrap();

            let mut props = db.get_user_defined_properties().unwrap();
            assert!(props
                .get_default()
                .remove(crate::property::REPLICATE_PROJECT_PROP)
                .is_some());
            props
                .get_default()
                .put_value("MyFlag", Value::Bool(true))
                .unwrap();
            props
                .get_default()
                .put_value("MyText", Value::Text("user text".to_string()))
                .unwrap();
            props.save(&mut db).unwrap();
            db.flush().unwrap();
        }

        let mut db = Database::open_path(&path).unwrap();
        let mut props = db.get_database_properties().unwrap();
        let default = props.get_default();
        assert_eq!(
            default.get_value(TITLE_PROP),
            Some(&Value::Text("Changed DB title".to_string()))
        );
        assert!(default.get("NavPane Width").is_none());

        let props = db.get_summary_properties().unwrap();
        let default = props.default_map().unwrap();
        assert_eq!(
            default.get_value(TITLE_PROP),
            Some(&Value::Text("The summary title".to_string()))
        );
        assert!(default.get(crate::property::AUTHOR_PROP).is_none());

        let props = db.get_user_defined_properties().unwrap();
        let default = props.default_map().unwrap();
        assert!(default
            .get(crate::property::REPLICATE_PROJECT_PROP)
            .is_none());
        assert_eq!(default.get_value("MyFlag"), Some(&Value::Bool(true)));
        assert_eq!(
            default.get_value("MyText"),
            Some(&Value::Text("user text".to_string()))
        );
    }

    // ------------------------------------------------------------------
    // linked tables
    // ------------------------------------------------------------------

    /// The `MSysObjects` row of a named catalog object, as (id, type,
    /// parent id, Database, ForeignName).
    #[expect(
        clippy::type_complexity,
        reason = "test helper mirroring the five catalog columns the Java Row API exposes"
    )]
    fn catalog_object(
        db: &mut Database,
        name: &str,
    ) -> Option<(i64, i64, i64, Option<String>, Option<String>)> {
        let rows = db.read_table_rows(PAGE_SYSTEM_CATALOG).unwrap();
        let table = db.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
        let idx_id = table.column_index(CAT_COL_ID).unwrap();
        let idx_name = table.column_index(CAT_COL_NAME).unwrap();
        let idx_type = table.column_index(CAT_COL_TYPE).unwrap();
        let idx_parent = table.column_index(CAT_COL_PARENT_ID).unwrap();
        let idx_db = table.column_index(CAT_COL_DATABASE).unwrap();
        let idx_foreign = table.column_index(CAT_COL_FOREIGN_NAME).unwrap();
        for values in rows.into_iter().flatten() {
            if values.get(idx_name).and_then(|v| v.as_text()) != Some(name) {
                continue;
            }
            return Some((
                values.get(idx_id).and_then(|v| v.as_i64()).unwrap(),
                values.get(idx_type).and_then(|v| v.as_i64()).unwrap_or(-1),
                values
                    .get(idx_parent)
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1),
                values
                    .get(idx_db)
                    .and_then(|v| v.as_text())
                    .map(|s| s.to_string()),
                values
                    .get(idx_foreign)
                    .and_then(|v| v.as_text())
                    .map(|s| s.to_string()),
            ));
        }
        None
    }

    /// `DatabaseImpl.createLinkedTable`: a type 6 catalog row holding the
    /// external database/table, access control entries and a synthetic
    /// (negative) object id counted up from the highest one in use.
    #[test]
    fn linked_tables_use_synthetic_ids_and_write_catalog_rows() {
        let path = test_tmp_dir().join("linked.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let parent_id = db.table_parent_id;

        // the template already uses some synthetic ids (its navigation pane
        // groups), so the first free one is not Integer.MIN_VALUE
        let first_free = db.next_free_synthetic_id().unwrap();
        assert!(first_free < 0);

        db.create_linked_table("L1", r"C:\data\one.mdb", "Remote1")
            .unwrap();
        db.create_linked_table("L2", r"C:\data\two.accdb", "Remote2")
            .unwrap();

        // one synthetic id per created object, counting up
        assert_eq!(db.next_free_synthetic_id().unwrap(), first_free + 2);
        assert_eq!(db.table_lookup_entry("L1").unwrap(), (first_free, 0));
        assert_eq!(db.table_lookup_entry("L2").unwrap(), (first_free + 1, 0));

        let (id, typ, parent, db_name, foreign_name) =
            catalog_object(&mut db, "L1").expect("L1 catalog row");
        assert_eq!(id, first_free as i64);
        assert_eq!(typ, TYPE_LINKED_TABLE as i64);
        assert_eq!(parent, parent_id as i64);
        assert_eq!(db_name.as_deref(), Some(r"C:\data\one.mdb"));
        assert_eq!(foreign_name.as_deref(), Some("Remote1"));

        let (id, typ, _, db_name, foreign_name) =
            catalog_object(&mut db, "L2").expect("L2 catalog row");
        assert_eq!(id, (first_free + 1) as i64);
        assert_eq!(typ, TYPE_LINKED_TABLE as i64);
        assert_eq!(db_name.as_deref(), Some(r"C:\data\two.accdb"));
        assert_eq!(foreign_name.as_deref(), Some("Remote2"));

        // access control entries for both objects, like every other object
        let (aces_page, _) = db.table_lookup_entry(TABLE_SYSTEM_ACES).unwrap();
        let ace_rows = db.read_table_rows(aces_page).unwrap();
        let table = db.tables.get(&aces_page).unwrap();
        let idx_obj = table.column_index(ACE_COL_OBJECT_ID).unwrap();
        let idx_sid = table.column_index(ACE_COL_SID).unwrap();
        let mut ace_object_ids = Vec::new();
        for values in ace_rows.into_iter().flatten() {
            let object_id = match values.get(idx_obj).and_then(|v| v.as_i64()) {
                Some(id) => id,
                None => continue,
            };
            if object_id == first_free as i64 || object_id == (first_free + 1) as i64 {
                assert!(
                    values.get(idx_sid).and_then(|v| v.as_bytes()).is_some(),
                    "the ACE of a linked table carries a SID"
                );
                ace_object_ids.push(object_id);
            }
        }
        ace_object_ids.sort_unstable();
        let l1_aces = ace_object_ids
            .iter()
            .filter(|id| **id == first_free as i64)
            .count();
        let l2_aces = ace_object_ids
            .iter()
            .filter(|id| **id == (first_free + 1) as i64)
            .count();
        // every SID of the parent object gets an entry for each new object
        assert!(l1_aces > 0, "L1 has access control entries");
        assert_eq!(l1_aces, l2_aces, "both linked tables get the same SIDs");

        db.flush().unwrap();
        drop(db);

        // the lookup entry survives a reopen, so the duplicate name check
        // applies to linked tables of an existing database as well
        let mut db = Database::open_path(&path).unwrap();
        assert_eq!(db.table_lookup_entry("L1"), Some((first_free, 0)));
        assert!(matches!(
            db.create_linked_table("L1", "other.mdb", "Other"),
            Err(Error::IllegalArgument(_))
        ));
        // the default resolver refuses to open links, naming the linkee file
        // (Java's `LinkResolver.DEFAULT` throws AccessDeniedException)
        match db.get_table("L1") {
            Err(Error::Jackcess(message)) => {
                assert!(
                    message.contains(r"C:\data\one.mdb"),
                    "denied message names the linkee: {}",
                    message
                );
                assert!(message.contains("Linked database resolution is disabled."));
            }
            Ok(_) => panic!("expected a denied link resolution, got a table"),
            Err(e) => panic!("unexpected error: {}", e),
        }
        assert!(db.get_linked_databases().is_empty());
        // and the id allocation continues from the persisted rows
        assert_eq!(db.next_free_synthetic_id().unwrap(), first_free + 2);
    }

    /// The name checks of `DatabaseImpl.createLinkedTable`.
    #[test]
    fn linked_table_arguments_are_validated() {
        let path = test_tmp_dir().join("linked_validation.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        db.create_table(&TableSpec::new(
            "T",
            vec![ColumnSpec::new("ID", DataType::Long)],
        ))
        .unwrap();
        let first_free = db.next_free_synthetic_id().unwrap();

        // duplicate table name (case insensitively)
        assert!(matches!(
            db.create_linked_table("T", "x.mdb", "X"),
            Err(Error::IllegalArgument(_))
        ));
        assert!(matches!(
            db.create_linked_table("t", "x.mdb", "X"),
            Err(Error::IllegalArgument(_))
        ));
        // a blank linked database name
        assert!(matches!(
            db.create_linked_table("L", "   ", "X"),
            Err(Error::IllegalArgument(_))
        ));
        // a period is not allowed in an identifier
        assert!(matches!(
            db.create_linked_table("L", "x.mdb", "a.b"),
            Err(Error::IllegalArgument(_))
        ));
        // a name may not start with a space
        assert!(matches!(
            db.create_linked_table("Leading", "x.mdb", " leading"),
            Err(Error::IllegalArgument(_))
        ));
        // longer than MAX_TABLE_NAME_LENGTH (64)
        let too_long = "x".repeat(65);
        assert!(matches!(
            db.create_linked_table(&too_long, "x.mdb", "X"),
            Err(Error::IllegalArgument(_))
        ));

        // the failed calls did not create anything
        assert!(db.table_lookup_entry("L").is_none());
        assert_eq!(db.next_free_synthetic_id().unwrap(), first_free);

        // a space in the middle of a name is fine (only leading spaces are not)
        db.create_linked_table("Linked Table", "x.mdb", "X")
            .unwrap();
        assert_eq!(db.table_lookup_entry("Linked Table").unwrap().0, first_free);
    }

    // ------------------------------------------------------------------
    // linked-table reading (`LinkResolver`, `getLinkedDatabases`,
    // `isLinkedTable`, `getTable` through a link)
    // ------------------------------------------------------------------

    /// Creates a target database with `Source(ID pk, Name, Num)` and 3 rows,
    /// and a linker database with two linked tables pointing at it and the
    /// local `Copy(ID, Name, Num)` table.  Returns both paths.
    fn make_link_fixture(stem: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = test_tmp_dir();
        let target_path = dir.join(format!("{}_target.mdb", stem));
        let linker_path = dir.join(format!("{}_linker.mdb", stem));
        let _ = std::fs::remove_file(&target_path);
        let _ = std::fs::remove_file(&linker_path);

        {
            let mut target = Database::create(FileFormat::V2003, &target_path).unwrap();
            target
                .create_table(
                    &TableSpec::new(
                        "Source",
                        vec![
                            ColumnSpec::new("ID", DataType::Long),
                            ColumnSpec::new("Name", DataType::Text).with_length(50),
                            ColumnSpec::new("Num", DataType::Long),
                        ],
                    )
                    .with_index(IndexSpec::primary_key(
                        "PrimaryKey",
                        vec![("ID".to_string(), true)],
                    )),
                )
                .unwrap();
            for i in 1..=3i32 {
                target
                    .add_rows(
                        "Source",
                        &[vec![
                            Value::Long(i),
                            Value::Text(format!("name{}", i)),
                            Value::Long(i * 10),
                        ]],
                    )
                    .unwrap();
            }
            target.flush().unwrap();
        }

        {
            let mut linker = Database::create(FileFormat::V2003, &linker_path).unwrap();
            let linkee = target_path.to_string_lossy().into_owned();
            linker
                .create_linked_table("LinkSource", &linkee, "Source")
                .unwrap();
            linker
                .create_linked_table("LinkSourceCopy", &linkee, "Source")
                .unwrap();
            linker
                .create_table(&TableSpec::new(
                    "Copy",
                    vec![
                        ColumnSpec::new("ID", DataType::Long),
                        ColumnSpec::new("Name", DataType::Text).with_length(50),
                        ColumnSpec::new("Num", DataType::Long),
                    ],
                ))
                .unwrap();
            linker.flush().unwrap();
        }
        (target_path, linker_path)
    }

    /// `DatabaseImpl.createTableInfo` / `LinkedTableInfo`: the linked catalog
    /// fields are available right after creation and after a reopen.
    #[test]
    fn linked_table_metadata_is_available_after_create_and_reopen() {
        let (target_path, linker_path) = make_link_fixture("linkmeta");
        let linkee = target_path.to_string_lossy().into_owned();
        {
            let linker = Database::open_path(&linker_path).unwrap();
            // the in-memory metadata the creator inserted
            let info = linker.linked_table_info("LinkSource").unwrap().clone();
            assert_eq!(info.name, "LinkSource");
            assert_eq!(info.linked_db_name, linkee);
            assert_eq!(info.linked_table_name, "Source");
            assert!(info.object_id < 0, "linked tables have synthetic ids");
            assert!(info.is_linked());
            assert_eq!(
                info.object_id,
                linker.table_lookup_entry("LinkSource").unwrap().0
            );
            assert_eq!(
                linker.linked_table_names(),
                vec!["LinkSource".to_string(), "LinkSourceCopy".to_string()]
            );
        }

        // the type 6 branch of `indexCatalog` reads the same fields back
        let linker = Database::open_path(&linker_path).unwrap();
        for name in ["LinkSource", "linksourcecopy"] {
            assert!(linker.is_linked_table(name), "{} is linked", name);
            let info = linker.linked_table_info(name).unwrap();
            assert_eq!(info.linked_db_name, linkee);
            assert_eq!(info.linked_table_name, "Source");
        }
        assert!(!linker.is_linked_table("Copy"));
        assert!(!linker.is_linked_table("MSysObjects"));
        assert!(linker.get_linked_databases().is_empty());
    }

    /// `LinkResolver.DEFAULT`: every link resolution fails with the Java
    /// `AccessDeniedException` message naming the linkee file.
    #[test]
    fn default_link_resolver_denies_with_the_linkee_file_name() {
        let (target_path, linker_path) = make_link_fixture("linkdenied");
        let linkee = target_path.to_string_lossy().into_owned();
        let mut db = Database::open_path(&linker_path).unwrap();

        match db.get_table("LinkSource") {
            Err(Error::Jackcess(message)) => {
                assert_eq!(message, crate::linked::denied_resolution_message(&linkee));
                assert!(message.starts_with(&linkee));
                assert!(message.contains("Linked database resolution is disabled."));
                assert!(message.contains("LinkResolver.UNRESTRICTED"));
            }
            Ok(_) => panic!("the default resolver must deny linked tables"),
            Err(e) => panic!("unexpected error: {}", e),
        }
        assert!(matches!(
            db.find_table("LinkSource"),
            Err(Error::Jackcess(_))
        ));
        assert!(matches!(
            db.read_rows_with_ids("LinkSource"),
            Err(Error::Jackcess(_))
        ));
        // a failed resolution is not cached
        assert!(db.get_linked_databases().is_empty());

        // the default policy can be restored after a custom one was set
        db.reset_link_resolver();
        assert!(matches!(
            db.get_table("LinkSourceCopy"),
            Err(Error::Jackcess(_))
        ));
    }

    /// `LinkResolver.UNRESTRICTED`: opens the target and reads every row
    /// through both links; `getLinkedDatabases` caches the linkee, and
    /// `find_table` / `getTable(int)` / `get_table` all resolve the link.
    #[test]
    fn unrestricted_link_resolver_opens_and_reads_the_target() {
        let (target_path, linker_path) = make_link_fixture("linkread");
        let linkee = target_path.to_string_lossy().into_owned();
        let mut db = Database::open_path(&linker_path).unwrap();
        db.set_link_resolver(crate::linked::UNRESTRICTED);

        {
            let table = db.get_table("LinkSource").unwrap();
            assert_eq!(table.name, "Source");
            assert_eq!(table.column_index("ID"), Some(0));
            assert_eq!(table.column_index("Name"), Some(1));
            assert_eq!(table.column_index("Num"), Some(2));
        }
        // the linkee is fully functional for reads (its system catalog was
        // read by `Database::open_path`), and cached by catalog file name
        assert_eq!(db.get_linked_databases().len(), 1);
        assert!(db.get_linked_databases().contains_key(&linkee));
        assert!(db.get_linked_database(&linkee).is_some());

        let rows = db.read_rows_with_ids("LinkSource").unwrap();
        assert_eq!(rows.len(), 3);
        let values: Vec<Vec<Value>> = rows.into_iter().filter_map(|(_, v)| v).collect();
        for (i, row) in values.iter().enumerate() {
            let id = i as i32 + 1;
            assert_eq!(row[0], Value::Long(id));
            assert_eq!(row[1], Value::Text(format!("name{}", id)));
            assert_eq!(row[2], Value::Long(id * 10));
        }

        // both links and the local table resolve through the same APIs
        {
            let table = db.find_table("LinkSourceCopy").unwrap().unwrap();
            assert_eq!(table.name, "Source");
        }
        let synthetic = db.linked_table_info("LinkSource").unwrap().object_id;
        {
            let table = db.get_table_by_page(synthetic).unwrap();
            assert_eq!(table.name, "Source");
        }

        // the linkee database is reachable through the public API
        let linked = db.get_linked_database_mut(&linkee).unwrap();
        assert!(linked.table_names().contains(&"SOURCE".to_string()));
        let target_rows = linked.read_rows_with_ids("Source").unwrap();
        assert_eq!(target_rows.len(), 3);
    }

    /// Reading through links must not change either file on disk: the linker is
    /// only opened for reading and the linkee is only read.
    #[test]
    fn reading_through_links_does_not_write_the_linker() {
        let (target_path, linker_path) = make_link_fixture("linknowrite");
        let before_linker = std::fs::read(&linker_path).unwrap();
        let before_target = std::fs::read(&target_path).unwrap();
        {
            let mut db = Database::open_path(&linker_path).unwrap();
            db.set_link_resolver(crate::linked::UNRESTRICTED);
            assert_eq!(db.read_rows_with_ids("LinkSource").unwrap().len(), 3);
            assert_eq!(db.read_rows_with_ids("LinkSourceCopy").unwrap().len(), 3);
            let _ = db.get_table("LinkSource").unwrap();
            // drop the linker and its cached linkee without flushing
        }
        assert_eq!(
            std::fs::read(&linker_path).unwrap(),
            before_linker,
            "reading through links must not write the linker"
        );
        assert_eq!(
            std::fs::read(&target_path).unwrap(),
            before_target,
            "reading a linkee must not write it"
        );
    }

    /// `LinkResolver.UNRESTRICTED` opens the linkee read-only when the linker
    /// itself was opened read-only.
    #[test]
    fn unrestricted_resolver_opens_linkee_read_only_for_a_read_only_linker() {
        let (target_path, linker_path) = make_link_fixture("linkro");
        let linkee = target_path.to_string_lossy().into_owned();
        let mut db = Database::open_path_read_only(&linker_path).unwrap();
        assert!(db.is_read_only());
        db.set_link_resolver(crate::linked::UNRESTRICTED);
        assert_eq!(db.read_rows_with_ids("LinkSource").unwrap().len(), 3);
        let linked = db.get_linked_database(&linkee).unwrap();
        assert!(linked.is_read_only(), "the linkee inherits read-only mode");
    }

    // ------------------------------------------------------------------
    // linked-table writing (writes through a link land in the linkee)
    // ------------------------------------------------------------------

    /// The `ID` values of the live rows of a table, in physical row order.
    fn linked_live_ids(db: &mut Database, table_name: &str) -> Vec<i64> {
        db.read_rows_with_ids(table_name)
            .unwrap()
            .into_iter()
            .filter_map(|(_, values)| values)
            .filter_map(|values| values.first().and_then(|v| v.as_i64()))
            .collect()
    }

    /// The row id of the live row whose `ID` column holds `id`.
    fn linked_row_id(db: &mut Database, table_name: &str, id: i64) -> crate::table::RowId {
        db.read_rows_with_ids(table_name)
            .unwrap()
            .into_iter()
            .find_map(|(row_id, values)| {
                let values = values?;
                if values.first().and_then(|v| v.as_i64()) == Some(id) {
                    Some(row_id)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("no row with ID {} in {}", id, table_name))
    }

    /// Inserts, updates and deletes rows through a link: every change is made
    /// to the linkee file with the linkee's table definition (indexed key
    /// change included), the linker file is untouched, and reads through the
    /// reopened link (and from the linkee directly) see the changes.
    #[test]
    fn linked_table_row_writes_land_in_the_linkee_not_the_linker() {
        let (target_path, linker_path) = make_link_fixture("linkwrite");
        let linkee = target_path.to_string_lossy().into_owned();
        let before_linker = std::fs::read(&linker_path).unwrap();
        let before_target = std::fs::read(&target_path).unwrap();

        {
            let mut db = Database::open_path(&linker_path).unwrap();
            db.set_link_resolver(crate::linked::UNRESTRICTED);

            // insert through the link
            db.add_rows(
                "LinkSource",
                &[vec![
                    Value::Long(4),
                    Value::Text("name4".to_string()),
                    Value::Long(40),
                ]],
            )
            .unwrap();
            // update row 2 through the link, with an indexed key change
            let id2 = linked_row_id(&mut db, "LinkSource", 2);
            db.update_row(
                "LinkSource",
                id2,
                &[
                    Value::Long(12),
                    Value::Text("TWO".to_string()),
                    Value::Long(22),
                ],
            )
            .unwrap();
            // update a single column through the link
            let id1 = linked_row_id(&mut db, "LinkSource", 1);
            db.update_value("LinkSource", id1, "Num", Value::Long(11))
                .unwrap();
            // update from a map through the link (the returned row is the
            // linkee's row after the write)
            let id4 = linked_row_id(&mut db, "LinkSource", 4);
            let mut map = std::collections::BTreeMap::new();
            map.insert("Name".to_string(), Value::Text("name4b".to_string()));
            map.insert("Num".to_string(), Value::Long(44));
            let values = db.update_row_from_map("LinkSource", id4, &map).unwrap();
            assert_eq!(values[0], Value::Long(4));
            assert_eq!(values[1], Value::Text("name4b".to_string()));
            assert_eq!(values[2], Value::Long(44));
            // delete row 3 through the link
            let id3 = linked_row_id(&mut db, "LinkSource", 3);
            db.delete_row("LinkSource", id3).unwrap();

            // the cached linkee is the one the writes went to, and reading
            // through it sees every change
            assert_eq!(db.get_linked_databases().len(), 1);
            assert!(db.get_linked_databases().contains_key(&linkee));
            assert_eq!(linked_live_ids(&mut db, "LinkSource"), vec![1, 12, 4]);
            let rows = db.read_rows_with_ids("LinkSource").unwrap();
            let values: Vec<Vec<Value>> = rows.into_iter().filter_map(|(_, v)| v).collect();
            assert_eq!(
                values[0],
                vec![
                    Value::Long(1),
                    Value::Text("name1".to_string()),
                    Value::Long(11)
                ]
            );
            assert_eq!(
                values[1],
                vec![
                    Value::Long(12),
                    Value::Text("TWO".to_string()),
                    Value::Long(22)
                ]
            );
            assert_eq!(
                values[2],
                vec![
                    Value::Long(4),
                    Value::Text("name4b".to_string()),
                    Value::Long(44)
                ]
            );

            // the linker file is not written by linked-table writes (the
            // linkee is opened as the writer and was changed on disk already)
            assert_eq!(
                std::fs::read(&linker_path).unwrap(),
                before_linker,
                "writes through the link must not modify the linker"
            );
            db.flush().unwrap();
        }

        assert_eq!(
            std::fs::read(&linker_path).unwrap(),
            before_linker,
            "closing the linker must not undo or add linked writes"
        );
        assert_ne!(
            std::fs::read(&target_path).unwrap(),
            before_target,
            "the linkee file must hold the linked writes"
        );

        // reopening the linker sees the updated rows through the link
        {
            let mut db = Database::open_path(&linker_path).unwrap();
            db.set_link_resolver(crate::linked::UNRESTRICTED);
            assert!(db.get_linked_databases().is_empty());
            assert_eq!(linked_live_ids(&mut db, "LinkSource"), vec![1, 12, 4]);
            assert_eq!(db.get_linked_databases().len(), 1);
        }

        // and the linkee itself (opened directly) holds the same rows
        {
            let mut target = Database::open_path(&target_path).unwrap();
            assert_eq!(linked_live_ids(&mut target, "Source"), vec![1, 12, 4]);
            let info = target.linked_table_info("Source");
            assert!(info.is_none(), "the target has no links of its own");
        }
    }

    /// A primary key and a unique index of a linked table are enforced by the
    /// linkee's indexes: a duplicate insert/update through the link fails and
    /// leaves the linkee unchanged.
    #[test]
    fn linked_table_writes_enforce_linkee_constraints() {
        let (_target_path, linker_path) = make_link_fixture("linkfkw");
        {
            let mut db = Database::open_path(&linker_path).unwrap();
            db.set_link_resolver(crate::linked::UNRESTRICTED);

            // primary key duplicate on insert
            let err = db
                .add_rows(
                    "LinkSource",
                    &[vec![
                        Value::Long(1),
                        Value::Text("dupe".to_string()),
                        Value::Long(99),
                    ]],
                )
                .unwrap_err();
            assert!(
                matches!(err, Error::Jackcess(_)),
                "duplicate primary key through a link: {:?}",
                err
            );
            // primary key duplicate on update, through both update paths
            let id2 = linked_row_id(&mut db, "LinkSource", 2);
            let err = db
                .update_row(
                    "LinkSource",
                    id2,
                    &[
                        Value::Long(1),
                        Value::Text("dupe".to_string()),
                        Value::Long(99),
                    ],
                )
                .unwrap_err();
            assert!(matches!(err, Error::Jackcess(_)));
            let err = db
                .update_value("LinkSource", id2, "ID", Value::Long(1))
                .unwrap_err();
            assert!(matches!(err, Error::Jackcess(_)));

            // the rejected writes left the linkee untouched
            assert_eq!(linked_live_ids(&mut db, "LinkSource"), vec![1, 2, 3]);
            let rows = db.read_rows_with_ids("LinkSource").unwrap();
            let row2 = rows
                .into_iter()
                .filter_map(|(_, v)| v)
                .find(|values| values.first() == Some(&Value::Long(2)))
                .unwrap();
            assert_eq!(row2[1], Value::Text("name2".to_string()));
            assert_eq!(row2[2], Value::Long(20));
        }

        // a unique (non-primary) index is enforced in the linkee as well
        let dir = test_tmp_dir();
        let unique_target = dir.join("linkunique_target.mdb");
        let unique_linker = dir.join("linkunique_linker.mdb");
        let _ = std::fs::remove_file(&unique_target);
        let _ = std::fs::remove_file(&unique_linker);
        {
            let mut target = Database::create(FileFormat::V2003, &unique_target).unwrap();
            target
                .create_table(
                    &TableSpec::new(
                        "U",
                        vec![
                            ColumnSpec::new("ID", DataType::Long),
                            ColumnSpec::new("Name", DataType::Text).with_length(50),
                        ],
                    )
                    .with_index(
                        crate::tablecreator::IndexSpec::new(
                            "UniqueName",
                            vec![("Name".to_string(), true)],
                        )
                        .unique(),
                    ),
                )
                .unwrap();
            target
                .add_rows(
                    "U",
                    &[
                        vec![Value::Long(1), Value::Text("a".to_string())],
                        vec![Value::Long(2), Value::Text("b".to_string())],
                    ],
                )
                .unwrap();
            target.flush().unwrap();

            let mut linker = Database::create(FileFormat::V2003, &unique_linker).unwrap();
            linker
                .create_linked_table("LU", &unique_target.to_string_lossy(), "U")
                .unwrap();
            linker.flush().unwrap();
        }
        {
            let mut db = Database::open_path(&unique_linker).unwrap();
            db.set_link_resolver(crate::linked::UNRESTRICTED);
            let err = db
                .add_rows("LU", &[vec![Value::Long(3), Value::Text("a".to_string())]])
                .unwrap_err();
            assert!(matches!(err, Error::Jackcess(_)));
            assert_eq!(linked_live_ids(&mut db, "LU"), vec![1, 2]);
        }
    }

    /// Writes through a link are visible to a freshly reopened link, and the
    /// linkee's own table definition page/row ids are used (an update keeps
    /// the row's identity and a delete removes exactly the deleted row).
    #[test]
    fn linked_writes_are_visible_after_reopen() {
        let (target_path, linker_path) = make_link_fixture("linkreopen");
        {
            let mut db = Database::open_path(&linker_path).unwrap();
            db.set_link_resolver(crate::linked::UNRESTRICTED);
            db.add_rows(
                "LinkSource",
                &[vec![
                    Value::Long(9),
                    Value::Text("nine".to_string()),
                    Value::Long(90),
                ]],
            )
            .unwrap();
            let id1 = linked_row_id(&mut db, "LinkSource", 1);
            db.update_row(
                "LinkSource",
                id1,
                &[
                    Value::Long(1),
                    Value::Text("ONE".to_string()),
                    Value::Long(15),
                ],
            )
            .unwrap();
            let id3 = linked_row_id(&mut db, "LinkSource", 3);
            db.delete_row("LinkSource", id3).unwrap();
            db.flush().unwrap();
        }

        // reopen the linker and read through the link: the writes are there
        let mut db = Database::open_path(&linker_path).unwrap();
        db.set_link_resolver(crate::linked::UNRESTRICTED);
        assert_eq!(linked_live_ids(&mut db, "LinkSource"), vec![1, 2, 9]);
        let rows = db.read_rows_with_ids("LinkSource").unwrap();
        let values: Vec<Vec<Value>> = rows.into_iter().filter_map(|(_, v)| v).collect();
        assert_eq!(values[2][0], Value::Long(9));
        assert_eq!(values[2][1], Value::Text("nine".to_string()));
        assert_eq!(values[2][2], Value::Long(90));
        assert_eq!(values[0][1], Value::Text("ONE".to_string()));
        assert_eq!(values[0][2], Value::Long(15));
        // the linkee still has rows 1, 2 and 9 (row 3 is gone)
        let linked = db
            .get_linked_database_mut(&target_path.to_string_lossy())
            .unwrap();
        assert_eq!(linked_live_ids(linked, "Source"), vec![1, 2, 9]);
    }

    /// The linkee's column defaults and property driven validators apply to
    /// writes made through a link (the linkee's table is an ordinary table of
    /// the linkee database).
    #[test]
    fn linked_writes_use_the_linkees_defaults_and_validators() {
        let dir = test_tmp_dir();
        let target_path = dir.join("linkdefaults_target.mdb");
        let linker_path = dir.join("linkdefaults_linker.mdb");
        let _ = std::fs::remove_file(&target_path);
        let _ = std::fs::remove_file(&linker_path);
        {
            let mut target = Database::create(FileFormat::V2003, &target_path).unwrap();
            let spec = TableSpec::new(
                "D",
                vec![
                    ColumnSpec::new("ID", DataType::Long),
                    ColumnSpec::new("Num", DataType::Long)
                        .put_property(
                            crate::property::DEFAULT_VALUE_PROP,
                            Value::Text("7".to_string()),
                        )
                        .unwrap(),
                    ColumnSpec::new("Name", DataType::Text)
                        .with_length(10)
                        .put_property(crate::property::REQUIRED_PROP, Value::Bool(true))
                        .unwrap(),
                ],
            )
            .with_index(IndexSpec::primary_key(
                "PrimaryKey",
                vec![("ID".to_string(), true)],
            ));
            target.create_table(&spec).unwrap();
            target.flush().unwrap();

            let mut linker = Database::create(FileFormat::V2003, &linker_path).unwrap();
            linker
                .create_linked_table("LD", &target_path.to_string_lossy(), "D")
                .unwrap();
            linker.flush().unwrap();
        }

        let mut db = Database::open_path(&linker_path).unwrap();
        db.set_link_resolver(crate::linked::UNRESTRICTED);
        // the linkee's default fills the omitted Num
        db.add_rows(
            "LD",
            &[vec![
                Value::Long(1),
                Value::Null,
                Value::Text("ok".to_string()),
            ]],
        )
        .unwrap();
        let rows = db.read_rows_with_ids("LD").unwrap();
        let first = rows
            .into_iter()
            .find_map(|(_, v)| v)
            .expect("the row inserted through the link");
        assert_eq!(first[0], Value::Long(1));
        assert_eq!(first[1], Value::Long(7), "the linkee's default applies");
        assert_eq!(first[2], Value::Text("ok".to_string()));

        // the linkee's required validator rejects a null written through the
        // link (on insert and on update)
        assert!(
            db.add_rows("LD", &[vec![Value::Long(2), Value::Null, Value::Null]],)
                .is_err(),
            "the linkee's Required validator must reject the row"
        );
        let id1 = linked_row_id(&mut db, "LD", 1);
        assert!(
            db.update_value("LD", id1, "Name", Value::Null).is_err(),
            "the linkee's Required validator must reject the update"
        );
        assert_eq!(linked_live_ids(&mut db, "LD"), vec![1]);
    }

    /// Schema writes on a linked table name change the linkee's table
    /// definition (and its indexes), not the linker.
    #[test]
    fn linked_schema_writes_change_the_linkee() {
        let (target_path, linker_path) = make_link_fixture("linkschema");
        let linkee = target_path.to_string_lossy().into_owned();
        let before_linker = std::fs::read(&linker_path).unwrap();
        {
            let mut db = Database::open_path(&linker_path).unwrap();
            db.set_link_resolver(crate::linked::UNRESTRICTED);
            db.add_column(
                "LinkSource",
                ColumnSpec::new("Extra", DataType::Text).with_length(20),
            )
            .unwrap();
            db.add_index(
                "LinkSource",
                crate::tablecreator::IndexSpec::new("NumIdx", vec![("Num".to_string(), true)]),
            )
            .unwrap();
            db.set_table_allow_auto_number_insert("LinkSource", Some(true))
                .unwrap();
            // the linkee table the link resolves to carries the new column
            // and index
            let table = db.get_table("LinkSource").unwrap();
            assert!(table.column_index("Extra").is_some());
            assert!(table.indexes.iter().any(|idx| idx.name == "NumIdx"));
            assert!(db.get_linked_database(&linkee).is_some());
        }
        assert_eq!(
            std::fs::read(&linker_path).unwrap(),
            before_linker,
            "schema writes through a link must not modify the linker"
        );
        // reopening the linkee directly shows the schema changes there
        let mut target = Database::open_path(&target_path).unwrap();
        let table = target.get_table("Source").unwrap();
        assert!(table.column_index("Extra").is_some());
        assert!(table.indexes.iter().any(|idx| idx.name == "NumIdx"));
    }

    /// A linked write through a read-only linker fails (the linkee is opened
    /// read-only by `UNRESTRICTED`, like Java) and changes neither file.
    #[test]
    fn linked_writes_are_rejected_for_a_read_only_linker() {
        let (target_path, linker_path) = make_link_fixture("linkrowrite");
        let before_target = std::fs::read(&target_path).unwrap();
        let before_linker = std::fs::read(&linker_path).unwrap();
        {
            let mut db = Database::open_path_read_only(&linker_path).unwrap();
            db.set_link_resolver(crate::linked::UNRESTRICTED);
            // reading works
            assert_eq!(linked_live_ids(&mut db, "LinkSource"), vec![1, 2, 3]);
            let err = db
                .add_rows(
                    "LinkSource",
                    &[vec![
                        Value::Long(4),
                        Value::Text("four".to_string()),
                        Value::Long(40),
                    ]],
                )
                .unwrap_err();
            // the batch add wraps the channel's read-only write failure in
            // `TableImpl.addRows`' `BatchUpdateException` (update count 0,
            // because a write failure leaves the row status unknown)
            assert_eq!(err.update_count(), Some(0), "unexpected error: {err:?}");
            assert!(
                err.to_string().contains("Failed adding rows (Db="),
                "unexpected error: {err:?}"
            );
            let cause = err.batch_cause().expect("the original failure is kept");
            assert!(
                cause.to_string().contains("NonWritableChannelException"),
                "unexpected cause: {cause:?}"
            );
        }
        assert_eq!(std::fs::read(&target_path).unwrap(), before_target);
        assert_eq!(std::fs::read(&linker_path).unwrap(), before_linker);
    }

    // ------------------------------------------------------------------
    // explicit auto-number insertion
    // ------------------------------------------------------------------

    /// The `ID` values of a table in row order.
    fn row_ids(db: &mut Database, table_name: &str) -> Vec<i32> {
        let id_column = db
            .get_table(table_name)
            .unwrap()
            .column_index("ID")
            .unwrap();
        db.read_rows_with_ids(table_name)
            .unwrap()
            .into_iter()
            .filter_map(|(_, values)| values)
            .filter_map(|values| match values.get(id_column) {
                Some(Value::Long(id)) => Some(*id),
                _ => None,
            })
            .collect()
    }

    fn add_named_row(db: &mut Database, id: Option<i32>, name: &str) {
        db.add_rows(
            "T",
            &[vec![
                id.map(Value::Long).unwrap_or(Value::Null),
                Value::Text(name.to_string()),
            ]],
        )
        .unwrap();
    }

    /// `TableImpl.getInputAutoNumberRowValue` + `LongAutoNumberGenerator
    /// .handleInsert` / `adjustLongAutoNumber`: an explicit auto number is
    /// used when allowed, the saved high water mark is the highest value used,
    /// and generated values continue after it.
    #[test]
    fn explicit_auto_number_inserts_adjust_the_high_water_mark() {
        let path = test_tmp_dir().join("autonum.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(50),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).unwrap();

        // the database policy is off by default: an explicit value is ignored
        add_named_row(&mut db, Some(50), "ignored");
        assert_eq!(row_ids(&mut db, "T"), vec![1]);

        // enable explicit insertion
        db.set_allow_auto_number_insert(true);
        assert!(db.is_allow_auto_number_insert());
        for id in [5i32, 100, 3, 7] {
            add_named_row(&mut db, Some(id), &format!("explicit{}", id));
        }
        assert_eq!(row_ids(&mut db, "T"), vec![1, 5, 100, 3, 7]);
        assert_eq!(db.get_table("T").unwrap().last_long_auto_number, 100);

        // generated values continue after the highest value used
        add_named_row(&mut db, None, "auto");
        assert_eq!(row_ids(&mut db, "T"), vec![1, 5, 100, 3, 7, 101]);

        // deleting a row does not move the high water mark down
        let row_id = {
            let (row_id, _) = db
                .read_rows_with_ids("T")
                .unwrap()
                .into_iter()
                .find(|(_, values)| {
                    values
                        .as_ref()
                        .and_then(|v| v.first())
                        .and_then(|v| v.as_i64())
                        == Some(101)
                })
                .expect("the row with id 101");
            row_id
        };
        db.delete_row("T", row_id).unwrap();
        add_named_row(&mut db, None, "after");
        assert_eq!(db.get_table("T").unwrap().last_long_auto_number, 102);

        // the table override wins over the database policy, in both directions
        db.set_allow_auto_number_insert(false);
        db.set_table_allow_auto_number_insert("T", Some(true))
            .unwrap();
        add_named_row(&mut db, Some(200), "table override on");
        assert_eq!(db.get_table("T").unwrap().last_long_auto_number, 200);

        db.set_allow_auto_number_insert(true);
        db.set_table_allow_auto_number_insert("T", Some(false))
            .unwrap();
        add_named_row(&mut db, Some(300), "table override off");
        // 300 was ignored, so the generated value continued from 200
        assert_eq!(db.get_table("T").unwrap().last_long_auto_number, 201);

        // removing the override restores the database policy
        db.set_table_allow_auto_number_insert("T", None).unwrap();
        add_named_row(&mut db, Some(400), "database policy on");
        assert_eq!(db.get_table("T").unwrap().last_long_auto_number, 400);

        // an update with an explicit value goes through the same bookkeeping
        // (`TableImpl.handleAutoNumbersForUpdate`)
        let (row_id, mut values) = db
            .read_rows_with_ids("T")
            .unwrap()
            .into_iter()
            .find(|(_, values)| {
                values
                    .as_ref()
                    .and_then(|v| v.first())
                    .and_then(|v| v.as_i64())
                    == Some(5)
            })
            .expect("the row with id 5");
        values.as_mut().unwrap()[0] = Value::Long(500);
        db.update_row("T", row_id, &values.unwrap()).unwrap();
        assert_eq!(db.get_table("T").unwrap().last_long_auto_number, 500);

        // the saved value is in the table definition for the next open
        db.flush().unwrap();
        drop(db);
        let mut db = Database::open_path(&path).unwrap();
        assert_eq!(db.get_table("T").unwrap().last_long_auto_number, 500);
    }

    /// `evaluateExpressions=false` must gate exactly what Java gates
    /// (`ColumnImpl.reloadPropertiesValidators`, `TableImpl.reloadRowValidator`,
    /// `TableImpl.CalcColEvaluator.add`): the default values and the calculated
    /// column values are not produced, while the `Required` and
    /// `AllowZeroLength` validators (installed before the flag is checked) still
    /// reject bad rows.
    #[test]
    fn evaluate_expressions_gates_defaults_and_calculated_columns() {
        let path = test_tmp_dir().join("db_eval_expressions.accdb");
        let _ = std::fs::remove_file(&path);

        let mut db = Database::create(FileFormat::V2016, &path).unwrap();
        // jackcess' default is on (`getDefaultEvaluateExpressions`)
        assert!(db.is_evaluate_expressions());
        db.set_evaluate_expressions(false);
        assert!(!db.is_evaluate_expressions());

        let spec = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("L", DataType::Long)
                    .put_property(DEFAULT_VALUE_PROP, Value::Text("7".to_string()))
                    .unwrap(),
                ColumnSpec::new("Req", DataType::Long)
                    .put_property(crate::property::REQUIRED_PROP, Value::Bool(true))
                    .unwrap(),
                ColumnSpec::new("T", DataType::Text)
                    .with_length(20)
                    .put_property(DEFAULT_VALUE_PROP, Value::Text("\"abc\"".to_string()))
                    .unwrap()
                    .put_property(crate::property::ALLOW_ZERO_LEN_PROP, Value::Bool(false))
                    .unwrap(),
                ColumnSpec::new("C", DataType::Long).set_calculated_info("[L]*2"),
            ],
        );
        db.create_table(&spec).unwrap();

        // row 1: only the required column, so a default-enabled database would
        // fill L, T and C; with the policy off they stay null
        db.add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Null,
                Value::Long(1),
                Value::Null,
                Value::Null,
            ]],
        )
        .unwrap();
        // row 2: an explicitly supplied calculated value is written as given
        // (Java stores it inside the calculated value wrapper, without
        // evaluating the expression)
        db.add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Long(5),
                Value::Long(2),
                Value::Text("q".to_string()),
                Value::Long(99),
            ]],
        )
        .unwrap();

        let rows: Vec<(i32, Vec<Value>)> = db
            .read_rows_with_ids("Test")
            .unwrap()
            .into_iter()
            .map(|(_, values)| {
                let values = values.expect("row values");
                let id = values[0].as_i64().expect("row id") as i32;
                (id, values)
            })
            .collect();
        let row1 = &rows.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert_eq!(row1[1], Value::Null, "DefaultValue must not be applied");
        assert_eq!(row1[3], Value::Null, "DefaultValue must not be applied");
        assert_eq!(
            row1[4],
            Value::Null,
            "calculated column must not be evaluated"
        );
        let row2 = &rows.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(row2[1], Value::Long(5));
        assert_eq!(
            row2[4],
            Value::Long(99),
            "an explicit calculated value is stored as given"
        );

        // `Required` and `AllowZeroLength` are not gated by the policy
        let err = db
            .add_rows(
                "Test",
                &[vec![
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Text("bad".to_string()),
                    Value::Null,
                ]],
            )
            .expect_err("a null required value must still be rejected");
        assert!(
            err.to_string()
                .contains("Missing value for required column"),
            "unexpected error: {}",
            err
        );
        let err = db
            .add_rows(
                "Test",
                &[vec![
                    Value::Null,
                    Value::Null,
                    Value::Long(99),
                    Value::Text(String::new()),
                    Value::Null,
                ]],
            )
            .expect_err("an empty string must still be rejected");
        assert!(
            err.to_string()
                .contains("Zero length string is not allowed"),
            "unexpected error: {}",
            err
        );
        assert_eq!(
            rows.len(),
            db.read_rows_with_ids("Test").unwrap().len(),
            "the rejected inserts must not add a row"
        );

        // the flag is a runtime setting: a reopened database starts from the
        // default (`true`), so the same table now applies the default value and
        // evaluates the calculated column
        db.flush().unwrap();
        drop(db);
        let mut db = Database::open_path(&path).unwrap();
        assert!(db.is_evaluate_expressions());
        db.add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Null,
                Value::Long(3),
                Value::Null,
                Value::Null,
            ]],
        )
        .unwrap();
        let rows = db.read_rows_with_ids("Test").unwrap();
        let row3 = rows
            .iter()
            .find_map(|(_, values)| values.as_ref().filter(|v| v[0] == Value::Long(3)).cloned())
            .expect("row 3");
        assert_eq!(row3[1], Value::Long(7), "the default value is applied");
        assert_eq!(
            row3[3],
            Value::Text("abc".to_string()),
            "the default value is applied"
        );
        assert_eq!(
            row3[4],
            Value::Long(14),
            "the calculated column is evaluated"
        );
    }

    /// The database time zone drives the system catalog timestamps and the
    /// conversion of `java.util.Date` style epoch values, like
    /// `DatabaseImpl.getZoneId()` / `ColumnImpl.toDateDouble(Date)` do.
    #[test]
    fn time_zone_offset_controls_catalog_and_date_conversions() {
        let path = test_tmp_dir().join("db_time_zone.accdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2016, &path).unwrap();

        // `TimeZone.getTimeZone("UTC")`
        db.set_time_zone_offset_minutes(0);
        assert_eq!(db.time_zone_offset_minutes(), 0);
        assert_eq!(db.get_time_zone().get_raw_offset(), 0);
        let instant = 1_584_246_645_000i64; // 2020-03-15T10:30:45Z
        db.now_ms = Some(instant);
        assert_eq!(
            db.now_local_datetime(),
            crate::datenums::LocalDateTime::from_epoch_millis(instant)
        );

        // `TimeZone.getTimeZone("GMT-05:00")`: no DST, so the same offset for
        // every instant
        db.set_time_zone_offset_minutes(-300);
        assert_eq!(db.time_zone_offset_minutes(), -300);
        let minus_five = crate::datenums::TimeZone::from_offset_minutes(-300);
        assert_eq!(minus_five.get_offset(0), -5 * 3_600_000);
        assert_eq!(minus_five.get_offset(instant), -5 * 3_600_000);
        assert_eq!(
            db.local_date_time_from_millis(instant),
            crate::datenums::LocalDateTime::from_epoch_millis(instant - 5 * 3_600_000)
        );
        db.now_ms = Some(instant);
        assert_eq!(
            db.now_local_datetime(),
            crate::datenums::LocalDateTime::from_epoch_millis(instant - 5 * 3_600_000)
        );

        // a numeric (java.util.Date style) date value written through a column
        // is converted with the database zone
        let bits = crate::column::date_value_bits(&Value::BigInt(instant), &db.tz).unwrap();
        let expected = crate::datenums::to_date_double(
            crate::datenums::LocalDateTime::from_epoch_millis(instant - 5 * 3_600_000),
        );
        assert_eq!(bits, expected.to_bits());

        // the catalog row of the new table stores its DateCreate/DateUpdate in
        // the configured zone
        let spec = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("D", DataType::ShortDateTime),
            ],
        );
        db.create_table(&spec).unwrap();
        db.flush().unwrap();

        let (idx_name, idx_date) = {
            let table = db.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
            (
                table.column_index(CAT_COL_NAME).unwrap(),
                table.column_index(CAT_COL_DATE_CREATE).unwrap(),
            )
        };
        let expected = Value::DateTime(crate::datenums::LocalDateTime::from_epoch_millis(
            instant - 5 * 3_600_000,
        ));
        let mut found = false;
        for (_, values) in db.read_rows_with_ids(TABLE_SYSTEM_CATALOG).unwrap() {
            let values = match values {
                Some(values) => values,
                None => continue,
            };
            if values.get(idx_name) == Some(&Value::Text("Test".to_string())) {
                assert_eq!(values[idx_date], expected, "catalog DateCreate");
                found = true;
            }
        }
        assert!(found, "the Test catalog row was not found");
    }

    /// `Database.setZoneId(ZoneId)` switches the database to a named IANA zone:
    /// the id round trips, the derived `TimeZone` (raw offset) comes with it,
    /// and the catalog `DateCreate`/`DateUpdate` of a new table are the
    /// `LocalDateTime` of the instant in that zone (a DST rule included).
    #[test]
    fn named_zone_drives_the_catalog_dates() {
        let path = test_tmp_dir().join("db_named_zone.accdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2016, &path).unwrap();
        db.set_zone_id("America/New_York").unwrap();
        assert_eq!(db.get_zone_id(), "America/New_York");
        assert_eq!(db.time_zone_offset_minutes(), -300);
        assert!(db.get_time_zone().is_named());
        // an unknown id is rejected, leaving the zone unchanged
        assert!(db.set_zone_id("Nowhere/Special").is_err());
        assert_eq!(db.get_zone_id(), "America/New_York");

        // 2020-01-15T12:00:00Z is EST (-5), 2020-07-15T12:00:00Z is EDT (-4)
        let winter = 1_579_089_600_000i64;
        let summer = 1_594_814_400_000i64;
        db.now_ms = Some(winter);
        assert_eq!(
            db.now_local_datetime(),
            crate::datenums::LocalDateTime::new(2020, 1, 15, 7, 0, 0, 0)
        );
        db.now_ms = Some(summer);
        assert_eq!(
            db.now_local_datetime(),
            crate::datenums::LocalDateTime::new(2020, 7, 15, 8, 0, 0, 0)
        );

        // the catalog dates of a table created at the summer instant use the
        // zone's wall clock time (EDT)
        db.now_ms = Some(summer);
        let spec = TableSpec::new("Zoned", vec![ColumnSpec::new("ID", DataType::Long)]);
        db.create_table(&spec).unwrap();
        db.flush().unwrap();
        let meta = db.get_table_meta_data("Zoned").unwrap().unwrap();
        assert_eq!(
            meta.created_date,
            Some(crate::datenums::LocalDateTime::new(2020, 7, 15, 8, 0, 0, 0))
        );
        assert_eq!(meta.updated_date, meta.created_date);

        // the fixed offset API still works and swaps the zone back
        db.set_time_zone_offset_minutes(330);
        assert_eq!(db.get_zone_id(), "GMT+05:30");
        assert!(!db.get_time_zone().is_named());
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `Database::set_time_zone_id` / `get_time_zone_id` are the explicit
    /// aliases of `setZoneId` / `getZoneId`: they accept the same IANA and
    /// fixed offset ids and drive the same date conversions.
    #[test]
    fn set_time_zone_id_switches_named_and_fixed_zones() {
        let path = test_tmp_dir().join("db_tz_id_api.accdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();

        // a named zone with daylight saving time and a historical LMT offset
        db.set_time_zone_id("Europe/Berlin").unwrap();
        assert_eq!(db.get_time_zone_id(), "Europe/Berlin");
        assert!(db.get_time_zone().is_named());
        assert_eq!(db.time_zone_offset_minutes(), 60);
        // 2020-07-01T12:00:00Z is CEST (+2), 2020-01-01T12:00:00Z is CET (+1)
        assert_eq!(
            db.get_time_zone().get_offset(1_593_604_800_000),
            2 * 3_600_000
        );
        assert_eq!(db.get_time_zone().get_offset(1_577_880_000_000), 3_600_000);
        // the LMT offset before the 1893 transition (ZoneId face)
        assert_eq!(
            db.get_time_zone().zone_offset(-3_000_000_000_000),
            3_208_000
        );

        // the fixed offset form still behaves exactly like
        // `TimeZone.getTimeZone("GMT-05:00")`: no DST, same id, same offset
        db.set_time_zone_id("GMT-05:00").unwrap();
        assert_eq!(db.get_time_zone_id(), "GMT-05:00");
        assert!(!db.get_time_zone().is_named());
        assert_eq!(db.time_zone_offset_minutes(), -300);
        assert_eq!(
            db.get_time_zone().get_offset(1_593_604_800_000),
            -5 * 3_600_000
        );
        assert_eq!(
            db.get_time_zone().get_offset(1_577_880_000_000),
            -5 * 3_600_000
        );

        // and so does the minutes API
        db.set_time_zone_offset_minutes(330);
        assert_eq!(db.get_time_zone_id(), "GMT+05:30");
        assert_eq!(db.time_zone_offset_minutes(), 330);

        // invalid ids are rejected and leave the zone untouched
        assert!(db.set_time_zone_id("Nowhere/Special").is_err());
        assert_eq!(db.get_time_zone_id(), "GMT+05:30");

        db.set_time_zone_id("UTC").unwrap();
        assert_eq!(db.get_time_zone_id(), "UTC");
        assert_eq!(db.time_zone_offset_minutes(), 0);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `DateTimeType`: the default is `LOCAL_DATE_TIME`, `setDateTimeType`
    /// switches the conversion of an already loaded table too (Java's columns
    /// ask the database for their `DateTimeFactory` on every conversion).
    #[test]
    fn date_time_type_switches_loaded_tables() {
        let path = test_tmp_dir().join("db_order_type.accdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        assert_eq!(db.get_column_order(), ColumnOrder::Data);
        assert_eq!(db.get_date_time_type(), DateTimeType::LocalDateTime);
        assert_eq!(
            db.get_date_time_type(),
            crate::database::DEFAULT_DATE_TIME_TYPE
        );

        let spec = crate::tablecreator::TableSpec::new(
            "Test",
            vec![
                crate::column::ColumnSpec::new("ID", crate::datatype::DataType::Long),
                crate::column::ColumnSpec::new("D", crate::datatype::DataType::ShortDateTime),
            ],
        );
        let page = db.create_table(&spec).unwrap();
        // every table a database hands out carries the current type
        {
            let table = db.get_table_by_page(page).unwrap();
            assert_eq!(table.get_date_time_type(), DateTimeType::LocalDateTime);
            assert_eq!(table.get_column_order(), ColumnOrder::Data);
        }

        db.set_date_time_type(DateTimeType::Date);
        assert_eq!(db.get_date_time_type(), DateTimeType::Date);
        // the loaded table follows the database (Java reads it from the
        // database each time a value is converted)
        assert_eq!(
            db.get_table_by_page(page).unwrap().get_date_time_type(),
            DateTimeType::Date
        );

        db.set_date_time_type(DateTimeType::LocalDateTime);
        assert_eq!(
            db.get_table_by_page(page).unwrap().get_date_time_type(),
            DateTimeType::LocalDateTime
        );
    }

    /// `Database.setColumnOrder` only affects the tables loaded afterwards
    /// (`TableImpl` sorts its column list while it is constructed), and the
    /// order is a view: the row value array stays in data order.
    #[test]
    fn column_order_is_a_view_of_the_columns() {
        let path = test_tmp_dir().join("db_column_order.accdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let spec = crate::tablecreator::TableSpec::new(
            "Test",
            vec![
                crate::column::ColumnSpec::new("A", crate::datatype::DataType::Long),
                crate::column::ColumnSpec::new("B", crate::datatype::DataType::Long),
                crate::column::ColumnSpec::new("C", crate::datatype::DataType::Long),
            ],
        );
        let page = db.create_table(&spec).unwrap();

        // DATA order: the table definition order (the row array order)
        {
            let table = db.get_table_by_page(page).unwrap();
            let names: Vec<String> = table.get_columns().iter().map(|c| c.name.clone()).collect();
            assert_eq!(names, vec!["A", "B", "C"]);
            assert_eq!(
                table
                    .get_columns()
                    .iter()
                    .map(|c| c.column_index)
                    .collect::<Vec<_>>(),
                vec![0, 1, 2]
            );
        }

        // a table loaded while DISPLAY is configured sorts by display index.
        // jackcess 5.0.4 assigns the display index from the load order, so the
        // two orders coincide; reverse the display indexes to make the view
        // observable (the row array order must not change)
        db.set_column_order(ColumnOrder::Display);
        assert_eq!(db.get_column_order(), ColumnOrder::Display);
        {
            // the order applies while a table is *loaded* (Java's TableImpl
            // sorts in its constructor), so drop the cached table to load it
            // again with the new order
            db.tables.remove(&page);
            let table = db.get_table_by_page(page).unwrap();
            assert_eq!(table.get_column_order(), ColumnOrder::Display);
            let count = table.columns.len();
            for (index, column) in table.columns.iter_mut().enumerate() {
                column.display_index = count - 1 - index;
            }
            let names: Vec<String> = table.get_columns().iter().map(|c| c.name.clone()).collect();
            assert_eq!(names, vec!["C", "B", "A"]);
            // the view is only an ordering: the data order columns still carry
            // their row array index
            let names: Vec<String> = table
                .get_columns_in_data_order()
                .iter()
                .map(|c| c.name.clone())
                .collect();
            assert_eq!(names, vec!["A", "B", "C"]);
            assert_eq!(
                table
                    .get_columns_in_data_order()
                    .iter()
                    .map(|c| c.column_index)
                    .collect::<Vec<_>>(),
                vec![0, 1, 2]
            );
        }

        // an already loaded table keeps the order it was loaded with
        db.reset_column_order();
        assert_eq!(
            db.get_table_by_page(page).unwrap().get_column_order(),
            ColumnOrder::Display
        );
        assert_eq!(db.get_column_order(), ColumnOrder::Data);
    }

    /// Builds a full row value list for a system table from `(column, value)`
    /// pairs (the column order of a system table is not part of the public API).
    fn system_value_row(
        db: &mut Database,
        table_name: &str,
        values: &[(&str, Value)],
    ) -> Vec<Value> {
        let page = db
            .table_lookup
            .get(&table_name.to_uppercase())
            .expect("system table lookup")
            .0;
        db.load_table_by_page(page).expect("load system table");
        let table = db.tables.get(&page).unwrap();
        let mut row = vec![Value::Null; table.columns.len()];
        for (name, value) in values {
            let idx = table
                .column_index(name)
                .unwrap_or_else(|| panic!("{} has no column {}", table_name, name));
            row[idx] = value.clone();
        }
        row
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "test helper taking one argument per MSysQueries column, matching the SQL row it builds"
    )]
    fn add_missing_query_row(
        db: &mut Database,
        object_id: i32,
        attribute: u8,
        order: i32,
        expression: Option<&str>,
        flag: Option<i16>,
        extra: Option<i32>,
        name1: Option<&str>,
        name2: Option<&str>,
    ) {
        let row = system_value_row(
            db,
            TABLE_SYSTEM_QUERIES,
            &[
                ("ObjectId", Value::Long(object_id)),
                ("Attribute", Value::Byte(attribute)),
                ("Order", Value::Bytes(order.to_be_bytes().to_vec())),
                (
                    "Expression",
                    expression
                        .map(|s| Value::Text(s.to_string()))
                        .unwrap_or(Value::Null),
                ),
                ("Flag", flag.map(Value::Int).unwrap_or(Value::Null)),
                ("LvExtra", extra.map(Value::Long).unwrap_or(Value::Null)),
                (
                    "Name1",
                    name1
                        .map(|s| Value::Text(s.to_string()))
                        .unwrap_or(Value::Null),
                ),
                (
                    "Name2",
                    name2
                        .map(|s| Value::Text(s.to_string()))
                        .unwrap_or(Value::Null),
                ),
            ],
        );
        db.add_system_row(TABLE_SYSTEM_QUERIES, row)
            .expect("add MSysQueries row");
    }

    fn add_query_catalog_row(db: &mut Database, object_id: i32, name: &str, flags: Option<i32>) {
        let parent = db.table_parent_id;
        let row = system_value_row(
            db,
            TABLE_SYSTEM_CATALOG,
            &[
                ("Id", Value::Long(object_id)),
                ("ParentId", Value::Long(parent)),
                ("Name", Value::Text(name.to_string())),
                ("Type", Value::Int(TYPE_QUERY)),
                ("Flags", flags.map(Value::Long).unwrap_or(Value::Null)),
            ],
        );
        db.add_system_row(TABLE_SYSTEM_CATALOG, row)
            .expect("add MSysObjects row");
    }

    /// `Database.getQueries`: the catalog rows (`Type == 5`) are grouped with
    /// their `MSysQueries` rows (`ObjectId`), rendered through the query model,
    /// and rows without a catalog entry (or without flags) are skipped.
    #[test]
    fn query_grouping_and_lookup() {
        let path = test_tmp_dir().join("db_queries.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();

        // q1: a select with two columns, a table, a parameter and a where row
        add_missing_query_row(&mut db, 9001, 1, 1, None, Some(1), None, None, None);
        add_missing_query_row(
            &mut db,
            9001,
            2,
            1,
            None,
            Some(10),
            Some(50),
            Some("P"),
            None,
        );
        add_missing_query_row(&mut db, 9001, 6, 1, Some("T.A"), None, None, None, None);
        add_missing_query_row(
            &mut db,
            9001,
            6,
            2,
            Some("T.B"),
            None,
            None,
            Some("Alias"),
            None,
        );
        add_missing_query_row(&mut db, 9001, 5, 1, None, None, None, Some("T"), None);
        add_missing_query_row(&mut db, 9001, 8, 1, Some("T.A>0"), None, None, None, None);
        add_query_catalog_row(&mut db, 9001, "qryOne", Some(0));

        // q2: a system query (the catalog flags mark a system object)
        add_missing_query_row(&mut db, 9002, 1, 1, None, Some(1), None, None, None);
        add_missing_query_row(&mut db, 9002, 3, 1, None, Some(1), None, None, None);
        add_missing_query_row(&mut db, 9002, 5, 1, None, None, None, Some("T"), None);
        add_query_catalog_row(&mut db, 9002, "qrySystem", Some(SYSTEM_OBJECT_FLAG));

        // an orphan MSysQueries row and a catalog row without flags
        add_missing_query_row(&mut db, 9999, 1, 1, None, Some(1), None, None, None);
        add_missing_query_row(&mut db, 9003, 1, 1, None, Some(1), None, None, None);
        add_query_catalog_row(&mut db, 9003, "qryNoFlags", None);

        let queries = db.get_queries().expect("get queries");
        assert_eq!(queries.len(), 2, "orphan and flagless rows are skipped");
        assert_eq!(queries[0].name(), "qryOne");
        assert_eq!(queries[0].query_type(), crate::query::QueryType::Select);
        assert_eq!(queries[0].object_id(), 9001);
        assert_eq!(
            queries[0].get_parameters().unwrap(),
            vec!["P Text(50)".to_string()]
        );
        assert_eq!(
            queries[0].to_sql_string().unwrap(),
            format!(
                "PARAMETERS P Text(50);{nl}SELECT T.A, T.B AS Alias{nl}FROM T{nl}WHERE T.A>0;",
                nl = crate::query::NEWLINE
            )
        );
        assert_eq!(queries[1].name(), "qrySystem");
        assert_eq!(
            queries[1].to_sql_string().unwrap(),
            format!("SELECT *{nl}FROM T;", nl = crate::query::NEWLINE)
        );

        // name lookup and parameters
        let query = db.get_query("qryone").unwrap().expect("case-insensitive");
        assert_eq!(query.object_id(), 9001);
        assert_eq!(
            db.get_query_parameters("QRYONE").unwrap(),
            Some(vec!["P Text(50)".to_string()])
        );
        assert!(db.get_query("missing").unwrap().is_none());

        // system/hidden split
        let system = db.get_system_queries().unwrap();
        assert_eq!(
            system.iter().map(|q| q.name()).collect::<Vec<_>>(),
            vec!["qrySystem"]
        );
        assert!(db.get_hidden_queries().unwrap().is_empty());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `Database.getTableMetaData`/`getTableNames`/`getSystemTableNames` for a
    /// normal, a type 6 (linked) and a crafted type 4 (linked ODBC) table.  A
    /// type 4 table opens as a definition only: its metadata works but any row
    /// access reports Java's "TableDefinition has no data access".
    #[test]
    fn table_metadata_linked_and_odbc() {
        let path = test_tmp_dir().join("db_meta.mdb");
        let _ = std::fs::remove_file(&path);
        let page_o;
        {
            let mut db = Database::create(FileFormat::V2003, &path).unwrap();
            let table_spec = TableSpec::new(
                "T",
                vec![
                    ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                    ColumnSpec::new("Name", DataType::Text).with_length(10),
                ],
            )
            .with_index(IndexSpec::primary_key(
                "PrimaryKey",
                vec![("ID".to_string(), true)],
            ));
            db.create_table(&table_spec).unwrap();

            let odbc_spec = TableSpec::new(
                "O",
                vec![
                    ColumnSpec::new("ID", DataType::Long),
                    ColumnSpec::new("Label", DataType::Text).with_length(10),
                ],
            );
            page_o = db.create_table(&odbc_spec).unwrap();
            db.add_rows("O", &[vec![Value::Long(1), Value::Text("row".to_string())]])
                .unwrap();

            db.create_linked_table("L", "linkee.mdb", "Remote").unwrap();

            // rewrite the catalog row of O into a type 4 (linked ODBC) row
            let rows = db.read_rows_with_ids(TABLE_SYSTEM_CATALOG).unwrap();
            let (idx_id, idx_type, idx_connect, idx_foreign) = {
                let catalog = db.tables.get(&PAGE_SYSTEM_CATALOG).unwrap();
                (
                    catalog.column_index(CAT_COL_ID).unwrap(),
                    catalog.column_index(CAT_COL_TYPE).unwrap(),
                    catalog.column_index(CAT_COL_CONNECT).unwrap(),
                    catalog.column_index(CAT_COL_FOREIGN_NAME).unwrap(),
                )
            };
            let mut updated = false;
            for (row_id, values) in rows {
                let mut values = match values {
                    Some(values) => values,
                    None => continue,
                };
                if values.get(idx_id).and_then(|v| v.as_i64()) == Some(page_o as i64) {
                    values[idx_type] = Value::Int(TYPE_LINKED_ODBC_TABLE);
                    values[idx_connect] =
                        Value::Text("ODBC;DSN=Test;DBQ=C:\\remote.accdb".to_string());
                    values[idx_foreign] = Value::Text("RemoteOdbc".to_string());
                    let tz = db.tz;
                    db.tables
                        .get_mut(&PAGE_SYSTEM_CATALOG)
                        .unwrap()
                        .update_row(&mut db.channel, row_id, &values, &tz)
                        .unwrap();
                    updated = true;
                    break;
                }
            }
            assert!(updated, "O catalog row found and rewritten");
            db.flush().unwrap();
        }

        let mut db = Database::open_path(&path).unwrap();

        let local = db.get_table_meta_data("T").unwrap().expect("T metadata");
        assert_eq!(local.table_type, TableMetaDataType::Local);
        assert!(!local.is_linked());
        assert!(!local.is_system());
        assert!(local.created_date.is_some());
        assert!(local.updated_date.is_some());

        let linked = db.get_table_meta_data("l").unwrap().expect("L metadata");
        assert_eq!(linked.table_type, TableMetaDataType::Linked);
        assert!(linked.is_linked());
        assert_eq!(linked.linked_db_name.as_deref(), Some("linkee.mdb"));
        assert_eq!(linked.linked_table_name.as_deref(), Some("Remote"));
        assert_eq!(linked.connection_name, None);
        assert!(linked.page_number.unwrap() < 0, "synthetic object id");
        assert!(linked.created_date.is_some());

        let odbc = db.get_table_meta_data("O").unwrap().expect("O metadata");
        assert_eq!(odbc.table_type, TableMetaDataType::LinkedOdbc);
        assert!(odbc.is_linked());
        assert!(!odbc.is_system());
        assert_eq!(odbc.linked_db_name, None);
        assert_eq!(odbc.linked_table_name.as_deref(), Some("RemoteOdbc"));
        assert_eq!(
            odbc.connection_name.as_deref(),
            Some("ODBC;DSN=Test;DBQ=C:\\remote.accdb")
        );
        assert!(odbc.created_date.is_some());

        // the definition metadata works, the row count comes from the tdef
        let page = {
            let table = db.get_table("O").unwrap();
            assert!(table.is_definition_only());
            assert_eq!(table.get_row_count(), 1);
            assert!(table.get_created_date().is_some());
            assert!(table.updated_date.is_some());
            table.table_def_page_number
        };
        let error = db.read_table_rows(page).unwrap_err();
        match error {
            Error::Unsupported(message) => {
                assert!(
                    message.starts_with("TableDefinition has no data access (Db="),
                    "unexpected message: {}",
                    message
                );
                assert!(
                    message.contains(";Table=O)"),
                    "unexpected message: {}",
                    message
                );
            }
            other => panic!("expected Unsupported, got {:?}", other),
        }

        // normal table names exclude system tables; system names are type 1
        let names = db.get_table_names().unwrap();
        assert!(names.contains(&"T".to_string()));
        assert!(names.contains(&"L".to_string()));
        assert!(names.contains(&"O".to_string()));
        assert!(!names.contains(&"MSysObjects".to_string()));
        assert!(!names.contains(&"MSysQueries".to_string()));
        assert!(db
            .get_system_table_names()
            .unwrap()
            .contains(&"MSysQueries".to_string()));
        assert!(!db
            .get_system_table_names()
            .unwrap()
            .contains(&"T".to_string()));

        // `Table.getCreatedDate` of a normal table reads the catalog row
        let table = db.get_table("T").unwrap();
        assert!(table.get_created_date().is_some());
        assert_eq!(table.get_row_count(), 0);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The read-side getters which were added for API surface parity:
    /// format, charset, path/name, sort order/code page, write-broken-index,
    /// the system-table lookup, the zone id, the password field and read-only
    /// reclaim.
    #[test]
    fn read_side_getters_and_policies() {
        let path = test_tmp_dir().join(format!("jackcess_surface_{}.mdb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut db = Database::create(FileFormat::V2003, &path).expect("create");
            assert_eq!(db.get_file_format().unwrap(), FileFormat::V2003);
            assert_eq!(db.get_name(), path.file_name().unwrap().to_str().unwrap());
            assert_eq!(db.get_path(), Some(path.as_path()));
            assert_eq!(db.get_charset(), Charset::Utf16Le);
            assert_eq!(db.default_charset().unwrap(), Charset::Utf16Le);
            assert_eq!(db.get_column_order(), ColumnOrder::Data);
            assert_eq!(db.get_date_time_type(), DateTimeType::LocalDateTime);
            assert!(db.is_enforce_foreign_keys());
            assert!(db.is_evaluate_expressions());
            assert!(!db.is_allow_auto_number_insert());
            assert!(!db.is_reuse_free_pages());
            assert!(!db.is_write_broken_index());
            assert!(!db.is_read_only());
            assert_eq!(db.get_database_password().unwrap(), None);
            // the V2003 template stores the legacy Jet 4 sort order and the
            // 1252 code page in page 0
            assert_eq!(db.get_default_sort_order().version, 0);
            assert_eq!(db.get_default_code_page(), 1252);
            assert_eq!(db.time_zone_offset_minutes(), 480);

            // runtime policies (Java's `Boolean` argument: None is the default)
            db.set_write_broken_index(Some(true));
            assert!(db.is_write_broken_index());
            db.set_write_broken_index(None);
            assert!(!db.is_write_broken_index());

            // get_table resolves system tables too (the port keeps one lookup
            // which covers them); get_system_table is Java's system-inclusive
            // lookup and reports a missing name as None
            assert!(db.get_system_table("MSysObjects").unwrap().is_some());
            assert!(db.get_table("MSysObjects").is_ok());
            assert!(db.get_system_table("__missing__").unwrap().is_none());

            // charset override (None restores the file default) keeps the page
            // channel, which decodes row text, in sync
            db.set_charset(Some(Charset::Cp1252));
            assert_eq!(db.get_charset(), Charset::Cp1252);
            assert_eq!(db.channel.charset, Charset::Cp1252);
            db.set_charset(None);
            assert_eq!(db.get_charset(), Charset::Utf16Le);
            assert_eq!(db.channel.charset, Charset::Utf16Le);

            // ZoneId equivalent for the fixed offset zones of this port
            db.set_time_zone_offset_minutes(330);
            assert_eq!(db.time_zone_offset_minutes(), 330);
            assert_eq!(db.get_zone_id(), "GMT+05:30");
            db.set_time_zone_offset_minutes(-300);
            assert_eq!(db.get_zone_id(), "GMT-05:00");
            db.set_time_zone_offset_minutes(0);
            assert_eq!(db.get_zone_id(), "UTC");
            db.set_time_zone_offset_minutes(480);
        }
        // reopening detects the format from the `AccessVersion` property of
        // the file (`DatabaseImpl.getFileFormat`'s Jet 4 lookup)
        {
            let mut db = Database::open_path(&path).expect("reopen");
            assert_eq!(db.get_file_format().unwrap(), FileFormat::V2003);
        }
        // a read-only database gives no free pages back instead of failing on
        // the truncate (`DatabaseImpl.reclaimFreeSpace`)
        {
            let mut db = Database::open_path_read_only(&path).expect("read-only open");
            assert!(db.is_read_only());
            assert_eq!(db.reclaim_free_space().unwrap(), 0);
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod depth_parity_tests {
    //! The API gaps the Jet 3 depth audit reported: the catalog dates of
    //! `MSysObjects`, the access control entry rows, the referenced index
    //! resolution (with Java's reciprocal validation), the name maps of system
    //! tables and `TableMetaData.isSystem`.
    use super::*;
    use crate::column::ColumnSpec;
    use crate::database::TableMetaDataType;
    use crate::index::ForeignKeyReference;
    use crate::relationship::RelationshipSpec;
    use crate::tablecreator::{IndexSpec, TableSpec};

    fn test_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn fixture(name: &str) -> Option<std::path::PathBuf> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("external")
            .join("jackcess-test-resources")
            .join("data")
            .join("V1997")
            .join(name);
        if path.exists() {
            Some(path)
        } else {
            eprintln!("skipping jet 3 fixture test: {} is missing", path.display());
            None
        }
    }

    /// `Table.getCreatedDate`/`getUpdatedDate` of the system catalog itself:
    /// Java reads the `DateCreate`/`DateUpdate` of the `MSysObjects` catalog
    /// row (`Id == 2`), which the port used to skip.
    #[test]
    fn system_catalog_has_its_catalog_dates() {
        let path = match fixture("common1V1997.mdb") {
            Some(path) => path,
            None => return,
        };
        let mut db = Database::open_path(&path).expect("open fixture");
        let expected = crate::datenums::LocalDateTime {
            year: 2010,
            month: 3,
            day: 5,
            hour: 14,
            minute: 48,
            second: 26,
            nano: 295_000_000,
        };
        {
            let catalog = db
                .get_system_table("MSysObjects")
                .unwrap()
                .expect("catalog");
            assert_eq!(catalog.get_created_date(), Some(expected));
            assert_eq!(catalog.get_updated_date(), Some(expected));
        }
        // the other system tables keep their dates too
        let aces = db.get_system_table("MSysACEs").unwrap().expect("aces");
        assert_eq!(aces.get_created_date(), Some(expected));
        // `TableMetaData.isSystem` (Java `TableMetaData.isSystem`)
        assert!(db
            .get_table_meta_data("MSysObjects")
            .unwrap()
            .expect("metadata")
            .is_system());
        assert!(!db
            .get_table_meta_data("Table1")
            .unwrap()
            .expect("metadata")
            .is_system());
    }

    /// `Database.getAccessControlEntries` (the table) and its rows:
    /// `ObjectId`/`SID`/`ACM`/`FInheritable` in cursor order with Java's types.
    #[test]
    fn access_control_entries_match_the_java_rows() {
        let path = match fixture("common1V1997.mdb") {
            Some(path) => path,
            None => return,
        };
        let mut db = Database::open_path(&path).expect("open fixture");
        {
            let table = db.get_access_control_entries().expect("ACE table");
            assert_eq!(table.get_name(), "MSysACEs");
            assert_eq!(table.get_row_count(), 67);
        }
        let entries = db.get_access_control_entries_rows().expect("ACE rows");
        assert_eq!(entries.len(), 67);
        // the first rows of the Java dump (`MSysACEs` cursor order)
        assert_eq!(
            entries[0],
            AceEntry {
                object_id: 2,
                sid: vec![0x03, 0x01],
                acm: 393_216,
                f_inheritable: false,
            }
        );
        assert_eq!(entries[1].object_id, 3);
        assert_eq!(entries[2].object_id, 4);
        // a row with the inheritable flag set, as Java's `Boolean` reports it
        assert!(entries.iter().any(|entry| entry.f_inheritable));
        // Java's `Integer` for the signed synthetic object ids
        assert!(entries.iter().any(|entry| entry.object_id < 0));
    }

    /// `Index.getReferencedIndex` resolves the foreign key index on the other
    /// side of the relationship, and validates the reference back.
    #[test]
    fn referenced_index_resolves_the_foreign_key_index() {
        let path = match fixture("indexV1997.mdb") {
            Some(path) => path,
            None => return,
        };
        let mut db = Database::open_path(&path).expect("open fixture");
        // `Table2Table1` is the foreign key index of `Table1`; its reference is
        // `Table2`'s `.rC` index, which references it back
        let referenced = db
            .get_referenced_index("Table1", "Table2Table1")
            .expect("referenced index")
            .expect("a reference");
        assert_eq!(referenced.name, ".rC");
        // an index without a foreign key reference resolves to `null`
        assert!(db
            .get_referenced_index("Table1", "PrimaryKey")
            .expect("no reference")
            .is_none());
        // Java's `Table.getIndex` failure for an unknown name
        let err = db
            .get_referenced_index("Table1", "__MISSING__")
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Index with name __MISSING__ does not exist on this table"),
            "{err}"
        );
    }

    fn make_pc(db: &mut Database) {
        db.create_table(
            &TableSpec::new("P", vec![ColumnSpec::new("ID", DataType::Long)]).with_index(
                IndexSpec::primary_key("PrimaryKey", vec![("ID".to_string(), true)]),
            ),
        )
        .expect("create P");
        db.create_table(
            &TableSpec::new(
                "C",
                vec![
                    ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                    ColumnSpec::new("P_ID", DataType::Long),
                ],
            )
            .with_index(IndexSpec::primary_key(
                "PrimaryKey",
                vec![("ID".to_string(), true)],
            )),
        )
        .expect("create C");
        db.add_rows("P", &[vec![Value::Long(1)]])
            .expect("add P row");
        db.add_relationship(
            RelationshipSpec::new("P", "C")
                .add_columns("ID", "P_ID")
                .set_referential_integrity(),
        )
        .expect("relationship");
    }

    /// The foreign key index of `C` (and its reference), the page of the
    /// referenced index.
    fn foreign_key_of(db: &mut Database) -> (String, i32, i32) {
        let table = db.get_table("C").expect("C");
        let index = table
            .indexes
            .iter()
            .find(|index| index.is_foreign_key())
            .expect("the foreign key index");
        let reference = index.reference.expect("reference");
        (
            index.name.clone(),
            reference.other_table_page_number,
            reference.other_index_number,
        )
    }

    /// A reference to a missing table is Java's `Reference to missing table N`
    /// `IOException` with the `(Db=...;Index=...)` context.
    #[test]
    fn referenced_index_reports_a_missing_table() {
        let path = test_tmp_dir().join("refindex_missing_table.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create");
        make_pc(&mut db);
        let (fk_name, _, _) = foreign_key_of(&mut db);
        let c_page = db.table_lookup_entry("C").unwrap().0;
        {
            let table = db.tables.get_mut(&c_page).unwrap();
            let index = table
                .indexes
                .iter_mut()
                .find(|index| index.name == fk_name)
                .unwrap();
            let mut reference = index.reference.unwrap();
            reference.other_table_page_number = 9999;
            index.reference = Some(reference);
        }
        let err = db.get_referenced_index("C", &fk_name).unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with("Reference to missing table 9999 (Db="),
            "{message}"
        );
        assert!(message.contains(";Index="), "{message}");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A reference to a missing index names the index and the table.
    #[test]
    fn referenced_index_reports_a_missing_index() {
        let path = test_tmp_dir().join("refindex_missing_index.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create");
        make_pc(&mut db);
        let (fk_name, _, _) = foreign_key_of(&mut db);
        let c_page = db.table_lookup_entry("C").unwrap().0;
        {
            let table = db.tables.get_mut(&c_page).unwrap();
            let index = table
                .indexes
                .iter_mut()
                .find(|index| index.name == fk_name)
                .unwrap();
            let mut reference = index.reference.unwrap();
            reference.other_index_number = 9999;
            index.reference = Some(reference);
        }
        let err = db.get_referenced_index("C", &fk_name).unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with("Reference to missing index 9999 on table P (Db="),
            "{message}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The reciprocal-reference check: the referenced index must point back at
    /// the index which named it (`Found unexpected index ... with reference
    /// ...`).
    #[test]
    fn referenced_index_validates_the_reference_back() {
        let path = test_tmp_dir().join("refindex_reciprocal.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create");
        make_pc(&mut db);
        let (fk_name, ref_page, ref_number) = foreign_key_of(&mut db);
        let c_page = db.table_lookup_entry("C").unwrap().0;
        {
            let table = db.tables.get_mut(&ref_page).unwrap();
            let index = table
                .indexes
                .iter_mut()
                .find(|index| index.index_number == ref_number)
                .expect("referenced index");
            // a reference to some other index which does not point back
            index.reference = Some(ForeignKeyReference::new(
                2, 999, c_page, false, false, false,
            ));
        }
        let err = db.get_referenced_index("C", &fk_name).unwrap_err();
        let message = err.to_string();
        assert!(message.starts_with("Found unexpected index "), "{message}");
        assert!(
            message.contains(" with reference ForeignKeyReference["),
            "{message}"
        );
        assert!(message.contains("otherIndexNumber: 999"), "{message}");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A `NameMap` blob with one table record (guid bytes `1..=16`).
    fn synthetic_name_map() -> Vec<u8> {
        let mut blob = Vec::new();
        blob.extend_from_slice(&0x550E_CC0Au32.to_le_bytes());
        blob.extend_from_slice(&0i32.to_le_bytes());
        for byte in 1u8..=16 {
            blob.push(byte);
        }
        blob.extend_from_slice(&0i32.to_le_bytes());
        blob.extend_from_slice(&[0u8; 16]);
        for unit in "SYSTEST".encode_utf16() {
            blob.extend_from_slice(&unit.to_le_bytes());
        }
        blob.extend_from_slice(&0u16.to_le_bytes());
        blob
    }

    /// `Table.getNameMap` works for a system table: Java loads the `NameMap`
    /// property of any table, and the port now captures it while loading a
    /// system table too.
    #[test]
    fn system_table_name_map_is_read() {
        let path = test_tmp_dir().join("sys_namemap.mdb");
        let _ = std::fs::remove_file(&path);
        {
            let mut db = Database::create(FileFormat::V2003, &path).expect("create");
            let mut maps = db
                .get_table_properties_by_name(TABLE_SYSTEM_ACES)
                .expect("ACE properties");
            maps.get_default()
                .put_value(
                    crate::property::NAME_MAP_PROP,
                    Value::Bytes(synthetic_name_map()),
                )
                .expect("put name map");
            db.save_properties(&mut maps).expect("save name map");
        }
        let mut db = Database::open_path(&path).expect("reopen");
        let from_table = {
            let table = db
                .get_system_table(TABLE_SYSTEM_ACES)
                .unwrap()
                .expect("aces");
            table.get_name_map().expect("parse name map")
        };
        let from_table = from_table.expect("a NameMap on the system table");
        assert_eq!(from_table.name_map().records().len(), 1);
        assert_eq!(
            from_table.get_table_record().unwrap().get_guid().as_deref(),
            Some("{04030201-0605-0807-090A-0B0C0D0E0F10}")
        );
        // the database level API returns the same map (Java `Table.getNameMap`
        // for any table) and caches it on the table
        let from_database = db
            .get_table_name_map(TABLE_SYSTEM_ACES)
            .expect("database name map")
            .expect("a NameMap");
        assert_eq!(from_database.name_map().records().len(), 1);
        // the catalog itself has no NameMap in this database
        assert!(db
            .get_table_name_map(TABLE_SYSTEM_CATALOG)
            .expect("catalog name map")
            .is_none());
        // the catalog metadata still reports the local table type
        assert_eq!(
            db.get_table_meta_data(TABLE_SYSTEM_ACES)
                .unwrap()
                .expect("metadata")
                .table_type,
            TableMetaDataType::Local
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
