//! Port of `com.healthmarketscience.jackcess.Row`: a row of data as column
//! name -> value pairs, carrying its [`RowId`](crate::table::RowId), with the
//! Java typed getters, plus the row-object conveniences of `Table` and `Cursor`
//! (`Table.asRow`, `Table.addRowFromMap`, `Table.iterator`, `Cursor.iterator`,
//! `Cursor.getNextRow`, ...).
//!
//! The rest of the crate already exposes the rows as positional `Vec<Value>`
//! arrays ([`crate::database::Database::add_rows`],
//! [`crate::cursor::Cursor::get_current_row`], ...); this module is purely
//! additive and leaves those entry points (and therefore every written byte)
//! untouched.  A `Row` is built from such a vector and a column list, and is
//! converted back to one with [`Row::to_values`] / [`Row::to_update_values`].
//!
//! # `Row`
//!
//! Java's `Row` is a `Map<String,Object>` (`RowImpl` is a `LinkedHashMap`)
//! carrying a `RowId`.  The Rust [`Row`] keeps the same insertion-ordered
//! column -> value pairs, so iterating a `Row` yields the columns in the order
//! Java's `RowImpl` holds them (the table's column order), and the map style
//! accessors (`get`, `insert`, `remove`, `contains_key`, `len`, `iter`,
//! `keys`, `values`, `entries`) mirror `Map`.  Like `RowImpl.equals`, [`Row`]
//! equality compares the contents only (order and row id are ignored).
//!
//! `Row.getId()` is nullable in Java (`RowImpl` accepts a `null` id for query
//! rows), so [`Row::get_id`] returns `Option<RowId>`; a row built from a plain
//! map ([`Row::from_map`]) has no id, and [`Row::new`] /
//! [`Row::from_values_with_id`] set one.
//!
//! # Java name mapping
//!
//! | Java (`com.healthmarketscience.jackcess`) | Rust |
//! |---|---|
//! | `Row extends Map<String,Object>` | [`Row`] (ordered entries, `get`/`insert`/`remove`/`iter`/...) |
//! | `Row.getId()` | [`Row::get_id`] (`Option<RowId>`, Java allows `null`) |
//! | `Row.getString(name)` | [`Row::get_string`] |
//! | `Row.getBoolean(name)` | [`Row::get_boolean`] |
//! | `Row.getByte(name)` | [`Row::get_byte`] (`i8`, Java's signed `Byte`) |
//! | `Row.getShort(name)` | [`Row::get_short`] |
//! | `Row.getInt(name)` | [`Row::get_int`] |
//! | `Row.getBigDecimal(name)` | [`Row::get_big_decimal`] (`bigdecimal::BigDecimal`) |
//! | `Row.getFloat(name)` / `getDouble(name)` | [`Row::get_float`] / [`Row::get_double`] |
//! | `Row.getDate(name)` | [`Row::get_date`] (the stored OLE date double; `Date` has no Rust type) |
//! | `Row.getLocalDateTime(name)` | [`Row::get_local_date_time`] |
//! | `Row.getBytes(name)` | [`Row::get_bytes`] |
//! | `Row.getForeignKey(name)` | [`Row::get_foreign_key`] (the `COMPLEX_TYPE` id; see the limitation below) |
//! | `Row.getBlob(name)` | [`Row::get_blob`] (`crate::ole::OleBlob`) |
//! | `Table.asRow(Map)` | [`crate::table::Table::as_row`] |
//! | `Table.asUpdateRow(Map)` | [`crate::table::Table::as_update_row`] |
//! | `Table.addRowFromMap(M)` | [`crate::database::Database::add_row_from_map`] / [`crate::table::Table::add_row_from_map`] |
//! | `Table.addRowsFromMaps(List)` | [`crate::database::Database::add_rows_from_maps`] / [`crate::table::Table::add_rows_from_maps`] |
//! | `Table.updateRow(Row)` | [`crate::database::Database::update_row_object`] / [`crate::table::Table::update_row_object`] |
//! | `Table.deleteRow(Row)` | [`crate::database::Database::delete_row_object`] / [`crate::table::Table::delete_row_object`] |
//! | `Table.iterator()` / `Table.stream()` | [`TableRowIter`] (`Database::row_iter`), [`crate::database::Database::rows`], [`crate::table::Table::stream`] |
//! | `Table.newCursor()` / `Table.getDefaultCursor()` | [`crate::table::Table::new_cursor`] / [`crate::table::Table::default_cursor`] (both return a `CursorBuilder`) |
//! | `Cursor.iterator()` / `Cursor.stream()` | [`CursorRowIter`] (`TableScanCursor::row_iter` / `IndexCursor::row_iter`), [`crate::cursor::Cursor::stream`] |
//! | `Cursor.getNextRow()` | [`crate::cursor::Cursor::get_next_row_object`] |
//! | `Cursor.getNextRow(Collection)` | [`crate::cursor::Cursor::get_next_row_object_columns`] |
//! | `Cursor.getPreviousRow()` / `getPreviousRow(Collection)` | `Cursor::get_previous_row_object` / `_columns` |
//! | `Cursor.getCurrentRow()` / `getCurrentRow(Collection)` | [`crate::cursor::Cursor::get_current_row_object`] / `_columns` |
//! | `Cursor.updateCurrentRowFromMap(Map)` | [`crate::cursor::Cursor::update_current_row_object`] |
//! | `Cursor.newIterable()` | [`crate::cursor::Cursor::new_iterable`] |
//! | `IndexCursor.newEntryIterable(...)` | [`crate::cursor::IndexCursor::new_entry_iterable`] |
//!
//! # Not expressible in Rust
//!
//! * Java's overloads `Table.updateRow(RowId, Object...)`, `Table.updateRow(Row)`
//!   and `Table.deleteRow(Row)` cannot share one Rust name/signature.  The
//!   existing positional entry points keep their names
//!   ([`crate::database::Database::update_row`],
//!   [`crate::database::Database::delete_row`]); the `Row` based ones are
//!   `update_row_object` / `delete_row_object`.
//! * A `Table` does not hold the [`Database`](crate::database::Database) it was
//!   read from (the port's table operations take the database explicitly, like
//!   the existing `Database::add_rows`), so a `Table` *instance* cannot do
//!   `addRow`/`updateRow`/`iterator` by itself.  Those are provided as
//!   [`crate::database::Database`] methods (which take the table name) and as
//!   associated functions on [`crate::table::Table`] taking `(db, table_name,
//!   ...)`, e.g. `Table::add_row_object(&mut db, "T", row)`.
//! * `Row.getForeignKey` returns a live `ComplexValueForeignKey` in Java, but
//!   the port's [`crate::complex::ComplexValueForeignKey`] borrows a `&mut
//!   Database`.  [`Row::get_foreign_key`] therefore returns the stored
//!   `COMPLEX_TYPE` foreign key id; build the live handle with
//!   [`crate::database::Database::complex_value_fk`].  Because the `Row` keeps
//!   no column metadata, this getter cannot tell a `COMPLEX_TYPE` value (Java's
//!   `ComplexValueForeignKey`) from a plain `LONG` (Java's `Integer`): both are
//!   the same stored i32 and are accepted (Java would throw a
//!   `ClassCastException` for the `LONG`).
//! * `Table.getNextRow`/`Table.reset` use a cached default cursor inside the
//!   Java `TableImpl`; the stateless Rust table cannot keep that position, so
//!   iteration is expressed with [`TableRowIter`] (or a cursor) instead.
//! * [`TableRowIter`] snapshots the row ids when it is created (one pass over
//!   the table's row slots) and then decodes the rows lazily; rows added or
//!   removed through another handle during the iteration are not visible.  A
//!   `CursorRowIter` walks a live cursor and does see table modifications, like
//!   Java's `Cursor`/`Table` iterators.

