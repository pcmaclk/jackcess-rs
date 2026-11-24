//! Port of `com.healthmarketscience.jackcess.impl.FKEnforcer`.
//!
//! Foreign-key enforcement is driven entirely by the foreign key indexes the
//! relationship creation writes (an index with a [`ForeignKeyReference`] on
//! each side of the relationship); the `MSysRelationships` rows are not
//! consulted.  A relationship whose referential integrity is not enforced has
//! no such indexes, so it is never checked.
//!
//! The checks and cascades run in exactly the order `FKEnforcer` runs them, and
//! a rejected operation throws an [`Error::ConstraintViolation`] carrying the
//! same message as jackcess' `ConstraintViolationException`.

use std::collections::BTreeSet;

use crate::column::Column;
use crate::database::Database;
use crate::datatype::DataType;
use crate::error::{Error, Result};
use crate::index::ForeignKeyReference;
use crate::table::RowId;
use crate::value::Value;

/// `FKEnforcer.MATCHER` - every foreign key index is case-insensitive for text
/// (`CaseInsensitiveColumnMatcher`), other columns compare by value
/// (`SimpleColumnMatcher`).
fn values_match(column: &Column, value1: &Value, value2: &Value) -> bool {
    if column.data_type == DataType::Text || column.data_type == DataType::Memo {
        // CaseInsensitiveColumnMatcher: ColumnImpl.toCharSequence on both
        // values and String.equalsIgnoreCase
        match (value1, value2) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            _ => {
                let cs1 = crate::column::to_char_sequence(value1);
                let cs2 = crate::column::to_char_sequence(value2);
                match (cs1, cs2) {
                    (Ok(a), Ok(b)) => java_equals_ignore_case(&a, &b),
                    _ => false,
                }
            }
        }
    } else {
        simple_equals(value1, value2)
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
            (Some(c1), Some(c2)) => {
                if c1 == c2 {
                    continue;
                }
                let upper1 = c1.to_uppercase().next().unwrap_or(c1);
                let upper2 = c2.to_uppercase().next().unwrap_or(c2);
                if upper1 == upper2 {
                    continue;
                }
                let lower1 = c1.to_lowercase().next().unwrap_or(c1);
                let lower2 = c2.to_lowercase().next().unwrap_or(c2);
                if lower1 != lower2 {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// `ColumnImpl.getRowValue(Object[] row)`
fn row_value<'a>(row: &'a [Value], column: &Column) -> &'a Value {
    row.get(column.column_index).unwrap_or(&Value::Null)
}

/// One side of a foreign key relationship: the join from the table holding the
/// index to the table holding the referenced index (`util.Joiner`).
#[derive(Clone, Debug)]
struct Joiner {
    /// tdef page of the table holding the index (the "from" table)
    from_page: i32,
    /// tdef page of the referenced table (the "to" table)
    to_page: i32,
    /// the logical index number of the referenced index on the "to" table
    to_index_number: i32,
    /// the "from" index columns, in index order
    from_columns: Vec<Column>,
    /// the referenced index columns, in index order
    to_columns: Vec<Column>,
    /// whether the "from" index is the primary side of the relationship
    is_primary: bool,
    cascade_updates: bool,
    cascade_deletes: bool,
    cascade_null: bool,
}

impl Joiner {
    // named after Java's `Joiner.fromTableName`, so the `from_` prefix is the
    // port's naming, not a conversion constructor
    #[expect(
        clippy::wrong_self_convention,
        reason = "mirrors Java Joiner.fromTableName naming"
    )]
    fn from_table_name<'a>(&self, db: &'a Database) -> &'a str {
        db.tables
            .get(&self.from_page)
            .map(|t| t.name.as_str())
            .unwrap_or("")
    }

    fn to_table_name<'a>(&self, db: &'a Database) -> &'a str {
        db.tables
            .get(&self.to_page)
            .map(|t| t.name.as_str())
            .unwrap_or("")
    }

    /// `Joiner.toFKString`
    fn to_fk_string(&self, db: &Database) -> String {
        let from_type = if self.is_primary {
            "] (primary)"
        } else {
            "] (secondary)"
        };
        let to_type = if self.is_primary {
            "] (secondary)"
        } else {
            "] (primary)"
        };
        let join = |cols: &[Column]| -> String {
            cols.iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "Foreign Key from {}[{}{} to {}[{}{} (Db={})",
            self.from_table_name(db),
            join(&self.from_columns),
            from_type,
            self.to_table_name(db),
            join(&self.to_columns),
            to_type,
            db.name
        )
    }
}

/// The foreign-key state of one table: its joiners (`FKEnforcer.initialize`)
/// and the columns involved in any relationship (`FKEnforcer._cols`).
#[derive(Clone, Debug, Default)]
pub(crate) struct FkContext {
    joiners: Vec<Joiner>,
    columns: Vec<Column>,
}

