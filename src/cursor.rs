//! Port of Jackcess' cursor APIs (`Cursor`, `TableScanCursor`, `IndexCursor`,
//! `IndexCursorImpl`, `EntryIterableBuilder`): table scans and index lookups
//! with the update/delete operations on the current row.
//!
//! The cursors are stateless with respect to the database: every operation
//! takes the [`Database`] it works on, so a cursor never keeps a borrow of the
//! database and multiple cursors may traverse the same table.  The current
//! position is always the database row id (for a table scan) or the index entry
//! key/row id pair (for an index cursor), which keeps the cursor usable across
//! row and index modifications (the position is re-located when it is needed,
//! the equivalent of the Java `EntryCursor` modification tracking).
//!
//! # Matching values
//!
//! [`TableScanCursor`] defaults to [`ColumnMatcher::Simple`], like
//! `CursorImpl._columnMatcher` (`SimpleColumnMatcher.INSTANCE`), and an
//! [`IndexCursor`] over a unique index defaults to
//! [`ColumnMatcher::CaseInsensitive`] (which is what jackcess'
//! `IndexCursorImpl.getDefaultColumnMatcher` does).  An index cursor over a
//! non-unique index defaults to [`ColumnMatcher::Simple`], like jackcess.
//!
//! # Ranges, savepoints and iterables
//!
//! [`IndexCursor`] supports the range-bounded traversal of `CursorBuilder`
//! (`setStartRow`/`setStartEntry`/`setEndRow`/`setEndEntry` plus the inclusive
//! flags), see [`IndexRange`] and [`CursorBuilder`].  An [`IndexCursor`] keeps
//! its bounds as the two *boundary index entries* jackcess builds
//! (`IndexData.cursor(startRow, startInclusive, endRow, endInclusive)`), so the
//! cursor position is always an index entry - either a real entry or one of the
//! boundary entries, whose row id is invalid.  That is exactly what
//! `CursorImpl.PositionImpl`/`IndexData.EntryCursor` model, and it reproduces
//! jackcess' behaviour for reversed ranges, empty ranges, and boundary
//! positions (including the `getRowId` of a boundary position, which is
//! `FIRST_ROW_ID`/`LAST_ROW_ID` depending on the inclusive flag).  Like jackcess
//! 5.0.4, the range applies to index cursors only: a [`TableScanCursor`]
//! ignores it (`CursorBuilder.toCursor` only passes the range to
//! `IndexCursorImpl.createCursor`).
//!
//! [`Savepoint`] ports `Cursor.getSavepoint`/`restoreSavepoint` (5.0.4 has no
//! `deleteSavepoint`; a savepoint is a plain value which is simply dropped).
//! That includes the cursor id check, the "the restored position becomes the
//! previous position" quirk of `CursorImpl.restorePositionImpl`, and the
//! rejection of a boundary position of a cursor with a different range
//! (`EntryCursor.updatePosition`).  [`IterableBuilder`]/[`EntryIterableBuilder`]
//! port the `IterableBuilder`/`EntryIterableBuilder` iterators:
//! forward/reverse traversal, a subset of columns, column/row match patterns,
//! by-entry iteration and `remove` (delete the row the iterator is on).  A
//! [`RowIterator`]/[`EntryRowIterator`] leaves the cursor on the row it last
//! returned, so the caller can update that row through the cursor while
//! iterating.
//!
//! # Error handlers
//!
//! [`Cursor::set_error_handler`] installs a cursor level
//! [`ErrorHandler`](crate::table::ErrorHandler) which takes precedence over the
//! table and database handlers for the rows the cursor reads
//! (`CursorImpl.setErrorHandler`).  Moving the cursor only inspects row
//! positions, so it never invokes the handler; reading a row does.
//!
//! # The `Row` conveniences (`Cursor extends Iterable<Row>`)
//!
//! The positional `Vec<Value>` accessors above are the port's established
//! entry points (kept unchanged).  On top of them the trait provides the Java
//! `Row` surface as *provided* methods, so no implementor changes:
//!
//! * [`Cursor::get_current_row_object`] /
//!   [`Cursor::get_current_row_object_columns`] (`getCurrentRow` /
//!   `getCurrentRow(Collection<String>)`), and the `get_next_*` /
//!   `get_previous_*` pairs; the returned [`Row`] carries the cursor's row id
//!   and its columns in **table** order (Java's `TableImpl.getRow` builds the
//!   `RowImpl` by iterating the table columns, filtering the requested ones).
//! * [`Cursor::update_current_row_object`] (`updateCurrentRowFromMap(Map)` with
//!   a `Row`), which updates only the columns the row holds and returns the
//!   stored row.
//! * [`Cursor::get_current_row_subset`], [`Cursor::get_next_row_subset`] and
//!   [`Cursor::get_previous_row_subset`]: explicit aliases for the
//!   `(Collection<String>)` overloads of the positional getters (the
//!   `*_columns` methods are the primary Rust names).
//! * [`Cursor::new_iterable`] (`newIterable`; the returned
//!   [`IterableBuilder`] does not capture the cursor, so it is used together
//!   with the cursor, see [`IterableBuilder::rows_object`]) and
//!   [`IndexCursor::new_entry_iterable`] (`newEntryIterable`).
//! * [`Cursor::stream`] (`stream().collect()`), and the lazy
//!   [`CursorRowIter`] behind [`TableScanCursor::row_iter`] /
//!   [`IndexCursor::row_iter`] (plus the eager `rows()` helpers).
//!
//! Java's `findFirstRow(Column, Object)` / `findNextRow(Column, Object)` /
//! `currentRowMatches(Column, Object)` are the existing
//! [`Cursor::find_first_row_by_column`], [`Cursor::find_next_row_by_column`]
//! and [`Cursor::current_row_matches_column`] (the `Column` parameter is the
//! column name, since the port resolves columns by name).

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::column::Column;
use crate::database::Database;
use crate::datatype::DataType;
use crate::error::{Error, Result};
use crate::index::{Entry, EntryValue, IndexColumn, IndexImpl, RowId as IndexRowId};
use crate::row::{row_from_selected_values, CursorRowIter, Row};
use crate::table::{ErrorHandler, IndexFeature, RowId, RowLocation};
use crate::value::Value;

/// The row id of the "before first" cursor position
/// (`RowIdImpl.FIRST_ROW_ID`).
pub const FIRST_ROW_ID: RowId = (-1, -1);
/// The row id of the "after last" cursor position
/// (`RowIdImpl.LAST_ROW_ID`).
pub const LAST_ROW_ID: RowId = (-2, -1);

/// The value matcher of a cursor (`ColumnMatcher`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColumnMatcher {
    /// `SimpleColumnMatcher`: `Objects.equals` with byte array equality and a
    /// numeric coercion for differently typed numbers.
    #[default]
    Simple,
    /// `CaseInsensitiveColumnMatcher`: textual columns (`TEXT`, `MEMO`) are
    /// compared case-insensitively, all other columns with [`Self::Simple`].
    CaseInsensitive,
}

/// Identifier for a cursor (`Cursor.Id`).  Will be equal to any other cursor of
/// the same type for the same table, which is what makes a [`Savepoint`] usable
/// for another cursor instance of the same table/index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorId {
    /// the page number of the table definition
    /// (`TableImpl.getTableDefPageNumber`)
    pub table_page: i32,
    /// the logical index number, or `-1` for a table scan
    /// (`IdImpl._indexNumber`)
    pub index_number: i32,
}

/// A cursor position as stored in a [`Savepoint`] (`Cursor.Position`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SavedPos {
    /// A table scan cursor position (`TableScanCursor.ScanPosition`).
    Scan(ScanPos),
    /// An index cursor position (`IndexCursorImpl.IndexPosition`): the index
    /// entry the cursor is on, which may be one of the range boundary entries
    /// (whose row id is invalid).
    Index(Entry),
}

impl SavedPos {
    /// `Cursor.Position.getRowId`.
    pub fn get_row_id(&self) -> RowId {
        match self {
            SavedPos::Scan(pos) => match pos {
                ScanPos::BeforeFirst => FIRST_ROW_ID,
                ScanPos::AfterLast => LAST_ROW_ID,
                ScanPos::Row(id) => *id,
            },
            SavedPos::Index(entry) => to_table_row_id(*entry.get_row_id()),
        }
    }
}

/// `Cursor.Savepoint`: the complete save state of a cursor (its id and its
/// current and previous positions).
///
/// A savepoint is created by [`Cursor::get_savepoint`] and restored by
/// [`Cursor::restore_savepoint`].  Savepoints may be used across different
/// cursor instances for the same table/index, but the cursor ids must match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Savepoint {
    id: CursorId,
    cur: SavedPos,
    prev: SavedPos,
}

impl Savepoint {
    fn new(id: CursorId, cur: SavedPos, prev: SavedPos) -> Savepoint {
        Savepoint { id, cur, prev }
    }

    /// `Cursor.Savepoint.getCursorId`.
    pub fn get_cursor_id(&self) -> CursorId {
        self.id
    }

    /// `Cursor.Savepoint.getCurrentPosition`.
    pub fn get_current_position(&self) -> &SavedPos {
        &self.cur
    }

    /// The row id of the saved position (`Cursor.Position.getRowId`).
    pub fn get_current_row_id(&self) -> RowId {
        self.cur.get_row_id()
    }
}

/// The public cursor API: the common operations of `Cursor`.
///
/// Every operation which needs to read or write the database takes the
/// [`Database`] it belongs to.  The cursor is positioned before the first row
/// when it is created.
pub trait Cursor {
    /// `Cursor.reset`: resets for forward traversal (calls `beforeFirst`).
    fn reset(&mut self) {
        self.before_first();
    }

    /// `Cursor.getSavepoint`: the complete current state of this cursor.
    fn get_savepoint(&self) -> Savepoint;

    /// `Cursor.restoreSavepoint`: moves the cursor back to a state previously
    /// returned from [`Cursor::get_savepoint`].
    ///
    /// Like `CursorImpl.restoreSavepoint`, the restored state must belong to
    /// this cursor (same table, same index), and the position the cursor was on
    /// becomes the previous position (`restorePositionImpl`).
    fn restore_savepoint(&mut self, savepoint: &Savepoint) -> Result<()>;

    /// `Cursor.beforeFirst`: sets the cursor before the first row.
    fn before_first(&mut self);

    /// `Cursor.afterLast`: sets the cursor after the last row.
    fn after_last(&mut self);

    /// `Cursor.isBeforeFirst`.
    fn is_before_first(&self) -> bool;

    /// `Cursor.isAfterLast`.
    fn is_after_last(&self) -> bool;

    /// The currently configured `ColumnMatcher`.
    fn get_column_matcher(&self) -> ColumnMatcher;

    /// Sets the `ColumnMatcher` (see [`Cursor::set_column_matcher`] in Java).
    fn set_column_matcher(&mut self, matcher: ColumnMatcher);

    /// The cursor level [`ErrorHandler`] override (`Cursor.getErrorHandler`'s
    /// own value).  Java returns the effective handler (the cursor's, or the
    /// table's); the Rust cursor does not keep a database reference, so this
    /// returns `None` when the table/database handler applies.  Row reads
    /// resolve the effective handler through the database.
    fn get_error_handler(&self) -> Option<&Arc<dyn ErrorHandler>>;

    /// `Cursor.setErrorHandler`: installs a cursor level handler.  `None`
    /// resets to the table level handler (and thus the database level one).
    fn set_error_handler(&mut self, new_error_handler: Option<Arc<dyn ErrorHandler>>);

    /// The row id of the current position.  The "before first" position uses
    /// [`FIRST_ROW_ID`] and the "after last" position [`LAST_ROW_ID`], both
    /// with an invalid row id, like `RowIdImpl.FIRST_ROW_ID`/`LAST_ROW_ID`.
    fn get_current_row_id(&self) -> RowId;

    /// The name of the owning table (`Cursor.getTable().getName()`).  The
    /// cursor keeps no reference to the owning [`crate::table::Table`] (its
    /// operations take the [`Database`] instead), so only the name is exposed;
    /// a full `getTable()` is intentionally out of scope.
    fn table_name(&self) -> &str;

    /// `Cursor.getId`: the id of this cursor.  Two cursors of the same table
    /// and index share an id (which is what makes a [`Savepoint`] usable
    /// across cursor instances and is also available as
    /// [`Savepoint::get_cursor_id`]).
    fn get_id(&self) -> CursorId;

    /// `Table.getRowCount`: the row count stored in the table definition.
    fn get_row_count(&mut self, db: &mut Database) -> Result<usize>;

    /// `Cursor.isCurrentRowDeleted`: whether the current row is an existing
    /// (not invalid) row which has been deleted.
    fn is_current_row_deleted(&mut self, db: &mut Database) -> Result<bool>;

    /// Whether the cursor is on a valid, non-deleted, existing row.
    fn is_current_row_valid(&mut self, db: &mut Database) -> Result<bool>;

    /// Moves to the next row as defined by the cursor.
    fn move_to_next_row(&mut self, db: &mut Database) -> Result<bool>;

    /// Moves to the previous row as defined by the cursor.
    fn move_to_previous_row(&mut self, db: &mut Database) -> Result<bool>;

    /// `Cursor.moveNextRows`: the number of rows actually moved.
    fn move_next_rows(&mut self, db: &mut Database, num_rows: usize) -> Result<usize> {
        let mut moved = 0;
        while moved < num_rows && self.move_to_next_row(db)? {
            moved += 1;
        }
        Ok(moved)
    }

    /// `Cursor.movePreviousRows`: the number of rows actually moved.
    fn move_previous_rows(&mut self, db: &mut Database, num_rows: usize) -> Result<usize> {
        let mut moved = 0;
        while moved < num_rows && self.move_to_previous_row(db)? {
            moved += 1;
        }
        Ok(moved)
    }

    /// `Cursor.getNextRow`: moves to the next row and returns it, or `None`.
    fn get_next_row(&mut self, db: &mut Database) -> Result<Option<Vec<Value>>> {
        if self.move_to_next_row(db)? {
            Ok(Some(self.get_current_row(db)?))
        } else {
            Ok(None)
        }
    }

    /// `Cursor.getPreviousRow`: moves to the previous row and returns it.
    fn get_previous_row(&mut self, db: &mut Database) -> Result<Option<Vec<Value>>> {
        if self.move_to_previous_row(db)? {
            Ok(Some(self.get_current_row(db)?))
        } else {
            Ok(None)
        }
    }