use std::collections::BTreeMap;
use std::fmt;

use bigdecimal::num_bigint::BigInt;
use bigdecimal::BigDecimal;

use crate::column::Column;
use crate::cursor::{Cursor, CursorBuilder};
use crate::database::Database;
use crate::datenums::LocalDateTime;
use crate::error::{Error, Result};
use crate::ole::OleBlob;
use crate::table::{RowId, Table};
use crate::value::Value;

// ---------------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------------

/// A row of data as column name -> value pairs, carrying its
/// [`RowId`](crate::table::RowId) (`com.healthmarketscience.jackcess.Row`,
/// implemented by `RowImpl`).
///
/// The entries are kept in insertion order, like Java's `RowImpl` (a
/// `LinkedHashMap`), so iterating a row returns its columns in the order the
/// table (or the requested column subset) yielded them.  Column names are
/// case sensitive, like Java's map based access.
#[derive(Clone, Debug, Default)]
pub struct Row {
    /// `RowImpl._id`: `None` maps Java's `null` id (a query row).
    id: Option<RowId>,
    entries: Vec<(String, Value)>,
}

impl Row {
    /// A row with the given id and no values.
    pub fn new(id: RowId) -> Row {
        Row {
            id: Some(id),
            entries: Vec::new(),
        }
    }

    /// A row with no id and no values (a plain input map, like the `Map` handed
    /// to `Table.addRowFromMap`).
    pub fn empty() -> Row {
        Row::default()
    }

    /// A row built from an explicit id (`None` is Java's `null` id) and entries.
    ///
    /// If a name occurs more than once, the last value wins and the entry keeps
    /// the position of the first occurrence, like `LinkedHashMap.put`.
    pub fn with_entries<I, K>(id: Option<RowId>, entries: I) -> Row
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<String>,
    {
        let mut row = Row {
            id,
            entries: Vec::new(),
        };
        for (name, value) in entries {
            row.insert(name, value);
        }
        row
    }

    /// Builds a row from the positional values of a table row and the table's
    /// column names (`TableImpl.getRow`).  Missing values are null; extra
    /// values are ignored.
    ///
    /// The row has no id; use [`Row::from_values_with_id`] for a row read from
    /// the table.
    pub fn from_values(columns: &[String], values: Vec<Value>) -> Row {
        Row::with_entries(
            None,
            columns
                .iter()
                .enumerate()
                .map(|(i, name)| (name.clone(), values.get(i).cloned().unwrap_or(Value::Null))),
        )
    }

    /// [`Row::from_values`] with the row id of the row which was read.
    pub fn from_values_with_id(id: RowId, columns: &[String], values: &[Value]) -> Row {
        Row::with_entries(
            Some(id),
            columns
                .iter()
                .enumerate()
                .map(|(i, name)| (name.clone(), values.get(i).cloned().unwrap_or(Value::Null))),
        )
    }

    /// [`Row::from_values`] with `&Column` definitions instead of names.
    pub fn from_columns(columns: &[&Column], values: Vec<Value>) -> Row {
        Row::with_entries(
            None,
            columns.iter().enumerate().map(|(i, c)| {
                (
                    c.name.clone(),
                    values.get(i).cloned().unwrap_or(Value::Null),
                )
            }),
        )
    }

    /// A copy of the given map as a row without an id (`RowImpl(Row)` copies
    /// the id as well, see [`Row::from_map_with_id`]).
    pub fn from_map(map: BTreeMap<String, Value>) -> Row {
        Row::with_entries(None, map)
    }

    /// A copy of the given map as a row with the given id.
    pub fn from_map_with_id(id: Option<RowId>, map: BTreeMap<String, Value>) -> Row {
        Row::with_entries(id, map)
    }

    /// `Row.getId()`: the id of this row, or `None` for Java's `null` id.
    pub fn get_id(&self) -> Option<RowId> {
        self.id
    }

    /// Sets the row id (Java's rows are read-only in practice; useful for the
    /// port's value style construction).
    pub fn set_id(&mut self, id: Option<RowId>) {
        self.id = id;
    }

    // -- Map style access ---------------------------------------------------

    /// `Map.get`: the value of the named column (case sensitive), or `None`
    /// when the row has no such column.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// `Map.get` with a mutable reference.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        self.entries
            .iter_mut()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// `Map.put`: inserts/replaces a value, returning the old value.  Replacing
    /// keeps the original position (`LinkedHashMap` semantics).
    pub fn insert(&mut self, name: impl Into<String>, value: Value) -> Option<Value> {
        let name = name.into();
        if let Some((_, old)) = self.entries.iter_mut().find(|(key, _)| *key == name) {
            return Some(std::mem::replace(old, value));
        }
        self.entries.push((name, value));
        None
    }

    /// `Map.remove`: removes the named column, returning its value.
    pub fn remove(&mut self, name: &str) -> Option<Value> {
        let pos = self.entries.iter().position(|(key, _)| key == name)?;
        Some(self.entries.remove(pos).1)
    }

    /// `Map.containsKey`.
    pub fn contains_key(&self, name: &str) -> bool {
        self.entries.iter().any(|(key, _)| key == name)
    }

    /// `Map.size`.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `Map.isEmpty`.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The column names, in row order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }

    /// The values, in row order.
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|(_, value)| value)
    }

    /// The entries, in row order (`Map.entrySet().iterator()`).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    /// The entries with the column name owned, in row order.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }

    /// A copy of the row as a `BTreeMap` (`new LinkedHashMap<>(row)`).
    pub fn to_map(&self) -> BTreeMap<String, Value> {
        self.entries.iter().cloned().collect()
    }

    /// Consumes the row into a `BTreeMap`.
    pub fn into_map(self) -> BTreeMap<String, Value> {
        self.entries.into_iter().collect()
    }

    /// The row values in the order of the given column list; a column the row
    /// does not hold is [`Value::Null`] (`Table.asRow` leaves absent columns
    /// null).
    pub fn to_values(&self, columns: &[String]) -> Vec<Value> {
        columns
            .iter()
            .map(|name| self.get(name).cloned().unwrap_or(Value::Null))
            .collect()
    }

    /// [`Row::to_values`] for a list of `&str` column names.
    pub fn to_values_named(&self, columns: &[&str]) -> Vec<Value> {
        columns
            .iter()
            .map(|name| self.get(name).cloned().unwrap_or(Value::Null))
            .collect()
    }

    /// The row values for a partial update: a column the row does not hold is
    /// [`Value::Keep`] (`Table.asUpdateRow`), so the update keeps its old value.
    pub fn to_update_values(&self, columns: &[String]) -> Vec<Value> {
        columns
            .iter()
            .map(|name| self.get(name).cloned().unwrap_or(Value::Keep))
            .collect()
    }

    /// [`Row::to_update_values`] for a list of `&str` column names.
    pub fn to_update_values_named(&self, columns: &[&str]) -> Vec<Value> {
        columns
            .iter()
            .map(|name| self.get(name).cloned().unwrap_or(Value::Keep))
            .collect()
    }

    // -- Java typed getters -------------------------------------------------

    /// `Row.getString` (DataTypes TEXT, MEMO, GUID).
    ///
    /// A GUID value is rendered the way jackcess' `readGUIDValue` renders it
    /// (braced, upper case hex), i.e. the string Java's `Row` holds for a GUID
    /// column.  A missing column or a null value is `None` (`null` in Java);
    /// any other value type is an error (Java's `ClassCastException`).
    pub fn get_string(&self, name: &str) -> Result<Option<String>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Text(text)) => Ok(Some(text.clone())),
            Some(Value::Uuid(bytes)) => Ok(Some(guid_string(bytes))),
            Some(other) => Err(wrong_type(name, "String", other)),
        }
    }

    /// `Row.getBoolean` (DataType BOOLEAN).
    pub fn get_boolean(&self, name: &str) -> Result<Option<bool>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(other) => Err(wrong_type(name, "Boolean", other)),
        }
    }

    /// `Row.getByte` (DataType BYTE).  Java's `Byte` is signed, so the port's
    /// stored `u8` is returned as `i8`.
    pub fn get_byte(&self, name: &str) -> Result<Option<i8>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Byte(value)) => Ok(Some(*value as i8)),
            Some(other) => Err(wrong_type(name, "Byte", other)),
        }
    }

    /// `Row.getShort` (DataType INT).
    pub fn get_short(&self, name: &str) -> Result<Option<i16>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Int(value)) => Ok(Some(*value)),
            Some(other) => Err(wrong_type(name, "Short", other)),
        }
    }

    /// `Row.getInt` (DataType LONG).
    pub fn get_int(&self, name: &str) -> Result<Option<i32>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Long(value)) => Ok(Some(*value)),
            Some(other) => Err(wrong_type(name, "Integer", other)),
        }
    }

    /// `Row.getBigDecimal` (DataTypes MONEY, NUMERIC).
    pub fn get_big_decimal(&self, name: &str) -> Result<Option<BigDecimal>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Decimal { unscaled, scale }) => Ok(Some(BigDecimal::new(
                BigInt::from(*unscaled),
                *scale as i64,
            ))),
            Some(other) => Err(wrong_type(name, "BigDecimal", other)),
        }
    }

    /// `Row.getFloat` (DataType FLOAT).
    pub fn get_float(&self, name: &str) -> Result<Option<f32>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Float(value)) => Ok(Some(*value)),
            Some(other) => Err(wrong_type(name, "Float", other)),
        }
    }

    /// `Row.getDouble` (DataType DOUBLE).
    pub fn get_double(&self, name: &str) -> Result<Option<f64>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Double(value)) => Ok(Some(*value)),
            Some(other) => Err(wrong_type(name, "Double", other)),
        }
    }

    /// `Row.getDate` (DataType SHORT_DATE_TIME, legacy
    /// `DateTimeType.DATE` mode).
    ///
    /// Java returns a `java.util.Date`; the port's value model keeps the stored
    /// OLE date double ([`Value::DateBits`], days since 1899-12-30), so this
    /// returns that double.  [`Row::get_date_millis`] converts it with the
    /// database zone the way `Date.getTime()` does.  A `LocalDateTime` value
    /// (the default `LOCAL_DATE_TIME` mode) is an error, like Java's
    /// `ClassCastException`.
    pub fn get_date(&self, name: &str) -> Result<Option<f64>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::DateBits(bits)) => Ok(Some(*bits)),
            Some(other) => Err(wrong_type(name, "Date", other)),
        }
    }

    /// `Row.getDate(...).getTime()`: the epoch milliseconds of a legacy
    /// [`Value::DateBits`] value, using the database zone.
    ///
    /// A [`Value::DateTime`] is converted with the given zone as well (jackcess'
    /// `Date` face of a `LocalDateTime`), so this works in both date modes.
    pub fn get_date_millis(
        &self,
        name: &str,
        tz: &crate::datenums::TimeZone,
    ) -> Result<Option<i64>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::DateBits(bits)) => Ok(Some(crate::datenums::from_date_double(*bits, tz))),
            Some(Value::DateTime(ldt)) => Ok(Some(tz.local_to_epoch_millis(*ldt))),
            Some(other) => Err(wrong_type(name, "Date", other)),
        }
    }

    /// `Row.getLocalDateTime` (DataTypes SHORT_DATE_TIME, EXT_DATE_TIME, in the
    /// default `LOCAL_DATE_TIME` mode).
    pub fn get_local_date_time(&self, name: &str) -> Result<Option<LocalDateTime>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::DateTime(ldt)) => Ok(Some(*ldt)),
            Some(other) => Err(wrong_type(name, "LocalDateTime", other)),
        }
    }

    /// `Row.getBytes` (DataTypes BINARY, OLE, BIG_BINARY).
    pub fn get_bytes(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bytes(bytes)) => Ok(Some(bytes.clone())),
            Some(other) => Err(wrong_type(name, "byte[]", other)),
        }
    }

    /// `Row.getForeignKey` (DataType COMPLEX_TYPE): the stored foreign key id,
    /// which the port's [`crate::complex::ComplexValueForeignKey`] wraps (see
    /// the module documentation for the Java `ComplexValueForeignKey` mapping).
    pub fn get_foreign_key(&self, name: &str) -> Result<Option<i32>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Long(value)) => Ok(Some(*value)),
            Some(other) => Err(wrong_type(name, "ComplexValueForeignKey", other)),
        }
    }

    /// `Row.getBlob` (DataType OLE): the value wrapped in
    /// [`crate::ole::OleBlob`] (`OleBlob.Builder.fromInternalData`).
    ///
    /// Note, like Java's `OleBlob`, the blob is just a view over the stored
    /// bytes: it should be used before the row is dropped.
    pub fn get_blob(&self, name: &str) -> Result<Option<OleBlob>> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bytes(bytes)) => Ok(Some(OleBlob::from_internal_data(bytes))),
            Some(other) => Err(wrong_type(name, "OleBlob", other)),
        }
    }
}

/// `Row.toString()` (`ToStringBuilder.valueBuilder("Row[" + id + "]")
/// .append(null, map)`): `Row[page:row]{name=value,...}` with Java's map style
/// rendering.  `None` ids render as `null`, like Java's `Row[null]`.
impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.id {
            Some((page, row)) => write!(f, "Row[{}:{}]{{", page, row)?,
            None => write!(f, "Row[null]{{")?,
        }
        for (i, (name, value)) in self.entries.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            write!(f, "{}={}", name, display_value(value))?;
        }
        write!(f, "}}")
    }
}

fn display_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Byte(v) => (*v as i8).to_string(),
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        Value::BigInt(v) => v.to_string(),
        Value::Decimal { unscaled, scale } => format!("{unscaled}/{scale}"),
        Value::Float(v) => v.to_string(),
        Value::Double(v) => v.to_string(),
        Value::Text(v) => v.clone(),
        Value::Bytes(v) => format!("[{}]", hex(v)),
        Value::DateTime(v) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}",
            v.year, v.month, v.day, v.hour, v.minute, v.second, v.nano
        ),
        Value::DateBits(v) => v.to_string(),
        Value::Uuid(v) => guid_string(v),
        Value::Keep => "<KEEP_VALUE>".to_string(),
    }
}

/// `RowImpl.equals`/`hashCode` work on the row contents only (the id is
/// ignored), and `LinkedHashMap.equals` is order insensitive.
impl PartialEq for Row {
    fn eq(&self, other: &Row) -> bool {
        self.entries.len() == other.entries.len()
            && self
                .entries
                .iter()
                .all(|(name, value)| other.get(name).map(|v| v == value).unwrap_or(false))
    }
}

impl<'a> IntoIterator for &'a Row {
    type Item = (&'a str, &'a Value);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a Value)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl IntoIterator for Row {
    type Item = (String, Value);
    type IntoIter = std::vec::IntoIter<(String, Value)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

// -- conversions between Row and the positional value arrays ----------------

impl From<BTreeMap<String, Value>> for Row {
    fn from(map: BTreeMap<String, Value>) -> Row {
        Row::from_map(map)
    }
}

impl From<Row> for BTreeMap<String, Value> {
    fn from(row: Row) -> BTreeMap<String, Value> {
        row.into_map()
    }
}

/// The values of a row in row order (Java's `new ArrayList<>(row.values())`).
///
/// The column-list conversion is [`Row::to_values`] (or the [`RowValues`]
/// wrapper); this one needs no column list because the row already holds the
/// iteration order.
impl From<Row> for Vec<Value> {
    fn from(row: Row) -> Vec<Value> {
        row.into_iter().map(|(_, value)| value).collect()
    }
}

impl<'a> From<(Vec<Value>, &'a [String])> for Row {
    fn from((values, columns): (Vec<Value>, &'a [String])) -> Row {
        Row::from_values(columns, values)
    }
}

impl From<(Vec<Value>, Vec<String>)> for Row {
    fn from((values, columns): (Vec<Value>, Vec<String>)) -> Row {
        Row::from_values(&columns, values)
    }
}

impl<'a, 'b> From<(Vec<Value>, &'a [&'b str])> for Row {
    fn from((values, columns): (Vec<Value>, &'a [&'b str])) -> Row {
        Row::with_entries(
            None,
            columns
                .iter()
                .enumerate()
                .map(|(i, name)| (*name, values.get(i).cloned().unwrap_or(Value::Null))),
        )
    }
}