impl FkContext {
    /// Builds the context of the given table.  jackcess creates the joiners
    /// lazily and caches them (clearing them after a table definition
    /// modification); building them per operation from the current table
    /// definition is equivalent and always up to date.
    pub(crate) fn for_table(db: &mut Database, table_page: i32) -> Result<FkContext> {
        let references: Vec<(i32, ForeignKeyReference)> = {
            let table = db
                .tables
                .get(&table_page)
                .ok_or_else(|| Error::IllegalState("fk table not loaded".into()))?;
            table
                .indexes
                .iter()
                .filter_map(|idx| idx.reference.map(|reference| (idx.index_number, reference)))
                .collect()
        };

        let mut joiners = Vec::with_capacity(references.len());
        let mut column_positions: BTreeSet<usize> = BTreeSet::new();
        for (index_number, reference) in references {
            let from_columns = {
                let table = db.tables.get(&table_page).unwrap();
                columns_of_index(table, index_number)?
            };
            // `IndexImpl.getReferencedIndex` loads the other table (and fails
            // for a reference to a missing table or index)
            db.load_table_by_page(reference.other_table_page_number)?;
            let to_columns = {
                let table = db
                    .tables
                    .get(&reference.other_table_page_number)
                    .ok_or_else(|| {
                        Error::Jackcess(format!(
                            "Reference to missing table {}",
                            reference.other_table_page_number
                        ))
                    })?;
                columns_of_index(table, reference.other_index_number).map_err(|_| {
                    Error::Jackcess(format!(
                        "Reference to missing index {}",
                        reference.other_index_number
                    ))
                })?
            };
            for col in &from_columns {
                column_positions.insert(col.column_index);
            }
            joiners.push(Joiner {
                from_page: table_page,
                to_page: reference.other_table_page_number,
                to_index_number: reference.other_index_number,
                from_columns,
                to_columns,
                is_primary: reference.is_primary_table(),
                cascade_updates: reference.cascade_updates,
                cascade_deletes: reference.cascade_deletes,
                cascade_null: reference.cascade_null,
            });
        }

        let columns = {
            let table = db.tables.get(&table_page).unwrap();
            column_positions
                .into_iter()
                .filter_map(|pos| table.columns.get(pos).cloned())
                .collect()
        };
        Ok(FkContext { joiners, columns })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.joiners.is_empty()
    }

    /// `FKEnforcer.addRow`
    pub(crate) fn add_row(&self, db: &mut Database, row: &[Value]) -> Result<()> {
        for joiner in self.joiners.iter().filter(|j| !j.is_primary) {
            require_primary_values(db, joiner, row)?;
        }
        Ok(())
    }

    /// `FKEnforcer.updateRow`
    pub(crate) fn update_row(
        &self,
        db: &mut Database,
        old_row: &[Value],
        new_row: &[Value],
    ) -> Result<()> {
        if !self.any_updates(old_row, new_row) {
            // no changes were made to any relevant columns
            return Ok(());
        }

        if db.fk_update_depth == 0 {
            // only the "top-level" of an update operation checks the secondary
            // relationships; in nested levels we are only ever changing the fk
            // values themselves, so the new values are always valid
            for joiner in self.joiners.iter().filter(|j| !j.is_primary) {
                if any_joiner_updates(joiner, old_row, new_row) {
                    require_primary_values(db, joiner, new_row)?;
                }
            }
        }

        db.fk_update_depth += 1;
        let result = (|| -> Result<()> {
            // the tables for which this table is the primary table (but not
            // cascading)
            for joiner in self
                .joiners
                .iter()
                .filter(|j| j.is_primary && !j.cascade_updates)
            {
                if any_joiner_updates(joiner, old_row, new_row) {
                    require_no_secondary_values(db, joiner, old_row)?;
                }
            }
            // lastly, the cascading updates
            for joiner in self
                .joiners
                .iter()
                .filter(|j| j.is_primary && j.cascade_updates)
            {
                if any_joiner_updates(joiner, old_row, new_row) {
                    update_secondary_values(db, joiner, old_row, new_row)?;
                }
            }
            Ok(())
        })();
        db.fk_update_depth -= 1;
        result
    }

    /// `FKEnforcer.deleteRow`
    pub(crate) fn delete_row(&self, db: &mut Database, row: &[Value]) -> Result<()> {
        // first, the tables for which this table is the primary table (but not
        // cascading)
        for joiner in self
            .joiners
            .iter()
            .filter(|j| j.is_primary && !j.cascade_deletes && !j.cascade_null)
        {
            require_no_secondary_values(db, joiner, row)?;
        }
        // next, delete from the cascading tables
        for joiner in self
            .joiners
            .iter()
            .filter(|j| j.is_primary && j.cascade_deletes)
        {
            delete_secondary_rows(db, joiner, row)?;
        }
        // lastly, null the cascading tables
        for joiner in self
            .joiners
            .iter()
            .filter(|j| j.is_primary && !j.cascade_deletes && j.cascade_null)
        {
            null_secondary_values(db, joiner, row)?;
        }
        Ok(())
    }