    /// `Cursor.getNextRow(Collection)`: moves to the next row and returns only
    /// the named columns, in the order the names were given.
    fn get_next_row_columns(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Option<Vec<Value>>> {
        if self.move_to_next_row(db)? {
            Ok(Some(self.get_current_row_columns(db, column_names)?))
        } else {
            Ok(None)
        }
    }

    /// `Cursor.getPreviousRow(Collection)`: the previous row, restricted to the
    /// named columns.
    fn get_previous_row_columns(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Option<Vec<Value>>> {
        if self.move_to_previous_row(db)? {
            Ok(Some(self.get_current_row_columns(db, column_names)?))
        } else {
            Ok(None)
        }
    }

    /// `Cursor.getCurrentRow`: the current row in table column order.
    fn get_current_row(&mut self, db: &mut Database) -> Result<Vec<Value>>;

    /// `Cursor.getCurrentRow(Collection<String>)`: the named columns of the
    /// current row, in the order the names were given.
    fn get_current_row_columns(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Vec<Value>>;

    /// The current row as a column name to value map (table column order, like
    /// Java's `Row`).
    fn get_current_row_as_map(&mut self, db: &mut Database) -> Result<BTreeMap<String, Value>>;

    /// `Cursor.getCurrentRowValue`.
    fn get_current_row_value(&mut self, db: &mut Database, column: &str) -> Result<Value>;

    /// `Cursor.setCurrentRowValue`: updates a single value of the current row
    /// (every other column is kept, `Column.KEEP_VALUE`).
    fn set_current_row_value(
        &mut self,
        db: &mut Database,
        column: &str,
        value: Value,
    ) -> Result<()>;

    /// `Cursor.updateCurrentRow`: updates the current row with the given
    /// (possibly partial) values, which may contain [`Value::Keep`].  Returns
    /// the row values which were written (padded to the column count).
    fn update_current_row(&mut self, db: &mut Database, values: &[Value]) -> Result<Vec<Value>>;

    /// `Cursor.updateCurrentRowFromMap`: updates the current row with the
    /// columns named by the map, keeping every other column.  Returns the row
    /// as it is now stored.
    fn update_current_row_from_map(
        &mut self,
        db: &mut Database,
        row: &BTreeMap<String, Value>,
    ) -> Result<Vec<Value>>;

    /// `Cursor.deleteCurrentRow`.
    fn delete_current_row(&mut self, db: &mut Database) -> Result<()>;

    /// `Cursor.findRow`: moves to the row with the given id.
    ///
    /// Like jackcess, a row id which is invalid for the table (unknown page or
    /// row) is not an error: `false` is returned and the cursor keeps its
    /// position.  An index cursor reproduces the `IndexCursorImpl` lookups,
    /// which are index based: the cursor lands on the first row of the index
    /// entry of the requested row, so for a non-unique index (and for a row
    /// which sorts before the cursor's range) it can land on, and report, a
    /// *different* row.  Asking for the id of a deleted row is an
    /// `IllegalStateException` (the row cannot be read to build the index
    /// entry), exactly like `TableImpl.getRow`.
    fn find_row(&mut self, db: &mut Database, row_id: RowId) -> Result<bool>;

    /// `Cursor.findFirstRow(Map)`: searches from the beginning of the table.
    fn find_first_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool>;

    /// `Cursor.findNextRow(Map)`: searches forward from the current position.
    fn find_next_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool>;

    /// Searches backwards from the end of the table for a matching row (the
    /// reverse of `findFirstRow`; jackcess has no such convenience method).
    fn find_last_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool>;

    /// `Cursor.findFirstRow(Column,Object)`.
    fn find_first_row_by_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<bool>;

    /// `Cursor.findNextRow(Column,Object)`.
    fn find_next_row_by_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<bool>;

    /// `Cursor.currentRowMatches(Map)`.
    fn current_row_matches(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool>;

    /// `Cursor.currentRowMatches(Column,Object)`.
    fn current_row_matches_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<bool>;

    /// `CursorImpl.findAnotherRow(Map, reset, moveForward, matcher)`: moves to
    /// the next row in the requested direction which matches the given pattern,
    /// restoring the cursor to its previous state when no row is found.
    ///
    /// This is the search the `IterableBuilder` row-match iterator performs.
    /// The default implementation is the plain table scan of
    /// `CursorImpl.findAnotherRowImpl`; [`IndexCursor`] overrides it with the
    /// index assisted search of `IndexCursorImpl`.
    fn find_another_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
        move_forward: bool,
        reset: bool,
    ) -> Result<bool> {
        let saved = self.get_savepoint();
        let found = (|| -> Result<bool> {
            if reset {
                if move_forward {
                    self.before_first();
                } else {
                    self.after_last();
                }
            }
            loop {
                let moved = if move_forward {
                    self.move_to_next_row(db)?
                } else {
                    self.move_to_previous_row(db)?
                };
                if !moved {
                    return Ok(false);
                }
                if self.current_row_matches(db, row_pattern)? {
                    return Ok(true);
                }
            }
        })();
        match found {
            Ok(true) => Ok(true),
            other => {
                self.restore_savepoint(&saved)?;
                other
            }
        }
    }

    /// The `Cursor.findFirstRow`/`findNextRow(Column,Object)` search for a
    /// single column, in either direction (`CursorImpl.findAnotherRow`).
    fn find_another_row_by_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
        move_forward: bool,
        reset: bool,
    ) -> Result<bool> {
        let mut pattern = BTreeMap::new();
        pattern.insert(column.to_string(), value.clone());
        self.find_another_row(db, &pattern, move_forward, reset)
    }

    // -- `Row` based conveniences (`Cursor extends Iterable<Row>`) -----------
    //
    // Java's cursor returns `Row` objects.  The port's established entry points
    // keep returning the positional `Vec<Value>` rows (they are what the
    // byte-level harnesses use); these provided methods build a [`Row`] on top
    // of them, with the row id and the columns in table order, exactly like
    // `CursorImpl.getCurrentRow` builds a `RowImpl`.  They are additive: no
    // existing signature changes.

    /// `Cursor.newIterable`: a builder for flexible iteration over this cursor.
    ///
    /// The Rust [`IterableBuilder`] does not capture the cursor (the port's
    /// cursor operations take the database explicitly), so the returned builder
    /// is used with [`IterableBuilder::rows`] and the cursor.
    fn new_iterable(&self) -> IterableBuilder {
        IterableBuilder::new()
    }

    /// `Cursor.iterator()`'s row: the current row as a [`Row`] carrying the
    /// cursor's row id.
    fn get_current_row_object(&mut self, db: &mut Database) -> Result<Row> {
        let id = self.get_current_row_id();
        let values = self.get_current_row(db)?;
        cursor_row(db, self.table_name(), id, &values, None)
    }

    /// `Cursor.getCurrentRow(Collection<String>)`: the named columns of the
    /// current row as a [`Row`], in table column order (like Java's
    /// `TableImpl.getRow(..., columnNames)`).
    fn get_current_row_object_columns(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Row> {
        let id = self.get_current_row_id();
        let values = self.get_current_row_columns(db, column_names)?;
        cursor_row(db, self.table_name(), id, &values, Some(column_names))
    }

    /// `Cursor.getNextRow()` as a [`Row`].
    fn get_next_row_object(&mut self, db: &mut Database) -> Result<Option<Row>> {
        if self.move_to_next_row(db)? {
            Ok(Some(self.get_current_row_object(db)?))
        } else {
            Ok(None)
        }
    }

    /// `Cursor.getNextRow(Collection<String>)` as a [`Row`].
    fn get_next_row_object_columns(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Option<Row>> {
        if self.move_to_next_row(db)? {
            Ok(Some(self.get_current_row_object_columns(db, column_names)?))
        } else {
            Ok(None)
        }
    }

    /// `Cursor.getPreviousRow()` as a [`Row`].
    fn get_previous_row_object(&mut self, db: &mut Database) -> Result<Option<Row>> {
        if self.move_to_previous_row(db)? {
            Ok(Some(self.get_current_row_object(db)?))
        } else {
            Ok(None)
        }
    }

    /// `Cursor.getPreviousRow(Collection<String>)` as a [`Row`].
    fn get_previous_row_object_columns(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Option<Row>> {
        if self.move_to_previous_row(db)? {
            Ok(Some(self.get_current_row_object_columns(db, column_names)?))
        } else {
            Ok(None)
        }
    }

    /// `Cursor.updateCurrentRowFromMap(Map)` with a [`Row`]: updates the
    /// columns the row holds (a column the row does not hold keeps its old
    /// value) and returns the stored current row as a [`Row`].
    fn update_current_row_object(&mut self, db: &mut Database, row: &Row) -> Result<Row> {
        let id = self.get_current_row_id();
        let values = self.update_current_row_from_map(db, &row.to_map())?;
        cursor_row(db, self.table_name(), id, &values, None)
    }

    /// The explicit alias of [`Cursor::get_current_row_columns`] for Java's
    /// `getCurrentRow(Collection<String>)` overload (Rust has no overloading;
    /// `*_columns` is the primary name, this is the alternate one).
    fn get_current_row_subset(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Vec<Value>> {
        self.get_current_row_columns(db, column_names)
    }

    /// The explicit alias of [`Cursor::get_next_row_columns`] for Java's
    /// `getNextRow(Collection<String>)` overload.
    fn get_next_row_subset(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Option<Vec<Value>>> {
        self.get_next_row_columns(db, column_names)
    }

    /// The explicit alias of [`Cursor::get_previous_row_columns`] for Java's
    /// `getPreviousRow(Collection<String>)` overload.
    fn get_previous_row_subset(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Option<Vec<Value>>> {
        self.get_previous_row_columns(db, column_names)
    }

    /// `Cursor.stream().collect()`: resets the cursor and returns every row as
    /// a [`Row`].
    ///
    /// Rust iterators must own the cursor, so the lazy form is
    /// [`CursorRowIter`] (see [`TableScanCursor::row_iter`] /
    /// [`IndexCursor::row_iter`]); this is the eager `stream().collect()`
    /// equivalent.
    fn stream(&mut self, db: &mut Database) -> Result<Vec<Row>> {
        self.before_first();
        let mut rows = Vec::new();
        while self.move_to_next_row(db)? {
            rows.push(self.get_current_row_object(db)?);
        }
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// value matching (`SimpleColumnMatcher` / `CaseInsensitiveColumnMatcher`)
// ---------------------------------------------------------------------------

/// The `ColumnMatcher.matches` of one value pair.
pub(crate) fn value_matches(
    column: &Column,
    pattern: &Value,
    actual: &Value,
    matcher: ColumnMatcher,
) -> bool {
    // `Column.KEEP_VALUE` is never a real column value
    if pattern.is_keep() {
        return false;
    }
    if matcher == ColumnMatcher::CaseInsensitive
        && matches!(column.data_type, DataType::Text | DataType::Memo)
    {
        // CaseInsensitiveColumnMatcher: ColumnImpl.toCharSequence on both
        // values and String.equalsIgnoreCase
        match (pattern, actual) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            _ => match (
                crate::column::to_char_sequence(pattern),
                crate::column::to_char_sequence(actual),
            ) {
                (Ok(a), Ok(b)) => java_equals_ignore_case(&a, &b),
                _ => false,
            },
        }
    } else {
        simple_equals(pattern, actual)
    }
}

/// `SimpleColumnMatcher.equals`: `Objects.equals` plus byte array equality,
/// with the coercion of differently typed values to the column's internal type
/// (`ColumnImpl.toInternalValue`) approximated by the numeric conversion.
fn simple_equals(value1: &Value, value2: &Value) -> bool {
    if value1 == value2 {
        return true;
    }
    if value1.is_null() || value2.is_null() {
        return false;
    }
    if std::mem::discriminant(value1) != std::mem::discriminant(value2) {
        return match (numeric(value1), numeric(value2)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
    }
    false
}

fn numeric(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Byte(_)
        | Value::Int(_)
        | Value::Long(_)
        | Value::BigInt(_)
        | Value::Float(_)
        | Value::Double(_)
        | Value::Decimal { .. } => value.as_f64(),
        _ => None,
    }
}

/// Java's `String.equalsIgnoreCase`: character by character, comparing the
/// upper case forms and then the lower case forms.
fn java_equals_ignore_case(a: &str, b: &str) -> bool {
    let mut ai = a.chars();
    let mut bi = b.chars();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                if x == y {
                    continue;
                }
                let (xu, yu) = (
                    x.to_uppercase().next().unwrap_or(x),
                    y.to_uppercase().next().unwrap_or(y),
                );
                if xu != yu {
                    return false;
                }
                if x.to_lowercase().next().unwrap_or(x) != y.to_lowercase().next().unwrap_or(y) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

fn column_by_name<'a>(table: &'a crate::table::Table, name: &str) -> Option<&'a Column> {
    table.get_column(name)
}

/// Builds the [`Row`] of a cursor position (`CursorImpl.getCurrentRow` ->
/// `TableImpl.getRow`): the values are named with the table's columns (in
/// table order), restricted to the requested columns when one is given.
fn cursor_row(
    db: &mut Database,
    table: &str,
    id: RowId,
    values: &[Value],
    selected: Option<&[&str]>,
) -> Result<Row> {
    let columns = {
        let page = db.table_page(table)?;
        db.tables
            .get(&page)
            .expect("the table was just loaded")
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect::<Vec<_>>()
    };
    Ok(row_from_selected_values(id, &columns, values, selected))
}

// ---------------------------------------------------------------------------
// table scan cursor (`TableScanCursor`)
// ---------------------------------------------------------------------------

/// The position of a table scan cursor (`TableScanCursor.ScanPosition`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanPos {
    BeforeFirst,
    Row(RowId),
    AfterLast,
}

/// Simple un-indexed cursor which walks a table in row id order
/// (`TableScanCursor`).
#[derive(Clone)]
pub struct TableScanCursor {
    table: String,
    table_page: i32,
    cur: ScanPos,
    prev: ScanPos,
    matcher: ColumnMatcher,
    error_handler: Option<Arc<dyn ErrorHandler>>,
}

impl std::fmt::Debug for TableScanCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableScanCursor")
            .field("table", &self.table)
            .field("table_page", &self.table_page)
            .field("cur", &self.cur)
            .field("prev", &self.prev)
            .field("matcher", &self.matcher)
            .field("error_handler", &self.error_handler.is_some())
            .finish()
    }
}

impl TableScanCursor {
    /// Creates a cursor over the named table, positioned before the first row.
    pub fn new(db: &mut Database, table: &str) -> Result<TableScanCursor> {
        // make sure the table exists (and is loaded) like `Table.newCursor`
        let table_page = db.table_page(table)?;
        Ok(TableScanCursor {
            table: table.to_string(),
            table_page,
            cur: ScanPos::BeforeFirst,
            prev: ScanPos::BeforeFirst,
            matcher: ColumnMatcher::Simple,
            error_handler: None,
        })
    }

    /// `Cursor.getId`.
    pub fn get_id(&self) -> CursorId {
        CursorId {
            table_page: self.table_page,
            index_number: -1,
        }
    }

    /// The name of the table this cursor traverses.
    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// `Cursor.iterator()`: an iterator over this cursor's rows yielding
    /// [`Row`] values (the cursor is consumed by the iterator, which borrows the
    /// database).
    pub fn row_iter<'a>(self, db: &'a mut Database) -> CursorRowIter<'a> {
        CursorRowIter::new(db, Box::new(self))
    }

    /// `Cursor.stream().collect()`: resets the cursor and returns every row as
    /// a [`Row`].
    pub fn rows(mut self, db: &mut Database) -> Result<Vec<Row>> {
        self.before_first();
        let mut rows = Vec::new();
        while self.move_to_next_row(db)? {
            rows.push(self.get_current_row_object(db)?);
        }
        Ok(rows)
    }

    fn restore_position_impl(&mut self, cur: ScanPos) {
        // `CursorImpl.restorePositionImpl`: the current position becomes the
        // previous one and the given position is restored
        self.prev = self.cur.clone();
        self.cur = cur;
    }

    fn read_row_statuses(&self, db: &mut Database) -> Result<Vec<(RowId, bool)>> {
        db.row_id_statuses(&self.table)
    }

    fn read_row_by_id(&self, db: &mut Database, row_id: RowId) -> Result<Option<Vec<Value>>> {
        db.read_row_by_id_using(&self.table, row_id, self.error_handler.clone())
    }

    fn move_scan(&mut self, db: &mut Database, forward: bool) -> Result<bool> {
        // `CursorImpl.findAnotherPosition`: only row positions are inspected
        // here, the current row is decoded when it is actually requested
        let rows = self.read_row_statuses(db)?;
        if forward {
            let start = match &self.cur {
                // already at the end: `moveToAnotherRow` rechecks and gives up
                ScanPos::AfterLast => return Ok(false),
                ScanPos::BeforeFirst => 0usize,
                ScanPos::Row(id) => match rows.iter().position(|(rid, _)| rid == id) {
                    Some(pos) => pos + 1,
                    None => rows
                        .iter()
                        .position(|(rid, _)| *rid > *id)
                        .unwrap_or(rows.len()),
                },
            };
            for (row_id, deleted) in rows.iter().skip(start) {
                if !deleted {
                    self.prev = self.cur.clone();
                    self.cur = ScanPos::Row(*row_id);
                    return Ok(true);
                }
            }
            self.prev = self.cur.clone();
            self.cur = ScanPos::AfterLast;
            Ok(false)
        } else {
            let mut pos = match &self.cur {
                ScanPos::BeforeFirst => return Ok(false),
                ScanPos::AfterLast => rows.len() as i64,
                ScanPos::Row(id) => match rows.iter().position(|(rid, _)| rid == id) {
                    Some(pos) => pos as i64,
                    None => rows
                        .iter()
                        .position(|(rid, _)| *rid >= *id)
                        .unwrap_or(rows.len()) as i64,
                },
            } - 1;
            while pos >= 0 {
                let (row_id, deleted) = &rows[pos as usize];
                if !deleted {
                    self.prev = self.cur.clone();
                    self.cur = ScanPos::Row(*row_id);
                    return Ok(true);
                }
                pos -= 1;
            }
            self.prev = self.cur.clone();
            self.cur = ScanPos::BeforeFirst;
            Ok(false)
        }
    }

    fn row_matches(
        &self,
        db: &mut Database,
        values: &[Value],
        pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        for (name, pat_value) in pattern {
            let column = match column_by_name(table, name) {
                Some(c) => c,
                // a pattern naming no column never matches (`getCurrentRow`
                // only returns the pattern columns, so the sizes differ)
                None => return Ok(false),
            };
            let actual = values
                .get(column.column_index)
                .cloned()
                .unwrap_or(Value::Null);
            if !value_matches(column, pat_value, &actual, self.matcher) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn find_forward_matching(
        &mut self,
        db: &mut Database,
        pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        loop {
            if !self.move_scan(db, true)? {
                return Ok(false);
            }
            let values = self.get_current_row(db)?;
            if self.row_matches(db, &values, pattern)? {
                return Ok(true);
            }
            // a table scan always keeps searching
        }
    }
}

impl Cursor for TableScanCursor {
    fn table_name(&self) -> &str {
        TableScanCursor::table_name(self)
    }

    fn get_id(&self) -> CursorId {
        TableScanCursor::get_id(self)
    }

    fn before_first(&mut self) {
        self.cur = ScanPos::BeforeFirst;
        self.prev = ScanPos::BeforeFirst;
    }

    fn after_last(&mut self) {
        self.cur = ScanPos::AfterLast;
        self.prev = ScanPos::AfterLast;
    }

    fn is_before_first(&self) -> bool {
        self.cur == ScanPos::BeforeFirst
    }

    fn is_after_last(&self) -> bool {
        self.cur == ScanPos::AfterLast
    }

    fn get_column_matcher(&self) -> ColumnMatcher {
        self.matcher
    }

    fn set_column_matcher(&mut self, matcher: ColumnMatcher) {
        self.matcher = matcher;
    }

    fn get_error_handler(&self) -> Option<&Arc<dyn ErrorHandler>> {
        self.error_handler.as_ref()
    }

    fn set_error_handler(&mut self, new_error_handler: Option<Arc<dyn ErrorHandler>>) {
        self.error_handler = new_error_handler;
    }

    fn get_savepoint(&self) -> Savepoint {
        Savepoint::new(
            self.get_id(),
            SavedPos::Scan(self.cur.clone()),
            SavedPos::Scan(self.prev.clone()),
        )
    }

    fn restore_savepoint(&mut self, savepoint: &Savepoint) -> Result<()> {
        // `CursorImpl.restoreSavepoint`: the savepoint must belong to this
        // cursor
        if savepoint.get_cursor_id() != self.get_id() {
            return Err(Error::IllegalArgument(format!(
                "Savepoint {:?} is not valid for this cursor with id {:?}",
                savepoint.get_cursor_id(),
                self.get_id()
            )));
        }
        let (cur, prev) = match (&savepoint.cur, &savepoint.prev) {
            (SavedPos::Scan(cur), SavedPos::Scan(prev)) => (cur.clone(), prev.clone()),
            _ => {
                return Err(Error::IllegalArgument(
                    "Restored position must be a table scan position".into(),
                ))
            }
        };
        // `restorePosition` + `restorePositionImpl`: only when the positions
        // differ, and the current position becomes the previous one
        if cur != self.cur || prev != self.prev {
            self.prev = self.cur.clone();
            self.cur = cur;
        }
        Ok(())
    }

    fn get_current_row_id(&self) -> RowId {
        match &self.cur {
            ScanPos::BeforeFirst => FIRST_ROW_ID,
            ScanPos::AfterLast => LAST_ROW_ID,
            ScanPos::Row(id) => *id,
        }
    }

    fn get_row_count(&mut self, db: &mut Database) -> Result<usize> {
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        Ok(table.row_count.max(0) as usize)
    }

    fn is_current_row_deleted(&mut self, db: &mut Database) -> Result<bool> {
        let id = self.get_current_row_id();
        if id.0 < 0 || id.1 < 0 {
            return Ok(false);
        }
        for (row_id, deleted) in self.read_row_statuses(db)? {
            if row_id == id {
                return Ok(deleted);
            }
        }
        Ok(false)
    }

    fn is_current_row_valid(&mut self, db: &mut Database) -> Result<bool> {
        let id = self.get_current_row_id();
        if id.0 < 0 || id.1 < 0 {
            return Ok(false);
        }
        Ok(self
            .read_row_statuses(db)?
            .iter()
            .any(|(row_id, deleted)| *row_id == id && !deleted))
    }

    fn move_to_next_row(&mut self, db: &mut Database) -> Result<bool> {
        self.move_scan(db, true)
    }

    fn move_to_previous_row(&mut self, db: &mut Database) -> Result<bool> {
        self.move_scan(db, false)
    }

    fn get_current_row(&mut self, db: &mut Database) -> Result<Vec<Value>> {
        let id = self.get_current_row_id();
        if id.0 < 0 || id.1 < 0 {
            return Err(Error::IllegalState(
                "Current row is not valid (cursor is at the beginning or end of the table)".into(),
            ));
        }
        self.read_row_by_id(db, id)?
            .ok_or_else(|| Error::IllegalState(format!("Row is deleted: ({}, {})", id.0, id.1)))
    }

    fn get_current_row_columns(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Vec<Value>> {
        let row = self.get_current_row(db)?;
        select_columns(db, &self.table, &row, column_names)
    }

    fn get_current_row_as_map(&mut self, db: &mut Database) -> Result<BTreeMap<String, Value>> {
        let row = self.get_current_row(db)?;
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        Ok(table
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), row.get(i).cloned().unwrap_or(Value::Null)))
            .collect())
    }

    fn get_current_row_value(&mut self, db: &mut Database, column: &str) -> Result<Value> {
        let row = self.get_current_row(db)?;
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        let column = column_by_name(table, column).ok_or_else(|| {
            Error::IllegalArgument(format!(
                "Column {} does not exist in table {}",
                column, self.table
            ))
        })?;
        Ok(row.get(column.column_index).cloned().unwrap_or(Value::Null))
    }

    fn set_current_row_value(
        &mut self,
        db: &mut Database,
        column: &str,
        value: Value,
    ) -> Result<()> {
        self.require_valid(db)?;
        let id = self.get_current_row_id();
        let page = db.table_page(&self.table)?;
        let (ci, num_columns) = {
            let table = db.tables.get(&page).unwrap();
            let ci = table.column_index(column).ok_or_else(|| {
                Error::IllegalArgument(format!(
                    "Column {} does not exist in table {}",
                    column, self.table
                ))
            })?;
            (ci, table.columns.len())
        };
        let mut values = vec![Value::Keep; num_columns];
        values[ci] = value;
        db.update_row(&self.table, id, &values)
    }

    fn update_current_row(&mut self, db: &mut Database, values: &[Value]) -> Result<Vec<Value>> {
        self.require_valid(db)?;
        let id = self.get_current_row_id();
        let num_columns = {
            let page = db.table_page(&self.table)?;
            db.tables.get(&page).unwrap().columns.len()
        };
        let mut row = values.to_vec();
        row.resize(num_columns, Value::Null);
        db.update_row(&self.table, id, &row)?;
        // `TableImpl.updateRow` resolves the KEEP_VALUE entries into the row
        // array it returns; read the row back so the caller sees the values
        // which are now stored
        self.read_row_by_id(db, id)?
            .ok_or_else(|| Error::IllegalState(format!("Row is deleted: ({}, {})", id.0, id.1)))
    }

    fn update_current_row_from_map(
        &mut self,
        db: &mut Database,
        row: &BTreeMap<String, Value>,
    ) -> Result<Vec<Value>> {
        self.require_valid(db)?;
        let id = self.get_current_row_id();
        db.update_row_from_map(&self.table, id, row)
    }

    fn delete_current_row(&mut self, db: &mut Database) -> Result<()> {
        self.require_valid(db)?;
        let id = self.get_current_row_id();
        db.delete_row(&self.table, id)
    }

    fn find_row(&mut self, db: &mut Database, row_id: RowId) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        // `CursorImpl.findRow`: `TableImpl.positionAtRowHeader` returning null
        // (an invalid page/row id for this table) is simply "no such row", and
        // a deleted row is not valid either
        let found = match self.read_row_by_id(db, row_id) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(Error::IllegalArgument(_)) => false,
            Err(e) => return Err(e),
        };
        if found {
            self.prev = self.cur.clone();
            self.cur = ScanPos::Row(row_id);
            Ok(true)
        } else {
            if self.cur != saved_cur || self.prev != saved_prev {
                self.restore_position_impl(saved_cur);
            }
            Ok(false)
        }
    }

    fn find_first_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        self.before_first();
        let found = self.find_forward_matching(db, row_pattern)?;
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    fn find_next_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        let found = self.find_forward_matching(db, row_pattern)?;
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    fn find_last_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        self.after_last();
        let mut found = false;
        loop {
            if !self.move_scan(db, false)? {
                break;
            }
            let values = self.get_current_row(db)?;
            if self.row_matches(db, &values, row_pattern)? {
                found = true;
                break;
            }
        }
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    fn find_first_row_by_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<bool> {
        let mut pattern = BTreeMap::new();
        pattern.insert(column.to_string(), value.clone());
        self.find_first_row(db, &pattern)
    }

    fn find_next_row_by_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<bool> {
        let mut pattern = BTreeMap::new();
        pattern.insert(column.to_string(), value.clone());
        self.find_next_row(db, &pattern)
    }

    fn current_row_matches(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        if !self.is_current_row_valid(db)? {
            return Ok(false);
        }
        let values = self.get_current_row(db)?;
        self.row_matches(db, &values, row_pattern)
    }

    fn current_row_matches_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<bool> {
        let mut pattern = BTreeMap::new();
        pattern.insert(column.to_string(), value.clone());
        self.current_row_matches(db, &pattern)
    }
}

impl TableScanCursor {
    fn require_valid(&mut self, db: &mut Database) -> Result<()> {
        if self.is_current_row_valid(db)? {
            Ok(())
        } else {
            Err(Error::IllegalState(
                "Current row is not valid (cursor is at the beginning or end of the table, or \
                 on a deleted row)"
                    .into(),
            ))
        }
    }
}

/// Returns the named columns of a row, in the given order.
fn select_columns(
    db: &mut Database,
    table_name: &str,
    row: &[Value],
    column_names: &[&str],
) -> Result<Vec<Value>> {
    let page = db.table_page(table_name)?;
    let table = db.tables.get(&page).unwrap();
    let mut out = Vec::with_capacity(column_names.len());
    for name in column_names {
        let column = column_by_name(table, name).ok_or_else(|| {
            Error::IllegalArgument(format!(
                "Column {} does not exist in table {}",
                name, table_name
            ))
        })?;
        out.push(row.get(column.column_index).cloned().unwrap_or(Value::Null));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// index cursor (`IndexCursorImpl`)
// ---------------------------------------------------------------------------

/// Converts an index row id to the table row id used by the row APIs.
fn to_table_row_id(id: IndexRowId) -> RowId {
    (id.page_number, id.row_number)
}

/// Converts a table row id to the index row id used by the entry APIs.
pub fn to_index_row_id(id: RowId) -> IndexRowId {
    IndexRowId::new(id.0, id.1)
}

/// The values a range boundary entry is built from (`CursorBuilder.setStartRow`
/// / `setStartEntry`).
#[derive(Clone, Debug, PartialEq)]
pub enum RangeKey {
    /// A row of table values, indexed by column index (the sparse row array
    /// `CursorBuilder.setStartRow(Object...)` takes).  Missing values are
    /// `null`, and only the index's columns are read
    /// (`IndexData.createEntryBytes`).
    Row(Vec<Value>),
    /// Index entry values: a non-empty prefix of the index's columns, the
    /// trailing columns using `IndexData.MIN_VALUE`
    /// (`CursorBuilder.setStartEntry(Object...)`).
    Entry(Vec<Value>),
}

/// The row range an [`IndexCursor`] is narrowed to (`CursorBuilder.setStartRow`
/// / `setEndRow` / `setStartEntry` / `setEndEntry` and the inclusive flags).
///
/// Both bounds are inclusive by default, like `CursorBuilder`.  Like jackcess
/// 5.0.4 the range applies to index cursors only.
#[derive(Clone, Debug)]
pub struct IndexRange {
    start: Option<RangeKey>,
    start_inclusive: bool,
    end: Option<RangeKey>,
    end_inclusive: bool,
}

impl Default for IndexRange {
    fn default() -> Self {
        IndexRange::new()
    }
}

impl IndexRange {
    /// A range with no bounds (the whole index).  Both bounds are inclusive,
    /// like `CursorBuilder`.
    pub fn new() -> IndexRange {
        IndexRange {
            start: None,
            start_inclusive: true,
            end: None,
            end_inclusive: true,
        }
    }

    /// `CursorBuilder.setStartRow`: the first row of the cursor, as a row of
    /// table column values.
    pub fn set_start_row(&mut self, row: Vec<Value>) -> &mut Self {
        self.start = Some(RangeKey::Row(row));
        self
    }

    /// `CursorBuilder.setStartEntry`: the first row of the cursor, as index
    /// entry values (a prefix of the index's columns).
    pub fn set_start_entry(&mut self, entry_values: Vec<Value>) -> &mut Self {
        self.start = Some(RangeKey::Entry(entry_values));
        self
    }

    /// `CursorBuilder.setStartRowInclusive`.
    pub fn set_start_row_inclusive(&mut self, inclusive: bool) -> &mut Self {
        self.start_inclusive = inclusive;
        self
    }

    /// `CursorBuilder.setEndRow`: the last row of the cursor, as a row of table
    /// column values.
    pub fn set_end_row(&mut self, row: Vec<Value>) -> &mut Self {
        self.end = Some(RangeKey::Row(row));
        self
    }

    /// `CursorBuilder.setEndEntry`: the last row of the cursor, as index entry
    /// values (a prefix of the index's columns).
    pub fn set_end_entry(&mut self, entry_values: Vec<Value>) -> &mut Self {
        self.end = Some(RangeKey::Entry(entry_values));
        self
    }

    /// `CursorBuilder.setEndRowInclusive`.
    pub fn set_end_row_inclusive(&mut self, inclusive: bool) -> &mut Self {
        self.end_inclusive = inclusive;
        self
    }

    /// `CursorBuilder.setSpecificRow`: a range of the single given row.
    pub fn set_specific_row(&mut self, row: Vec<Value>) -> &mut Self {
        self.set_start_row(row.clone()).set_end_row(row)
    }

    /// `CursorBuilder.setSpecificEntry`: a range holding the rows of the single
    /// given index entry.
    pub fn set_specific_entry(&mut self, entry_values: Vec<Value>) -> &mut Self {
        self.set_start_entry(entry_values.clone())
            .set_end_entry(entry_values)
    }

    /// The start bound (with its inclusive flag), if any.
    pub fn start(&self) -> Option<&RangeKey> {
        self.start.as_ref()
    }

    /// The end bound (with its inclusive flag), if any.
    pub fn end(&self) -> Option<&RangeKey> {
        self.end.as_ref()
    }
}

/// Cursor backed by an index (`IndexCursorImpl`): traverses a table in index
/// order and uses the index for lookups.
///
/// The cursor position is always an index entry, exactly like
/// `IndexCursorImpl.IndexPosition`: either a real entry of the index or one of
/// the two range boundary entries (whose row id is invalid, and which therefore
/// play the role of the "before first"/"after last" positions).  For an
/// unbounded cursor those are `IndexData.FIRST_ENTRY`/`LAST_ENTRY`.
#[derive(Clone)]
pub struct IndexCursor {
    table: String,
    table_page: i32,
    data_number: i32,
    index_number: i32,
    cur: Entry,
    prev: Entry,
    start_bound: Entry,
    end_bound: Entry,
    matcher: ColumnMatcher,
    error_handler: Option<Arc<dyn ErrorHandler>>,
}

impl std::fmt::Debug for IndexCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexCursor")
            .field("table", &self.table)
            .field("table_page", &self.table_page)
            .field("data_number", &self.data_number)
            .field("index_number", &self.index_number)
            .field("cur", &self.cur)
            .field("prev", &self.prev)
            .field("start_bound", &self.start_bound)
            .field("end_bound", &self.end_bound)
            .field("matcher", &self.matcher)
            .field("error_handler", &self.error_handler.is_some())
            .finish()
    }
}

impl IndexCursor {
    /// Creates a cursor over the named index of the named table, positioned
    /// before the first entry.
    ///
    /// Like `IndexCursorImpl.createCursor`, the index data is initialized (the
    /// index pages are read) and an index which cannot be used for lookups is
    /// rejected.
    pub fn new(db: &mut Database, table: &str, index_name: &str) -> Result<IndexCursor> {
        IndexCursor::new_with_range(db, table, index_name, &IndexRange::new())
    }