impl<'a> From<(&'a [String], Vec<Value>)> for Row {
    fn from((columns, values): (&'a [String], Vec<Value>)) -> Row {
        Row::from_values(columns, values)
    }
}

impl<'a, 'b> From<(&'a [&'b str], Vec<Value>)> for Row {
    fn from((columns, values): (&'a [&'b str], Vec<Value>)) -> Row {
        Row::with_entries(
            None,
            columns
                .iter()
                .enumerate()
                .map(|(i, name)| (*name, values.get(i).cloned().unwrap_or(Value::Null))),
        )
    }
}

/// The positional values of a [`Row`] for a column list (the `Row` ->
/// `Vec<Value>` direction of [`Table::as_row`]).
///
/// Rust's orphan rules do not allow `From<(Row, &[String])> for Vec<Value>`
/// (both the tuple and `Vec` are foreign types), so the conversion goes through
/// this local wrapper: `RowValues::from((row, columns))`, then
/// [`RowValues::into_vec`] / [`RowValues::as_slice`].  [`Row::to_values`] is the
/// direct method when no `From` conversion is needed.
#[derive(Clone, Debug, PartialEq)]
pub struct RowValues(Vec<Value>);

impl RowValues {
    /// Wraps an already positional value vector.
    pub fn new(values: Vec<Value>) -> RowValues {
        RowValues(values)
    }

    /// The values.
    pub fn as_slice(&self) -> &[Value] {
        &self.0
    }

    /// Consumes the wrapper, returning the values.
    pub fn into_vec(self) -> Vec<Value> {
        self.0
    }
}

impl AsRef<[Value]> for RowValues {
    fn as_ref(&self) -> &[Value] {
        &self.0
    }
}

impl std::ops::Deref for RowValues {
    type Target = [Value];

    fn deref(&self) -> &[Value] {
        &self.0
    }
}

impl From<RowValues> for Vec<Value> {
    fn from(values: RowValues) -> Vec<Value> {
        values.0
    }
}

impl<'a> From<(Row, &'a [String])> for RowValues {
    fn from((row, columns): (Row, &'a [String])) -> RowValues {
        RowValues(row.to_values(columns))
    }
}

impl From<(Row, Vec<String>)> for RowValues {
    fn from((row, columns): (Row, Vec<String>)) -> RowValues {
        RowValues(row.to_values(&columns))
    }
}

impl<'a, 'b> From<(Row, &'a [&'b str])> for RowValues {
    fn from((row, columns): (Row, &'a [&'b str])) -> RowValues {
        RowValues(row.to_values_named(columns))
    }
}

/// A GUID value rendered like jackcess' `ColumnImpl.readGUIDValue`: the
/// [`Value::Uuid`] bytes are the raw (little endian) on-disk representation, so
/// the first three integer components are byte swapped, then the result is
/// braced and upper case (the string Java's `Row` holds for a GUID column).
pub fn guid_string(bytes: &[u8; 16]) -> String {
    let mut bytes = *bytes;
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!(
        "{{{}-{}-{}-{}-{}}}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "Boolean",
        Value::Byte(_) => "Byte",
        Value::Int(_) => "Short",
        Value::Long(_) => "Integer",
        Value::BigInt(_) => "Long",
        Value::Decimal { .. } => "BigDecimal",
        Value::Float(_) => "Float",
        Value::Double(_) => "Double",
        Value::Text(_) => "String",
        Value::Bytes(_) => "byte[]",
        Value::DateTime(_) => "LocalDateTime",
        Value::DateBits(_) => "Date",
        Value::Uuid(_) => "String",
        Value::Keep => "Column.KEEP_VALUE",
    }
}

fn wrong_type(name: &str, expected: &str, actual: &Value) -> Error {
    Error::IllegalArgument(format!(
        "Row value for column '{}' is not a {} (it is a {})",
        name,
        expected,
        value_kind(actual)
    ))
}

// ---------------------------------------------------------------------------
// table iteration (`Table.iterator()` / `Table.stream()`)
// ---------------------------------------------------------------------------

/// An iterator over a [`Database`] table yielding [`Row`] values
/// (`Table.iterator()` / the `Table.stream()` source).
///
/// The iterator snapshots the row ids of the table when it is created (one
/// pass over the row slots, without decoding any value) and decodes each row
/// lazily into a [`Row`], carrying the row id.  Deleted rows are skipped.
/// Rows added or removed through another handle during the iteration are not
/// visible (Java's `TableImpl` iterator walks a live cursor instead); use a
/// [`crate::cursor::Cursor`] row iterator for a live traversal.
///
/// This is created by [`Database::row_iter`] or [`Table::row_iter`]; the item
/// type is `Result<Row>` because a row read may fail (the table's
/// [`crate::table::ErrorHandler`] is consulted, like every other row read).
pub struct TableRowIter<'a> {
    db: &'a mut Database,
    table: String,
    column_names: Vec<String>,
    row_ids: Vec<RowId>,
    next: usize,
    failed: bool,
}

impl<'a> TableRowIter<'a> {
    /// Creates the iterator for the named table (`Table.iterator` after
    /// `Table.reset`).
    pub fn new(db: &'a mut Database, table: &str) -> Result<TableRowIter<'a>> {
        let column_names = {
            let t = db.get_table(table)?;
            t.columns.iter().map(|c| c.name.clone()).collect()
        };
        let row_ids = db
            .row_id_statuses(table)?
            .into_iter()
            .filter_map(|(row_id, deleted)| if deleted { None } else { Some(row_id) })
            .collect();
        Ok(TableRowIter {
            db,
            table: table.to_string(),
            column_names,
            row_ids,
            next: 0,
            failed: false,
        })
    }

    /// The name of the table being iterated.
    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// The column names of the rows this iterator yields (the table's columns,
    /// in table order).
    pub fn column_names(&self) -> &[String] {
        &self.column_names
    }

    /// `Iterator.next`: the next row, or `None` at the end of the table.
    ///
    /// This is the explicit form of the [`Iterator`] implementation, useful
    /// when the `Result` should be handled without an `Iterator` adapter.
    pub fn next_row(&mut self) -> Result<Option<Row>> {
        while self.next < self.row_ids.len() {
            let id = self.row_ids[self.next];
            self.next += 1;
            match self.db.read_row_by_id_using(&self.table, id, None)? {
                Some(values) => {
                    return Ok(Some(Row::from_values_with_id(
                        id,
                        &self.column_names,
                        &values,
                    )))
                }
                // deleted since the snapshot
                None => continue,
            }
        }
        Ok(None)
    }
}

impl Iterator for TableRowIter<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        if self.failed {
            return None;
        }
        match self.next_row() {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => None,
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// cursor iteration (`Cursor.iterator()` / `Cursor.stream()`)
// ---------------------------------------------------------------------------

/// An iterator over a cursor yielding [`Row`] values (`Cursor extends
/// Iterable<Row>`).
///
/// The iterator owns the cursor and borrows the database it traverses, so it is
/// the Rust form of Java's `for (Row row : cursor)`: it starts before the first
/// row and moves forward.  While the iterator is alive the cursor cannot be
/// used directly; [`CursorRowIter::cursor_mut`] gives access for the current
/// row (e.g. to update it while iterating), like a Java iterator over a
/// modifiable table.
///
/// The item type is `Result<Row>` because a row read may fail (the cursor's
/// [`crate::table::ErrorHandler`] is consulted).
pub struct CursorRowIter<'a> {
    db: &'a mut Database,
    cursor: Box<dyn Cursor>,
    started: bool,
    failed: bool,
}

impl<'a> CursorRowIter<'a> {
    /// Wraps an owned cursor, positioned before its first row.
    pub fn new(db: &'a mut Database, mut cursor: Box<dyn Cursor>) -> CursorRowIter<'a> {
        cursor.before_first();
        CursorRowIter {
            db,
            cursor,
            started: false,
            failed: false,
        }
    }

    /// Wraps a concrete cursor type (the [`crate::cursor::TableScanCursor`] /
    /// [`crate::cursor::IndexCursor`] convenience).
    pub fn with_cursor<C: Cursor + 'static>(db: &'a mut Database, cursor: C) -> CursorRowIter<'a> {
        CursorRowIter::new(db, Box::new(cursor))
    }