    /// `FKEnforcer.anyUpdates(Object[], Object[])`
    fn any_updates(&self, old_row: &[Value], new_row: &[Value]) -> bool {
        self.columns
            .iter()
            .any(|col| !values_match(col, row_value(old_row, col), row_value(new_row, col)))
    }
}

/// The columns of the logical index with the given number.
fn columns_of_index(table: &crate::table::Table, index_number: i32) -> Result<Vec<Column>> {
    let index = table
        .indexes
        .iter()
        .find(|idx| idx.index_number == index_number)
        .ok_or_else(|| Error::Jackcess(format!("missing index {}", index_number)))?;
    let data = table
        .index_data_for(index)
        .ok_or_else(|| Error::Jackcess(format!("missing index data for index {}", index_number)))?;
    Ok(data.columns.iter().map(|c| c.column.clone()).collect())
}

/// `FKEnforcer.anyUpdates(Joiner, Object[], Object[])`
fn any_joiner_updates(joiner: &Joiner, old_row: &[Value], new_row: &[Value]) -> bool {
    joiner
        .from_columns
        .iter()
        .any(|col| !values_match(col, row_value(old_row, col), row_value(new_row, col)))
}

/// `FKEnforcer.areNull`
fn are_null(joiner: &Joiner, row: &[Value]) -> bool {
    joiner
        .from_columns
        .iter()
        .all(|col| row_value(row, col).is_null())
}

/// `FKEnforcer.isOwnPrimaryRow`: a row of a table related to itself may be its
/// own parent (the row is not written yet when the constraint is checked, so
/// the index lookup cannot find the match the row supplies itself).
fn is_own_primary_row(joiner: &Joiner, row: &[Value]) -> bool {
    if joiner.from_page != joiner.to_page {
        return false;
    }
    for i in 0..joiner.from_columns.len() {
        let fk_value = row_value(row, &joiner.from_columns[i]);
        let pk_value = row_value(row, &joiner.to_columns[i]);
        if !values_match(&joiner.to_columns[i], fk_value, pk_value) {
            return false;
        }
    }
    true
}

fn constraint_violation(kind: &str, row: &[Value], joiner: &Joiner, db: &Database) -> Error {
    Error::ConstraintViolation(format!(
        "{} {} violates constraint {}",
        kind,
        java_list_to_string(row),
        joiner.to_fk_string(db)
    ))
}

/// `FKEnforcer.requirePrimaryValues`
fn require_primary_values(db: &mut Database, joiner: &Joiner, row: &[Value]) -> Result<()> {
    // null values are allowed
    if are_null(joiner, row) || is_own_primary_row(joiner, row) || joiner_has_rows(db, joiner, row)?
    {
        return Ok(());
    }
    Err(constraint_violation("Adding new row", row, joiner, db))
}

/// `FKEnforcer.requireNoSecondaryValues`
fn require_no_secondary_values(db: &mut Database, joiner: &Joiner, row: &[Value]) -> Result<()> {
    if has_other_secondary_rows(db, joiner, row)? {
        return Err(constraint_violation("Removing old row", row, joiner, db));
    }
    Ok(())
}