    /// Creates a cursor over the named index of the named table, narrowed to
    /// the given range (`IndexCursorImpl.createCursor` with the `startRow` /
    /// `endRow` arguments of `IndexData.cursor`).
    pub fn new_with_range(
        db: &mut Database,
        table: &str,
        index_name: &str,
        range: &IndexRange,
    ) -> Result<IndexCursor> {
        let table_page = db.table_page(table)?;
        let (index_number, data_number, is_unique) = {
            let t = db
                .tables
                .get(&table_page)
                .ok_or_else(|| Error::IllegalState(format!("table {} is not loaded", table)))?;
            let index = t
                .indexes
                .iter()
                .find(|i| i.name.eq_ignore_ascii_case(index_name))
                .ok_or_else(|| {
                    Error::IllegalArgument(format!(
                        "Index with name {} does not exist in table {}",
                        index_name, table
                    ))
                })?;
            let data = t
                .index_datas
                .iter()
                .find(|d| d.number == index.index_data_number)
                .ok_or_else(|| {
                    Error::Jackcess(format!(
                        "missing index data for index {}",
                        index_data_label(index)
                    ))
                })?;
            (
                index.index_number,
                index.index_data_number,
                data.is_unique(),
            )
        };

        {
            let t = db.tables.get_mut(&table_page).unwrap();
            let data = t
                .index_datas
                .iter_mut()
                .find(|d| d.number == data_number)
                .ok_or_else(|| Error::Jackcess("missing index data".into()))?;
            if data.is_read_only() {
                return Err(Error::IllegalArgument(format!(
                    "Given index {} is not usable for indexed lookups due to {}",
                    index_name, data.unsupported_reason
                )));
            }
            let format = db.channel.format.clone();
            data.initialize(&mut db.channel, &format)?;
        }

        // `IndexCursorImpl.getDefaultColumnMatcher`: a unique index (whose text
        // entries fold case) uses a case-insensitive matcher
        let matcher = if is_unique {
            ColumnMatcher::CaseInsensitive
        } else {
            ColumnMatcher::Simple
        };
        let mut cursor = IndexCursor {
            table: table.to_string(),
            table_page,
            data_number,
            index_number,
            cur: Entry::first_entry(),
            prev: Entry::first_entry(),
            start_bound: Entry::first_entry(),
            end_bound: Entry::last_entry(),
            matcher,
            error_handler: None,
        };
        // `IndexData.cursor(startRow, startInclusive, endRow, endInclusive)`:
        // an entry based start bound fills with MIN_VALUE
        // (`CursorBuilder.setStartEntry`) and an end bound with MAX_VALUE
        // (`CursorBuilder.setEndEntry`)
        cursor.start_bound = match &range.start {
            Some(key) => cursor.build_bound(
                db,
                key,
                if range.start_inclusive {
                    IndexRowId::FIRST
                } else {
                    IndexRowId::LAST
                },
                EntryValue::Min,
            )?,
            None => Entry::first_entry(),
        };
        cursor.end_bound = match &range.end {
            Some(key) => cursor.build_bound(
                db,
                key,
                if range.end_inclusive {
                    IndexRowId::LAST
                } else {
                    IndexRowId::FIRST
                },
                EntryValue::Max,
            )?,
            None => Entry::last_entry(),
        };
        // `CursorImpl._curPos = _firstPos`
        cursor.cur = cursor.start_bound.clone();
        cursor.prev = cursor.cur.clone();
        Ok(cursor)
    }

    /// Builds one of the two range boundary entries
    /// (`IndexData.cursor(startRow, startInclusive, endRow, endInclusive)`):
    /// the encoded entry bytes of the given key with the given invalid row id.
    ///
    /// The row id decides where the boundary sorts relative to an existing entry
    /// with the same bytes: `FIRST_ROW_ID` (`EntryType.FIRST_VALID`) sorts
    /// before it, which makes an inclusive start / an exclusive end bound, and
    /// `LAST_ROW_ID` (`LAST_VALID`) sorts after it, which makes an exclusive
    /// start / an inclusive end bound.  An entry-based key fills its trailing
    /// index columns with `MIN_VALUE` for a start bound
    /// (`CursorBuilder.setStartEntry`) and with `MAX_VALUE` for an end bound
    /// (`CursorBuilder.setEndEntry`).
    fn build_bound(
        &mut self,
        db: &mut Database,
        key: &RangeKey,
        row_id: IndexRowId,
        filler: EntryValue,
    ) -> Result<Entry> {
        let bytes = match key {
            RangeKey::Row(row) => self.with_index_data(db, |data, ch| {
                let format = ch.format.clone();
                let charset = ch.charset;
                data.create_entry_bytes(row, &format, charset)
            })?,
            RangeKey::Entry(entry_values) => self.with_index_data(db, |data, ch| {
                let key = entry_values_prefix(entry_values, data.columns.len(), filler)?;
                let format = ch.format.clone();
                let charset = ch.charset;
                data.create_entry_bytes_partial(&key, &format, charset)
            })?,
        };
        Entry::new(Some(bytes), row_id)
    }

    /// `Cursor.getId`.
    pub fn get_id(&self) -> CursorId {
        CursorId {
            table_page: self.table_page,
            index_number: self.index_number,
        }
    }

    /// The name of the table this cursor traverses.
    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// The logical index number of the backing index.
    pub fn index_number(&self) -> i32 {
        self.index_number
    }

    /// `IndexCursor.newEntryIterable(Object...)`: a builder for iterating the
    /// rows of one index entry.
    pub fn new_entry_iterable(&self, entry_values: Vec<Value>) -> EntryIterableBuilder {
        EntryIterableBuilder::new(entry_values)
    }