    /// The name of the table the cursor traverses.
    pub fn table_name(&self) -> &str {
        self.cursor.table_name()
    }

    /// The wrapped cursor (`Cursor` itself).
    pub fn cursor(&self) -> &dyn Cursor {
        self.cursor.as_ref()
    }

    /// The wrapped cursor, mutably: for instance to update or delete the current
    /// row after returning it.
    pub fn cursor_mut(&mut self) -> &mut dyn Cursor {
        self.cursor.as_mut()
    }

    /// `Iterator.next`: moves to the next row and returns it, or `None` at the
    /// end of the traversal.
    pub fn next_row(&mut self) -> Result<Option<Row>> {
        self.started = true;
        if !self.cursor.move_to_next_row(self.db)? {
            return Ok(None);
        }
        self.cursor.get_current_row_object(self.db).map(Some)
    }

    /// Whether the first [`CursorRowIter::next_row`] call has happened.
    pub fn is_started(&self) -> bool {
        self.started
    }
}

impl Iterator for CursorRowIter<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        if self.failed {
            return None;
        }
        match self.next_row() {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => None,
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// shared row construction
// ---------------------------------------------------------------------------

/// Builds a row from a cursor/table row's positional values, restricted to the
/// given column names when one is given.
///
/// Java's `TableImpl.getRow(RowState, RowId, Collection)` iterates the table's
/// columns and includes the requested ones, so the resulting `RowImpl` holds
/// the requested columns in **table** column order (not in the requested order).
/// The subset matching is case insensitive, like the port's `*_columns`
/// cursor accessors.
pub(crate) fn row_from_selected_values(
    id: RowId,
    columns: &[String],
    values: &[Value],
    selected: Option<&[&str]>,
) -> Row {
    match selected {
        None => Row::from_values_with_id(id, columns, values),
        Some(names) => Row::with_entries(
            Some(id),
            columns.iter().filter_map(|column| {
                let pos = names
                    .iter()
                    .position(|name| name.eq_ignore_ascii_case(column))?;
                Some((
                    column.clone(),
                    values.get(pos).cloned().unwrap_or(Value::Null),
                ))
            }),
        ),
    }
}

// ---------------------------------------------------------------------------
// Table conveniences (the instance methods which need no database)
// ---------------------------------------------------------------------------

impl Table {
    /// `Table.asRow(Map)`: converts a column name -> value map to the positional
    /// row values of this table (`Table.addRow(Object...)` /
    /// `Cursor.updateCurrentRow(Object...)` input).
    ///
    /// The names are matched case sensitively, like Java's `containsKey`; a
    /// column the map does not name stays [`Value::Null`] and a map key which
    /// names no column is ignored.
    pub fn as_row(&self, row_map: &BTreeMap<String, Value>) -> Result<Vec<Value>> {
        let mut row = vec![Value::Null; self.columns.len()];
        for column in &self.columns {
            if let Some(value) = row_map.get(&column.name) {
                row[column.column_index] = value.clone();
            }
        }
        Ok(row)
    }

    /// `Table.asUpdateRow(Map)`: like [`Table::as_row`], but a column the map
    /// does not name is [`Value::Keep`] (`Column.KEEP_VALUE`), so an update
    /// keeps its old value.
    pub fn as_update_row(&self, row_map: &BTreeMap<String, Value>) -> Result<Vec<Value>> {
        let mut row = vec![Value::Keep; self.columns.len()];
        for column in &self.columns {
            if let Some(value) = row_map.get(&column.name) {
                row[column.column_index] = value.clone();
            }
        }
        Ok(row)
    }

    /// `Table.newCursor()`: a [`CursorBuilder`] for this table.
    ///
    /// The builder only holds the table name (like the port's other cursor
    /// builders), so it is resolved with
    /// [`CursorBuilder::to_cursor`](crate::cursor::CursorBuilder::to_cursor)
    /// when a [`Database`] is available.
    pub fn new_cursor(&self) -> CursorBuilder {
        CursorBuilder::new(&self.name)
    }

    /// `Table.getDefaultCursor()`: the cursor which backs the table's row
    /// traversal (Java caches a simple table scan cursor).
    ///
    /// The Rust port's cursors are stateless, so this returns the same fresh
    /// table scan [`CursorBuilder`] as [`Table::new_cursor`] (a builder, not a
    /// cached cursor instance); pass it a [`Database`] to obtain the cursor.
    pub fn default_cursor(&self) -> CursorBuilder {
        CursorBuilder::new(&self.name)
    }

    /// `Table.iterator()` / `Table.stream()`: an iterator over the rows of the
    /// named table, decoded into [`Row`] values.
    ///
    /// This is an associated function because a Rust `Table` does not hold the
    /// [`Database`] it was read from; use [`Database::row_iter`] for the method
    /// form.
    pub fn row_iter<'a>(db: &'a mut Database, table_name: &str) -> Result<TableRowIter<'a>> {
        TableRowIter::new(db, table_name)
    }

    /// `Table.stream().collect()`: every row of the named table as [`Row`]s,
    /// in table order.
    pub fn stream(db: &mut Database, table_name: &str) -> Result<Vec<Row>> {
        db.rows(table_name)
    }

    /// `Table.addRow(Row)` for a database handle: see
    /// [`Database::add_row_object`].  The Rust `Table` cannot perform the write
    /// itself (it holds no `Database`), so the database is passed explicitly.
    pub fn add_row_object(db: &mut Database, table_name: &str, row: Row) -> Result<Row> {
        db.add_row_object(table_name, row)
    }

    /// `Table.addRowFromMap(Map)`: see [`Database::add_row_from_map`].
    pub fn add_row_from_map(
        db: &mut Database,
        table_name: &str,
        row: &BTreeMap<String, Value>,
    ) -> Result<Row> {
        db.add_row_from_map(table_name, row)
    }

    /// `Table.addRowsFromMaps(List)`: see [`Database::add_rows_from_maps`].
    pub fn add_rows_from_maps(
        db: &mut Database,
        table_name: &str,
        rows: &[BTreeMap<String, Value>],
    ) -> Result<Vec<Row>> {
        db.add_rows_from_maps(table_name, rows)
    }

    /// `Table.updateRow(Row)`: see [`Database::update_row_object`].
    pub fn update_row_object(db: &mut Database, table_name: &str, row: Row) -> Result<Row> {
        db.update_row_object(table_name, row)
    }

    /// `Table.deleteRow(Row)`: see [`Database::delete_row_object`].
    pub fn delete_row_object(db: &mut Database, table_name: &str, row: Row) -> Result<Row> {
        db.delete_row_object(table_name, row)
    }
}