/// `FKEnforcer.hasOtherSecondaryRows`: a row of a table related to itself can
/// be its own parent, and MS Access removes such a row; it is the only row
/// naming it which goes away with it, so it does not count.
fn has_other_secondary_rows(db: &mut Database, joiner: &Joiner, row: &[Value]) -> Result<bool> {
    if joiner.from_page != joiner.to_page {
        return joiner_has_rows(db, joiner, row);
    }

    // the "from" columns are the primary ones, so two rows of this table are
    // the same row exactly when they agree on all of them
    for other_id in matching_rows(db, joiner, row, false)? {
        let other = read_row(db, joiner.to_page, other_id)?;
        for col in &joiner.from_columns {
            if !values_match(col, row_value(row, col), row_value(&other, col)) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// `FKEnforcer.updateSecondaryValues`
fn update_secondary_values(
    db: &mut Database,
    joiner: &Joiner,
    old_from_row: &[Value],
    new_from_row: &[Value],
) -> Result<()> {
    for row_id in matching_rows(db, joiner, old_from_row, false)? {
        let mut to_row = read_row(db, joiner.to_page, row_id)?;
        for i in 0..joiner.from_columns.len() {
            let value = row_value(new_from_row, &joiner.from_columns[i]).clone();
            let pos = joiner.to_columns[i].column_index;
            if to_row.len() <= pos {
                to_row.resize(pos + 1, Value::Null);
            }
            to_row[pos] = value;
        }
        update_row_by_id(db, joiner.to_page, row_id, &to_row)?;
    }
    Ok(())
}

/// `FKEnforcer.nullSecondaryValues`
fn null_secondary_values(db: &mut Database, joiner: &Joiner, old_from_row: &[Value]) -> Result<()> {
    for row_id in matching_rows(db, joiner, old_from_row, false)? {
        let mut to_row = read_row(db, joiner.to_page, row_id)?;
        for col in &joiner.to_columns {
            let pos = col.column_index;
            if to_row.len() <= pos {
                to_row.resize(pos + 1, Value::Null);
            }
            to_row[pos] = Value::Null;
        }
        update_row_by_id(db, joiner.to_page, row_id, &to_row)?;
    }
    Ok(())
}

/// `Joiner.deleteRows`
fn delete_secondary_rows(db: &mut Database, joiner: &Joiner, from_row: &[Value]) -> Result<()> {
    for row_id in matching_rows(db, joiner, from_row, false)? {
        let name = table_name(db, joiner.to_page)?;
        db.delete_row(&name, row_id)?;
    }
    Ok(())
}

fn table_name(db: &Database, page: i32) -> Result<String> {
    db.tables
        .get(&page)
        .map(|t| t.name.clone())
        .ok_or_else(|| Error::IllegalState(format!("fk table {} not loaded", page)))
}

fn update_row_by_id(db: &mut Database, page: i32, row_id: RowId, row: &[Value]) -> Result<()> {
    let name = table_name(db, page)?;
    db.update_row(&name, row_id, row)
}

fn read_row(db: &mut Database, page: i32, row_id: RowId) -> Result<Vec<Value>> {
    let table = db
        .tables
        .get(&page)
        .ok_or_else(|| Error::IllegalState(format!("fk table {} not loaded", page)))?;
    table.read_row_at(&mut db.channel, row_id)
}

/// `Joiner.hasRows`: whether the "to" table has a row whose index columns
/// match the "from" row's values.
fn joiner_has_rows(db: &mut Database, joiner: &Joiner, from_row: &[Value]) -> Result<bool> {
    Ok(!matching_rows(db, joiner, from_row, true)?.is_empty())
}

/// The rows of the "to" table the join finds for the given "from" row: the
/// entries sharing the first entry at or after the key, whose actual column
/// values match (`IndexCursorImpl.findFirstRowByEntry`/`EntryIterator`).
///
/// `stop_at_first` mirrors `hasRows` (which only needs to know whether one
/// row matches).
fn matching_rows(
    db: &mut Database,
    joiner: &Joiner,
    from_row: &[Value],
    stop_at_first: bool,
) -> Result<Vec<RowId>> {
    let values: Vec<Value> = joiner
        .from_columns
        .iter()
        .map(|col| row_value(from_row, col).clone())
        .collect();

    let candidate_ids = {
        let table = db.tables.get_mut(&joiner.to_page).ok_or_else(|| {
            Error::IllegalState(format!("fk table {} not loaded", joiner.to_page))
        })?;
        let index = table
            .indexes
            .iter()
            .find(|idx| idx.index_number == joiner.to_index_number)
            .ok_or_else(|| Error::Jackcess(format!("missing index {}", joiner.to_index_number)))?;
        let data_number = index.index_data_number;
        let data = table
            .index_datas
            .iter_mut()
            .find(|d| d.number == data_number)
            .ok_or_else(|| {
                Error::Jackcess(format!("missing index data for index {}", data_number))
            })?;
        data.find_rows_by_entry(&mut db.channel, &values)?
    };

    let mut rows = Vec::new();
    for row_id in candidate_ids {
        let row_id: RowId = (row_id.page_number, row_id.row_number);
        let to_row = read_row(db, joiner.to_page, row_id)?;
        let matches = joiner.to_columns.iter().enumerate().all(|(i, col)| {
            values_match(
                col,
                values.get(i).unwrap_or(&Value::Null),
                row_value(&to_row, col),
            )
        });
        if matches {
            rows.push(row_id);
            if stop_at_first {
                break;
            }
        }
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// error text rendering (`Arrays.asList(row)` / `String.valueOf`)
// ---------------------------------------------------------------------------

/// `Arrays.asList(row).toString()`: `[v1, v2, ...]` using `String.valueOf`.
fn java_list_to_string(row: &[Value]) -> String {
    let parts: Vec<String> = row.iter().map(java_value_to_string).collect();
    format!("[{}]", parts.join(", "))
}

fn java_value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Byte(v) => format!("{}", *v as i8),
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        Value::BigInt(v) => v.to_string(),
        Value::Float(v) => java_float_to_string(*v as f64, true),
        Value::Double(v) => java_float_to_string(*v, false),
        Value::Decimal { unscaled, scale } => format_decimal(*unscaled, *scale),
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => format!(
            "[B@{}",
            b.iter()
                .fold(0i32, |acc, x| acc.wrapping_mul(31).wrapping_add(*x as i32))
        ),
        Value::DateTime(ldt) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            ldt.year, ldt.month, ldt.day, ldt.hour, ldt.minute, ldt.second
        ),
        Value::DateBits(bits) => java_float_to_string(*bits, false),
        Value::Uuid(bytes) => format_uuid(bytes),
        // `Column.KEEP_VALUE` is a String, so `String.valueOf` gives its text
        Value::Keep => "<KEEP_VALUE>".to_string(),
    }
}

/// `Double.toString`/`Float.toString` (shortest round-trip representation,
/// always with a fractional part for integral values).  Rust's formatting
/// differs from Java's for the extreme exponents, which the error text of a
/// violation never reached in the ported scenarios.
fn java_float_to_string(value: f64, is_float: bool) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string();
    }
    if is_float {
        let narrowed = value as f32;
        let mut out = format!("{}", narrowed);
        if !out.contains('.') {
            out.push_str(".0");
        }
        return out;
    }
    let abs = value.abs();
    if !(1e-3..1e7).contains(&abs) {
        // Java uses computerised scientific notation here
        let s = format!("{:e}", value).replace('e', "E");
        let (mantissa, exponent) = match s.split_once('E') {
            Some(parts) => parts,
            None => return s,
        };
        let mantissa = if mantissa.contains('.') {
            mantissa.to_string()
        } else {
            format!("{}.0", mantissa)
        };
        let exponent = exponent.trim_start_matches('+');
        return format!("{}E{}", mantissa, exponent);
    }
    let mut out = format!("{}", value);
    if !out.contains('.') {
        out.push_str(".0");
    }
    out
}