    /// `Cursor.iterator()`: an iterator over this index cursor's rows yielding
    /// [`Row`] values (the cursor is consumed by the iterator, which borrows the
    /// database).
    pub fn row_iter<'a>(self, db: &'a mut Database) -> CursorRowIter<'a> {
        CursorRowIter::new(db, Box::new(self))
    }

    /// `Cursor.stream().collect()`: resets the cursor and returns every row as
    /// a [`Row`].
    pub fn rows(mut self, db: &mut Database) -> Result<Vec<Row>> {
        self.before_first();
        let mut rows = Vec::new();
        while self.move_to_next_row(db)? {
            rows.push(self.get_current_row_object(db)?);
        }
        Ok(rows)
    }

    /// `IndexCursor.getIndex`.
    pub fn get_index(&self, db: &mut Database) -> Result<IndexImpl> {
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        table
            .indexes
            .iter()
            .find(|i| i.index_number == self.index_number)
            .cloned()
            .ok_or_else(|| Error::Jackcess("index does not exist".into()))
    }

    /// `IndexCursor.getIndex` plus the index data: the columns of the backing
    /// index, in index order (with their ascending flags).
    pub fn get_index_columns(&self, db: &mut Database) -> Result<Vec<IndexColumn>> {
        self.index_columns(db)
    }

    /// `Index.isUnique` of the backing index.
    pub fn is_unique(&self, db: &mut Database) -> Result<bool> {
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        let data = table
            .index_datas
            .iter()
            .find(|d| d.number == self.data_number)
            .ok_or_else(|| Error::Jackcess("missing index data".into()))?;
        Ok(data.is_unique())
    }

    /// `Index.shouldIgnoreNulls` of the backing index.
    pub fn should_ignore_nulls(&self, db: &mut Database) -> Result<bool> {
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        let data = table
            .index_datas
            .iter()
            .find(|d| d.number == self.data_number)
            .ok_or_else(|| Error::Jackcess("missing index data".into()))?;
        Ok(data.should_ignore_nulls())
    }

    fn with_index_data<R>(
        &self,
        db: &mut Database,
        f: impl FnOnce(&mut crate::index::IndexData, &mut crate::pagechannel::PageChannel) -> Result<R>,
    ) -> Result<R> {
        let page = db.table_page(&self.table)?;
        let table = db
            .tables
            .get_mut(&page)
            .ok_or_else(|| Error::IllegalState(format!("table {} is not loaded", self.table)))?;
        let data = table
            .index_datas
            .iter_mut()
            .find(|d| d.number == self.data_number)
            .ok_or_else(|| Error::Jackcess("missing index data".into()))?;
        f(data, &mut db.channel)
    }

    fn index_columns(&self, db: &mut Database) -> Result<Vec<IndexColumn>> {
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        let data = table
            .index_datas
            .iter()
            .find(|d| d.number == self.data_number)
            .ok_or_else(|| Error::Jackcess("missing index data".into()))?;
        Ok(data.columns.clone())
    }

    /// `TableImpl.positionAtRowHeader` for this cursor's table: the location of
    /// the row's header.  An id which does not belong to this table is an
    /// `IllegalArgument` error (jackcess returns `null` here, which is what
    /// `CursorImpl.findRow` tests for).
    fn position_at_row_header(&self, db: &mut Database, row_id: RowId) -> Result<RowLocation> {
        let page = db.table_page(&self.table)?;
        let table = db
            .tables
            .get(&page)
            .ok_or_else(|| Error::IllegalState(format!("table {} is not loaded", self.table)))?;
        table.position_at_row_header(&mut db.channel, row_id)
    }

    fn read_row_by_id(&self, db: &mut Database, row_id: RowId) -> Result<Option<Vec<Value>>> {
        db.read_row_by_id_using(&self.table, row_id, self.error_handler.clone())
    }

    fn restore_position_impl(&mut self, cur: Entry) {
        self.prev = self.cur.clone();
        self.cur = cur;
    }

    fn move_to_entry(&mut self, entry: Entry) {
        self.prev = self.cur.clone();
        self.cur = entry;
    }

    /// The entry at the cursor's next position (`EntryCursor.getNextEntry`),
    /// clamped to the end boundary of the range
    /// (`ForwardIndexDirHandler.getAnotherPosition`).
    fn next_position(&mut self, db: &mut Database) -> Result<Entry> {
        let cur = self.cur.clone();
        let next = self.with_index_data(db, |data, ch| data.get_next_entry_from(ch, &cur))?;
        match next {
            Some((_pos, entry)) if entry.compare_to(&self.end_bound) < 0 => Ok(entry),
            _ => Ok(self.end_bound.clone()),
        }
    }

    /// The entry at the cursor's previous position
    /// (`EntryCursor.getPreviousEntry`), clamped to the start boundary.
    fn previous_position(&mut self, db: &mut Database) -> Result<Entry> {
        let cur = self.cur.clone();
        let prev = self.with_index_data(db, |data, ch| data.get_previous_entry_from(ch, &cur))?;
        match prev {
            Some((_pos, entry)) if entry.compare_to(&self.start_bound) > 0 => Ok(entry),
            _ => Ok(self.start_bound.clone()),
        }
    }

    /// `CursorImpl.moveToAnotherRow`: moves one entry in the requested
    /// direction, clamped to the cursor's range.  Returns `false` (leaving the
    /// cursor on the boundary position) when there is no further entry in that
    /// direction, like `moveToAnotherRowImpl`.
    fn move_index(&mut self, db: &mut Database, forward: bool) -> Result<bool> {
        if forward {
            // `moveToAnotherRow`: already at the end position (a cursor is
            // always up to date, and rechecking cannot move it in this model)
            if self.cur.compare_to(&self.end_bound) == 0 {
                return Ok(false);
            }
            let next = self.next_position(db)?;
            self.move_to_entry(next);
            Ok(self.cur.compare_to(&self.end_bound) != 0)
        } else {
            if self.cur.compare_to(&self.start_bound) == 0 {
                return Ok(false);
            }
            let prev = self.previous_position(db)?;
            self.move_to_entry(prev);
            Ok(self.cur.compare_to(&self.start_bound) != 0)
        }
    }

    /// Builds a partial index row (in index column order) from a row pattern:
    /// the contiguous prefix of index columns named by the pattern, then
    /// `MIN_VALUE`.  `None` when the pattern does not name the first index
    /// column (the lookup has to fall back to a full scan), like
    /// `IndexData.constructPartialIndexRow`.
    fn key_from_map(
        &self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<Option<Vec<EntryValue>>> {
        let columns = self.index_columns(db)?;
        let mut key = Vec::with_capacity(columns.len());
        let mut prefix_done = false;
        for col in &columns {
            if !prefix_done {
                if let Some(value) = row_pattern.get(&col.column.name) {
                    key.push(EntryValue::from_value(value));
                    continue;
                }
                // `constructPartialIndexRow` needs at least the first column
                if key.is_empty() {
                    return Ok(None);
                }
                prefix_done = true;
            }
            key.push(EntryValue::Min);
        }
        Ok(Some(key))
    }

    /// Builds a partial index row from a single column value, which must be the
    /// first column of the index (`IndexData.constructPartialIndexRow(filler,
    /// colName, value)`).
    fn key_from_column(
        &self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<Option<Vec<EntryValue>>> {
        let canonical = {
            let page = db.table_page(&self.table)?;
            let table = db.tables.get(&page).unwrap();
            match column_by_name(table, column) {
                Some(c) => c.name.clone(),
                None => return Ok(None),
            }
        };
        let columns = self.index_columns(db)?;
        let first = match columns.first() {
            Some(c) => c.column.name.clone(),
            None => return Ok(None),
        };
        if first != canonical {
            return Ok(None);
        }
        let mut key = Vec::with_capacity(columns.len());
        key.push(EntryValue::from_value(value));
        key.resize(columns.len(), EntryValue::Min);
        Ok(Some(key))
    }

    /// Builds a partial index row (in index column order) from the given entry
    /// values, which must be a non-empty prefix of the index columns
    /// (`IndexData.constructPartialIndexRowFromEntry(MIN_VALUE, values)`).
    fn key_from_entry_values(
        &self,
        db: &mut Database,
        entry_values: &[Value],
    ) -> Result<Vec<EntryValue>> {
        let columns = self.index_columns(db)?;
        entry_values_prefix(entry_values, columns.len(), EntryValue::Min)
    }

    /// `IndexCursorImpl.currentRowMatchesEntryImpl`: whether the current row's
    /// index column values match the given partial index row (a `MIN`/`MAX`
    /// filler matches everything from that column on).
    fn current_row_matches_entry_key(
        &mut self,
        db: &mut Database,
        key: &[EntryValue],
    ) -> Result<bool> {
        let values = self.get_current_row(db)?;
        let columns = self.index_columns(db)?;
        for (col, entry_value) in columns.iter().zip(key.iter()) {
            match entry_value {
                EntryValue::Min | EntryValue::Max => return Ok(true),
                EntryValue::Value(pat_value) => {
                    let actual = values
                        .get(col.column.column_index)
                        .cloned()
                        .unwrap_or(Value::Null);
                    if !value_matches(&col.column, pat_value, &actual, self.matcher) {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    fn current_row_matches_map(
        &mut self,
        db: &mut Database,
        pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        let values = self.get_current_row(db)?;
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        for (name, pat_value) in pattern {
            let column = match column_by_name(table, name) {
                Some(c) => c,
                None => return Ok(false),
            };
            let actual = values
                .get(column.column_index)
                .cloned()
                .unwrap_or(Value::Null);
            if !value_matches(column, pat_value, &actual, self.matcher) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The encoded bytes of the entry the cursor is on, or `None` when it is on
    /// one of the boundary positions (`IndexCursorImpl.currentEntryBytes`).
    fn current_entry_bytes(&self) -> Option<Vec<u8>> {
        self.cur.get_entry_bytes().map(|b| b.to_vec())
    }

    /// The search entry of an index lookup (`EntryCursor.beforeEntry`): the
    /// encoded key with the first row id, so it sorts before every existing
    /// entry with the same bytes.
    fn make_search_entry(&self, db: &mut Database, key: &[EntryValue]) -> Result<Entry> {
        let bytes = self.with_index_data(db, |data, ch| {
            let format = ch.format.clone();
            let charset = ch.charset;
            data.create_entry_bytes_partial(key, &format, charset)
        })?;
        Entry::new(Some(bytes), IndexRowId::FIRST)
    }

    /// Moves to the first entry of the index whose key is >= the given values
    /// (`IndexCursorImpl.findPotentialRow`), clamped to the cursor's range.
    ///
    /// `OK(false)` (with the cursor left where it was) corresponds to
    /// `findPotentialRow` returning `false`: the search key is at or past the
    /// end of the range.  When no match is required (a "closest" lookup) the
    /// cursor is left on the end position instead, like the
    /// `restorePosition(new IndexPosition(startEntry))` of the end position.
    fn find_potential_row(
        &mut self,
        db: &mut Database,
        key: &[EntryValue],
        require_match: bool,
    ) -> Result<bool> {
        let search = self.make_search_entry(db, key)?;
        // `EntryCursor.beforeEntry` + `getNextEntry`
        let mut found =
            self.with_index_data(db, |data, ch| data.get_next_entry_from(ch, &search))?;
        // `EntryCursor.restorePosition` clamps the search position into the
        // range, after which the next entry is the first one of the range
        if let Some((_pos, entry)) = &found {
            if entry.compare_to(&self.start_bound) < 0 {
                let start = self.start_bound.clone();
                found =
                    self.with_index_data(db, |data, ch| data.get_next_entry_from(ch, &start))?;
            }
        }
        let entry = match found {
            Some((_pos, entry)) if entry.compare_to(&self.end_bound) < 0 => entry,
            _ => {
                if require_match {
                    return Ok(false);
                }
                self.move_to_entry(self.end_bound.clone());
                return Ok(true);
            }
        };
        self.move_to_entry(entry);
        Ok(true)
    }

    /// The test run over the rows which could match a search
    /// (`IndexCursorImpl.RowTest`).
    fn run_test(&mut self, db: &mut Database, test: &RowTest) -> Result<bool> {
        match test {
            RowTest::MatchesEntry => Ok(true),
            RowTest::RowPattern(pattern, exact_column_match) => {
                if *exact_column_match {
                    // `if exactColumnMatch, no need to do an extra comparison
                    // with the current row`
                    Ok(true)
                } else {
                    self.current_row_matches_map(db, pattern)
                }
            }
        }
    }

    /// `IndexCursorImpl.findMatchingRowInRun`: leaves the cursor on the first
    /// row of the current run which passes the test.
    fn find_matching_row_in_run(
        &mut self,
        db: &mut Database,
        key: &[EntryValue],
        forward: bool,
        test: &RowTest,
    ) -> Result<bool> {
        let run_bytes = self.current_entry_bytes();
        loop {
            if self.current_row_matches_entry_key(db, key)? && self.run_test(db, test)? {
                return Ok(true);
            }
            // the rest of the run is read before the search ends, but a row
            // past it has a greater entry and so cannot match
            if !self.move_index(db, forward)? {
                return Ok(false);
            }
            if !self.could_still_match(db, key, run_bytes.as_deref())? {
                return Ok(false);
            }
        }
    }

    /// `IndexCursorImpl.couldStillMatch`.
    fn could_still_match(
        &mut self,
        db: &mut Database,
        key: &[EntryValue],
        run_bytes: Option<&[u8]>,
    ) -> Result<bool> {
        if self.current_row_matches_entry_key(db, key)? {
            return Ok(true);
        }
        Ok(run_bytes.is_some() && self.current_entry_bytes().as_deref() == run_bytes)
    }

    fn find_first_row_by_entry_impl(
        &mut self,
        db: &mut Database,
        entry_values: &[Value],
        require_match: bool,
    ) -> Result<bool> {
        let key = self.key_from_entry_values(db, entry_values)?;
        if !self.find_potential_row(db, &key, require_match)? {
            return Ok(false);
        }
        if !require_match {
            return Ok(true);
        }
        self.find_matching_row_in_run(db, &key, true, &RowTest::MatchesEntry)
    }

    /// `IndexCursorImpl.findMatchingRowInRun` for the row after the current
    /// position (`EntryIterator.findNext`).
    fn find_next_row_by_entry_impl(
        &mut self,
        db: &mut Database,
        entry_values: &[Value],
    ) -> Result<bool> {
        if !self.move_index(db, true)? {
            return Ok(false);
        }
        let key = self.key_from_entry_values(db, entry_values)?;
        self.find_matching_row_in_run(db, &key, true, &RowTest::MatchesEntry)
    }
}

/// The test run over the rows which can match a search
/// (`IndexCursorImpl.RowTest`): either nothing beyond the index entry match, or
/// the whole row pattern (when the pattern columns are not exactly the index
/// columns).
#[derive(Clone, Debug)]
enum RowTest {
    MatchesEntry,
    RowPattern(BTreeMap<String, Value>, bool),
}

/// The generic scan in index order (`CursorImpl.findAnotherRowImpl`): used when
/// the lookup cannot use the index, or when the cursor is mid-traversal.
impl IndexCursor {
    fn scan_matching(
        &mut self,
        db: &mut Database,
        pattern: &BTreeMap<String, Value>,
        move_forward: bool,
        stop_key: Option<&[EntryValue]>,
    ) -> Result<bool> {
        loop {
            if !self.move_index(db, move_forward)? {
                return Ok(false);
            }
            if self.current_row_matches_map(db, pattern)? {
                return Ok(true);
            }
            if let Some(key) = stop_key {
                // `IndexCursorImpl.keepSearching`: once the rows can no longer
                // match the index entry, no later row can match the search
                if !self.current_row_matches_entry_key(db, key)? {
                    return Ok(false);
                }
            }
        }
    }
}

/// Builds the index-column-order key of `IndexData.constructPartialIndexRow
/// FromEntry(filler, values)`: the given values must be a non-empty prefix of
/// the index's columns, and the remaining columns use the given filler.
fn entry_values_prefix(
    entry_values: &[Value],
    num_columns: usize,
    filler: EntryValue,
) -> Result<Vec<EntryValue>> {
    if entry_values.is_empty() {
        return Err(Error::IllegalArgument(
            "At least one column value must be provided".into(),
        ));
    }
    if entry_values.len() > num_columns {
        return Err(Error::IllegalArgument(format!(
            "Too many column values given {}, expected at most {}",
            entry_values.len(),
            num_columns
        )));
    }
    let mut key: Vec<EntryValue> = entry_values.iter().map(EntryValue::from_value).collect();
    key.resize(num_columns, filler);
    Ok(key)
}

impl IndexCursor {
    // -- index cursor specific public API --

    /// `IndexCursor.findFirstRowByEntry`.
    pub fn find_first_row_by_entry(
        &mut self,
        db: &mut Database,
        entry_values: &[Value],
    ) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        let found = self.find_first_row_by_entry_impl(db, entry_values, true)?;
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    /// `IndexCursor.findNextRowByEntry`: continues a by-entry search within the
    /// current run of entries (the operation `EntryIterator.findNext`
    /// performs; jackcess exposes it through `newEntryIterable`).
    pub fn find_next_row_by_entry(
        &mut self,
        db: &mut Database,
        entry_values: &[Value],
    ) -> Result<bool> {
        self.find_next_row_by_entry_impl(db, entry_values)
    }

    /// `IndexCursor.findClosestRowByEntry`: moves to the first entry whose key
    /// is >= the given values.  Returns `true` when a real entry was found and
    /// `false` when the cursor ended up after the last entry.
    pub fn find_closest_row_by_entry(
        &mut self,
        db: &mut Database,
        entry_values: &[Value],
    ) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        let key = self.key_from_entry_values(db, entry_values)?;
        let found = self.find_potential_row(db, &key, false)?;
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    /// `IndexCursor.findRowByEntry`: the matching row, or `None`.
    pub fn find_row_by_entry(
        &mut self,
        db: &mut Database,
        entry_values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        if self.find_first_row_by_entry(db, entry_values)? {
            Ok(Some(self.get_current_row(db)?))
        } else {
            Ok(None)
        }
    }

    /// `IndexCursor.currentRowMatchesEntry`.
    pub fn current_row_matches_entry(
        &mut self,
        db: &mut Database,
        entry_values: &[Value],
    ) -> Result<bool> {
        if !self.is_current_row_valid(db)? {
            return Ok(false);
        }
        let key = self.key_from_entry_values(db, entry_values)?;
        self.current_row_matches_entry_key(db, &key)
    }

    /// Whether the current row's full index key equals the given values (all
    /// index columns must be provided).  This is what makes an index lookup
    /// "exact": the row values, not just the encoded (case folded, possibly
    /// truncated) entry bytes, have to match.
    pub fn current_row_matches_full_entry(
        &mut self,
        db: &mut Database,
        entry_values: &[Value],
    ) -> Result<bool> {
        let columns = self.index_columns(db)?;
        if entry_values.len() != columns.len() {
            return Ok(false);
        }
        self.current_row_matches_entry(db, entry_values)
    }

    /// The current row's index column values, in index column order (the
    /// values the index entry was built from).
    pub fn get_current_entry_row_values(&mut self, db: &mut Database) -> Result<Vec<Value>> {
        let values = self.get_current_row(db)?;
        let columns = self.index_columns(db)?;
        Ok(columns
            .iter()
            .map(|c| {
                values
                    .get(c.column.column_index)
                    .cloned()
                    .unwrap_or(Value::Null)
            })
            .collect())
    }
}

impl Cursor for IndexCursor {
    fn table_name(&self) -> &str {
        IndexCursor::table_name(self)
    }

    fn get_id(&self) -> CursorId {
        IndexCursor::get_id(self)
    }

    fn before_first(&mut self) {
        // `CursorImpl.reset(MOVE_FORWARD)`: the cursor starts on the first
        // (start boundary) position of its range
        self.cur = self.start_bound.clone();
        self.prev = self.cur.clone();
    }

    fn after_last(&mut self) {
        self.cur = self.end_bound.clone();
        self.prev = self.cur.clone();
    }

    fn is_before_first(&self) -> bool {
        self.cur.compare_to(&self.start_bound) == 0
    }

    fn is_after_last(&self) -> bool {
        self.cur.compare_to(&self.end_bound) == 0
    }

    fn get_column_matcher(&self) -> ColumnMatcher {
        self.matcher
    }

    fn set_column_matcher(&mut self, matcher: ColumnMatcher) {
        self.matcher = matcher;
    }

    fn get_error_handler(&self) -> Option<&Arc<dyn ErrorHandler>> {
        self.error_handler.as_ref()
    }

    fn set_error_handler(&mut self, new_error_handler: Option<Arc<dyn ErrorHandler>>) {
        self.error_handler = new_error_handler;
    }

    fn get_savepoint(&self) -> Savepoint {
        Savepoint::new(
            self.get_id(),
            SavedPos::Index(self.cur.clone()),
            SavedPos::Index(self.prev.clone()),
        )
    }

    fn restore_savepoint(&mut self, savepoint: &Savepoint) -> Result<()> {
        if savepoint.get_cursor_id() != self.get_id() {
            return Err(Error::IllegalArgument(format!(
                "Savepoint {:?} is not valid for this cursor with id {:?}",
                savepoint.get_cursor_id(),
                self.get_id()
            )));
        }
        let (cur, prev) = match (&savepoint.cur, &savepoint.prev) {
            (SavedPos::Index(cur), SavedPos::Index(prev)) => (cur.clone(), prev.clone()),
            _ => {
                return Err(Error::IllegalArgument(
                    "Restored positions must be index positions".into(),
                ))
            }
        };
        // `EntryCursor.restorePosition` -> `updatePosition`: a position which is
        // not a real entry must be one of this cursor's boundary positions
        // (a savepoint of a cursor with a different range can therefore be
        // rejected, like jackcess)
        self.check_restorable_position(&cur)?;
        self.check_restorable_position(&prev)?;
        // `IndexCursorImpl.restorePositionImpl`: the entry cursor is restored
        // with the savepoint's positions, while the cursor's previous position
        // becomes the current one (`CursorImpl.restorePositionImpl`)
        if cur.compare_to(&self.cur) != 0 || prev.compare_to(&self.prev) != 0 {
            self.prev = self.cur.clone();
            self.cur = cur;
        }
        Ok(())
    }

    fn get_current_row_id(&self) -> RowId {
        to_table_row_id(*self.cur.get_row_id())
    }

    fn get_row_count(&mut self, db: &mut Database) -> Result<usize> {
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        Ok(table.row_count.max(0) as usize)
    }

    fn is_current_row_deleted(&mut self, db: &mut Database) -> Result<bool> {
        let id = self.get_current_row_id();
        if id.0 < 0 || id.1 < 0 {
            return Ok(false);
        }
        for (row_id, deleted) in db.row_id_statuses(&self.table)? {
            if row_id == id {
                return Ok(deleted);
            }
        }
        Ok(false)
    }

    fn is_current_row_valid(&mut self, db: &mut Database) -> Result<bool> {
        let id = self.get_current_row_id();
        if id.0 < 0 || id.1 < 0 {
            return Ok(false);
        }
        Ok(db
            .row_id_statuses(&self.table)?
            .iter()
            .any(|(row_id, deleted)| *row_id == id && !deleted))
    }

    fn move_to_next_row(&mut self, db: &mut Database) -> Result<bool> {
        self.move_index(db, true)
    }

    fn move_to_previous_row(&mut self, db: &mut Database) -> Result<bool> {
        self.move_index(db, false)
    }

    fn get_current_row(&mut self, db: &mut Database) -> Result<Vec<Value>> {
        let id = self.get_current_row_id();
        if id.0 < 0 || id.1 < 0 {
            return Err(Error::IllegalState(
                "Current row is not valid (cursor is at the beginning or end of the table)".into(),
            ));
        }
        self.read_row_by_id(db, id)?
            .ok_or_else(|| Error::IllegalState(format!("Row is deleted: ({}, {})", id.0, id.1)))
    }

    fn get_current_row_columns(
        &mut self,
        db: &mut Database,
        column_names: &[&str],
    ) -> Result<Vec<Value>> {
        let row = self.get_current_row(db)?;
        select_columns(db, &self.table, &row, column_names)
    }

    fn get_current_row_as_map(&mut self, db: &mut Database) -> Result<BTreeMap<String, Value>> {
        let row = self.get_current_row(db)?;
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        Ok(table
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), row.get(i).cloned().unwrap_or(Value::Null)))
            .collect())
    }

    fn get_current_row_value(&mut self, db: &mut Database, column: &str) -> Result<Value> {
        let row = self.get_current_row(db)?;
        let page = db.table_page(&self.table)?;
        let table = db.tables.get(&page).unwrap();
        let column = column_by_name(table, column).ok_or_else(|| {
            Error::IllegalArgument(format!(
                "Column {} does not exist in table {}",
                column, self.table
            ))
        })?;
        Ok(row.get(column.column_index).cloned().unwrap_or(Value::Null))
    }

    fn set_current_row_value(
        &mut self,
        db: &mut Database,
        column: &str,
        value: Value,
    ) -> Result<()> {
        self.require_valid(db)?;
        let id = self.get_current_row_id();
        let page = db.table_page(&self.table)?;
        let (ci, num_columns) = {
            let table = db.tables.get(&page).unwrap();
            let ci = table.column_index(column).ok_or_else(|| {
                Error::IllegalArgument(format!(
                    "Column {} does not exist in table {}",
                    column, self.table
                ))
            })?;
            (ci, table.columns.len())
        };
        let mut values = vec![Value::Keep; num_columns];
        values[ci] = value;
        db.update_row(&self.table, id, &values)
    }

    fn update_current_row(&mut self, db: &mut Database, values: &[Value]) -> Result<Vec<Value>> {
        self.require_valid(db)?;
        let id = self.get_current_row_id();
        let num_columns = {
            let page = db.table_page(&self.table)?;
            db.tables.get(&page).unwrap().columns.len()
        };
        let mut row = values.to_vec();
        row.resize(num_columns, Value::Null);
        db.update_row(&self.table, id, &row)?;
        // `TableImpl.updateRow` resolves the KEEP_VALUE entries into the row
        // array it returns; read the row back so the caller sees the values
        // which are now stored
        self.read_row_by_id(db, id)?
            .ok_or_else(|| Error::IllegalState(format!("Row is deleted: ({}, {})", id.0, id.1)))
    }

    fn update_current_row_from_map(
        &mut self,
        db: &mut Database,
        row: &BTreeMap<String, Value>,
    ) -> Result<Vec<Value>> {
        self.require_valid(db)?;
        let id = self.get_current_row_id();
        db.update_row_from_map(&self.table, id, row)
    }

    fn delete_current_row(&mut self, db: &mut Database) -> Result<()> {
        self.require_valid(db)?;
        let id = self.get_current_row_id();
        db.delete_row(&self.table, id)
    }

    fn find_row(&mut self, db: &mut Database, row_id: RowId) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        let found = (|| -> Result<bool> {
            // `TableImpl.positionAtRowHeader`: an invalid page/row id for this
            // table means that the row cannot be found (it is not an error).
            // An `IndexCursorImpl.getRowPosition` then reads the row
            // (`TableImpl.getRow`) to build its index entry, and reading a
            // deleted row is an error.
            let deleted = match self.position_at_row_header(db, row_id) {
                Ok(loc) => loc.deleted,
                Err(Error::IllegalArgument(_)) => return Ok(false),
                Err(e) => return Err(e),
            };
            if deleted {
                return Err(Error::IllegalState(format!(
                    "Row is deleted: ({}, {})",
                    row_id.0, row_id.1
                )));
            }
            let values = self.read_row_by_id(db, row_id)?.ok_or_else(|| {
                Error::IllegalState(format!("Row is deleted: ({}, {})", row_id.0, row_id.1))
            })?;
            // `IndexCursorImpl.getRowPosition`: look the row's own index entry
            // up (`EntryCursor.beforeEntry(row)` + `getNextEntry`, clamped into
            // the cursor's range) and stop on the entry found.  jackcess does
            // *not* continue through the rows sharing the entry, so on a
            // non-unique index (or for a row before the range start) the cursor
            // lands on, and `findRow` reports, the first row of that entry.
            let columns = self.index_columns(db)?;
            let key: Vec<EntryValue> = columns
                .iter()
                .map(|c| {
                    EntryValue::from_value(
                        values.get(c.column.column_index).unwrap_or(&Value::Null),
                    )
                })
                .collect();
            if !self.find_potential_row(db, &key, true)? {
                return Ok(false);
            }
            // `if(!isCurrentRowValid()) return false`
            self.is_current_row_valid(db)
        })();
        let found = found?;
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    fn find_first_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        self.before_first();
        let found = (|| -> Result<bool> {
            match self.key_from_map(db, row_pattern)? {
                Some(key) => {
                    if !self.find_potential_row(db, &key, true)? {
                        return Ok(false);
                    }
                    // `findMatchingRowInRun(x, FORWARD, matcher, test)` where
                    // the test is "the entry matched" for an exact column
                    // pattern and the full row otherwise
                    let exact_column_match = {
                        let columns = self.index_columns(db)?;
                        let names: Vec<String> =
                            columns.iter().map(|c| c.column.name.clone()).collect();
                        row_pattern.len() == names.len()
                            && names.iter().all(|n| row_pattern.contains_key(n))
                    };
                    self.find_matching_row_in_run(
                        db,
                        &key,
                        true,
                        &RowTest::RowPattern(row_pattern.clone(), exact_column_match),
                    )
                }
                None => self.scan_matching(db, row_pattern, true, None),
            }
        })();
        let found = found?;
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    fn find_next_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        let found = (|| -> Result<bool> {
            let key = self.key_from_map(db, row_pattern)?;
            if self.is_before_first() && key.is_some() {
                // the same index-assisted search as findFirstRow
                let mut fresh = self.clone();
                fresh.before_first();
                let found = fresh.find_first_row(db, row_pattern)?;
                self.cur = fresh.cur;
                self.prev = fresh.prev;
                return Ok(found);
            }
            let stop_key = key;
            self.scan_matching(db, row_pattern, true, stop_key.as_deref())
        })();
        let found = found?;
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    fn find_last_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        self.after_last();
        let mut found = false;
        loop {
            if !self.move_index(db, false)? {
                break;
            }
            if self.current_row_matches_map(db, row_pattern)? {
                found = true;
                break;
            }
        }
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    fn find_first_row_by_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<bool> {
        let saved_cur = self.cur.clone();
        let saved_prev = self.prev.clone();
        let mut pattern = BTreeMap::new();
        pattern.insert(column.to_string(), value.clone());
        self.before_first();
        let found = (|| -> Result<bool> {
            match self.key_from_column(db, column, value)? {
                Some(key) => {
                    if !self.find_potential_row(db, &key, true)? {
                        return Ok(false);
                    }
                    // walk the run of rows which share the entry bytes (text
                    // keys fold case)
                    self.find_matching_row_in_run(
                        db,
                        &key,
                        true,
                        &RowTest::RowPattern(pattern.clone(), false),
                    )
                }
                None => self.scan_matching(db, &pattern, true, None),
            }
        })();
        let found = found?;
        if !found && (self.cur != saved_cur || self.prev != saved_prev) {
            self.restore_position_impl(saved_cur);
        }
        Ok(found)
    }

    fn find_next_row_by_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<bool> {
        let mut pattern = BTreeMap::new();
        pattern.insert(column.to_string(), value.clone());
        self.find_next_row(db, &pattern)
    }

    fn current_row_matches(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        if !self.is_current_row_valid(db)? {
            return Ok(false);
        }
        self.current_row_matches_map(db, row_pattern)
    }

    fn current_row_matches_column(
        &mut self,
        db: &mut Database,
        column: &str,
        value: &Value,
    ) -> Result<bool> {
        let mut pattern = BTreeMap::new();
        pattern.insert(column.to_string(), value.clone());
        self.current_row_matches(db, &pattern)
    }

    /// `IndexCursorImpl.findAnotherRowImpl`: the index-assisted search, falling
    /// back to a scan of the cursor when the index cannot be used (or when the
    /// cursor is already in the middle of the traversal).
    fn find_another_row(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
        move_forward: bool,
        reset: bool,
    ) -> Result<bool> {
        let saved = self.get_savepoint();
        let found = self.find_another_row_impl(db, row_pattern, move_forward, reset);
        match found {
            Ok(true) => found,
            _ => {
                self.restore_savepoint(&saved)?;
                found
            }
        }
    }
}

impl IndexCursor {
    /// `IndexCursorImpl.getRowPosition` + `EntryCursor.updatePosition`: whether
    /// a saved position can be restored into this cursor.  A real entry is
    /// always accepted (and clamped by the movement of the cursor), while a
    /// boundary position has to be one of this cursor's own bounds.
    fn check_restorable_position(&self, entry: &Entry) -> Result<()> {
        if entry.is_valid()
            || entry.compare_to(&self.start_bound) == 0
            || entry.compare_to(&self.end_bound) == 0
        {
            return Ok(());
        }
        Err(Error::IllegalArgument(format!(
            "Invalid entry given {:?}",
            entry
        )))
    }

    fn find_another_row_impl(
        &mut self,
        db: &mut Database,
        row_pattern: &BTreeMap<String, Value>,
        move_forward: bool,
        reset: bool,
    ) -> Result<bool> {
        let row_values = self.key_from_map(db, row_pattern)?;
        if reset {
            if move_forward {
                self.before_first();
            } else {
                self.after_last();
            }
        }
        let at_beginning = if move_forward {
            self.is_before_first()
        } else {
            self.is_after_last()
        };
        match row_values {
            Some(row_values) if at_beginning => {
                // sweet, we can use our index
                if !self.find_potential_row(db, &row_values, true)? {
                    // at end of index, no potential matches
                    return Ok(false);
                }
                // determine if the pattern columns exactly match the index
                // columns
                let exact_column_match = {
                    let columns = self.index_columns(db)?;
                    let names: Vec<String> =
                        columns.iter().map(|c| c.column.name.clone()).collect();
                    row_pattern.len() == names.len()
                        && names.iter().all(|n| row_pattern.contains_key(n))
                };
                self.find_matching_row_in_run(
                    db,
                    &row_values,
                    move_forward,
                    &RowTest::RowPattern(row_pattern.clone(), exact_column_match),
                )
            }
            _ => {
                // use the default table scan if we don't have index data or we
                // are mid-cursor
                self.scan_matching(db, row_pattern, move_forward, row_values.as_deref())
            }
        }
    }
}

impl IndexCursor {
    fn require_valid(&mut self, db: &mut Database) -> Result<()> {
        if self.is_current_row_valid(db)? {
            Ok(())
        } else {
            Err(Error::IllegalState(
                "Current row is not valid (cursor is at the beginning or end of the table, or \
                 on a deleted row)"
                    .into(),
            ))
        }
    }

    // -- the entry iterator of `EntryIterableBuilder` --

    /// The initial lookup of `EntryIterator`: `findFirstRowByEntryImpl(values,
    /// true, matcher)`.  Unlike [`IndexCursor::find_first_row_by_entry`] a
    /// failed lookup does not restore the cursor, like the iterator
    /// constructor.
    pub fn entry_iter_start(&mut self, db: &mut Database, entry_values: &[Value]) -> Result<bool> {
        self.find_first_row_by_entry_impl(db, entry_values, true)
    }

    /// `EntryIterator.findNext`: `moveToNextRow() && findMatchingRowInRun(...)`.
    pub fn entry_iter_next(&mut self, db: &mut Database, entry_values: &[Value]) -> Result<bool> {
        self.find_next_row_by_entry_impl(db, entry_values)
    }
}

// ---------------------------------------------------------------------------
// iterables (`IterableBuilder` / `EntryIterableBuilder`)
// ---------------------------------------------------------------------------

/// The match type of an [`IterableBuilder`] (`IterableBuilder.Type`).
#[derive(Clone, Debug, PartialEq)]
pub enum IterMatch {
    /// `Type.SIMPLE`: iterate every row of the cursor.
    Simple,
    /// `Type.COLUMN_MATCH`: iterate the rows where one column matches a value.
    Column(String, Value),
    /// `Type.ROW_MATCH`: iterate the rows which match a row pattern.
    Row(BTreeMap<String, Value>),
}

/// Builder style options for iterating a cursor's rows (`IterableBuilder`).
///
/// The [`RowIterator`] produced by [`IterableBuilder::rows`] behaves like
/// `CursorImpl.BaseIterator`: a `reset` builder (the default) starts the
/// traversal at the beginning of the cursor (or its end when reversed),
/// otherwise the current row is the first row when it is valid.  The column
/// names restrict the returned rows to that subset of columns, and the match
/// patterns iterate only matching rows (forward or backward).
#[derive(Clone, Debug)]
pub struct IterableBuilder {
    forward: bool,
    reset: bool,
    column_names: Option<Vec<String>>,
    matcher: Option<ColumnMatcher>,
    match_pattern: IterMatch,
}

impl Default for IterableBuilder {
    fn default() -> Self {
        IterableBuilder::new()
    }
}

impl IterableBuilder {
    /// A new builder: forward, reset, every column, every row.
    pub fn new() -> IterableBuilder {
        IterableBuilder {
            forward: true,
            reset: true,
            column_names: None,
            matcher: None,
            match_pattern: IterMatch::Simple,
        }
    }

    /// `IterableBuilder.forward`.
    pub fn forward(mut self) -> Self {
        self.forward = true;
        self
    }

    /// `IterableBuilder.reverse`.
    pub fn reverse(mut self) -> Self {
        self.forward = false;
        self
    }

    /// `IterableBuilder.setForward`.
    pub fn set_forward(mut self, forward: bool) -> Self {
        self.forward = forward;
        self
    }

    /// `IterableBuilder.reset`.
    pub fn reset(mut self, reset: bool) -> Self {
        self.reset = reset;
        self
    }

    /// `IterableBuilder.setColumnNames`: only these columns are returned.
    pub fn set_column_names(mut self, column_names: Vec<String>) -> Self {
        self.column_names = Some(column_names);
        self
    }

    /// `IterableBuilder.addColumnNames`.
    pub fn add_column_names(&mut self, column_names: &[&str]) -> &mut Self {
        let names = self.column_names.get_or_insert_with(Vec::new);
        for name in column_names {
            if !names.iter().any(|n| n == name) {
                names.push((*name).to_string());
            }
        }
        self
    }

    /// `IterableBuilder.setMatchPattern(Column, Object)`.
    pub fn set_match_pattern_column(&mut self, column: &str, value: Value) -> &mut Self {
        self.match_pattern = IterMatch::Column(column.to_string(), value);
        self
    }

    /// `IterableBuilder.setMatchPattern(Map)`.
    pub fn set_match_pattern_row(&mut self, pattern: BTreeMap<String, Value>) -> &mut Self {
        self.match_pattern = IterMatch::Row(pattern);
        self
    }

    /// `IterableBuilder.addMatchPattern`.
    pub fn add_match_pattern(&mut self, column: &str, value: Value) -> &mut Self {
        match &mut self.match_pattern {
            IterMatch::Row(pattern) => {
                pattern.insert(column.to_string(), value);
            }
            _ => {
                let mut pattern = BTreeMap::new();
                pattern.insert(column.to_string(), value);
                self.match_pattern = IterMatch::Row(pattern);
            }
        }
        self
    }

    /// `IterableBuilder.setColumnMatcher`.
    pub fn set_column_matcher(mut self, matcher: ColumnMatcher) -> Self {
        self.matcher = Some(matcher);
        self
    }

    /// The configured column subset, if any.
    pub fn get_column_names(&self) -> Option<&Vec<String>> {
        self.column_names.as_ref()
    }

    /// The configured direction.
    pub fn is_forward(&self) -> bool {
        self.forward
    }

    /// Whether the iteration resets the cursor first.
    pub fn is_reset(&self) -> bool {
        self.reset
    }

    /// The configured match pattern.
    pub fn get_match_pattern(&self) -> &IterMatch {
        &self.match_pattern
    }

    /// Creates the row iterator (`CursorImpl.iterator(IterableBuilder)`).
    pub fn rows(&self) -> RowIterator {
        RowIterator {
            column_names: self.column_names.clone(),
            forward: self.forward,
            reset: self.reset,
            matcher: self.matcher,
            match_pattern: self.match_pattern.clone(),
            started: false,
            has_next: None,
            valid_row: false,
        }
    }

    /// `IterableBuilder.stream().collect()`: every row of the configured
    /// iteration as a [`Row`] (`IterableBuilder` implements `Iterable<Row>` in
    /// Java; the Rust builder does not own the cursor, so the cursor and
    /// database are passed in).
    pub fn rows_object(&self, cursor: &mut dyn Cursor, db: &mut Database) -> Result<Vec<Row>> {
        let mut iter = self.rows();
        let mut rows = Vec::new();
        while let Some(row) = iter.next_row_object(cursor, db)? {
            rows.push(row);
        }
        Ok(rows)
    }
}

/// The iterator of an [`IterableBuilder`] (`CursorImpl.BaseIterator`): a
/// modifiable row iterator.  `remove` deletes the row the iterator is on.
pub struct RowIterator {
    column_names: Option<Vec<String>>,
    forward: bool,
    reset: bool,
    matcher: Option<ColumnMatcher>,
    match_pattern: IterMatch,
    started: bool,
    has_next: Option<bool>,
    valid_row: bool,
}

impl RowIterator {
    /// `BaseIterator` construction: resets the cursor when requested, otherwise
    /// the current row (when valid) is the first row.
    fn start(&mut self, cursor: &mut dyn Cursor, db: &mut Database) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        if self.matcher.is_none() {
            self.matcher = Some(cursor.get_column_matcher());
        }
        if self.reset {
            if self.forward {
                cursor.before_first();
            } else {
                cursor.after_last();
            }
        } else if cursor.is_current_row_valid(db)? {
            self.has_next = Some(true);
            self.valid_row = true;
        }
        Ok(())
    }

    /// `Iterator.hasNext`.
    pub fn has_next(&mut self, cursor: &mut dyn Cursor, db: &mut Database) -> Result<bool> {
        self.start(cursor, db)?;
        if let Some(has_next) = self.has_next {
            return Ok(has_next);
        }
        let found = self.find_next(cursor, db)?;
        self.has_next = Some(found);
        self.valid_row = found;
        Ok(found)
    }

    /// `BaseIterator.findNext` for the configured iterator type.
    fn find_next(&mut self, cursor: &mut dyn Cursor, db: &mut Database) -> Result<bool> {
        let forward = self.forward;
        let matcher = self.matcher.unwrap_or(ColumnMatcher::Simple);
        match self.match_pattern.clone() {
            // `RowIterator`
            IterMatch::Simple => {
                if forward {
                    cursor.move_to_next_row(db)
                } else {
                    cursor.move_to_previous_row(db)
                }
            }
            // `ColumnMatchIterator`
            IterMatch::Column(column, value) => with_column_matcher(cursor, matcher, |c| {
                c.find_another_row_by_column(db, &column, &value, forward, false)
            }),
            // `RowMatchIterator`
            IterMatch::Row(pattern) => with_column_matcher(cursor, matcher, |c| {
                c.find_another_row(db, &pattern, forward, false)
            }),
        }
    }

    /// `Iterator.next`: the current row (restricted to the configured columns),
    /// or `None` when the iteration is over.
    pub fn next_row(
        &mut self,
        cursor: &mut dyn Cursor,
        db: &mut Database,
    ) -> Result<Option<Vec<Value>>> {
        if !self.has_next(cursor, db)? {
            return Ok(None);
        }
        let row = current_row_for_columns(cursor, db, &self.column_names)?;
        self.has_next = None;
        Ok(Some(row))
    }

    /// [`RowIterator::next_row`] as a [`Row`] (`IterableBuilder` implements
    /// `Iterable<Row>` in Java).
    pub fn next_row_object(
        &mut self,
        cursor: &mut dyn Cursor,
        db: &mut Database,
    ) -> Result<Option<Row>> {
        if !self.has_next(cursor, db)? {
            return Ok(None);
        }
        let column_names = self.column_names.clone();
        let row = match column_names {
            None => cursor.get_current_row_object(db)?,
            Some(names) => {
                let refs: Vec<&str> = names.iter().map(|name| name.as_str()).collect();
                cursor.get_current_row_object_columns(db, &refs)?
            }
        };
        self.has_next = None;
        Ok(Some(row))
    }

    /// `Iterator.remove`: deletes the current row through the cursor.  Only
    /// valid once per row, like jackcess.
    pub fn remove(&mut self, cursor: &mut dyn Cursor, db: &mut Database) -> Result<()> {
        if !self.valid_row {
            return Err(Error::IllegalState("Not at valid row".into()));
        }
        cursor.delete_current_row(db)?;
        self.valid_row = false;
        Ok(())
    }
}

/// `EntryIterableBuilder`: builder style options for iterating the rows of one
/// index entry (`IndexCursor.newEntryIterable`).
#[derive(Clone, Debug, Default)]
pub struct EntryIterableBuilder {
    entry_values: Vec<Value>,
    column_names: Option<Vec<String>>,
    matcher: Option<ColumnMatcher>,
}

impl EntryIterableBuilder {
    /// A builder for the rows of the given index entry values
    /// (`IndexCursor.newEntryIterable`).
    pub fn new(entry_values: Vec<Value>) -> EntryIterableBuilder {
        EntryIterableBuilder {
            entry_values,
            column_names: None,
            matcher: None,
        }
    }

    /// `EntryIterableBuilder.setColumnNames`.
    pub fn set_column_names(mut self, column_names: Vec<String>) -> Self {
        self.column_names = Some(column_names);
        self
    }

    /// `EntryIterableBuilder.addColumnNames`.
    pub fn add_column_names(&mut self, column_names: &[&str]) -> &mut Self {
        let names = self.column_names.get_or_insert_with(Vec::new);
        for name in column_names {
            if !names.iter().any(|n| n == name) {
                names.push((*name).to_string());
            }
        }
        self
    }

    /// `EntryIterableBuilder.setEntryValues`.
    pub fn set_entry_values(&mut self, entry_values: Vec<Value>) -> &mut Self {
        self.entry_values = entry_values;
        self
    }

    /// `EntryIterableBuilder.setColumnMatcher`.
    pub fn set_column_matcher(mut self, matcher: ColumnMatcher) -> Self {
        self.matcher = Some(matcher);
        self
    }

    /// The configured entry values.
    pub fn get_entry_values(&self) -> &[Value] {
        &self.entry_values
    }

    /// The configured column subset, if any.
    pub fn get_column_names(&self) -> Option<&Vec<String>> {
        self.column_names.as_ref()
    }

    /// Creates the entry row iterator (`IndexCursorImpl.entryIterator`).
    pub fn rows(&self) -> EntryRowIterator {
        EntryRowIterator {
            entry_values: self.entry_values.clone(),
            column_names: self.column_names.clone(),
            matcher: self.matcher,
            started: false,
            has_next: None,
            valid_row: false,
        }
    }
}

/// The iterator of an [`EntryIterableBuilder`] (`IndexCursorImpl.EntryIterator`):
/// walks the rows of one index entry, forward, and can delete them.
pub struct EntryRowIterator {
    entry_values: Vec<Value>,
    column_names: Option<Vec<String>>,
    matcher: Option<ColumnMatcher>,
    started: bool,
    has_next: Option<bool>,
    valid_row: bool,
}

impl EntryRowIterator {
    fn start(&mut self, cursor: &mut IndexCursor, db: &mut Database) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        if self.matcher.is_none() {
            self.matcher = Some(cursor.get_column_matcher());
        }
        let matcher = self.matcher.unwrap_or(ColumnMatcher::Simple);
        let values = self.entry_values.clone();
        let found = with_index_matcher(cursor, matcher, |c| c.entry_iter_start(db, &values))?;
        self.has_next = Some(found);
        self.valid_row = found;
        Ok(())
    }

    /// `Iterator.hasNext`.
    pub fn has_next(&mut self, cursor: &mut IndexCursor, db: &mut Database) -> Result<bool> {
        self.start(cursor, db)?;
        if let Some(has_next) = self.has_next {
            return Ok(has_next);
        }
        let matcher = self.matcher.unwrap_or(ColumnMatcher::Simple);
        let values = self.entry_values.clone();
        let found = with_index_matcher(cursor, matcher, |c| c.entry_iter_next(db, &values))?;
        self.has_next = Some(found);
        self.valid_row = found;
        Ok(found)
    }

    /// `Iterator.next`: the current row (restricted to the configured columns),
    /// or `None` when the run of rows is exhausted.
    pub fn next_row(
        &mut self,
        cursor: &mut IndexCursor,
        db: &mut Database,
    ) -> Result<Option<Vec<Value>>> {
        if !self.has_next(cursor, db)? {
            return Ok(None);
        }
        let row = current_row_for_columns(cursor, db, &self.column_names)?;
        self.has_next = None;
        Ok(Some(row))
    }

    /// [`EntryRowIterator::next_row`] as a [`Row`]
    /// (`IndexCursor.newEntryIterable` also iterates `Row`s in Java).
    pub fn next_row_object(
        &mut self,
        cursor: &mut IndexCursor,
        db: &mut Database,
    ) -> Result<Option<Row>> {
        if !self.has_next(cursor, db)? {
            return Ok(None);
        }
        let column_names = self.column_names.clone();
        let row = match column_names {
            None => cursor.get_current_row_object(db)?,
            Some(names) => {
                let refs: Vec<&str> = names.iter().map(|name| name.as_str()).collect();
                cursor.get_current_row_object_columns(db, &refs)?
            }
        };
        self.has_next = None;
        Ok(Some(row))
    }

    /// `Iterator.remove`: deletes the current row through the cursor.
    pub fn remove(&mut self, cursor: &mut IndexCursor, db: &mut Database) -> Result<()> {
        if !self.valid_row {
            return Err(Error::IllegalState("Not at valid row".into()));
        }
        cursor.delete_current_row(db)?;
        self.valid_row = false;
        Ok(())
    }
}

/// The current row in the given column subset (`Cursor.getCurrentRow
/// (Collection)`).
fn current_row_for_columns(
    cursor: &mut dyn Cursor,
    db: &mut Database,
    column_names: &Option<Vec<String>>,
) -> Result<Vec<Value>> {
    match column_names {
        None => cursor.get_current_row(db),
        Some(names) => {
            let refs: Vec<&str> = names.iter().map(|n| n.as_str()).collect();
            cursor.get_current_row_columns(db, &refs)
        }
    }
}

/// Runs `f` with the given `ColumnMatcher` installed on the cursor, restoring
/// the cursor's own matcher afterwards (`CursorImpl` passes the matcher of an
/// iterator through to its searches instead).
fn with_column_matcher<R>(
    cursor: &mut dyn Cursor,
    matcher: ColumnMatcher,
    f: impl FnOnce(&mut dyn Cursor) -> Result<R>,
) -> Result<R> {
    let saved = cursor.get_column_matcher();
    if saved != matcher {
        cursor.set_column_matcher(matcher);
    }
    let result = f(cursor);
    if saved != matcher {
        cursor.set_column_matcher(saved);
    }
    result
}

/// The [`IndexCursor`] flavour of [`with_column_matcher`].
fn with_index_matcher<R>(
    cursor: &mut IndexCursor,
    matcher: ColumnMatcher,
    f: impl FnOnce(&mut IndexCursor) -> Result<R>,
) -> Result<R> {
    let saved = cursor.get_column_matcher();
    if saved != matcher {
        cursor.set_column_matcher(matcher);
    }
    let result = f(cursor);
    if saved != matcher {
        cursor.set_column_matcher(saved);
    }
    result
}

// ---------------------------------------------------------------------------
// cursor builder (`CursorBuilder`)
// ---------------------------------------------------------------------------

/// A row of a range bound (`CursorBuilder.setStartRow`): either the sparse row
/// array itself or a column name to value map which is resolved against the
/// table's columns.
#[derive(Clone, Debug)]
enum RowSpec {
    Row(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

/// Builder style construction of a cursor (`CursorBuilder`).
///
/// By default a cursor is created at the beginning of the table, its start and
/// end rows are inclusive, and a table scan cursor is created.  A range narrows
/// an index cursor (`set_index_by_name` / `set_index_by_columns`); like jackcess
/// 5.0.4 a table scan cursor ignores it.
///
/// An index can be chosen by name ([`CursorBuilder::set_index_by_name`], like
/// `CursorBuilder.setIndexByName`), from a logical index
/// ([`CursorBuilder::set_index`], like `CursorBuilder.setIndex`), or by its
/// columns ([`CursorBuilder::set_index_by_columns`] /
/// [`CursorBuilder::set_index_by_column_names`], like
/// `CursorBuilder.setIndexByColumns` / `setIndexByColumnNames`).  The column
/// list selects the index the way `TableImpl.findIndexForColumns(...,
/// ANY_MATCH)` does: the first index whose columns are exactly the given columns
/// (in order, matched case insensitively), otherwise the shortest index which
/// begins with them; when no index matches, building the cursor fails with an
/// `IllegalArgumentException` (`"Index with columns [...] does not exist in
/// table ..."`).  jackcess throws that while the column names are set, but this
/// builder only holds the table's name, so the index is resolved when the cursor
/// is built - or on demand with [`CursorBuilder::resolve_index`].
#[derive(Clone, Debug)]
pub struct CursorBuilder {
    table: String,
    index: Option<String>,
    /// `setIndexByColumns`: the index is resolved from these column names
    index_columns: Option<Vec<String>>,
    start_row: Option<RowSpec>,
    start_entry: Option<Vec<Value>>,
    start_inclusive: bool,
    end_row: Option<RowSpec>,
    end_entry: Option<Vec<Value>>,
    end_inclusive: bool,
    before_first: bool,
    savepoint: Option<Savepoint>,
    matcher: Option<ColumnMatcher>,
}

impl CursorBuilder {
    /// A builder for the named table.
    pub fn new(table: &str) -> CursorBuilder {
        CursorBuilder {
            table: table.to_string(),
            index: None,
            index_columns: None,
            start_row: None,
            start_entry: None,
            start_inclusive: true,
            end_row: None,
            end_entry: None,
            end_inclusive: true,
            before_first: true,
            savepoint: None,
            matcher: None,
        }
    }

    /// `CursorBuilder.beforeFirst`: start at the beginning (unless a savepoint
    /// is given).
    pub fn before_first(&mut self) -> &mut Self {
        self.before_first = true;
        self
    }

    /// `CursorBuilder.afterLast`: start at the end (unless a savepoint is
    /// given).
    pub fn after_last(&mut self) -> &mut Self {
        self.before_first = false;
        self
    }

    /// `CursorBuilder.restoreSavepoint`.
    pub fn restore_savepoint(&mut self, savepoint: &Savepoint) -> &mut Self {
        self.savepoint = Some(savepoint.clone());
        self
    }

    /// `CursorBuilder.setIndexByName`.
    pub fn set_index_by_name(&mut self, index_name: &str) -> &mut Self {
        self.index = Some(index_name.to_string());
        self.index_columns = None;
        self
    }

    /// `CursorBuilder.setIndex(Index)`: the given logical index of the
    /// builder's table (an index of another table is not detected until the
    /// cursor is built, unlike jackcess, because the index carries no table
    /// reference).
    pub fn set_index(&mut self, index: &IndexImpl) -> &mut Self {
        self.set_index_by_name(&index.name)
    }

    /// `CursorBuilder.setIndexByColumns` / `setIndexByColumnNames`: the index of
    /// the table whose leading columns are exactly the given columns.
    ///
    /// The columns are matched case insensitively and in order; see the type
    /// documentation for how an index is chosen and for the error reported when
    /// none matches.
    pub fn set_index_by_columns(&mut self, column_names: &[&str]) -> &mut Self {
        self.index = None;
        self.index_columns = Some(column_names.iter().map(|c| c.to_string()).collect());
        self
    }

    /// `CursorBuilder.setIndexByColumnNames` (the varargs flavour of
    /// [`CursorBuilder::set_index_by_columns`]).
    pub fn set_index_by_column_names(&mut self, column_names: &[&str]) -> &mut Self {
        self.set_index_by_columns(column_names)
    }

    /// The index the builder will use for an index cursor: the index named by
    /// `setIndex`/`setIndexByName`, or the index selected by
    /// `setIndexByColumns`/`setIndexByColumnNames`
    /// (`TableImpl.findIndexForColumns`).
    ///
    /// This is the eager form of the resolution `toIndexCursor` performs: it
    /// reports the same errors (`IllegalArgumentException` for an unknown index
    /// name or for column names no index starts with, and for a builder without
    /// an index).
    pub fn resolve_index(&self, db: &mut Database) -> Result<IndexImpl> {
        let page = db.table_page(&self.table)?;
        let table = db
            .tables
            .get(&page)
            .ok_or_else(|| Error::IllegalState(format!("table {} is not loaded", self.table)))?;
        if let Some(column_names) = &self.index_columns {
            return table
                .find_index_for_columns(column_names, IndexFeature::AnyMatch)
                .cloned()
                .ok_or_else(|| {
                    // `CursorBuilder.setIndexByColumns`:
                    // "Index with columns " + searchColumns + " does not exist in
                    // table " + _table
                    Error::IllegalArgument(format!(
                        "Index with columns [{}] does not exist in table {}",
                        column_names.join(", "),
                        self.table
                    ))
                });
        }
        let index_name = self.index.as_ref().ok_or_else(|| {
            Error::IllegalArgument("an index must be specified for an index cursor".into())
        })?;
        // `TableImpl.getIndex`
        table
            .indexes
            .iter()
            .find(|i| i.name.eq_ignore_ascii_case(index_name))
            .cloned()
            .ok_or_else(|| {
                Error::IllegalArgument(format!(
                    "Index with name {} does not exist in table {}",
                    index_name, self.table
                ))
            })
    }

    /// `CursorBuilder.setSpecificRow`.
    pub fn set_specific_row(&mut self, row: Vec<Value>) -> &mut Self {
        self.start_row = Some(RowSpec::Row(row.clone()));
        self.end_row = Some(RowSpec::Row(row));
        self
    }

    /// `CursorBuilder.setSpecificEntry`.
    pub fn set_specific_entry(&mut self, entry_values: Vec<Value>) -> &mut Self {
        self.start_entry = Some(entry_values.clone());
        self.end_entry = Some(entry_values);
        self
    }

    /// `CursorBuilder.setStartRow`.
    pub fn set_start_row(&mut self, row: Vec<Value>) -> &mut Self {
        self.start_row = Some(RowSpec::Row(row));
        self.start_entry = None;
        self
    }

    /// `CursorBuilder.setStartRow` with the row given as a column map.
    pub fn set_start_row_map(&mut self, row: &BTreeMap<String, Value>) -> &mut Self {
        self.start_row = Some(RowSpec::Map(row.clone()));
        self.start_entry = None;
        self
    }

    /// `CursorBuilder.setStartEntry`.
    pub fn set_start_entry(&mut self, entry_values: Vec<Value>) -> &mut Self {
        self.start_entry = Some(entry_values);
        self.start_row = None;
        self
    }

    /// `CursorBuilder.setStartRowInclusive`.
    pub fn set_start_row_inclusive(&mut self, inclusive: bool) -> &mut Self {
        self.start_inclusive = inclusive;
        self
    }

    /// `CursorBuilder.setEndRow`.
    pub fn set_end_row(&mut self, row: Vec<Value>) -> &mut Self {
        self.end_row = Some(RowSpec::Row(row));
        self.end_entry = None;
        self
    }

    /// `CursorBuilder.setEndRow` with the row given as a column map.
    pub fn set_end_row_map(&mut self, row: &BTreeMap<String, Value>) -> &mut Self {
        self.end_row = Some(RowSpec::Map(row.clone()));
        self.end_entry = None;
        self
    }

    /// `CursorBuilder.setEndEntry`.
    pub fn set_end_entry(&mut self, entry_values: Vec<Value>) -> &mut Self {
        self.end_entry = Some(entry_values);
        self.end_row = None;
        self
    }

    /// `CursorBuilder.setEndRowInclusive`.
    pub fn set_end_row_inclusive(&mut self, inclusive: bool) -> &mut Self {
        self.end_inclusive = inclusive;
        self
    }

    /// `CursorBuilder.setColumnMatcher`.
    pub fn set_column_matcher(&mut self, matcher: ColumnMatcher) -> &mut Self {
        self.matcher = Some(matcher);
        self
    }

    /// `CursorBuilder.setCaseInsensitive`.
    pub fn set_case_insensitive(&mut self) -> &mut Self {
        self.set_column_matcher(ColumnMatcher::CaseInsensitive)
    }

    /// Converts a `RowSpec` into the sparse row array `setStartRow` takes.
    fn resolve_row(&self, db: &mut Database, row: &RowSpec) -> Result<Vec<Value>> {
        match row {
            RowSpec::Row(values) => Ok(values.clone()),
            RowSpec::Map(map) => {
                let page = db.table_page(&self.table)?;
                let table = db.tables.get(&page).unwrap();
                let mut values = vec![Value::Null; table.columns.len()];
                for (name, value) in map {
                    let column = column_by_name(table, name).ok_or_else(|| {
                        Error::IllegalArgument(format!(
                            "Column {} does not exist in table {}",
                            name, self.table
                        ))
                    })?;
                    values[column.column_index] = value.clone();
                }
                Ok(values)
            }
        }
    }

    fn build_range(&self, db: &mut Database) -> Result<IndexRange> {
        let mut range = IndexRange::new();
        range.start_inclusive = self.start_inclusive;
        range.end_inclusive = self.end_inclusive;
        if let Some(entry) = &self.start_entry {
            range.start = Some(RangeKey::Entry(entry.clone()));
        } else if let Some(row) = &self.start_row {
            range.start = Some(RangeKey::Row(self.resolve_row(db, row)?));
        }
        if let Some(entry) = &self.end_entry {
            range.end = Some(RangeKey::Entry(entry.clone()));
        } else if let Some(row) = &self.end_row {
            range.end = Some(RangeKey::Row(self.resolve_row(db, row)?));
        }
        Ok(range)
    }

    /// `CursorBuilder.toCursor`: a cursor for the given table, narrowed to the
    /// configured range when an index is set.
    pub fn to_cursor(&mut self, db: &mut Database) -> Result<Box<dyn Cursor>> {
        if self.index.is_some() || self.index_columns.is_some() {
            return Ok(Box::new(self.to_index_cursor(db)?));
        }
        let mut cursor = TableScanCursor::new(db, &self.table)?;
        if let Some(matcher) = self.matcher {
            cursor.set_column_matcher(matcher);
        }
        if let Some(savepoint) = &self.savepoint {
            cursor.restore_savepoint(savepoint)?;
        } else if !self.before_first {
            cursor.after_last();
        }
        Ok(Box::new(cursor))
    }

    /// `CursorBuilder.toIndexCursor`: an index cursor narrowed to the
    /// configured range.
    pub fn to_index_cursor(&mut self, db: &mut Database) -> Result<IndexCursor> {
        let index = self.resolve_index(db)?;
        let range = self.build_range(db)?;
        let mut cursor = IndexCursor::new_with_range(db, &self.table, &index.name, &range)?;
        if let Some(matcher) = self.matcher {
            cursor.set_column_matcher(matcher);
        }
        if let Some(savepoint) = &self.savepoint {
            cursor.restore_savepoint(savepoint)?;
        } else if !self.before_first {
            cursor.after_last();
        }
        Ok(cursor)
    }

    // -- the static `CursorBuilder` helpers of jackcess 5.0.4 --

    /// `CursorBuilder.createCursor(Table)`: a normal, un-indexed cursor.
    pub fn create_cursor(db: &mut Database, table: &str) -> Result<Box<dyn Cursor>> {
        CursorBuilder::new(table).to_cursor(db)
    }

    /// `CursorBuilder.createCursor(Index)`: an indexed cursor over the named
    /// index of the named table.  Index based traversal may not include all rows
    /// of the table (an index which ignores null entries skips those rows).
    pub fn create_index_cursor(
        db: &mut Database,
        table: &str,
        index_name: &str,
    ) -> Result<IndexCursor> {
        IndexCursor::new(db, table, index_name)
    }

    /// `CursorBuilder.createPrimaryKeyCursor(Table)`.
    pub fn create_primary_key_cursor(db: &mut Database, table: &str) -> Result<IndexCursor> {
        let index_name = {
            let page = db.table_page(table)?;
            let t = db
                .tables
                .get(&page)
                .ok_or_else(|| Error::IllegalState(format!("table {} is not loaded", table)))?;
            t.get_primary_key_index()?.name.clone()
        };
        IndexCursor::new(db, table, &index_name)
    }

    /// `CursorBuilder.createCursor(Index, Object[] startRow, Object[] endRow)`:
    /// an indexed cursor narrowed to the given (inclusive) table rows, where the
    /// rows are the sparse row arrays `setStartRow`/`setEndRow` take.
    pub fn create_cursor_with_range(
        db: &mut Database,
        table: &str,
        index_name: &str,
        start_row: Option<Vec<Value>>,
        end_row: Option<Vec<Value>>,
    ) -> Result<IndexCursor> {
        CursorBuilder::create_cursor_with_range_inclusive(
            db, table, index_name, start_row, true, end_row, true,
        )
    }

    /// `CursorBuilder.createCursor(Index, Object[] startRow, boolean
    /// startInclusive, Object[] endRow, boolean endInclusive)`.
    pub fn create_cursor_with_range_inclusive(
        db: &mut Database,
        table: &str,
        index_name: &str,
        start_row: Option<Vec<Value>>,
        start_inclusive: bool,
        end_row: Option<Vec<Value>>,
        end_inclusive: bool,
    ) -> Result<IndexCursor> {
        let mut builder = CursorBuilder::new(table);
        builder.set_index_by_name(index_name);
        if let Some(row) = start_row {
            builder.set_start_row(row);
        }
        builder.set_start_row_inclusive(start_inclusive);
        if let Some(row) = end_row {
            builder.set_end_row(row);
        }
        builder.set_end_row_inclusive(end_inclusive);
        builder.to_index_cursor(db)
    }

    /// `CursorBuilder.findRow(Table, Map)`: the first row of the table matching
    /// the pattern (always searched from the beginning of the table), or `None`.
    pub fn find_row(
        db: &mut Database,
        table: &str,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<Option<Vec<Value>>> {
        let mut cursor = CursorBuilder::create_cursor(db, table)?;
        if cursor.find_first_row(db, row_pattern)? {
            Ok(Some(cursor.get_current_row(db)?))
        } else {
            Ok(None)
        }
    }

    /// `CursorBuilder.findRow(Index, Map)`: the first row matching the pattern,
    /// searched through the named index.
    pub fn find_row_by_index(
        db: &mut Database,
        table: &str,
        index_name: &str,
        row_pattern: &BTreeMap<String, Value>,
    ) -> Result<Option<Vec<Value>>> {
        let mut cursor = CursorBuilder::create_index_cursor(db, table, index_name)?;
        if cursor.find_first_row(db, row_pattern)? {
            Ok(Some(cursor.get_current_row(db)?))
        } else {
            Ok(None)
        }
    }

    /// `CursorBuilder.findRowByEntry(Index, Object...)`.
    pub fn find_row_by_entry(
        db: &mut Database,
        table: &str,
        index_name: &str,
        entry_values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        let mut cursor = CursorBuilder::create_index_cursor(db, table, index_name)?;
        cursor.find_row_by_entry(db, entry_values)
    }

    /// `CursorBuilder.findRowByPrimaryKey(Table, Object...)`.
    pub fn find_row_by_primary_key(
        db: &mut Database,
        table: &str,
        entry_values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        let mut cursor = CursorBuilder::create_primary_key_cursor(db, table)?;
        cursor.find_row_by_entry(db, entry_values)
    }

    /// `CursorBuilder.findValue(Table, Column, Column, Object)`: the value of
    /// `column` in the first row whose `column_pattern` matches `value_pattern`
    /// (always searched from the beginning of the table).
    ///
    /// Unlike jackcess - whose `null` result cannot distinguish "no match" from
    /// a matching `null` value - the result is an `Option`, so the two cases are
    /// distinguishable.
    pub fn find_value(
        db: &mut Database,
        table: &str,
        column: &str,
        column_pattern: &str,
        value_pattern: &Value,
    ) -> Result<Option<Value>> {
        let mut cursor = CursorBuilder::create_cursor(db, table)?;
        CursorBuilder::find_value_with(&mut *cursor, db, column, column_pattern, value_pattern)
    }

    /// `CursorBuilder.findValue(Index, Column, Column, Object)`: the indexed
    /// flavour of [`CursorBuilder::find_value`].
    pub fn find_value_by_index(
        db: &mut Database,
        table: &str,
        index_name: &str,
        column: &str,
        column_pattern: &str,
        value_pattern: &Value,
    ) -> Result<Option<Value>> {
        let mut cursor = CursorBuilder::create_index_cursor(db, table, index_name)?;
        CursorBuilder::find_value_with(&mut cursor, db, column, column_pattern, value_pattern)
    }

    fn find_value_with(
        cursor: &mut dyn Cursor,
        db: &mut Database,
        column: &str,
        column_pattern: &str,
        value_pattern: &Value,
    ) -> Result<Option<Value>> {
        if cursor.find_first_row_by_column(db, column_pattern, value_pattern)? {
            Ok(Some(cursor.get_current_row_value(db, column)?))
        } else {
            Ok(None)
        }
    }
}

fn index_data_label(index: &IndexImpl) -> String {
    format!("{} ({})", index.name, index.index_data_number)
}

/// Ordering helper used by the unit tests and callers which compare row ids.
pub fn compare_row_ids(a: RowId, b: RowId) -> Ordering {
    a.0.cmp(&b.0).then(a.1.cmp(&b.1))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnSpec;
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
            "jackcess_cursor_{}_{}.mdb",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        (path, db)
    }

    /// `T(ID long pk, Name text(40) unique, Num long, Note text(40))` plus a
    /// non-unique ascending and a descending index on `Num`.
    fn table_spec() -> TableSpec {
        TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
                ColumnSpec::new("Num", DataType::Long),
                ColumnSpec::new("Note", DataType::Text).with_length(40),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ))
        .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique())
        .with_index(IndexSpec::new("ByNum", vec![("Num".to_string(), true)]))
        .with_index(IndexSpec::new(
            "ByNumDesc",
            vec![("Num".to_string(), false)],
        ))
    }

    fn make_table(db: &mut Database) {
        db.create_table(&table_spec()).expect("create T");
    }

    /// The same table plus the two indexes which make `setIndexByColumns` fall
    /// back to a *partial* (leading column) match: `(Note, ID)` and
    /// `(Note, Num, ID)`.
    fn make_table_with_note_indexes(db: &mut Database) {
        let spec = table_spec()
            .with_index(IndexSpec::new(
                "ByNumId",
                vec![("Num".to_string(), true), ("ID".to_string(), true)],
            ))
            .with_index(IndexSpec::new(
                "ByNoteId",
                vec![("Note".to_string(), true), ("ID".to_string(), true)],
            ))
            .with_index(IndexSpec::new(
                "ByNoteNumId",
                vec![
                    ("Note".to_string(), true),
                    ("Num".to_string(), true),
                    ("ID".to_string(), true),
                ],
            ));
        db.create_table(&spec).expect("create T");
    }

    fn row(id: i32, name: &str, num: Option<i32>, note: Option<&str>) -> Vec<Value> {
        vec![
            Value::Long(id),
            Value::Text(name.to_string()),
            match num {
                Some(n) => Value::Long(n),
                None => Value::Null,
            },
            match note {
                Some(n) => Value::Text(n.to_string()),
                None => Value::Null,
            },
        ]
    }

    fn add_test_rows(db: &mut Database) {
        let mut rows = Vec::new();
        for i in 1..=20i32 {
            let name = if i % 5 == 0 {
                format!("NAME{:02}", i)
            } else {
                format!("name{:02}", i)
            };
            let num = if i % 7 == 0 {
                None
            } else {
                Some((i * 37) % 11)
            };
            let note = if i % 4 == 0 {
                None
            } else {
                Some(format!("note {}", i))
            };
            rows.push(row(i, &name, num, note.as_deref()));
        }
        for r in &rows {
            db.add_rows("T", std::slice::from_ref(r)).expect("add row");
        }
    }

    fn ids(rows: &[Vec<Value>]) -> Vec<i32> {
        rows.iter()
            .map(|r| r[0].as_i64().expect("id") as i32)
            .collect()
    }

    fn by_id(db: &mut Database, id: i32) -> Vec<Value> {
        let rows = db.read_rows_with_ids("T").expect("read rows");
        rows.into_iter()
            .filter_map(|(_, v)| v)
            .find(|r| r[0].as_i64() == Some(id as i64))
            .unwrap_or_else(|| panic!("missing row {}", id))
    }

    fn pattern(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn table_scan_find_semantics() {
        let (path, mut db) = create_test_db("scan_find");
        make_table(&mut db);
        add_test_rows(&mut db);

        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        assert!(cursor.is_before_first());
        assert!(!cursor.is_current_row_valid(&mut db).unwrap());
        assert_eq!(cursor.get_row_count(&mut db).unwrap(), 20);

        // forward and backward iteration
        let mut forward = Vec::new();
        while let Some(r) = cursor.get_next_row(&mut db).unwrap() {
            forward.push(r);
        }
        assert_eq!(ids(&forward), (1..=20).collect::<Vec<_>>());
        assert!(cursor.is_after_last());
        assert!(cursor.get_next_row(&mut db).unwrap().is_none());

        cursor.after_last();
        let mut backward = Vec::new();
        while let Some(r) = cursor.get_previous_row(&mut db).unwrap() {
            backward.push(r);
        }
        assert_eq!(ids(&backward), (1..=20).rev().collect::<Vec<_>>());
        assert!(cursor.is_before_first());

        // findFirstRow(Map)
        assert!(cursor
            .find_first_row(&mut db, &pattern(&[("ID", Value::Long(10))]))
            .unwrap());
        let expected_row_id = db
            .read_rows_with_ids("T")
            .unwrap()
            .into_iter()
            .find(|(_, v)| v.as_ref().and_then(|r| r[0].as_i64()) == Some(10))
            .map(|(id, _)| id)
            .unwrap();
        assert_eq!(cursor.get_current_row_id(), expected_row_id);
        assert!(cursor.is_current_row_valid(&mut db).unwrap());

        // a table scan uses `SimpleColumnMatcher` (`CursorImpl`'s default):
        // text is compared exactly, not case-insensitively
        assert!(cursor
            .find_first_row(&mut db, &pattern(&[("Name", Value::Text("NAME05".into()))]))
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );
        assert!(!cursor
            .find_first_row(&mut db, &pattern(&[("Name", Value::Text("name05".into()))]))
            .unwrap());
        assert!(cursor
            .find_first_row_by_column(&mut db, "Name", &Value::Text("NAME05".into()))
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );
        assert!(!cursor
            .find_first_row_by_column(&mut db, "Name", &Value::Text("name05".into()))
            .unwrap());

        // duplicate values: findNextRow continues after the current row
        let mut found = Vec::new();
        assert!(cursor
            .find_first_row_by_column(&mut db, "Num", &Value::Long(5))
            .unwrap());
        found.push(cursor.get_current_row_value(&mut db, "ID").unwrap());
        while cursor
            .find_next_row_by_column(&mut db, "Num", &Value::Long(5))
            .unwrap()
        {
            found.push(cursor.get_current_row_value(&mut db, "ID").unwrap());
        }
        assert_eq!(found, vec![Value::Long(4), Value::Long(15)]);

        // a missing value fails and restores the previous position
        let before = cursor.get_current_row_id();
        assert!(!cursor
            .find_next_row(&mut db, &pattern(&[("ID", Value::Long(9999))]))
            .unwrap());
        assert_eq!(cursor.get_current_row_id(), before);

        // findLastRow scans backwards
        assert!(cursor
            .find_last_row(&mut db, &pattern(&[("Num", Value::Long(5))]))
            .unwrap());
        let last = cursor.get_current_row_value(&mut db, "ID").unwrap();
        assert_eq!(last, *found.last().unwrap());

        // getNextRow/getPreviousRow with a column subset (`getNextRow
        // (Collection)` in Java)
        cursor.before_first();
        let row = cursor
            .get_next_row_columns(&mut db, &["Note", "ID"])
            .unwrap()
            .unwrap();
        assert_eq!(row.len(), 2);
        assert_eq!(row[0], Value::Text("note 1".into()));
        assert_eq!(row[1], Value::Long(1));
        assert_eq!(
            cursor
                .get_next_row_columns(&mut db, &["ID"])
                .unwrap()
                .unwrap(),
            vec![Value::Long(2)]
        );
        assert_eq!(
            cursor
                .get_previous_row_columns(&mut db, &["ID"])
                .unwrap()
                .unwrap(),
            vec![Value::Long(1)]
        );
        assert!(cursor
            .get_previous_row_columns(&mut db, &["ID"])
            .unwrap()
            .is_none());

        // null values match null
        assert!(cursor
            .find_first_row(&mut db, &pattern(&[("Num", Value::Null)]))
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(7)
        );

        // a pattern naming no column never matches
        assert!(!cursor
            .find_first_row(&mut db, &pattern(&[("Nope", Value::Long(1))]))
            .unwrap());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn table_scan_update_and_delete() {
        let (path, mut db) = create_test_db("scan_update");
        make_table(&mut db);
        add_test_rows(&mut db);

        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        assert!(cursor
            .find_first_row(&mut db, &pattern(&[("ID", Value::Long(5))]))
            .unwrap());
        let row_id = cursor.get_current_row_id();

        // setCurrentRowValue keeps every other column
        cursor
            .set_current_row_value(&mut db, "Name", Value::Text("renamed".into()))
            .unwrap();
        cursor
            .set_current_row_value(&mut db, "Num", Value::Long(77))
            .unwrap();
        assert_eq!(
            by_id(&mut db, 5),
            row(5, "renamed", Some(77), Some("note 5"))
        );
        assert_eq!(cursor.get_current_row_id(), row_id, "row id stays stable");

        // updateCurrentRow with KEEP_VALUE for the other columns
        cursor
            .update_current_row(
                &mut db,
                &[
                    Value::Keep,
                    Value::Text("renamed-again".into()),
                    Value::Keep,
                    Value::Keep,
                ],
            )
            .unwrap();
        assert_eq!(
            by_id(&mut db, 5),
            row(5, "renamed-again", Some(77), Some("note 5"))
        );

        // updateCurrentRowFromMap
        let map = pattern(&[
            ("Note", Value::Text("from map".into())),
            ("Num", Value::Long(78)),
        ]);
        cursor.update_current_row_from_map(&mut db, &map).unwrap();
        assert_eq!(
            by_id(&mut db, 5),
            row(5, "renamed-again", Some(78), Some("from map"))
        );

        // deleteCurrentRow leaves the cursor on the deleted row
        cursor.delete_current_row(&mut db).unwrap();
        assert!(!cursor.is_current_row_valid(&mut db).unwrap());
        assert!(cursor.is_current_row_deleted(&mut db).unwrap());
        assert!(db
            .read_rows_with_ids("T")
            .unwrap()
            .iter()
            .all(|(id, v)| { !(*id == row_id && v.is_some()) }));
        // re-deleting an already deleted row is allowed (it does nothing)
        // but the cursor is no longer valid, so the operation is rejected
        assert!(cursor.delete_current_row(&mut db).is_err());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_ordering_and_lookup() {
        let (path, mut db) = create_test_db("index_order");
        make_table(&mut db);
        add_test_rows(&mut db);

        // primary key ascending
        let mut cursor = IndexCursor::new(&mut db, "T", "PrimaryKey").expect("pk cursor");
        assert!(cursor.is_before_first());
        let mut forward = Vec::new();
        while let Some(r) = cursor.get_next_row(&mut db).unwrap() {
            forward.push(r);
        }
        assert_eq!(ids(&forward), (1..=20).collect::<Vec<_>>());

        // primary key descending
        cursor.after_last();
        let mut backward = Vec::new();
        while let Some(r) = cursor.get_previous_row(&mut db).unwrap() {
            backward.push(r);
        }
        assert_eq!(ids(&backward), (1..=20).rev().collect::<Vec<_>>());

        // lookup by entry
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(12)])
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(12)
        );
        assert!(cursor
            .current_row_matches_entry(&mut db, &[Value::Long(12)])
            .unwrap());
        assert!(!cursor
            .current_row_matches_entry(&mut db, &[Value::Long(13)])
            .unwrap());
        assert!(cursor
            .find_row_by_entry(&mut db, &[Value::Long(13)])
            .unwrap()
            .is_some());
        assert!(cursor
            .find_row_by_entry(&mut db, &[Value::Long(999)])
            .unwrap()
            .is_none());

        // closest entry: first >= the given value
        assert!(cursor
            .find_closest_row_by_entry(&mut db, &[Value::Long(11)])
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(11)
        );

        // unique Name index: entries are ordered case-insensitively, so a
        // lookup with different case still finds the row (the matcher of a
        // unique index is case-insensitive)
        let mut name_cursor = IndexCursor::new(&mut db, "T", "ByName").expect("name cursor");
        assert!(name_cursor
            .find_first_row_by_entry(&mut db, &[Value::Text("name05".into())])
            .unwrap());
        assert_eq!(
            name_cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );
        assert!(name_cursor
            .find_first_row_by_entry(&mut db, &[Value::Text("NAME05".into())])
            .unwrap());
        assert_eq!(
            name_cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );

        // non-unique Num index: ascending, nulls (encoded with the ascending
        // null flag) sort first
        let mut num_cursor = IndexCursor::new(&mut db, "T", "ByNum").expect("num cursor");
        let mut nums = Vec::new();
        while let Some(r) = num_cursor.get_next_row(&mut db).unwrap() {
            nums.push(r[2].as_i64());
        }
        // nulls first, then the numbers ascending, ties broken by row id
        let mut expected: Vec<Option<i64>> = Vec::new();
        for _ in 0..2 {
            expected.push(None);
        }
        for n in 0..=10i64 {
            let mut found = vec![];
            for i in 1..=20i32 {
                if i % 7 == 0 {
                    continue;
                }
                if (((i * 37) % 11) as i64) == n {
                    found.push(Some(n));
                }
            }
            expected.extend(found);
        }
        assert_eq!(nums, expected);

        // descending Num index: descending, nulls (encoded with the descending
        // null flag) sort last
        let mut desc_cursor = IndexCursor::new(&mut db, "T", "ByNumDesc").expect("desc cursor");
        let mut desc_nums = Vec::new();
        while let Some(r) = desc_cursor.get_next_row(&mut db).unwrap() {
            desc_nums.push(r[2].as_i64());
        }
        let mut expected_desc: Vec<Option<i64>> = Vec::new();
        for n in (0..=10i64).rev() {
            for i in 1..=20i32 {
                if i % 7 == 0 {
                    continue;
                }
                if (((i * 37) % 11) as i64) == n {
                    expected_desc.push(Some(n));
                }
            }
        }
        expected_desc.push(None);
        expected_desc.push(None);
        assert_eq!(desc_nums, expected_desc);

        // closest lookup on the non-unique index lands on the first row of the
        // run for the value
        assert!(num_cursor
            .find_closest_row_by_entry(&mut db, &[Value::Long(4)])
            .unwrap());
        assert_eq!(
            num_cursor.get_current_row_value(&mut db, "Num").unwrap(),
            Value::Long(4)
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_map_lookups() {
        let (path, mut db) = create_test_db("index_map");
        make_table(&mut db);
        add_test_rows(&mut db);

        // a pattern keyed by the first index column uses the index (the run of
        // rows with Num 3 holds ids 9 and 20, and the first entry wins)
        let mut by_num = IndexCursor::new(&mut db, "T", "ByNum").expect("cursor");
        assert!(by_num
            .find_first_row(&mut db, &pattern(&[("Num", Value::Long(3))]))
            .unwrap());
        assert_eq!(
            by_num.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(9)
        );
        assert!(by_num
            .current_row_matches(&mut db, &pattern(&[("Num", Value::Long(3))]))
            .unwrap());

        // a unique text index matches case-insensitively
        let mut by_name = IndexCursor::new(&mut db, "T", "ByName").expect("cursor");
        assert!(by_name
            .find_first_row(&mut db, &pattern(&[("Name", Value::Text("name05".into()))]))
            .unwrap());
        assert_eq!(
            by_name.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );

        // a pattern which does not name the first index column falls back to a
        // scan in index order
        assert!(by_num
            .find_first_row(&mut db, &pattern(&[("Name", Value::Text("name13".into()))]))
            .unwrap());
        assert_eq!(
            by_num.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(13)
        );
        assert!(!by_num
            .find_first_row(&mut db, &pattern(&[("Name", Value::Text("nope".into()))]))
            .unwrap());

        // a null pattern matches the null entries of an index which does not
        // ignore nulls
        assert!(by_num
            .find_first_row(&mut db, &pattern(&[("Num", Value::Null)]))
            .unwrap());
        assert_eq!(
            by_num.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(7)
        );

        // the index metadata helpers
        assert!(by_name.is_unique(&mut db).unwrap());
        assert!(!by_num.is_unique(&mut db).unwrap());
        assert!(!by_num.should_ignore_nulls(&mut db).unwrap());
        assert_eq!(by_name.get_index_columns(&mut db).unwrap().len(), 1);
        assert!(by_name.get_index_columns(&mut db).unwrap()[0].is_ascending());
        let desc = IndexCursor::new(&mut db, "T", "ByNumDesc").expect("cursor");
        assert!(!desc.get_index_columns(&mut db).unwrap()[0].is_ascending());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_update_and_delete() {
        let (path, mut db) = create_test_db("index_update");
        make_table(&mut db);
        add_test_rows(&mut db);

        // update the indexed Name key through the unique index cursor
        let mut cursor = IndexCursor::new(&mut db, "T", "ByName").expect("cursor");
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Text("name05".into())])
            .unwrap());
        let row_id = cursor.get_current_row_id();
        cursor
            .set_current_row_value(&mut db, "Name", Value::Text("renamed05".into()))
            .unwrap();
        assert_eq!(
            by_id(&mut db, 5),
            row(5, "renamed05", Some((5 * 37) % 11), Some("note 5"))
        );
        assert_eq!(cursor.get_current_row_id(), row_id);
        // the old entry is gone, the new one is found
        assert!(!cursor
            .find_first_row_by_entry(&mut db, &[Value::Text("name05".into())])
            .unwrap());
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Text("RENAMED05".into())])
            .unwrap());

        // update Num through the non-unique index cursor (changes the indexed
        // key of another index)
        let mut num_cursor = IndexCursor::new(&mut db, "T", "ByNum").expect("cursor");
        assert!(num_cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(3)])
            .unwrap());
        let num_id = num_cursor.get_current_row_id();
        assert_eq!(
            num_cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(9)
        );
        num_cursor
            .update_current_row(
                &mut db,
                &[Value::Keep, Value::Keep, Value::Long(99), Value::Keep],
            )
            .unwrap();
        assert_eq!(num_cursor.get_current_row_id(), num_id);
        let updated = db
            .read_rows_with_ids("T")
            .unwrap()
            .into_iter()
            .filter_map(|(_, v)| v)
            .find(|r| r[2].as_i64() == Some(99))
            .expect("row with Num 99");
        assert_eq!(updated[0].as_i64(), Some(9));

        // delete the current row through the index cursor
        num_cursor.after_last();
        assert!(num_cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(99)])
            .unwrap());
        num_cursor.delete_current_row(&mut db).unwrap();
        assert!(!num_cursor.is_current_row_valid(&mut db).unwrap());
        assert!(db
            .read_rows_with_ids("T")
            .unwrap()
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .all(|r| r[2].as_i64() != Some(99)));
        let count = num_cursor.get_row_count(&mut db).unwrap();
        assert_eq!(count, 19);

        // adding rows through the table after the cursor operations
        db.add_rows("T", &[row(21, "name21", Some(4), Some("note 21"))])
            .unwrap();
        db.add_rows("T", &[row(22, "name22", None, None)]).unwrap();
        let mut cursor = IndexCursor::new(&mut db, "T", "PrimaryKey").expect("cursor");
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(22)])
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "Name").unwrap(),
            Value::Text("name22".into())
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_run_and_next_entry() {
        let (path, mut db) = create_test_db("index_run");
        make_table(&mut db);
        // rows 1..=4 share Num = 2 (non-unique index)
        let rows = vec![
            row(1, "a", Some(2), Some("one")),
            row(2, "b", Some(2), Some("two")),
            row(3, "c", Some(2), Some("three")),
            row(4, "d", Some(5), Some("four")),
        ];
        for r in &rows {
            db.add_rows("T", std::slice::from_ref(r)).unwrap();
        }
        let mut cursor = IndexCursor::new(&mut db, "T", "ByNum").expect("cursor");
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(2)])
            .unwrap());
        let mut found = vec![cursor.get_current_row_value(&mut db, "ID").unwrap()];
        while cursor
            .find_next_row_by_entry(&mut db, &[Value::Long(2)])
            .unwrap()
        {
            found.push(cursor.get_current_row_value(&mut db, "ID").unwrap());
        }
        assert_eq!(found, vec![Value::Long(1), Value::Long(2), Value::Long(3)]);
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(5)])
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(4)
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_truncated_key_lookup() {
        let (path, mut db) = create_test_db("index_truncated");
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(510),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ))
        .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique());
        db.create_table(&spec).unwrap();
        // a 255 character key is longer than the maximum index key (255 text
        // units = 510 bytes for Jet 4), so the index entry is truncated and
        // digested; the search key is built the same way
        let long_name = "x".repeat(255);
        db.add_rows(
            "T",
            &[
                vec![Value::Long(1), Value::Text(long_name.clone())],
                vec![Value::Long(2), Value::Text("short".into())],
            ],
        )
        .unwrap();
        let mut cursor = IndexCursor::new(&mut db, "T", "ByName").expect("cursor");
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Text(long_name)])
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(1)
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn current_row_validity_and_move_rows() {
        let (path, mut db) = create_test_db("validity");
        make_table(&mut db);
        add_test_rows(&mut db);

        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        assert!(!cursor.is_current_row_valid(&mut db).unwrap());
        assert!(cursor.get_current_row(&mut db).is_err());
        assert_eq!(cursor.move_next_rows(&mut db, 3).unwrap(), 3);
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(3)
        );
        assert_eq!(cursor.move_previous_rows(&mut db, 2).unwrap(), 2);
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(1)
        );
        assert_eq!(cursor.move_previous_rows(&mut db, 2).unwrap(), 0);
        assert!(cursor.is_before_first());

        cursor.after_last();
        assert_eq!(cursor.move_next_rows(&mut db, 2).unwrap(), 0);
        assert!(cursor.is_after_last());
        cursor.before_first();
        assert_eq!(cursor.move_previous_rows(&mut db, 2).unwrap(), 0);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // row ranges (`CursorBuilder.setStartRow`/`setEndRow`)
    // ------------------------------------------------------------------

    fn ranged_pk(range: &IndexRange, db: &mut Database) -> IndexCursor {
        IndexCursor::new_with_range(db, "T", "PrimaryKey", range).expect("range cursor")
    }

    fn scan_all(cursor: &mut IndexCursor, db: &mut Database, forward: bool) -> Vec<i32> {
        let mut out = Vec::new();
        if forward {
            while let Some(r) = cursor.get_next_row(db).unwrap() {
                out.push(r[0].as_i64().unwrap() as i32);
            }
        } else {
            while let Some(r) = cursor.get_previous_row(db).unwrap() {
                out.push(r[0].as_i64().unwrap() as i32);
            }
        }
        out
    }

    #[test]
    fn index_cursor_range_boundaries() {
        let (path, mut db) = create_test_db("range_boundaries");
        make_table(&mut db);
        add_test_rows(&mut db);

        // inclusive/inclusive: [5, 10]
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(5)])
            .set_end_entry(vec![Value::Long(10)]);
        let mut cursor = ranged_pk(&range, &mut db);
        // a fresh cursor is on the start (before first) position
        assert!(cursor.is_before_first());
        assert!(!cursor.is_after_last());
        assert_eq!(cursor.get_current_row_id(), FIRST_ROW_ID);
        assert_eq!(
            scan_all(&mut cursor, &mut db, true),
            (5..=10).collect::<Vec<_>>()
        );
        assert!(cursor.is_after_last());
        // an inclusive end boundary carries the last row id
        assert_eq!(cursor.get_current_row_id(), LAST_ROW_ID);
        // and back again (afterLast then reverse)
        assert_eq!(
            scan_all(&mut cursor, &mut db, false),
            (5..=10).rev().collect::<Vec<_>>()
        );
        assert!(cursor.is_before_first());
        assert_eq!(cursor.get_current_row_id(), FIRST_ROW_ID);
        // beforeFirst/afterLast/reset
        cursor.reset();
        assert!(cursor.is_before_first());
        assert_eq!(cursor.move_next_rows(&mut db, 2).unwrap(), 2);
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(6)
        );
        cursor.after_last();
        assert_eq!(cursor.move_previous_rows(&mut db, 3).unwrap(), 3);
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(8)
        );

        // inclusive start, exclusive end: [5, 10)
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(5)])
            .set_end_entry(vec![Value::Long(10)])
            .set_start_row_inclusive(true)
            .set_end_row_inclusive(false);
        let mut cursor = ranged_pk(&range, &mut db);
        assert_eq!(
            scan_all(&mut cursor, &mut db, true),
            (5..=9).collect::<Vec<_>>()
        );
        // an exclusive end boundary carries the first row id
        assert_eq!(cursor.get_current_row_id(), FIRST_ROW_ID);
        cursor.before_first();
        assert_eq!(cursor.get_current_row_id(), FIRST_ROW_ID);

        // exclusive start, inclusive end: (5, 10]
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(5)])
            .set_end_entry(vec![Value::Long(10)])
            .set_start_row_inclusive(false);
        let mut cursor = ranged_pk(&range, &mut db);
        // the exclusive start boundary carries the last row id
        assert_eq!(cursor.get_current_row_id(), LAST_ROW_ID);
        assert_eq!(
            scan_all(&mut cursor, &mut db, true),
            (6..=10).collect::<Vec<_>>()
        );
        cursor.after_last();
        assert_eq!(cursor.get_current_row_id(), LAST_ROW_ID);

        // exclusive both: (5, 10)
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(5)])
            .set_end_entry(vec![Value::Long(10)])
            .set_start_row_inclusive(false)
            .set_end_row_inclusive(false);
        let mut cursor = ranged_pk(&range, &mut db);
        assert_eq!(
            scan_all(&mut cursor, &mut db, true),
            (6..=9).collect::<Vec<_>>()
        );
        assert_eq!(
            scan_all(&mut cursor, &mut db, false),
            (6..=9).rev().collect::<Vec<_>>()
        );

        // open start / open end
        let mut range = IndexRange::new();
        range.set_end_entry(vec![Value::Long(3)]);
        let mut cursor = ranged_pk(&range, &mut db);
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![1, 2, 3]);

        let mut range = IndexRange::new();
        range.set_start_entry(vec![Value::Long(18)]);
        let mut cursor = ranged_pk(&range, &mut db);
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![18, 19, 20]);

        // a single row (`setSpecificEntry`)
        let mut range = IndexRange::new();
        range.set_specific_entry(vec![Value::Long(7)]);
        let mut cursor = ranged_pk(&range, &mut db);
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![7]);
        assert_eq!(scan_all(&mut cursor, &mut db, false), vec![7]);

        // rows addressed by a row (`setStartRow`) rather than an entry
        let mut range = IndexRange::new();
        range.set_start_row(vec![Value::Long(4)]).set_end_row(vec![
            Value::Long(6),
            Value::Null,
            Value::Null,
            Value::Null,
        ]);
        let mut cursor = ranged_pk(&range, &mut db);
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![4, 5, 6]);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_multi_column_ranges() {
        let (path, mut db) = create_test_db("range_multi");
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
                ColumnSpec::new("Num", DataType::Long),
                ColumnSpec::new("Note", DataType::Text).with_length(40),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ))
        .with_index(IndexSpec::new(
            "ByNumId",
            vec![("Num".to_string(), true), ("ID".to_string(), true)],
        ));
        db.create_table(&spec).unwrap();
        add_test_rows(&mut db);

        // a prefix entry bound: the trailing column is filled with MIN_VALUE for
        // the start and MAX_VALUE for the end, so the whole run of the prefix
        // key is held
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(3)])
            .set_end_entry(vec![Value::Long(3)]);
        let mut cursor = IndexCursor::new_with_range(&mut db, "T", "ByNumId", &range).unwrap();
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![9, 20]);

        // an exclusive prefix end still includes the run: the MAX_VALUE filler
        // makes the bound sort after every entry of the prefix key
        range
            .set_end_entry(vec![Value::Long(4)])
            .set_end_row_inclusive(false);
        let mut cursor = IndexCursor::new_with_range(&mut db, "T", "ByNumId", &range).unwrap();
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![9, 20, 1, 12]);

        // an exclusive prefix start still includes the run (MIN_VALUE filler)
        range
            .set_start_entry(vec![Value::Long(3)])
            .set_start_row_inclusive(false)
            .set_end_entry(vec![Value::Long(4)]);
        let mut cursor = IndexCursor::new_with_range(&mut db, "T", "ByNumId", &range).unwrap();
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![9, 20, 1, 12]);

        // a full entry bound holds exactly that entry
        range = IndexRange::new();
        range.set_specific_entry(vec![Value::Long(3), Value::Long(9)]);
        let mut cursor = IndexCursor::new_with_range(&mut db, "T", "ByNumId", &range).unwrap();
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![9]);

        // an exclusive full-value end excludes exactly that entry
        range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(4)])
            .set_end_entry(vec![Value::Long(4), Value::Long(12)])
            .set_end_row_inclusive(false);
        let mut cursor = IndexCursor::new_with_range(&mut db, "T", "ByNumId", &range).unwrap();
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![1]);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_reversed_and_empty_ranges() {
        let (path, mut db) = create_test_db("range_empty");
        make_table(&mut db);
        add_test_rows(&mut db);

        // a reversed range yields nothing in either direction
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(10)])
            .set_end_entry(vec![Value::Long(5)]);
        let mut cursor = ranged_pk(&range, &mut db);
        assert!(cursor.is_before_first());
        assert!(!cursor.is_after_last());
        let mut forward = Vec::new();
        while cursor.move_to_next_row(&mut db).unwrap() {
            forward.push(cursor.get_current_row_value(&mut db, "ID").unwrap());
        }
        assert!(forward.is_empty());
        // the failed forward move leaves the cursor on the end position
        assert!(cursor.is_after_last());
        assert!(!cursor.is_before_first());
        assert!(!cursor.move_to_next_row(&mut db).unwrap());
        assert!(cursor.get_current_row(&mut db).is_err());
        // and no rows backwards either
        assert!(scan_all(&mut cursor, &mut db, false).is_empty());
        assert!(cursor.is_before_first());

        // an empty range: start == end, both exclusive
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(10)])
            .set_end_entry(vec![Value::Long(10)])
            .set_start_row_inclusive(false)
            .set_end_row_inclusive(false);
        let mut cursor = ranged_pk(&range, &mut db);
        // the search positions differ (before/after the run of the key), so the
        // cursor is only at the beginning until the first move clamps it
        assert!(cursor.is_before_first());
        assert!(!cursor.is_after_last());
        assert!(!cursor.move_to_next_row(&mut db).unwrap());
        assert!(cursor.is_after_last());
        assert!(!cursor.move_to_previous_row(&mut db).unwrap());
        assert!(cursor.is_before_first());
        assert!(scan_all(&mut cursor, &mut db, true).is_empty());

        // [10, 10] (inclusive at a present row) is not empty
        let mut range = IndexRange::new();
        range.set_specific_entry(vec![Value::Long(10)]);
        let mut cursor = ranged_pk(&range, &mut db);
        assert!(!cursor.is_after_last());
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![10]);

        // a range beyond the data
        let mut range = IndexRange::new();
        range.set_start_entry(vec![Value::Long(100)]);
        let mut cursor = ranged_pk(&range, &mut db);
        assert!(!cursor.is_after_last());
        assert!(scan_all(&mut cursor, &mut db, true).is_empty());
        assert!(cursor.is_after_last());

        // a range before the data
        let mut range = IndexRange::new();
        range.set_end_entry(vec![Value::Long(0)]);
        let mut cursor = ranged_pk(&range, &mut db);
        assert!(scan_all(&mut cursor, &mut db, true).is_empty());

        // a key which does not exist inside the range (empty, but the bounds
        // differ)
        let mut range = IndexRange::new();
        range.set_specific_entry(vec![Value::Long(500)]);
        let mut cursor = ranged_pk(&range, &mut db);
        assert!(scan_all(&mut cursor, &mut db, true).is_empty());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_range_finds() {
        let (path, mut db) = create_test_db("range_finds");
        make_table(&mut db);
        add_test_rows(&mut db);

        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(5)])
            .set_end_entry(vec![Value::Long(10)]);
        let mut cursor = ranged_pk(&range, &mut db);

        // findFirstRow within the range
        assert!(cursor
            .find_first_row(&mut db, &pattern(&[("ID", Value::Long(7))]))
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(7)
        );
        // a row outside the range is not found, and the cursor is restored
        assert!(!cursor
            .find_first_row(&mut db, &pattern(&[("ID", Value::Long(12))]))
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(7)
        );
        // a row before the range is not found either
        assert!(!cursor
            .find_first_row(&mut db, &pattern(&[("ID", Value::Long(2))]))
            .unwrap());

        // findNextRow from the beginning uses the index, and a mid-cursor
        // findNextRow scan stops as soon as the row can no longer match the
        // index entry (`IndexCursorImpl.keepSearching`)
        cursor.before_first();
        assert!(cursor
            .find_next_row(&mut db, &pattern(&[("ID", Value::Long(9))]))
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(9)
        );
        assert!(!cursor
            .find_next_row(&mut db, &pattern(&[("ID", Value::Long(9))]))
            .unwrap());

        // findFirstRowByEntry in/out of the range
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(9)])
            .unwrap());
        assert!(!cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(12)])
            .unwrap());
        assert!(!cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(3)])
            .unwrap());

        // findClosestRowByEntry clamps to the start of the range
        assert!(cursor
            .find_closest_row_by_entry(&mut db, &[Value::Long(2)])
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );
        // and lands on the end position beyond the range
        assert!(cursor
            .find_closest_row_by_entry(&mut db, &[Value::Long(12)])
            .unwrap());
        assert!(!cursor.is_current_row_valid(&mut db).unwrap());
        assert!(cursor.is_after_last());
        assert!(cursor.get_current_row(&mut db).is_err());

        // a closest lookup of a missing key inside the range moves to the next
        // row of the range
        assert!(cursor
            .find_closest_row_by_entry(&mut db, &[Value::Long(8)])
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(8)
        );

        // findLastRow (backwards search) stays inside the range
        assert!(cursor
            .find_last_row(&mut db, &pattern(&[("Num", Value::Long(3))]))
            .unwrap());
        // Num 3 belongs to ids 9 and 20; only 9 is inside [5, 10]
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(9)
        );

        // moveToNextRow from a mid-range position whose row leaves the range
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(10)])
            .unwrap());
        assert!(!cursor.move_to_next_row(&mut db).unwrap());
        assert!(cursor.is_after_last());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_range_with_unique_and_non_unique_indexes() {
        let (path, mut db) = create_test_db("range_indexes");
        make_table(&mut db);
        add_test_rows(&mut db);

        // unique text index: entries fold case, so the bounds match either case
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Text("name05".into())])
            .set_end_entry(vec![Value::Text("name10".into())]);
        let mut cursor =
            IndexCursor::new_with_range(&mut db, "T", "ByName", &range).expect("range");
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Text("NAME07".into())])
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(7)
        );
        // a row of the index outside the range is not reachable
        assert!(!cursor
            .find_first_row_by_entry(&mut db, &[Value::Text("name12".into())])
            .unwrap());

        // non-unique ascending index with null entries: a null start row sorts
        // before every entry
        let mut range = IndexRange::new();
        range
            .set_start_row(vec![Value::Null])
            .set_end_entry(vec![Value::Long(2)]);
        let mut cursor = IndexCursor::new_with_range(&mut db, "T", "ByNum", &range).expect("range");
        let mut nums = Vec::new();
        while let Some(r) = cursor.get_next_row(&mut db).unwrap() {
            nums.push(r[2].as_i64());
        }
        // nulls first, then 0, 1, 2
        assert_eq!(nums, vec![None, None, Some(0), Some(1), Some(2), Some(2)]);

        // descending index: the entry value order is reversed, so a [6, 3]
        // range holds the rows with Num 3..=6
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(6)])
            .set_end_entry(vec![Value::Long(3)]);
        let mut cursor =
            IndexCursor::new_with_range(&mut db, "T", "ByNumDesc", &range).expect("range");
        let mut desc_nums = Vec::new();
        while let Some(r) = cursor.get_next_row(&mut db).unwrap() {
            desc_nums.push(r[2].as_i64());
        }
        assert_eq!(
            desc_nums,
            vec![
                Some(6),
                Some(5),
                Some(5),
                Some(4),
                Some(4),
                Some(3),
                Some(3)
            ]
        );
        // the reversed range on the same index holds nothing
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(3)])
            .set_end_entry(vec![Value::Long(6)]);
        let mut cursor =
            IndexCursor::new_with_range(&mut db, "T", "ByNumDesc", &range).expect("range");
        assert!(scan_all(&mut cursor, &mut db, true).is_empty());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_range_updates_and_deletes() {
        let (path, mut db) = create_test_db("range_update");
        make_table(&mut db);
        add_test_rows(&mut db);

        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(5)])
            .set_end_entry(vec![Value::Long(8)]);
        let mut cursor = ranged_pk(&range, &mut db);

        // update a non-indexed column inside the range
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(6)])
            .unwrap());
        cursor
            .set_current_row_value(&mut db, "Note", Value::Text("ranged".into()))
            .unwrap();
        assert_eq!(
            by_id(&mut db, 6),
            row(6, "name06", Some((6 * 37) % 11), Some("ranged"))
        );

        // delete a row of the range and continue iterating
        cursor.before_first();
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![5, 6, 7, 8]);
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(7)])
            .unwrap());
        cursor.delete_current_row(&mut db).unwrap();
        assert!(!cursor.is_current_row_valid(&mut db).unwrap());
        assert_eq!(cursor.move_next_rows(&mut db, 1).unwrap(), 1);
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(8)
        );
        // the next move clamps the cursor to the end of the range
        assert_eq!(cursor.move_next_rows(&mut db, 10).unwrap(), 0);
        assert!(cursor.is_after_last());
        // and forward again after beforeFirst
        cursor.before_first();
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![5, 6, 8]);

        // changing the indexed key moves the row out of the range
        cursor.before_first();
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(8)])
            .unwrap());
        cursor
            .set_current_row_value(&mut db, "ID", Value::Long(80))
            .unwrap();
        // the current (old) key is no longer in the range
        assert!(!cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(8)])
            .unwrap());
        cursor.before_first();
        assert_eq!(scan_all(&mut cursor, &mut db, true), vec![5, 6]);
        // the moved row is not part of the range
        assert!(!cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(80)])
            .unwrap());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // savepoints
    // ------------------------------------------------------------------

    #[test]
    fn table_scan_savepoints() {
        let (path, mut db) = create_test_db("scan_savepoint");
        make_table(&mut db);
        add_test_rows(&mut db);

        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        assert!(cursor
            .find_first_row(&mut db, &pattern(&[("ID", Value::Long(7))]))
            .unwrap());
        let savepoint = cursor.get_savepoint();
        assert_eq!(savepoint.get_current_row_id(), cursor.get_current_row_id());
        let row_id = cursor.get_current_row_id();

        assert_eq!(cursor.move_next_rows(&mut db, 3).unwrap(), 3);
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(10)
        );
        cursor.restore_savepoint(&savepoint).unwrap();
        assert_eq!(cursor.get_current_row_id(), row_id);
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(7)
        );

        // savepoints may come from another cursor instance of the same table
        let mut other = TableScanCursor::new(&mut db, "T").expect("cursor");
        other.restore_savepoint(&savepoint).unwrap();
        assert_eq!(
            other.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(7)
        );

        // a savepoint of an index cursor is not valid for a table scan
        let index_savepoint = {
            let mut index = IndexCursor::new(&mut db, "T", "PrimaryKey").unwrap();
            index.before_first();
            index.get_savepoint()
        };
        assert!(other.restore_savepoint(&index_savepoint).is_err());

        // restoring a savepoint taken on a deleted row leaves the cursor on the
        // deleted row (savepoints do not undo data changes)
        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        assert!(cursor
            .find_first_row(&mut db, &pattern(&[("ID", Value::Long(5))]))
            .unwrap());
        let deleted_savepoint = cursor.get_savepoint();
        cursor.delete_current_row(&mut db).unwrap();
        assert!(cursor.is_current_row_deleted(&mut db).unwrap());
        assert_eq!(cursor.move_next_rows(&mut db, 2).unwrap(), 2);
        cursor.restore_savepoint(&deleted_savepoint).unwrap();
        assert!(!cursor.is_current_row_valid(&mut db).unwrap());
        assert!(cursor.is_current_row_deleted(&mut db).unwrap());

        // the restored position becomes the previous position
        // (`restorePositionImpl`), so moving back from a restored "after last"
        // position continues from where the cursor was
        cursor.after_last();
        let after_last = cursor.get_savepoint();
        cursor.restore_savepoint(&after_last).unwrap();
        assert!(cursor.is_after_last());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_cursor_savepoints() {
        let (path, mut db) = create_test_db("index_savepoint");
        make_table(&mut db);
        add_test_rows(&mut db);

        let mut cursor = IndexCursor::new(&mut db, "T", "PrimaryKey").expect("cursor");
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(12)])
            .unwrap());
        let savepoint = cursor.get_savepoint();

        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Long(3)])
            .unwrap());
        cursor
            .set_current_row_value(&mut db, "Note", Value::Text("saved".into()))
            .unwrap();
        cursor.restore_savepoint(&savepoint).unwrap();
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(12)
        );

        // a savepoint of another index is rejected
        let other_index = {
            let mut other = IndexCursor::new(&mut db, "T", "ByName").unwrap();
            other.before_first();
            other.get_savepoint()
        };
        assert!(cursor.restore_savepoint(&other_index).is_err());

        // restoring works across cursor instances of the same index
        let mut other = IndexCursor::new(&mut db, "T", "PrimaryKey").unwrap();
        other.restore_savepoint(&savepoint).unwrap();
        assert_eq!(
            other.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(12)
        );

        // savepoint of a boundary position
        other.before_first();
        let before = other.get_savepoint();
        assert_eq!(before.get_current_row_id(), FIRST_ROW_ID);
        assert!(other.move_to_next_row(&mut db).unwrap());
        other.restore_savepoint(&before).unwrap();
        assert!(other.is_before_first());

        // a savepoint taken outside a ranged cursor can be restored into it
        // when its positions are real entries (a boundary position of another
        // range is rejected, like `EntryCursor.updatePosition`)
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(5)])
            .set_end_entry(vec![Value::Long(10)]);
        let mut ranged = ranged_pk(&range, &mut db);
        let mut unbounded = IndexCursor::new(&mut db, "T", "PrimaryKey").unwrap();
        assert!(unbounded
            .find_first_row_by_entry(&mut db, &[Value::Long(14)])
            .unwrap());
        assert!(unbounded.move_to_next_row(&mut db).unwrap());
        let outside = unbounded.get_savepoint();
        ranged.restore_savepoint(&outside).unwrap();
        // the cursor is on a row outside the range, and the next move clamps to
        // the end of the range
        assert_eq!(
            ranged.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(15)
        );
        assert!(!ranged.move_to_next_row(&mut db).unwrap());
        assert!(ranged.is_after_last());

        // a boundary position of a source cursor whose bounds differ is
        // rejected (jackcess throws "Invalid entry given")
        let unbounded = IndexCursor::new(&mut db, "T", "PrimaryKey").unwrap();
        let boundary_savepoint = unbounded.get_savepoint();
        assert!(ranged.restore_savepoint(&boundary_savepoint).is_err());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // iterables
    // ------------------------------------------------------------------

    fn ids_of(rows: &[Vec<Value>]) -> Vec<i32> {
        rows.iter()
            .map(|r| r[0].as_i64().expect("id") as i32)
            .collect()
    }

    #[test]
    fn iterable_builder_simple_and_columns() {
        let (path, mut db) = create_test_db("iterables");
        make_table(&mut db);
        add_test_rows(&mut db);

        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        let builder = IterableBuilder::new();
        let mut iter = builder.rows();
        let mut rows = Vec::new();
        while let Some(row) = iter.next_row(&mut cursor, &mut db).unwrap() {
            rows.push(row);
        }
        assert_eq!(ids_of(&rows), (1..=20).collect::<Vec<_>>());
        assert_eq!(rows[0].len(), 4);
        assert!(!iter.has_next(&mut cursor, &mut db).unwrap());

        // reverse iteration
        let builder = IterableBuilder::new().reverse();
        let mut iter = builder.rows();
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut cursor, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, (1..=20).rev().collect::<Vec<_>>());

        // a subset of columns, in the requested order
        let builder =
            IterableBuilder::new().set_column_names(vec!["Note".to_string(), "ID".to_string()]);
        let mut iter = builder.rows();
        let rows: Vec<Vec<Value>> =
            std::iter::from_fn(|| iter.next_row(&mut cursor, &mut db).unwrap()).collect();
        assert_eq!(rows.len(), 20);
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[0][0], Value::Text("note 1".into()));
        assert_eq!(rows[0][1], Value::Long(1));
        assert_eq!(rows[3][0], Value::Null);

        // without reset the iteration starts on the current row
        assert!(cursor
            .find_first_row(&mut db, &pattern(&[("ID", Value::Long(18))]))
            .unwrap());
        let builder = IterableBuilder::new().reset(false);
        let mut iter = builder.rows();
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut cursor, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![18, 19, 20]);

        // index cursor iteration (ByNum order, with a column subset)
        let mut index = IndexCursor::new(&mut db, "T", "ByNum").expect("cursor");
        let builder = IterableBuilder::new().set_column_names(vec!["Num".to_string()]);
        let mut iter = builder.rows();
        let mut nums = Vec::new();
        while let Some(row) = iter.next_row(&mut index, &mut db).unwrap() {
            nums.push(row[0].as_i64());
        }
        let mut expected = vec![None, None];
        for n in 0..=10i64 {
            for i in 1..=20i32 {
                if i % 7 != 0 && (((i * 37) % 11) as i64) == n {
                    expected.push(Some(n));
                }
            }
        }
        assert_eq!(nums, expected);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn iterable_builder_match_patterns() {
        let (path, mut db) = create_test_db("iter_matches");
        make_table(&mut db);
        add_test_rows(&mut db);

        // row match, forward and reverse, over a table scan
        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        let mut builder = IterableBuilder::new();
        builder.set_match_pattern_row(pattern(&[("Num", Value::Long(3))]));
        let mut iter = builder.rows();
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut cursor, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![9, 20]);

        let mut builder = IterableBuilder::new().reverse();
        builder.add_match_pattern("Num", Value::Long(3));
        let mut iter = builder.rows();
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut cursor, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![20, 9]);

        // column match
        let mut builder = IterableBuilder::new();
        builder.set_match_pattern_column("Num", Value::Long(4));
        let mut iter = builder.rows();
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut cursor, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![1, 12]);

        // no matching row: the iteration is empty and the cursor is restored
        assert!(cursor
            .find_first_row(&mut db, &pattern(&[("ID", Value::Long(7))]))
            .unwrap());
        let before = cursor.get_current_row_id();
        // without reset the current row is the first row of the iteration (like
        // `BaseIterator`), then the pattern scan runs and restores the position
        let mut builder = IterableBuilder::new().reset(false);
        builder.set_match_pattern_row(pattern(&[("Num", Value::Long(99))]));
        let mut iter = builder.rows();
        assert_eq!(
            iter.next_row(&mut cursor, &mut db).unwrap().unwrap()[0],
            Value::Long(7)
        );
        assert!(iter.next_row(&mut cursor, &mut db).unwrap().is_none());
        assert_eq!(cursor.get_current_row_id(), before);

        // row match over an index cursor (the index-assisted search)
        let mut index = IndexCursor::new(&mut db, "T", "ByNum").expect("cursor");
        let mut builder = IterableBuilder::new();
        builder.set_match_pattern_row(pattern(&[("Num", Value::Long(3))]));
        let mut iter = builder.rows();
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut index, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![9, 20]);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn entry_iterable_builder() {
        let (path, mut db) = create_test_db("entry_iter");
        make_table(&mut db);
        add_test_rows(&mut db);

        // the run of the non-unique index (Num 3 holds ids 9 and 20)
        let mut cursor = IndexCursor::new(&mut db, "T", "ByNum").expect("cursor");
        let builder = EntryIterableBuilder::new(vec![Value::Long(3)]);
        let mut iter = builder.rows();
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut cursor, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![9, 20]);

        // with a column subset
        let builder = EntryIterableBuilder::new(vec![Value::Long(4)])
            .set_column_names(vec!["ID".to_string(), "Note".to_string()]);
        let mut iter = builder.rows();
        let rows: Vec<Vec<Value>> =
            std::iter::from_fn(|| iter.next_row(&mut cursor, &mut db).unwrap()).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Long(1));
        assert_eq!(rows[0][1], Value::Text("note 1".into()));
        assert_eq!(rows[1][0], Value::Long(12));

        // a unique index entry, with a case variant of the key
        let mut by_name = IndexCursor::new(&mut db, "T", "ByName").expect("cursor");
        let builder = EntryIterableBuilder::new(vec![Value::Text("NAME05".into())]);
        let mut iter = builder.rows();
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut by_name, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![5]);

        // a key with no rows
        let builder = EntryIterableBuilder::new(vec![Value::Long(99)]);
        let mut iter = builder.rows();
        assert!(iter.next_row(&mut cursor, &mut db).unwrap().is_none());

        // remove deletes the row the iterator is on, and only once
        let builder = EntryIterableBuilder::new(vec![Value::Long(3)]);
        let mut iter = builder.rows();
        assert!(iter.next_row(&mut cursor, &mut db).unwrap().is_some());
        iter.remove(&mut cursor, &mut db).unwrap();
        assert!(iter.remove(&mut cursor, &mut db).is_err());
        assert!(db
            .read_rows_with_ids("T")
            .unwrap()
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .all(|r| r[0].as_i64() != Some(9)));
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut cursor, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![20]);

        // the same through the simple iterator
        let mut cursor = TableScanCursor::new(&mut db, "T").expect("cursor");
        let builder = IterableBuilder::new();
        let mut iter = builder.rows();
        assert!(iter.next_row(&mut cursor, &mut db).unwrap().is_some());
        iter.remove(&mut cursor, &mut db).unwrap();
        assert!(iter.remove(&mut cursor, &mut db).is_err());
        assert_eq!(cursor.get_row_count(&mut db).unwrap(), 18);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn entry_iterable_on_ranged_cursor() {
        let (path, mut db) = create_test_db("entry_iter_range");
        make_table(&mut db);
        add_test_rows(&mut db);

        // the range ends in the middle of the Num 3 run (ids 9 and 20)
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(2)])
            .set_end_entry(vec![Value::Long(3)])
            .set_end_row_inclusive(false);
        let mut cursor =
            IndexCursor::new_with_range(&mut db, "T", "ByNum", &range).expect("cursor");
        let mut ids = Vec::new();
        while let Some(row) = cursor.get_next_row(&mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        // Num 2 only, without the (excluded) Num 3 run
        assert_eq!(ids, vec![6, 17]);

        // the entry iteration of the excluded run yields nothing
        let builder = EntryIterableBuilder::new(vec![Value::Long(3)]);
        let mut iter = builder.rows();
        assert!(iter.next_row(&mut cursor, &mut db).unwrap().is_none());

        // the range ends inside the run (one of the two rows)
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(3)])
            .set_end_entry(vec![Value::Long(3)]);
        let mut cursor =
            IndexCursor::new_with_range(&mut db, "T", "ByNum", &range).expect("cursor");
        let builder = EntryIterableBuilder::new(vec![Value::Long(3)]);
        let mut iter = builder.rows();
        let mut ids = Vec::new();
        while let Some(row) = iter.next_row(&mut cursor, &mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![9, 20]);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cursor_builder() {
        let (path, mut db) = create_test_db("builder");
        make_table(&mut db);
        add_test_rows(&mut db);

        // a table scan cursor ignores the range (jackcess 5.0.4 only passes it
        // to the index cursor)
        let mut builder = CursorBuilder::new("T");
        builder.set_start_entry(vec![Value::Long(5)]);
        builder.set_end_entry(vec![Value::Long(7)]);
        let mut cursor = builder.to_cursor(&mut db).expect("cursor");
        let mut count = 0;
        while cursor.get_next_row(&mut db).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 20);
        assert!(builder.to_index_cursor(&mut db).is_err());

        // an index cursor narrowed by a range, starting after last
        let mut builder = CursorBuilder::new("T");
        builder
            .set_index_by_name("PrimaryKey")
            .set_start_entry(vec![Value::Long(5)])
            .set_end_entry(vec![Value::Long(7)])
            .after_last();
        let mut cursor = builder.to_index_cursor(&mut db).expect("cursor");
        assert!(cursor.is_after_last());
        assert_eq!(
            cursor.get_previous_row(&mut db).unwrap().unwrap()[0],
            Value::Long(7)
        );

        // exclusive bounds and a map based (row) range
        let mut builder = CursorBuilder::new("T");
        builder
            .set_index_by_name("PrimaryKey")
            .set_start_row_map(&pattern(&[("ID", Value::Long(5))]))
            .set_end_row_map(&pattern(&[("ID", Value::Long(7))]))
            .set_start_row_inclusive(false)
            .set_end_row_inclusive(false);
        let mut cursor = builder.to_index_cursor(&mut db).expect("cursor");
        let mut ids = Vec::new();
        while let Some(row) = cursor.get_next_row(&mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![6]);

        // a savepoint as the initial position
        let mut plain = IndexCursor::new(&mut db, "T", "PrimaryKey").unwrap();
        assert!(plain
            .find_first_row_by_entry(&mut db, &[Value::Long(9)])
            .unwrap());
        let savepoint = plain.get_savepoint();
        let mut builder = CursorBuilder::new("T");
        builder
            .set_index_by_name("PrimaryKey")
            .restore_savepoint(&savepoint);
        let mut cursor = builder.to_index_cursor(&mut db).expect("cursor");
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(9)
        );

        // the case insensitive matcher
        let mut builder = CursorBuilder::new("T");
        builder.set_index_by_name("ByName").set_case_insensitive();
        let mut cursor = builder.to_index_cursor(&mut db).expect("cursor");
        assert!(cursor
            .find_first_row_by_entry(&mut db, &[Value::Text("NAME11".into())])
            .unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(11)
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The index a column list selects
    /// (`CursorBuilder.setIndexByColumns`/`setIndexByColumnNames` ->
    /// `TableImpl.findIndexForColumns(..., ANY_MATCH)`).
    fn selected_index(db: &mut Database, columns: &[&str]) -> Result<String> {
        let mut builder = CursorBuilder::new("T");
        builder.set_index_by_column_names(columns);
        Ok(builder.resolve_index(db)?.name)
    }

    #[test]
    fn cursor_builder_index_selection() {
        let (path, mut db) = create_test_db("builder_index_columns");
        make_table_with_note_indexes(&mut db);
        add_test_rows(&mut db);

        // an exact column list match wins (the first such index in table order:
        // ByNum before the equally exact ByNumDesc)
        assert_eq!(selected_index(&mut db, &["Num"]).unwrap(), "ByNum");
        // column names are matched case insensitively, but in order
        assert_eq!(selected_index(&mut db, &["num"]).unwrap(), "ByNum");
        assert_eq!(selected_index(&mut db, &["Name"]).unwrap(), "ByName");
        assert_eq!(selected_index(&mut db, &["ID"]).unwrap(), "PrimaryKey");
        assert_eq!(selected_index(&mut db, &["Num", "ID"]).unwrap(), "ByNumId");
        assert_eq!(selected_index(&mut db, &["Num", "id"]).unwrap(), "ByNumId");
        assert_eq!(
            selected_index(&mut db, &["Note", "ID"]).unwrap(),
            "ByNoteId"
        );

        // no exact match: the shortest index which *begins* with the columns
        // wins (`ByNoteId` is shorter than `ByNoteNumId`)
        assert_eq!(selected_index(&mut db, &["Note"]).unwrap(), "ByNoteId");
        assert_eq!(
            selected_index(&mut db, &["Note", "Num"]).unwrap(),
            "ByNoteNumId"
        );

        // no index (the column order matters, and a prefix of the search list
        // is not enough)
        let err = selected_index(&mut db, &["ID", "Num"]).unwrap_err();
        match err {
            Error::IllegalArgument(message) => {
                assert_eq!(
                    message,
                    "Index with columns [ID, Num] does not exist in table T"
                );
            }
            other => panic!("unexpected error {:?}", other),
        }
        assert!(matches!(
            selected_index(&mut db, &["Nonexistent"]).unwrap_err(),
            Error::IllegalArgument(_)
        ));
        assert!(matches!(
            selected_index(&mut db, &["Num", "Nonexistent"]).unwrap_err(),
            Error::IllegalArgument(_)
        ));
        // ... and `toIndexCursor` reports the same error
        let mut builder = CursorBuilder::new("T");
        builder.set_index_by_columns(&["ID", "Num"]);
        assert!(matches!(
            builder.to_index_cursor(&mut db).unwrap_err(),
            Error::IllegalArgument(_)
        ));
        assert!(matches!(
            builder.to_cursor(&mut db).err(),
            Some(Error::IllegalArgument(_))
        ));

        // `setIndex` replaces the by-columns selection and vice versa
        let mut builder = CursorBuilder::new("T");
        builder.set_index_by_columns(&["Num"]);
        let by_name = {
            let page = db.table_page("T").unwrap();
            db.tables
                .get(&page)
                .unwrap()
                .indexes
                .iter()
                .find(|i| i.name == "ByName")
                .cloned()
                .unwrap()
        };
        builder.set_index(&by_name);
        assert_eq!(builder.resolve_index(&mut db).unwrap().name, "ByName");
        builder.set_index_by_column_names(&["Num", "ID"]);
        assert_eq!(builder.resolve_index(&mut db).unwrap().name, "ByNumId");
        assert!(matches!(
            CursorBuilder::new("T").resolve_index(&mut db).unwrap_err(),
            Error::IllegalArgument(_)
        ));

        // the selection is used by `toIndexCursor`, together with the range
        let mut builder = CursorBuilder::new("T");
        builder
            .set_index_by_columns(&["Num"])
            .set_start_entry(vec![Value::Long(3)])
            .set_end_entry(vec![Value::Long(3)]);
        let mut cursor = builder.to_index_cursor(&mut db).expect("cursor");
        assert_eq!(cursor.get_index(&mut db).unwrap().name, "ByNum");
        let mut ids = Vec::new();
        while let Some(row) = cursor.get_next_row(&mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![9, 20]);

        // a two column selection, with a range on both columns
        let mut builder = CursorBuilder::new("T");
        builder
            .set_index_by_columns(&["Num", "ID"])
            .set_start_entry(vec![Value::Long(3)])
            .set_end_entry(vec![Value::Long(3)]);
        let mut cursor = builder.to_index_cursor(&mut db).expect("cursor");
        assert_eq!(cursor.get_index(&mut db).unwrap().name, "ByNumId");
        let mut ids = Vec::new();
        while let Some(row) = cursor.get_next_row(&mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![9, 20]);

        // the static helpers (`CursorBuilder.createCursor(Index, startRow,
        // endRow)` and friends), where the range rows are table row arrays
        let mut pk = CursorBuilder::create_primary_key_cursor(&mut db, "T").unwrap();
        assert!(pk
            .find_first_row_by_entry(&mut db, &[Value::Long(6)])
            .unwrap());
        assert_eq!(
            pk.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(6)
        );
        let mut by_num = CursorBuilder::create_index_cursor(&mut db, "T", "ByNum").unwrap();
        let mut ids = Vec::new();
        while let Some(row) = by_num.get_next_row(&mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        // the non-unique index holds every row (nulls are indexed as well)
        assert_eq!(ids.len(), 20);
        let mut rounded = CursorBuilder::create_cursor_with_range(
            &mut db,
            "T",
            "ByNum",
            Some(row(0, "", Some(3), None)),
            Some(row(0, "", Some(4), None)),
        )
        .unwrap();
        let mut ids = Vec::new();
        while let Some(row) = rounded.get_next_row(&mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![9, 20, 1, 12]);
        let mut rounded = CursorBuilder::create_cursor_with_range_inclusive(
            &mut db,
            "T",
            "ByNum",
            Some(row(0, "", Some(3), None)),
            false,
            Some(row(0, "", Some(5), None)),
            true,
        )
        .unwrap();
        let mut ids = Vec::new();
        while let Some(row) = rounded.get_next_row(&mut db).unwrap() {
            ids.push(row[0].as_i64().unwrap() as i32);
        }
        assert_eq!(ids, vec![1, 12, 4, 15]);
        // `createCursor(Table)`: a table scan cursor
        let mut scan = CursorBuilder::create_cursor(&mut db, "T").unwrap();
        assert_eq!(scan.get_row_count(&mut db).unwrap(), 20);

        // the `findRow*`/`findValue` convenience statics
        let row = CursorBuilder::find_row_by_primary_key(&mut db, "T", &[Value::Long(6)])
            .unwrap()
            .expect("row 6");
        assert_eq!(row[0], Value::Long(6));
        let row = CursorBuilder::find_row_by_entry(&mut db, "T", "ByNum", &[Value::Long(3)])
            .unwrap()
            .expect("first row of Num = 3");
        assert_eq!(row[0], Value::Long(9));
        assert!(
            CursorBuilder::find_row_by_entry(&mut db, "T", "ByNum", &[Value::Long(100)])
                .unwrap()
                .is_none()
        );
        let row = CursorBuilder::find_row(
            &mut db,
            "T",
            &pattern(&[("Name", Value::Text("name04".into()))]),
        )
        .unwrap()
        .expect("row name04");
        assert_eq!(row[0], Value::Long(4));
        assert!(CursorBuilder::find_row(
            &mut db,
            "T",
            &pattern(&[("Name", Value::Text("nope".into()))])
        )
        .unwrap()
        .is_none());
        // `findValue`/`findValue(Index, ...)`: the value of the first matching
        // row
        let value =
            CursorBuilder::find_value(&mut db, "T", "Note", "Num", &Value::Long(3)).unwrap();
        assert_eq!(value, Some(Value::Text("note 9".into())));
        let value = CursorBuilder::find_value_by_index(
            &mut db,
            "T",
            "ByNum",
            "Note",
            "Num",
            &Value::Long(3),
        )
        .unwrap();
        assert_eq!(value, Some(Value::Text("note 9".into())));
        assert_eq!(
            CursorBuilder::find_value(&mut db, "T", "Note", "Num", &Value::Long(100)).unwrap(),
            None
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `Cursor.findRow` on an index cursor
    /// (`IndexCursorImpl.getRowPosition`): the row is looked up through the
    /// index entry of its own index column values, so the cursor lands on the
    /// first row sharing that entry - which is not necessarily the requested
    /// row.  A row before the cursor's range is found on the first row of the
    /// range; a row after it is not found.
    #[test]
    fn index_cursor_find_row_semantics() {
        let (path, mut db) = create_test_db("find_row");
        make_table(&mut db);
        add_test_rows(&mut db);
        let row_ids: BTreeMap<i32, RowId> = db
            .read_rows_with_ids("T")
            .unwrap()
            .into_iter()
            .filter_map(|(id, values)| values.map(|r| (r[0].as_i64().unwrap() as i32, id)))
            .collect();
        assert_eq!(row_ids.len(), 20);

        // the first row of the run of Num = 3
        let mut probe = IndexCursor::new(&mut db, "T", "ByNum").unwrap();
        assert!(probe
            .find_first_row_by_entry(&mut db, &[Value::Long(3)])
            .unwrap());
        let first_of_run = probe.get_current_row_id();
        let first_of_run_value = probe.get_current_row_value(&mut db, "ID").unwrap();
        assert_eq!(first_of_run_value, Value::Long(9));

        // a non-unique index: asking for the *last* row of the run lands on the
        // first one (jackcess does not walk the run in `findRow`)
        let mut cursor = IndexCursor::new(&mut db, "T", "ByNum").unwrap();
        assert!(cursor.find_row(&mut db, row_ids[&20]).unwrap());
        assert_eq!(cursor.get_current_row_id(), first_of_run);
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            first_of_run_value
        );
        // ... and asking for the first row of the run is an exact hit
        assert!(cursor.find_row(&mut db, row_ids[&9]).unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(9)
        );
        // a unique index finds the row itself
        let mut by_name = IndexCursor::new(&mut db, "T", "ByName").unwrap();
        assert!(by_name.find_row(&mut db, row_ids[&11]).unwrap());
        assert_eq!(
            by_name.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(11)
        );

        // a row id from another table (a page this table does not own) is not
        // an error, it just does not name a row of this table
        assert!(!cursor.find_row(&mut db, (999_999, 0)).unwrap());
        assert!(!cursor.find_row(&mut db, (1, -1)).unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(9)
        );

        // a range on the primary key
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(5)])
            .set_end_entry(vec![Value::Long(15)]);
        let mut cursor = IndexCursor::new_with_range(&mut db, "T", "PrimaryKey", &range).unwrap();
        // inside the range: the row itself
        assert!(cursor.find_row(&mut db, row_ids[&9]).unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(9)
        );
        // before the range: found, but on the first row of the range
        assert!(cursor.find_row(&mut db, row_ids[&2]).unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );
        assert!(cursor.find_row(&mut db, row_ids[&1]).unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );
        // after the range: not found, and the position is unchanged
        assert!(!cursor.find_row(&mut db, row_ids[&17]).unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );
        assert!(!cursor.find_row(&mut db, row_ids[&20]).unwrap());
        assert_eq!(
            cursor.get_current_row_value(&mut db, "ID").unwrap(),
            Value::Long(5)
        );

        // the same on a range of a non-unique index: every row before the
        // range lands on the first row of the range
        let mut range = IndexRange::new();
        range
            .set_start_entry(vec![Value::Long(3)])
            .set_end_entry(vec![Value::Long(5)]);
        let mut cursor = IndexCursor::new_with_range(&mut db, "T", "ByNum", &range).unwrap();
        for id in [3, 6, 7, 11, 14, 17] {
            assert!(cursor.find_row(&mut db, row_ids[&id]).unwrap());
            assert_eq!(
                cursor.get_current_row_value(&mut db, "ID").unwrap(),
                Value::Long(9)
            );
        }
        for id in [2, 5, 8, 10, 13, 16, 18, 19] {
            assert!(!cursor.find_row(&mut db, row_ids[&id]).unwrap());
        }

        // a deleted row cannot be read to build its index entry, which is an
        // error for an index cursor (`TableImpl.getRow`)...
        let deleted = row_ids[&20];
        let mut cursor = IndexCursor::new(&mut db, "T", "PrimaryKey").unwrap();
        db.delete_row("T", deleted).unwrap();
        match cursor.find_row(&mut db, deleted) {
            Err(Error::IllegalState(message)) => {
                assert!(message.starts_with("Row is deleted:"), "{}", message)
            }
            other => panic!("unexpected result {:?}", other),
        }
        // ... but the table scan flavour only reports that the row is gone
        let mut scan = TableScanCursor::new(&mut db, "T").unwrap();
        assert!(!scan.find_row(&mut db, deleted).unwrap());
        assert!(!scan.find_row(&mut db, (999_999, 0)).unwrap());
        assert!(scan.find_row(&mut db, row_ids[&19]).unwrap());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The `Row` based cursor conveniences: `Cursor.stream`, the row-object
    /// getters with a column subset, `newIterable`/`rows_object`, the
    /// `IndexCursor.newEntryIterable` row variant, and the row iterators of
    /// both cursor flavours (`Cursor implements Iterable<Row>`).
    #[test]
    fn row_object_cursor_conveniences() {
        let (path, mut db) = create_test_db("row_objects");
        make_table(&mut db);
        add_test_rows(&mut db);

        // `Cursor.stream()` (all rows as Row objects, cursor order)
        let mut scan = TableScanCursor::new(&mut db, "T").unwrap();
        let streamed = scan.stream(&mut db).unwrap();
        assert_eq!(streamed.len(), 20);
        assert_eq!(streamed[0].get_int("ID").unwrap(), Some(1));
        assert_eq!(streamed[19].get_int("ID").unwrap(), Some(20));
        assert!(streamed.iter().all(|row| row.get_id().is_some()));

        // the row-object getters carry the cursor's row id and the subset
        let mut scan = TableScanCursor::new(&mut db, "T").unwrap();
        let row = scan.get_next_row_object(&mut db).unwrap().expect("row");
        assert_eq!(row.get_int("ID").unwrap(), Some(1));
        assert_eq!(row.get_id().unwrap(), scan.get_current_row_id());
        let subset = scan
            .get_current_row_object_columns(&mut db, &["Name", "ID"])
            .unwrap();
        // the requested columns keep the table column order (`ID`, `Name`)
        assert_eq!(subset.keys().collect::<Vec<_>>(), vec!["ID", "Name"]);
        assert_eq!(subset.get_int("ID").unwrap(), Some(1));
        assert_eq!(
            subset.get_string("Name").unwrap().as_deref(),
            Some("name01")
        );
        let next = scan
            .get_next_row_object_columns(&mut db, &["Name"])
            .unwrap()
            .expect("row");
        assert_eq!(next.get_string("Name").unwrap().as_deref(), Some("name02"));
        assert_eq!(next.len(), 1);

        // `newIterable` + the Row flavour of its iterator
        let mut scan = TableScanCursor::new(&mut db, "T").unwrap();
        let mut iterator = scan
            .new_iterable()
            .set_column_names(vec!["Name".to_string()])
            .rows();
        let mut names = Vec::new();
        while let Some(row) = iterator.next_row_object(&mut scan, &mut db).unwrap() {
            names.push(row.get_string("Name").unwrap().unwrap());
        }
        assert_eq!(names.len(), 20);
        assert_eq!(names[0], "name01");
        let rows = scan
            .new_iterable()
            .set_column_names(vec!["ID".to_string()])
            .rows_object(&mut scan, &mut db)
            .unwrap();
        assert_eq!(rows.len(), 20);
        assert_eq!(rows[1].get_int("ID").unwrap(), Some(2));

        // `IndexCursor.newEntryIterable` and its Row iterator
        let mut index = IndexCursor::new(&mut db, "T", "PrimaryKey").unwrap();
        let mut entry = index
            .new_entry_iterable(vec![Value::Long(10)])
            .set_column_names(vec!["ID".to_string(), "Name".to_string()])
            .rows();
        let mut entry_rows = Vec::new();
        while let Some(row) = entry.next_row_object(&mut index, &mut db).unwrap() {
            entry_rows.push(row);
        }
        assert_eq!(entry_rows.len(), 1);
        assert_eq!(entry_rows[0].get_int("ID").unwrap(), Some(10));
        assert_eq!(entry_rows[0].len(), 2);

        // `IndexCursor::row_iter` / `rows`
        let index = IndexCursor::new(&mut db, "T", "PrimaryKey").unwrap();
        let mut count = 0;
        for row in index.row_iter(&mut db) {
            let row = row.unwrap();
            assert!(row.get_id().is_some());
            count += 1;
        }
        assert_eq!(count, 20);
        let index = IndexCursor::new(&mut db, "T", "PrimaryKey").unwrap();
        assert_eq!(index.rows(&mut db).unwrap().len(), 20);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