// ---------------------------------------------------------------------------
// Database conveniences (the `Table` methods which need the database)
// ---------------------------------------------------------------------------

impl Database {
    /// The column names of the table in storage order.
    fn table_column_names(&mut self, table_name: &str) -> Result<Vec<String>> {
        let table = self.get_table(table_name)?;
        Ok(table.columns.iter().map(|c| c.name.clone()).collect())
    }

    /// The values of the given maps converted with the table's
    /// [`Table::as_row`].
    fn rows_for_maps(
        &mut self,
        table_name: &str,
        rows: &[BTreeMap<String, Value>],
    ) -> Result<Vec<Vec<Value>>> {
        let table = self.get_table(table_name)?;
        rows.iter().map(|row| table.as_row(row)).collect()
    }

    /// The row ids which [`Database::add_rows`] created, in storage order.
    fn new_row_ids(before: &[(RowId, bool)], after: &[(RowId, bool)]) -> Vec<RowId> {
        let before: std::collections::HashSet<(RowId, bool)> = before.iter().copied().collect();
        after
            .iter()
            .filter(|slot| !before.contains(slot))
            .map(|(row_id, _)| *row_id)
            .collect()
    }

    /// Reads the rows of the given ids back as [`Row`]s.
    fn read_row_objects(&mut self, table_name: &str, ids: &[RowId]) -> Result<Vec<Row>> {
        let columns = self.table_column_names(table_name)?;
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            let values = self
                .read_row_by_id_using(table_name, *id, None)?
                .ok_or_else(|| {
                    Error::IllegalState(format!("Row is deleted: ({}, {})", id.0, id.1))
                })?;
            rows.push(Row::from_values_with_id(*id, &columns, &values));
        }
        Ok(rows)
    }

    /// The values of a [`Row`] converted with the table's [`Table::as_row`]
    /// (a row read from another table, or a partial row, still only contributes
    /// the columns the table defines).
    pub fn as_row_values(&mut self, table_name: &str, row: &Row) -> Result<Vec<Value>> {
        let table = self.get_table(table_name)?;
        table.as_row(&row.to_map())
    }

    /// `Table.addRow(Row)`: adds the row and returns the row as it is now
    /// stored, with its (generated) [`RowId`] and any generated auto-number /
    /// calculated value.
    ///
    /// This is the row-object form of the existing
    /// [`Database::add_rows`](Database::add_rows) (which is unchanged): the row
    /// values are built with [`Table::as_row`] and written through exactly the
    /// same path, so the bytes on disk are identical.  The new row id is
    /// discovered by comparing the table's row slots before and after the
    /// write, which also resolves a linked table correctly.
    pub fn add_row_object(&mut self, table_name: &str, row: Row) -> Result<Row> {
        let values = self.as_row_values(table_name, &row)?;
        self.add_row_values_object(table_name, values)
    }

    /// [`Database::add_row_object`] for an already positional value vector
    /// (used by [`Database::add_row_from_map`]).
    fn add_row_values_object(&mut self, table_name: &str, values: Vec<Value>) -> Result<Row> {
        let before = self.row_id_statuses(table_name)?;
        self.add_rows(table_name, &[values])?;
        let after = self.row_id_statuses(table_name)?;
        let ids = Self::new_row_ids(&before, &after);
        match ids.as_slice() {
            [id] => {
                let mut rows = self.read_row_objects(table_name, &[*id])?;
                Ok(rows.remove(0))
            }
            _ => Err(Error::Jackcess(format!(
                "Could not determine the row id of the row added to table {}",
                table_name
            ))),
        }
    }

    /// `Table.addRowFromMap(Map)`: [`Table::as_row`] followed by
    /// [`Database::add_row_object`].
    ///
    /// Java updates the given map in place with the generated auto numbers and
    /// returns it; the Rust port consumes nothing and returns the stored row
    /// (which carries the values and the id) instead.
    pub fn add_row_from_map(
        &mut self,
        table_name: &str,
        row: &BTreeMap<String, Value>,
    ) -> Result<Row> {
        let values = {
            let table = self.get_table(table_name)?;
            table.as_row(row)?
        };
        self.add_row_values_object(table_name, values)
    }

    /// `Table.addRowsFromMaps(List)`: converts every map with
    /// [`Table::as_row`], adds the rows with the existing
    /// [`Database::add_rows`](Database::add_rows) (Java's `Table.addRows(List)`
    /// batch write) and returns the stored rows, in input order, with their
    /// row ids and generated values.
    pub fn add_rows_from_maps(
        &mut self,
        table_name: &str,
        rows: &[BTreeMap<String, Value>],
    ) -> Result<Vec<Row>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let values = self.rows_for_maps(table_name, rows)?;
        let before = self.row_id_statuses(table_name)?;
        self.add_rows(table_name, &values)?;
        let after = self.row_id_statuses(table_name)?;
        let ids = Self::new_row_ids(&before, &after);
        if ids.len() != values.len() {
            return Err(Error::Jackcess(format!(
                "Could not determine the row ids of the {} row(s) added to table {}",
                values.len(),
                table_name
            )));
        }
        self.read_row_objects(table_name, &ids)
    }

    /// `Table.updateRow(Row)`: updates the row identified by
    /// [`Row::get_id`] with the columns the row holds (a column it does not
    /// hold keeps its old value), and returns the stored row.
    ///
    /// Java's `Table.updateRow(Row)` mutates the given row in place and returns
    /// it; the Rust port consumes the row and returns the updated one (same
    /// values plus every column of the table).
    pub fn update_row_object(&mut self, table_name: &str, row: Row) -> Result<Row> {
        let id = row.get_id().ok_or_else(|| {
            Error::IllegalState(format!(
                "The given row has no row id and cannot be used to update table {}",
                table_name
            ))
        })?;
        let map = row.to_map();
        let values = self.update_row_from_map(table_name, id, &map)?;
        let columns = self.table_column_names(table_name)?;
        Ok(Row::from_values_with_id(id, &columns, &values))
    }

    /// `Table.deleteRow(Row)`: deletes the row identified by
    /// [`Row::get_id`] and returns the given row (`TableImpl.deleteRow(Row)`
    /// returns the row unchanged).
    pub fn delete_row_object(&mut self, table_name: &str, row: Row) -> Result<Row> {
        let id = row.get_id().ok_or_else(|| {
            Error::IllegalState(format!(
                "The given row has no row id and cannot be used to delete from table {}",
                table_name
            ))
        })?;
        self.delete_row(table_name, id)?;
        Ok(row)
    }

    /// `Table.iterator()`: an iterator over the table's rows yielding [`Row`]
    /// values (see [`TableRowIter`]).
    pub fn row_iter(&mut self, table_name: &str) -> Result<TableRowIter<'_>> {
        TableRowIter::new(self, table_name)
    }

    /// `Table.stream().collect()`: every row of the table as [`Row`]s, in table
    /// order.
    pub fn rows(&mut self, table_name: &str) -> Result<Vec<Row>> {
        let mut iter = self.row_iter(table_name)?;
        let mut rows = Vec::new();
        while let Some(row) = iter.next_row()? {
            rows.push(row);
        }
        Ok(rows)
    }

    /// Wraps an owned cursor in a [`CursorRowIter`] (`Cursor.iterator()`).
    pub fn cursor_rows<'a>(&'a mut self, cursor: Box<dyn Cursor>) -> CursorRowIter<'a> {
        CursorRowIter::new(self, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnSpec;
    use crate::cursor::TableScanCursor;
    use crate::datatype::DataType;
    use crate::format::FileFormat;
    use crate::tablecreator::{IndexSpec, TableSpec};

    fn test_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn create_test_db(name: &str) -> (std::path::PathBuf, Database) {
        let path = test_tmp_dir().join(format!(
            "jackcess_rowapi_{}_{}.mdb",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        (path, db)
    }

    /// `T(ID long autonumber pk, Name text(40) unique, Num long, Amount numeric,
    /// Flag boolean, When date, Data binary(10), Note memo)`.
    fn table_spec() -> TableSpec {
        TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
                ColumnSpec::new("Num", DataType::Long),
                ColumnSpec::new("Amount", DataType::Numeric)
                    .with_precision(18)
                    .with_scale(4),
                ColumnSpec::new("Flag", DataType::Boolean),
                ColumnSpec::new("When", DataType::ShortDateTime),
                ColumnSpec::new("Data", DataType::Binary).with_length(10),
                ColumnSpec::new("Note", DataType::Memo),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ))
        .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique())
    }

    fn make_table(db: &mut Database) {
        db.create_table(&table_spec()).expect("create T");
    }

    fn row_map(name: &str, num: i32) -> BTreeMap<String, Value> {
        let mut map = BTreeMap::new();
        map.insert("Name".to_string(), Value::Text(name.to_string()));
        map.insert("Num".to_string(), Value::Long(num));
        map.insert(
            "Amount".to_string(),
            Value::Decimal {
                unscaled: 123_4500,
                scale: 4,
            },
        );
        map.insert("Flag".to_string(), Value::Bool(true));
        map.insert(
            "When".to_string(),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000)),
        );
        map.insert(
            "Data".to_string(),
            Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        );
        map.insert("Note".to_string(), Value::Text("a memo value".to_string()));
        map
    }

    #[test]
    fn row_is_an_ordered_map() {
        let mut row = Row::new((31, 0));
        assert_eq!(row.get_id(), Some((31, 0)));
        assert!(row.is_empty());
        assert_eq!(row.insert("A", Value::Text("a".into())), None);
        assert_eq!(row.insert("B", Value::Long(2)), None);
        assert_eq!(row.insert("C", Value::Null), None);
        // replacing keeps the LinkedHashMap position
        assert_eq!(
            row.insert("A", Value::Text("aa".into())),
            Some(Value::Text("a".into()))
        );
        assert_eq!(row.keys().collect::<Vec<_>>(), vec!["A", "B", "C"]);
        assert_eq!(row.len(), 3);
        assert!(row.contains_key("B"));
        assert_eq!(row.get("A"), Some(&Value::Text("aa".into())));
        assert!(row.get("missing").is_none());
        assert_eq!(
            row.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            vec!["A", "B", "C"]
        );
        assert_eq!(row.remove("B"), Some(Value::Long(2)));
        assert!(!row.contains_key("B"));
        assert_eq!(row.remove("B"), None);
        // Map style equality ignores the id (RowImpl.equals) and the order
        let mut reordered = Row::new((99, 7));
        reordered.insert("C", Value::Null);
        reordered.insert("A", Value::Text("aa".into()));
        assert_eq!(row, reordered);
        assert_eq!(row.to_string(), "Row[31:0]{A=aa,C=null}");
        assert_eq!(Row::empty().get_id(), None);

        assert_eq!(
            Row::from_values(&["X".to_string(), "Y".to_string()], vec![Value::Long(1)])
                .to_values_named(&["Y", "X", "Z"]),
            vec![Value::Null, Value::Long(1), Value::Null]
        );
        assert_eq!(
            Row::from_map(row_map("n", 3)).to_update_values_named(&["Name", "Missing"]),
            vec![Value::Text("n".into()), Value::Keep]
        );
    }

    #[test]
    fn row_typed_getters_match_jackcess_values() {
        let mut row = Row::new((1, 2));
        row.insert("S", Value::Text("text".into()));
        row.insert("B", Value::Bool(true));
        row.insert("Y", Value::Byte(0xff));
        row.insert("H", Value::Int(-12));
        row.insert("I", Value::Long(7));
        row.insert(
            "D",
            Value::Decimal {
                unscaled: -123_456_789,
                scale: 4,
            },
        );
        row.insert("F", Value::Float(1.5));
        row.insert("X", Value::Double(2.5));
        row.insert(
            "T",
            Value::DateTime(LocalDateTime::new(2020, 1, 2, 3, 4, 5, 6)),
        );
        row.insert("G", Value::Uuid([0x01; 16]));
        row.insert("V", Value::Bytes(vec![1, 2, 3]));
        row.insert("K", Value::Long(42));
        row.insert("N", Value::Null);
        row.insert("DateBits", Value::DateBits(2.5));

        assert_eq!(row.get_string("S").unwrap(), Some("text".to_string()));
        assert_eq!(
            row.get_string("G").unwrap(),
            Some("{01010101-0101-0101-0101-010101010101}".to_string())
        );
        assert_eq!(row.get_boolean("B").unwrap(), Some(true));
        assert_eq!(row.get_byte("Y").unwrap(), Some(-1));
        assert_eq!(row.get_short("H").unwrap(), Some(-12));
        assert_eq!(row.get_int("I").unwrap(), Some(7));
        assert_eq!(
            row.get_big_decimal("D").unwrap(),
            Some(BigDecimal::new(BigInt::from(-123_456_789i64), 4))
        );
        assert_eq!(row.get_float("F").unwrap(), Some(1.5));
        assert_eq!(row.get_double("X").unwrap(), Some(2.5));
        assert_eq!(
            row.get_local_date_time("T").unwrap(),
            Some(LocalDateTime::new(2020, 1, 2, 3, 4, 5, 6))
        );
        assert_eq!(row.get_bytes("V").unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(row.get_foreign_key("K").unwrap(), Some(42));
        assert_eq!(row.get_date("DateBits").unwrap(), Some(2.5));
        assert_eq!(
            row.get_date_millis("DateBits", &crate::datenums::TimeZone::utc())
                .unwrap(),
            Some(crate::datenums::from_date_double(
                2.5,
                &crate::datenums::TimeZone::utc()
            ))
        );
        let blob = row.get_blob("V").unwrap().expect("blob");
        assert_eq!(blob.length(), 3);
        // null values and missing columns are null (Java's get returns null)
        assert_eq!(row.get_string("N").unwrap(), None);
        assert_eq!(row.get_int("missing").unwrap(), None);
        // a wrong value type is an error (Java's ClassCastException)
        assert!(row.get_string("I").is_err());
        assert!(row.get_int("S").is_err());
        assert!(row.get_date("T").is_err());
        assert!(row.get_local_date_time("DateBits").is_err());

        let values: Vec<Value> =
            RowValues::from((row.clone(), vec!["S".to_string(), "N".to_string()])).into_vec();
        assert_eq!(values, vec![Value::Text("text".into()), Value::Null]);
        // the `From<Vec<Value>>` direction needs the column list to name the
        // values
        let rebuilt = Row::from((vec![Value::Long(1), Value::Long(2)], &["A", "B"][..]));
        assert_eq!(
            rebuilt.to_values_named(&["A", "B"]),
            vec![Value::Long(1), Value::Long(2)]
        );
        assert_eq!(rebuilt.get_id(), None);
        let reversed = Row::from((&["A", "B"][..], vec![Value::Long(1), Value::Long(2)]));
        assert_eq!(reversed.get_int("B").unwrap(), Some(2));
        // `From<Row> for Vec<Value>` keeps the row order
        let plain: Vec<Value> = reversed.into();
        assert_eq!(plain, vec![Value::Long(1), Value::Long(2)]);
    }

    #[test]
    fn table_as_row_and_as_update_row() {
        let (_path, mut db) = create_test_db("as_row");
        make_table(&mut db);

        let map = row_map("one", 11);
        let table = db.get_table("T").expect("table");
        let row = table.as_row(&map).unwrap();
        assert_eq!(row.len(), table.columns.len());
        // every column named by the map is in its positional slot
        let id_index = table.column_index("ID").unwrap();
        let name_index = table.column_index("Name").unwrap();
        assert_eq!(row[id_index], Value::Null);
        assert_eq!(row[name_index], Value::Text("one".into()));

        let update = table.as_update_row(&map).unwrap();
        assert_eq!(update[id_index], Value::Keep);
        assert_eq!(update[name_index], Value::Text("one".into()));

        // an unknown key is ignored, an unnamed column stays null
        let mut partial = BTreeMap::new();
        partial.insert("Num".to_string(), Value::Long(5));
        partial.insert("Bogus".to_string(), Value::Long(9));
        let partial_row = table.as_row(&partial).unwrap();
        assert_eq!(
            partial_row[table.column_index("Num").unwrap()],
            Value::Long(5)
        );
        assert_eq!(
            partial_row[table.column_index("Name").unwrap()],
            Value::Null
        );
        assert_eq!(db.get_table("T").unwrap().get_row_count(), 0);
    }

    #[test]
    fn add_row_object_returns_the_stored_row_and_its_id() {
        let (_path, mut db) = create_test_db("add_object");
        make_table(&mut db);

        let map = row_map("alpha", 1);
        let added = db.add_row_from_map("T", &map).expect("add row");
        let id = added.get_id().expect("row id");
        assert!(id.0 > 0 && id.1 >= 0);
        // the generated auto number is in the returned row
        let generated = added.get_int("ID").unwrap().expect("auto number");
        assert!(generated > 0);
        assert_eq!(added.get_string("Name").unwrap().as_deref(), Some("alpha"));
        assert_eq!(added.get_int("Num").unwrap(), Some(1));
        assert_eq!(
            added.get_big_decimal("Amount").unwrap(),
            Some(BigDecimal::new(BigInt::from(123_4500i64), 4))
        );
        assert_eq!(added.get_boolean("Flag").unwrap(), Some(true));
        assert_eq!(
            added.get_local_date_time("When").unwrap(),
            Some(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000))
        );
        assert_eq!(
            added.get_bytes("Data").unwrap(),
            Some(vec![0xde, 0xad, 0xbe, 0xef])
        );
        assert_eq!(
            added.get_string("Note").unwrap().as_deref(),
            Some("a memo value")
        );

        // the same row is read back by id and through the iterator
        let rows = db.rows("T").expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], added);
        assert_eq!(rows[0].get_id(), Some(id));
        assert_eq!(db.get_table("T").unwrap().get_row_count(), 1);

        // two more through the batch map path, in input order
        let maps = vec![row_map("beta", 2), row_map("gamma", 3)];
        let added_more = db.add_rows_from_maps("T", &maps).expect("add rows");
        assert_eq!(added_more.len(), 2);
        assert_eq!(
            added_more
                .iter()
                .map(|r| r.get_string("Name").unwrap().unwrap())
                .collect::<Vec<_>>(),
            vec!["beta".to_string(), "gamma".to_string()]
        );
        assert_eq!(db.get_table("T").unwrap().get_row_count(), 3);
        assert!(db.add_rows_from_maps("T", &[]).unwrap().is_empty());
    }

    #[test]
    fn update_and_delete_row_object() {
        let (_path, mut db) = create_test_db("update_object");
        make_table(&mut db);
        let first = db.add_row_from_map("T", &row_map("one", 1)).unwrap();
        let second = db.add_row_from_map("T", &row_map("two", 2)).unwrap();

        // a partial row: only the named columns change (Table.asUpdateRow)
        let mut partial = Row::new(first.get_id().expect("id"));
        partial.insert("Num", Value::Long(77));
        let updated = db.update_row_object("T", partial).expect("update");
        assert_eq!(updated.get_id(), first.get_id());
        assert_eq!(updated.get_int("Num").unwrap(), Some(77));
        // the unnamed columns keep their old values
        assert_eq!(updated.get_string("Name").unwrap().as_deref(), Some("one"));
        assert_eq!(updated.get_int("ID").unwrap(), first.get_int("ID").unwrap());

        let deleted = db.delete_row_object("T", second).expect("delete");
        assert_eq!(deleted.get_string("Name").unwrap().as_deref(), Some("two"));
        let rows = db.rows("T").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_id(), first.get_id());

        // a row without an id cannot be used for an update/delete
        assert!(db.update_row_object("T", Row::empty()).is_err());
        assert!(db.delete_row_object("T", Row::empty()).is_err());
    }

    #[test]
    fn table_and_cursor_iterators_yield_rows() {
        let (_path, mut db) = create_test_db("iterators");
        make_table(&mut db);
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            db.add_row_from_map("T", &row_map(name, i as i32)).unwrap();
        }

        // Table.iterator / Table.stream
        let mut iter = Table::row_iter(&mut db, "T").expect("iter");
        let mut names = Vec::new();
        while let Some(row) = iter.next_row().expect("next") {
            assert!(row.get_id().is_some());
            names.push(row.get_string("Name").unwrap().unwrap());
        }
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(Table::stream(&mut db, "T").unwrap().len(), 3);
        let collected: Vec<Row> = db
            .row_iter("T")
            .unwrap()
            .collect::<crate::error::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(collected.len(), 3);
        assert_eq!(
            collected[2].get_string("Name").unwrap().as_deref(),
            Some("c")
        );

        // Cursor.iterator / Cursor.stream and the row-object getters
        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        assert!(cursor.new_iterable().is_reset());
        let next = cursor.get_next_row_object(&mut db).unwrap().expect("row");
        assert_eq!(next.get_string("Name").unwrap().as_deref(), Some("a"));
        assert_eq!(
            cursor
                .get_current_row_object_columns(&mut db, &["Name"])
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["Name"]
        );
        assert_eq!(
            cursor.get_next_row_subset(&mut db, &["Name"]).unwrap(),
            Some(vec![Value::Text("b".into())])
        );
        assert_eq!(
            cursor
                .get_previous_row_object(&mut db)
                .unwrap()
                .expect("row")
                .get_string("Name")
                .unwrap()
                .as_deref(),
            Some("a")
        );
        let streamed = cursor.stream(&mut db).unwrap();
        assert_eq!(streamed.len(), 3);

        // the CursorRowIter owns a cursor and borrows the database
        let cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        let mut rows = cursor.row_iter(&mut db);
        let mut ids = Vec::new();
        while let Some(row) = rows.next().transpose().expect("row") {
            assert!(rows.cursor().table_name() == "T");
            ids.push(row.get_id().unwrap());
        }
        assert_eq!(ids.len(), 3);

        // the update helper updates the current row from a Row's columns
        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        cursor.move_to_next_row(&mut db).unwrap();
        let mut change = Row::empty();
        change.insert("Num", Value::Long(99));
        let updated = cursor.update_current_row_object(&mut db, &change).unwrap();
        assert_eq!(updated.get_int("Num").unwrap(), Some(99));
    }

    #[test]
    fn table_row_iter_skips_deleted_rows() {
        let (_path, mut db) = create_test_db("iter_snapshot");
        make_table(&mut db);
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            db.add_row_from_map("T", &row_map(name, i as i32)).unwrap();
        }
        let second = db.rows("T").unwrap().remove(1);
        db.delete_row_object("T", second.clone()).unwrap();

        let rows = db.rows("T").unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.get_string("Name").unwrap().unwrap())
                .collect::<Vec<_>>(),
            vec!["a".to_string(), "c".to_string()]
        );
        // the iterator snapshot was taken after the delete: still two rows
        let mut iter = db.row_iter("T").unwrap();
        let mut count = 0;
        while iter.next_row().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 2);
        // a row deleted before the snapshot is skipped by the snapshot itself
        assert_eq!(db.rows("T").unwrap().len(), 2);
    }
}