/// `BigDecimal.toString` for a BigDecimal built from an unscaled value and a
/// scale (as jackcess' MONEY/NUMERIC columns are).
fn format_decimal(unscaled: i128, scale: i32) -> String {
    if scale <= 0 {
        let mut out = unscaled.to_string();
        for _ in 0..(-scale) {
            out.push('0');
        }
        return out;
    }
    let negative = unscaled < 0;
    let mut digits = unscaled.unsigned_abs().to_string();
    let scale = scale as usize;
    if digits.len() <= scale {
        digits = format!("{}{}", "0".repeat(scale - digits.len() + 1), digits);
    }
    let split = digits.len() - scale;
    let out = format!("{}.{}", &digits[..split], &digits[split..]);
    if negative {
        format!("-{}", out)
    } else {
        out
    }
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex = |slice: &[u8]| -> String {
        slice
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(&bytes[0..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..16])
    )
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnSpec;
    use crate::format::FileFormat;
    use crate::relationship::RelationshipSpec;
    use crate::tablecreator::{IndexSpec, TableSpec};

    fn create_test_db(name: &str) -> (std::path::PathBuf, Database) {
        let path = test_tmp_dir().join(format!("jackcess_fk_{}_{}.mdb", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        (path, db)
    }

    /// `P(ID long pk, Name text)` with no auto number on the key (so the key
    /// can be changed by an update) and `C(ID autonumber pk, P_ID long,
    /// Note text)`.
    fn make_pc(db: &mut Database) {
        let p = TableSpec::new(
            "P",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&p).expect("create P");

        let c = TableSpec::new(
            "C",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("P_ID", DataType::Long),
                ColumnSpec::new("Note", DataType::Text).with_length(50),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&c).expect("create C");
    }

    fn p_row(id: i32, name: &str) -> Vec<Value> {
        vec![Value::Long(id), Value::Text(name.to_string())]
    }

    fn c_row(p_id: Option<i32>, note: &str) -> Vec<Value> {
        vec![
            Value::Null,
            p_id.map(Value::Long).unwrap_or(Value::Null),
            Value::Text(note.to_string()),
        ]
    }

    fn row_id_by_id(db: &mut Database, table: &str, id: i32) -> RowId {
        let id_col = db.get_table(table).unwrap().column_index("ID").unwrap();
        for (row_id, values) in db.read_rows_with_ids(table).unwrap() {
            if let Some(values) = values {
                if values.get(id_col).and_then(|v| v.as_i64()) == Some(id as i64) {
                    return row_id;
                }
            }
        }
        panic!("row {} not found in {}", id, table);
    }

    fn c_foreign_keys(db: &mut Database) -> Vec<i32> {
        let p_id = db.get_table("C").unwrap().column_index("P_ID").unwrap();
        let mut ids: Vec<i32> = db
            .read_rows_with_ids("C")
            .unwrap()
            .into_iter()
            .filter_map(|(_, values)| values)
            .filter_map(|values| values.get(p_id).and_then(|v| v.as_i64()).map(|v| v as i32))
            .collect();
        ids.sort_unstable();
        ids
    }

    fn test_tmp_dir() -> std::path::PathBuf {
        // inside the workspace: the sandbox does not allow the test process to
        // write elsewhere
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn index_lookup_finds_the_first_entry_at_or_after_a_key() {
        let (path, mut db) = create_test_db("lookup");
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("Code", DataType::Text).with_length(10),
                ColumnSpec::new("Num", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(10),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("Code".to_string(), true)],
        ))
        .with_index(IndexSpec::new(
            "ByNumName",
            vec![("Num".to_string(), true), ("Name".to_string(), true)],
        ));
        db.create_table(&spec).expect("create T");
        let row = |code: &str, num: i32, name: &str| {
            vec![
                Value::Text(code.to_string()),
                Value::Long(num),
                Value::Text(name.to_string()),
            ]
        };
        db.add_rows(
            "T",
            &[
                row("a", 1, "x"),
                row("b", 2, "y"),
                row("c", 2, "z"),
                row("d", 3, "w"),
                row("e", 4, "v"),
                // a second row with the same multi column key
                row("f", 2, "y"),
            ],
        )
        .expect("add rows");

        // an exact key finds the rows sharing its entry (the index is not
        // unique, so the run holds both rows)
        let codes = lookup_codes(
            &mut db,
            "ByNumName",
            &[Value::Long(2), Value::Text("y".into())],
        );
        assert_eq!(codes, vec!["b", "f"]);

        // a key which is not in the index lands on the first entry after it
        // (`EntryCursor.beforeEntry` + `getNextEntry`)
        let codes = lookup_codes(
            &mut db,
            "ByNumName",
            &[Value::Long(2), Value::Text("zz".into())],
        );
        assert_eq!(codes, vec!["d"]);

        // a key which sorts before every entry lands on the first one
        let codes = lookup_codes(
            &mut db,
            "ByNumName",
            &[Value::Long(1), Value::Text("".into())],
        );
        assert_eq!(codes, vec!["a"]);

        // a key which sorts after every entry finds nothing (the cursor's end
        // entry has no row)
        let codes = lookup_codes(
            &mut db,
            "ByNumName",
            &[Value::Long(9), Value::Text("a".into())],
        );
        assert!(codes.is_empty());

        // a single column index works the same way
        let codes = lookup_codes(
            &mut db,
            "ByNumName",
            &[Value::Long(3), Value::Text("w".into())],
        );
        assert_eq!(codes, vec!["d"]);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The `Code` of the rows the given index lookup finds (the run of entries
    /// at or after the key, which is what `IndexData::find_rows_by_entry`
    /// returns).
    fn lookup_codes(db: &mut Database, index_name: &str, values: &[Value]) -> Vec<String> {
        let page = db.table_page("T").unwrap();
        let rows = {
            let table = db.tables.get_mut(&page).unwrap();
            let index = table
                .indexes
                .iter()
                .find(|idx| idx.name == index_name)
                .expect("index");
            let data_number = index.index_data_number;
            let data = table
                .index_datas
                .iter_mut()
                .find(|d| d.number == data_number)
                .expect("index data");
            data.find_rows_by_entry(&mut db.channel, values)
                .expect("lookup")
        };
        let table = db.tables.get(&page).unwrap();
        rows.into_iter()
            .map(|row_id| {
                let values = table
                    .read_row_at(&mut db.channel, (row_id.page_number, row_id.row_number))
                    .expect("read row");
                values[0].as_text().unwrap().to_string()
            })
            .collect()
    }

    #[test]
    fn add_rejects_missing_parent_with_jackcess_message() {
        let (path, mut db) = create_test_db("add");
        make_pc(&mut db);
        db.add_rows("P", &[p_row(1, "p1"), p_row(2, "p2")])
            .expect("add P rows");
        db.add_rows("C", &[c_row(Some(1), "c1"), c_row(None, "c2")])
            .expect("add C rows");
        db.add_relationship(
            RelationshipSpec::new("P", "C")
                .add_columns("ID", "P_ID")
                .set_referential_integrity(),
        )
        .expect("relationship");

        let bytes_before = std::fs::read(&path).unwrap();
        let auto_before = db.get_table("C").unwrap().last_long_auto_number;
        let err = db
            .add_rows("C", &[c_row(Some(99), "bad")])
            .expect_err("missing parent must be rejected");
        assert!(
            matches!(err, Error::ConstraintViolation(_)),
            "unexpected error {:?}",
            err
        );
        let expected = format!(
            "Adding new row [3, 99, bad] violates constraint Foreign Key from C[P_ID] (secondary) to P[ID] (primary) (Db={})",
            db.name
        );
        assert_eq!(format!("{}", err), expected);
        // the rejected insert must not change the file
        assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
        // and must not consume an auto number
        assert_eq!(
            db.get_table("C").unwrap().last_long_auto_number,
            auto_before
        );

        // a null foreign key is allowed
        db.add_rows("C", &[c_row(None, "null fk")])
            .expect("null fk");
        // and a valid one
        db.add_rows("C", &[c_row(Some(2), "c3")]).expect("valid fk");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_cascades_or_is_rejected() {
        let (path, mut db) = create_test_db("del");
        make_pc(&mut db);
        db.add_rows("P", &[p_row(1, "p1"), p_row(2, "p2"), p_row(3, "p3")])
            .expect("add P rows");
        db.add_rows(
            "C",
            &[
                c_row(Some(1), "c1"),
                c_row(Some(2), "c2"),
                c_row(Some(1), "c3"),
            ],
        )
        .expect("add C rows");
        // the cascade is a relationship property (like jackcess)
        db.add_relationship(
            RelationshipSpec::new("P", "C")
                .add_columns("ID", "P_ID")
                .set_referential_integrity()
                .set_cascade_deletes(),
        )
        .expect("cascade relationship");
        assert_eq!(c_foreign_keys(&mut db), vec![1, 1, 2]);

        // deleting P 1 cascades to the two children naming it
        let row_id = row_id_by_id(&mut db, "P", 1);
        db.delete_row("P", row_id).expect("cascade delete");
        assert_eq!(c_foreign_keys(&mut db), vec![2]);

        let row_id = row_id_by_id(&mut db, "P", 2);
        db.delete_row("P", row_id).expect("cascade delete");
        assert_eq!(c_foreign_keys(&mut db), Vec::<i32>::new());

        // a restricted relationship rejects the delete and leaves the file
        // unchanged
        db.add_rows("P", &[p_row(4, "p4")]).expect("add P row");
        db.add_rows("C", &[c_row(Some(4), "c4")])
            .expect("add C row");
        db.add_relationship(
            RelationshipSpec::new("P", "C")
                .add_columns("ID", "P_ID")
                .set_referential_integrity(),
        )
        .expect("restricted relationship");
        let bytes_before = std::fs::read(&path).unwrap();
        let row_id = row_id_by_id(&mut db, "P", 4);
        let err = db.delete_row("P", row_id).expect_err("must be rejected");
        let expected = format!(
            "Removing old row [4, p4] violates constraint Foreign Key from P[ID] (primary) to C[P_ID] (secondary) (Db={})",
            db.name
        );
        assert_eq!(format!("{}", err), expected);
        assert_eq!(std::fs::read(&path).unwrap(), bytes_before);

        // without the child the delete goes through
        let c_row_id = row_id_by_id(&mut db, "C", 4);
        db.delete_row("C", c_row_id).expect("delete child");
        db.delete_row("P", row_id).expect("delete parent");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_cascades_or_is_rejected() {
        let (path, mut db) = create_test_db("upd");
        make_pc(&mut db);
        db.add_rows("P", &[p_row(1, "p1"), p_row(2, "p2")])
            .expect("add P rows");
        db.add_rows("C", &[c_row(Some(1), "c1"), c_row(Some(1), "c2")])
            .expect("add C rows");
        db.add_relationship(
            RelationshipSpec::new("P", "C")
                .add_columns("ID", "P_ID")
                .set_referential_integrity()
                .set_cascade_updates(),
        )
        .expect("cascade relationship");
        assert_eq!(c_foreign_keys(&mut db), vec![1, 1]);

        // changing the parent key updates the children's foreign keys
        let p_row_id = row_id_by_id(&mut db, "P", 1);
        let p_page = db.table_page("P").unwrap();
        let mut row = read_row(&mut db, p_page, p_row_id).expect("read P row");
        row[0] = Value::Long(100);
        db.update_row("P", p_row_id, &row).expect("cascade update");
        assert_eq!(c_foreign_keys(&mut db), vec![100, 100]);

        // an update which keeps the foreign key column passes the unchanged
        // old value to the enforcement (`Column.KEEP_VALUE`), so no constraint
        // is triggered and the value stays the same
        let kept_row_id = row_id_by_id(&mut db, "C", 2);
        db.update_row_partial(
            "C",
            kept_row_id,
            &[Value::Keep, Value::Keep, Value::Text("c2b".to_string())],
        )
        .expect("kept foreign key");
        let c_page = db.table_page("C").unwrap();
        let kept = read_row(&mut db, c_page, kept_row_id).expect("read C row");
        assert_eq!(kept[1], Value::Long(100));
        assert_eq!(kept[2], Value::Text("c2b".to_string()));
        assert_eq!(c_foreign_keys(&mut db), vec![100, 100]);

        // a child update to a missing parent is rejected
        let c_row_id = row_id_by_id(&mut db, "C", 1);
        let c_page = db.table_page("C").unwrap();
        let mut child = read_row(&mut db, c_page, c_row_id).expect("read C row");
        child[1] = Value::Long(999);
        let bytes_before = std::fs::read(&path).unwrap();
        let err = db
            .update_row("C", c_row_id, &child)
            .expect_err("missing parent must be rejected");
        let expected = format!(
            "Adding new row [1, 999, c1] violates constraint Foreign Key from C[P_ID] (secondary) to P[ID] (primary) (Db={})",
            db.name
        );
        assert_eq!(format!("{}", err), expected);
        assert_eq!(std::fs::read(&path).unwrap(), bytes_before);

        // a restricted (non cascading) relationship rejects a parent key change
        db.add_rows("P", &[p_row(3, "p3")]).expect("add P row");
        db.add_rows("C", &[c_row(Some(3), "c3")])
            .expect("add C row");
        db.add_relationship(
            RelationshipSpec::new("P", "C")
                .add_columns("ID", "P_ID")
                .set_referential_integrity(),
        )
        .expect("restricted relationship");
        let bytes_before = std::fs::read(&path).unwrap();
        let p_row_id = row_id_by_id(&mut db, "P", 3);
        let p_page = db.table_page("P").unwrap();
        let mut row = read_row(&mut db, p_page, p_row_id).expect("read P row");
        row[0] = Value::Long(300);
        let err = db
            .update_row("P", p_row_id, &row)
            .expect_err("restricted update must be rejected");
        let expected = format!(
            "Removing old row [3, p3] violates constraint Foreign Key from P[ID] (primary) to C[P_ID] (secondary) (Db={})",
            db.name
        );
        assert_eq!(format!("{}", err), expected);
        assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn self_relationship_allows_a_row_to_be_its_own_parent() {
        let (path, mut db) = create_test_db("self");
        let spec = TableSpec::new(
            "E",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Parent_ID", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(30),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).expect("create E");
        db.add_rows(
            "E",
            &[
                vec![Value::Long(1), Value::Null, Value::Text("e1".into())],
                vec![Value::Long(2), Value::Long(1), Value::Text("e2".into())],
            ],
        )
        .expect("add E rows");
        db.add_relationship(
            RelationshipSpec::new("E", "E")
                .add_columns("ID", "Parent_ID")
                .set_referential_integrity(),
        )
        .expect("self relationship");

        // a row which names itself as its parent is allowed
        db.add_rows(
            "E",
            &[vec![
                Value::Long(3),
                Value::Long(3),
                Value::Text("e3".into()),
            ]],
        )
        .expect("a row may be its own parent");
        // and one naming a missing row is not
        let err = db
            .add_rows(
                "E",
                &[vec![
                    Value::Long(4),
                    Value::Long(99),
                    Value::Text("e4".into()),
                ]],
            )
            .expect_err("missing parent must be rejected");
        assert!(
            format!("{}", err).starts_with("Adding new row [4, 99, e4] violates constraint"),
            "unexpected error {}",
            err
        );

        // deleting the row which is its own parent is allowed (it is the only
        // row naming itself)
        let row_id = row_id_by_id(&mut db, "E", 3);
        db.delete_row("E", row_id).expect("delete own-parent row");
        // deleting a row a child names is rejected
        let row_id = row_id_by_id(&mut db, "E", 1);
        let err = db.delete_row("E", row_id).expect_err("must be rejected");
        assert!(
            format!("{}", err).starts_with("Removing old row [1, null, e1] violates constraint"),
            "unexpected error {}",
            err
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn enforcement_can_be_disabled() {
        let (path, mut db) = create_test_db("off");
        make_pc(&mut db);
        db.add_rows("P", &[p_row(1, "p1")]).expect("add P row");
        db.add_relationship(
            RelationshipSpec::new("P", "C")
                .add_columns("ID", "P_ID")
                .set_referential_integrity(),
        )
        .expect("relationship");
        assert!(db.is_enforce_foreign_keys());
        db.set_enforce_foreign_keys(false);
        assert!(!db.is_enforce_foreign_keys());
        db.add_rows("C", &[c_row(Some(42), "orphan")])
            .expect("enforcement is disabled");
        db.set_enforce_foreign_keys(true);
        db.add_rows("C", &[c_row(Some(1), "c1")])
            .expect("add C row");
        let err = db
            .add_rows("C", &[c_row(Some(43), "orphan")])
            .expect_err("enforcement is enabled again");
        assert!(matches!(err, Error::ConstraintViolation(_)));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preview_auto_numbers_matches_the_written_rows() {
        let (path, mut db) = create_test_db("preview");
        make_pc(&mut db);
        let rows = vec![c_row(None, "a"), c_row(None, "b"), c_row(None, "c")];
        let preview = db
            .get_table("C")
            .unwrap()
            .preview_add_rows(&rows, false)
            .expect("preview rows");
        assert_eq!(
            preview.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
            vec![Value::Long(1), Value::Long(2), Value::Long(3)]
        );
        db.add_rows("C", &rows).expect("add rows");
        let written: Vec<Value> = db
            .read_rows_with_ids("C")
            .unwrap()
            .into_iter()
            .filter_map(|(_, values)| values)
            .map(|r| r[0].clone())
            .collect();
        assert_eq!(
            preview.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
            written
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn text_foreign_keys_match_case_insensitively() {
        let (path, mut db) = create_test_db("text");
        let p = TableSpec::new(
            "P",
            vec![
                ColumnSpec::new("Code", DataType::Text).with_length(20),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("Code".to_string(), true)],
        ));
        db.create_table(&p).expect("create P");
        let c = TableSpec::new(
            "C",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("P_Code", DataType::Text).with_length(20),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&c).expect("create C");
        db.add_rows(
            "P",
            &[
                vec![Value::Text("Alpha".into()), Value::Text("p1".into())],
                vec![Value::Text("beta".into()), Value::Text("p2".into())],
            ],
        )
        .expect("add P rows");
        db.add_relationship(
            RelationshipSpec::new("P", "C")
                .add_columns("Code", "P_Code")
                .set_referential_integrity(),
        )
        .expect("relationship");
        // a differently cased value names the same row
        db.add_rows("C", &[vec![Value::Null, Value::Text("ALPHA".into())]])
            .expect("case insensitive match");
        let err = db
            .add_rows("C", &[vec![Value::Null, Value::Text("gamma".into())]])
            .expect_err("missing parent");
        assert!(matches!(err, Error::ConstraintViolation(_)));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
