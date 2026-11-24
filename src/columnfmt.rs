//! Port of `com.healthmarketscience.jackcess.util.ColumnFormatter`.
//!
//! Java's `ColumnFormatter` loads the `Format` property of a column and builds
//! a formatter from it with the expression engine's `Format()` support
//! (`FormatUtil.createStandaloneFormatter(ctx, fmtStr, 1, 1)`), then applies
//! it to column values for display:
//!
//! ```java
//! ColumnFormatter fmt = new ColumnFormatter(table.getColumn("Price"));
//! String text = fmt.format(row.get("Price"));       // or fmt.getRowValue(row)
//! ```
//!
//! The port uses [`crate::expr::format_value`] (the port of
//! `FormatUtil.format`, i.e. the `Format()` builtin) so the formats behave
//! exactly like the expression engine's, and mirrors the Java class' details:
//!
//! * a column without a `Format` property behaves like `FormatUtil.DUMMY_FMT`:
//!   the value's plain string form (and the empty string for a `null`);
//! * a value which cannot be formatted by the pattern is returned as its plain
//!   string form (Java catches the `EvalException` and returns
//!   `val.toString()`);
//! * the first day of week / first week of year are `1`/`1`, like
//!   `ColumnFormatter.reload` passes to `createStandaloneFormatter`;
//! * an invalid format pattern fails when the formatter is built (Java's
//!   `reload` parses the pattern and lets the parse exception escape).
//!
//! Two runtime failures escape Java's `ColumnFormatter` and are reproduced by
//! the fallible [`ColumnFormatter::try_format`] (the plain [`ColumnFormatter::format`]
//! swallows them and falls back to the plain string form):
//!
//! * a `BOOLEAN` column value: `ColEvalContext.toValue` keeps the `Boolean`
//!   while a boolean column's value type is `LONG`, so the `(Number)` cast
//!   throws a `ClassCastException` (Java's utility therefore cannot format a
//!   boolean column at all; a `null` value still works);
//! * a non finite number in a numeric pattern: Java's `BigDecimal.valueOf`
//!   throws a `NumberFormatException`.  A text pattern (`@`) formats the
//!   specials as `1.#QNAN` / `1.#INF`, like Java.
//!
//! Numbers are converted the way Java's `Format()` does it: a double goes
//! through `BigDecimal.valueOf`, so its whole shortest decimal representation
//! is formatted (`Double.MAX_VALUE` with `0.0000` is a 309 digit integer),
//! while the general number patterns keep Java's 15 significant digits.
//!
//! ```no_run
//! use jackcess::columnfmt::ColumnFormatter;
//!
//! # fn main() -> jackcess::Result<()> {
//! # let mut db: jackcess::database::Database = unimplemented!();
//! let props = db.get_table_properties_by_name("Orders")?;
//! let table = db.get_table("Orders")?;
//! let column = table.get_column("Price").unwrap();
//! let fmt = ColumnFormatter::new(column, &props)?;
//! println!("{}", fmt.format(&jackcess::value::Value::Double(1234.5)));
//! # Ok(())
//! # }
//! ```
//!
//! `ColumnFormatter.setFormatString` is [`ColumnFormatter::set_format_string`]:
//! it updates the property map of the column and saves the group through
//! [`crate::database::Database::save_properties`], exactly like Java's internal
//! `props.save()`, then reloads the formatter.  [`ColumnFormatter::reload`]
//! only re-reads the map (Java's public `reload`, for a map which was modified
//! directly).

use std::collections::HashMap;

use crate::column::Column;
use crate::database::Database;
use crate::datatype::DataType;
use crate::error::{Error, Result};
use crate::expr::{Decimal, ExprError, ExprValue};
use crate::property::{Property, PropertyMap, PropertyMaps, FORMAT_PROP};
use crate::value::Value;

/// The Java `ColumnFormatter` for one column.
///
/// The formatter holds the column's `Format` string (not a parsed formatter, so
/// that the port can keep using the expression engine's parser) plus the
/// information needed by [`get_row_value`](ColumnFormatter::get_row_value).
#[derive(Clone, Debug)]
pub struct ColumnFormatter {
    /// The trimmed? no: the raw `Format` property value (`null` when unset)
    format_string: Option<String>,
    /// `ColumnImpl.getType()`, used to convert an input value to an expression
    /// value the way `ColEvalContext.toValue` does
    data_type: DataType,
    /// `ColumnImpl.getColumnIndex()` (the slot of the column in a row array)
    column_index: usize,
    /// The column name (`ColumnFormatter.getRowValue(Map)`)
    column_name: String,
}

impl ColumnFormatter {
    /// `new ColumnFormatter(Column)`: loads the `Format` property of the column
    /// from the table's property maps.
    ///
    /// A missing (or null) property means "no format".  Any other value than a
    /// string fails like the Java cast to `String` does.
    pub fn new(column: &Column, properties: &PropertyMaps) -> Result<ColumnFormatter> {
        let format_string = format_property(column, properties)?;
        ColumnFormatter::build(column, format_string)
    }

    /// The formatter with an explicit `Format` string (the equivalent of
    /// constructing a `ColumnFormatter` after setting the property, without
    /// saving it).  An empty string behaves like a missing property, exactly
    /// like `ColumnFormatter.reload` (`StringUtil.isEmpty` -> `DUMMY_FMT`).
    pub fn with_format(column: &Column, format_string: Option<&str>) -> Result<ColumnFormatter> {
        ColumnFormatter::build(column, format_string.map(str::to_string))
    }

    fn build(column: &Column, format_string: Option<String>) -> Result<ColumnFormatter> {
        let formatter = ColumnFormatter {
            format_string,
            data_type: column.data_type,
            column_index: column.column_index,
            column_name: column.name.clone(),
        };
        // `reload` parses the pattern eagerly; an invalid one fails here rather
        // than on the first value (Java lets the parse exception escape too).
        formatter.validate()?;
        Ok(formatter)
    }

    /// The currently loaded `Format` property (`ColumnFormatter.getFormatString`),
    /// which may be `None`.
    pub fn get_format_string(&self) -> Option<&str> {
        self.format_string.as_deref()
    }

    /// `ColumnFormatter.reload`: re-reads the column's `Format` property from
    /// the given property maps and re-parses it.
    pub fn reload(&mut self, column: &Column, properties: &PropertyMaps) -> Result<()> {
        self.format_string = format_property(column, properties)?;
        self.validate()
    }

    /// `ColumnFormatter.setFormatString`: stores (or, for `None`/an empty
    /// string, removes) the `Format` property of the column in `properties`,
    /// **saves the property group**, then reloads this formatter.
    ///
    /// `props.save()` is Java's write of the group into the owning catalog
    /// row's `LvProp` value ([`Database::save_properties`]), including the
    /// table's `propertiesUpdated` refresh of the column validators/defaults,
    /// so the new format survives a reopen of the database.
    ///
    /// The order of the Java method is reproduced: the map is updated first,
    /// then saved, then the formatter is reloaded.
    ///
    /// * a failed save (e.g. a read-only database, Java's
    ///   `NonWritableChannelException`, or a group without a row id, Java's
    ///   `IllegalStateException`) leaves the in-memory map updated and this
    ///   formatter on its old format, and the error is returned;
    /// * a saved format which the expression engine cannot parse (Java's
    ///   `ParseException` from `createStandaloneFormatter`) is returned after
    ///   the property was written; like Java's `reload`, the format string of
    ///   this formatter is updated before the parse is attempted (Java leaves
    ///   its parsed formatter null, so a later `format` throws a
    ///   `NullPointerException`; here [`ColumnFormatter::try_format`] reports
    ///   the parse failure and [`ColumnFormatter::format`] falls back to the
    ///   plain string form).
    pub fn set_format_string(
        &mut self,
        db: &mut Database,
        properties: &mut PropertyMaps,
        format_string: Option<&str>,
    ) -> Result<()> {
        let stored = match format_string.filter(|text| !text.is_empty()) {
            Some(text) => {
                properties.get_mut(&self.column_name).put(Property::new(
                    FORMAT_PROP.to_string(),
                    DataType::Text,
                    Value::Text(text.to_string()),
                ));
                Some(text.to_string())
            }
            None => {
                properties.get_mut(&self.column_name).remove(FORMAT_PROP);
                None
            }
        };
        // `props.save()`
        db.save_properties(properties)?;
        // `reload()`: `_fmtStr` is assigned before the pattern is parsed
        self.format_string = stored;
        self.validate()
    }

    /// Formats a value according to the column's format
    /// (`ColumnFormatter.format`): the formatted text, always non `null`.
    ///
    /// A value the format cannot handle is rendered with its plain string form
    /// (Java's `val.toString()` fallback).  The failures Java lets escape (the
    /// `BOOLEAN` conversion `ClassCastException`, the `NumberFormatException`
    /// of a non finite number) are reported by [`ColumnFormatter::try_format`]
    /// and swallowed here.
    pub fn format(&self, value: &Value) -> String {
        self.try_format(value)
            .unwrap_or_else(|_| crate::export::java_value_to_string(value))
    }

    /// The fallible form of [`ColumnFormatter::format`].
    ///
    /// The `Err` cases mirror the runtime exceptions Java's `ColumnFormatter`
    /// does not catch:
    ///
    /// * a `BOOLEAN` column value cannot be converted by
    ///   `ColEvalContext.toValue` (it keeps the `Boolean` while the value type
    ///   of a boolean column is `LONG`, so the cast to `Number` fails with a
    ///   `ClassCastException`);
    /// * a non finite number in a numeric format (Java's
    ///   `BigDecimal.valueOf` throws a `NumberFormatException`);
    /// * a value the expression port cannot represent (the error message says
    ///   what Java would have produced).
    ///
    /// A value the *format* cannot handle is not an error: it is returned as
    /// its plain string form, exactly like `format`.
    pub fn try_format(&self, value: &Value) -> Result<String> {
        let expr_value = match value_to_expr_value(self.data_type, value)? {
            Some(expr_value) => expr_value,
            // a value with no expression engine representation is rendered
            // with the Java `toString` of the matching object
            None => return Ok(crate::export::java_value_to_string(value)),
        };
        let format_string = self.format_string.as_deref().unwrap_or("");
        match crate::expr::format_value(format_string, &expr_value, 1, 1) {
            Ok(result) => result.get_as_string().map_err(map_expr_error),
            Err(error) => Err(map_expr_error(error)),
        }
    }

    /// `ColumnFormatter.getRowValue(Object[])`: formats the value of this
    /// column in a row array.
    pub fn get_row_value(&self, row: &[Value]) -> String {
        self.try_get_row_value(row)
            .unwrap_or_else(|_| crate::export::java_value_to_string(&Value::Null))
    }

    /// The fallible form of [`ColumnFormatter::get_row_value`].
    pub fn try_get_row_value(&self, row: &[Value]) -> Result<String> {
        match row.get(self.column_index) {
            Some(value) => self.try_format(value),
            None => self.try_format(&Value::Null),
        }
    }

    /// `ColumnFormatter.getRowValue(Map)`: formats the value of this column in
    /// a row map (the map lookup is case sensitive, like `Map.get`).
    pub fn get_row_value_map(&self, row: &HashMap<String, Value>) -> String {
        self.try_get_row_value_map(row)
            .unwrap_or_else(|_| crate::export::java_value_to_string(&Value::Null))
    }

    /// The fallible form of [`ColumnFormatter::get_row_value_map`].
    pub fn try_get_row_value_map(&self, row: &HashMap<String, Value>) -> Result<String> {
        match row.get(&self.column_name) {
            Some(value) => self.try_format(value),
            None => self.try_format(&Value::Null),
        }
    }

    /// `ColumnFormatter.getRowValue(Map)` for the port's ordered row maps.
    pub fn get_row_value_named<'a>(
        &self,
        row: impl IntoIterator<Item = (&'a String, &'a Value)>,
    ) -> String {
        self.try_get_row_value_named(row)
            .unwrap_or_else(|_| crate::export::java_value_to_string(&Value::Null))
    }

    /// The fallible form of [`ColumnFormatter::get_row_value_named`].
    pub fn try_get_row_value_named<'a>(
        &self,
        row: impl IntoIterator<Item = (&'a String, &'a Value)>,
    ) -> Result<String> {
        for (name, value) in row {
            if name == &self.column_name {
                return self.try_format(value);
            }
        }
        self.try_format(&Value::Null)
    }

    /// Re-parses the current format string, reporting an invalid pattern.
    fn validate(&self) -> Result<()> {
        let format_string = self.format_string.as_deref().unwrap_or("");
        // A value no format can format is a normal runtime failure, so only the
        // parse failures are reported (the `Format()` port handles the rest).
        match crate::expr::format_value(format_string, &ExprValue::Text(String::new()), 1, 1) {
            Ok(_) => Ok(()),
            Err(error) => Err(map_expr_error(error)),
        }
    }
}

/// Formats the values of the named columns of one row with their `Format`
/// properties, in the order of the given columns.
///
/// This is the row level convenience the Java utility is usually used for:
/// `columns.iter().map(|c| new ColumnFormatter(c).getRowValue(row))`.
pub fn format_columns(
    columns: &[&Column],
    properties: &PropertyMaps,
    row: &[Value],
) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(columns.len());
    for column in columns {
        let formatter = ColumnFormatter::new(column, properties)?;
        out.push(formatter.try_get_row_value(row)?);
    }
    Ok(out)
}

/// Reads the `Format` property of `column` from `properties`.
fn format_property(column: &Column, properties: &PropertyMaps) -> Result<Option<String>> {
    let value = properties
        .get(&column.name)
        .and_then(|map| map.get_value(FORMAT_PROP));
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Text(text)) => Ok(Some(text.clone())),
        Some(other) => Err(Error::Jackcess(format!(
            "Format property of column {} is not a string: {:?}",
            column.name, other
        ))),
    }
}

/// `ColumnImpl.toInternalValue` / `ColEvalContext.toValue(val)`: the expression
/// engine's representation of a stored value.  Values the engine has no
/// representation for (`byte[]`, GUID, KEEP_VALUE) are reported as `None` so
/// the caller can fall back to the plain string form.
///
/// The `BOOLEAN` case reproduces a Java quirk: `toInternalValue` keeps the
/// `Boolean`, but `toValueType(BOOLEAN)` is `LONG`, so the following
/// `(Number)val` cast throws a `ClassCastException`.  Every non null value of
/// a boolean column therefore fails in Java's `ColumnFormatter`.
fn value_to_expr_value(data_type: DataType, value: &Value) -> Result<Option<ExprValue>> {
    if data_type == DataType::Boolean && !value.is_null() {
        return Err(Error::IllegalState(
            "java.lang.ClassCastException: java.lang.Boolean cannot be cast to java.lang.Number"
                .to_string(),
        ));
    }
    Ok(match value {
        Value::Null => Some(ExprValue::Null),
        Value::Bool(value) => Some(ExprValue::Bool(*value)),
        Value::Byte(value) => Some(ExprValue::Byte(*value as i8)),
        Value::Int(value) => Some(ExprValue::Int(*value)),
        Value::Long(value) => Some(ExprValue::Long(*value)),
        Value::BigInt(value) => Some(ExprValue::BigInt(*value)),
        Value::Float(value) => Some(ExprValue::Double(*value as f64)),
        Value::Double(value) => Some(ExprValue::Double(*value)),
        Value::Decimal { unscaled, scale } => Decimal::from_unscaled(*unscaled, *scale)
            .ok()
            .map(ExprValue::Decimal),
        Value::Text(text) => Some(ExprValue::Text(text.clone())),
        Value::DateTime(ldt) => Some(ExprValue::date_time(*ldt)),
        Value::DateBits(bits) => Some(ExprValue::date_time(
            crate::column::ldt_from_local_date_double(*bits),
        )),
        Value::Uuid(_) | Value::Bytes(_) | Value::Keep => {
            // `toValueType` falls back to STRING and reads `val.toString()`:
            // for a GUID that is the UUID text, for a byte[] an identity hash
            // (not reproducible).  Only the GUID text form is modelled; the
            // stored GUID bytes are already the display string here.
            match (data_type, value) {
                (DataType::Guid, Value::Uuid(_)) => {
                    Some(ExprValue::Text(crate::export::java_value_to_string(value)))
                }
                _ => None,
            }
        }
    })
}

fn map_expr_error(error: ExprError) -> Error {
    match error {
        ExprError::Unsupported(message) => Error::Unsupported(message),
        ExprError::Parse(message) => Error::Jackcess(message),
        other => Error::Jackcess(other.to_string()),
    }
}

/// The `Format` property map of a column, if the table's maps were loaded from
/// a database that has one.
pub fn format_property_map<'a>(
    column: &Column,
    properties: &'a PropertyMaps,
) -> Option<&'a PropertyMap> {
    properties.get(&column.name)
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

    fn build_table() -> (Database, PathBuf) {
        // a unique file per test: the tests run in parallel and each database
        // must not be replaced by another test's `create`
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = test_tmp_dir().join(format!("columnfmt-{}-{unique}.accdb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).unwrap();
        db.now_ms = Some(1_600_000_000_000);
        let spec = TableSpec::new(
            "Fmt",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Price", DataType::Money),
                ColumnSpec::new("Qty", DataType::Long),
                ColumnSpec::new("Ratio", DataType::Double),
                ColumnSpec::new("Label", DataType::Text).with_length(40),
                ColumnSpec::new("Flag", DataType::Boolean),
                ColumnSpec::new("Stamp", DataType::ShortDateTime),
                ColumnSpec::new("Note", DataType::Memo),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).unwrap();
        (db, path)
    }

    fn formatter(db: &mut Database, column: &str, format: Option<&str>) -> ColumnFormatter {
        let mut maps = db.get_table_properties_by_name("Fmt").unwrap();
        let col = db
            .get_table("Fmt")
            .unwrap()
            .get_column(column)
            .unwrap()
            .clone();
        if let Some(format) = format {
            maps.get_mut(column).put(Property::new(
                FORMAT_PROP.to_string(),
                DataType::Text,
                Value::Text(format.to_string()),
            ));
            db.save_properties(&mut maps).unwrap();
        }
        ColumnFormatter::new(&col, &maps).unwrap()
    }

    #[test]
    fn boolean_columns_reproduce_the_java_class_cast_exception() {
        let (mut db, _path) = build_table();
        let flag = formatter(&mut db, "Flag", Some("Yes/No"));
        for value in [Value::Bool(true), Value::Bool(false), Value::Long(-1)] {
            let error = flag.try_format(&value).unwrap_err();
            assert!(
                error.to_string().contains("ClassCastException"),
                "unexpected error: {error}"
            );
            // the non fallible form keeps the Java `val.toString()` fallback
            assert_eq!(
                flag.format(&value),
                crate::export::java_value_to_string(&value)
            );
        }
        // a null boolean value is converted fine (and a predef format returns
        // the empty string for it)
        assert_eq!(flag.try_format(&Value::Null).unwrap(), "");
    }

    #[test]
    fn non_finite_numbers_report_the_java_number_format_exception() {
        let (mut db, _path) = build_table();
        let ratio = formatter(&mut db, "Ratio", Some("0.0000"));
        for value in [Value::Double(f64::NAN), Value::Double(f64::INFINITY)] {
            let error = ratio.try_format(&value).unwrap_err();
            assert!(
                error.to_string().contains("NumberFormatException"),
                "unexpected error: {error}"
            );
        }
        // a text format does format the specials, like Java's `getAsString`
        let text = formatter(&mut db, "Ratio", Some("@"));
        assert_eq!(
            text.try_format(&Value::Double(f64::NAN)).unwrap(),
            "1.#QNAN"
        );
        assert_eq!(
            text.try_format(&Value::Double(f64::INFINITY)).unwrap(),
            "1.#INF"
        );
        // Java's `Format` converts the double with `BigDecimal.valueOf`, i.e.
        // the shortest decimal representation widened with zeros, so
        // `Double.MAX_VALUE` is a 309 digit integer followed by the pattern's
        // decimals (the value has 17 significant digits and 292 trailing zeros)
        let max_text = ratio.try_format(&Value::Double(f64::MAX)).unwrap();
        assert_eq!(
            max_text,
            format!("17976931348623157{}.0000", "0".repeat(292))
        );
        // the tiniest values round to zero with four decimals
        assert_eq!(
            ratio.try_format(&Value::Double(f64::from_bits(1))).unwrap(),
            "0.0000"
        );
        assert_eq!(
            ratio.try_format(&Value::Double(f64::MIN_POSITIVE)).unwrap(),
            "0.0000"
        );
        // the general number pattern keeps Java's 15 significant digits
        let general = formatter(&mut db, "Ratio", Some("General Number"));
        assert_eq!(
            general.try_format(&Value::Double(f64::MAX)).unwrap(),
            "1.79769313486232E+308"
        );
        assert_eq!(
            general
                .try_format(&Value::Double(f64::from_bits(1)))
                .unwrap(),
            "4.94065645841247E-324"
        );
    }

    #[test]
    fn number_formats_match_the_expression_engine() {
        let (mut db, _path) = build_table();
        let money = formatter(&mut db, "Price", Some("Currency"));
        assert_eq!(
            money.format(&Value::Decimal {
                unscaled: 12345,
                scale: 2
            }),
            "$123.45"
        );
        assert_eq!(money.format(&Value::Null), "");

        let qty = formatter(&mut db, "Qty", Some("#,##0"));
        assert_eq!(qty.format(&Value::Long(1234567)), "1,234,567");
        assert_eq!(qty.format(&Value::Long(-42)), "-42");

        let ratio = formatter(&mut db, "Ratio", Some("0.00"));
        assert_eq!(ratio.format(&Value::Double(1.5)), "1.50");
        // a text value which cannot be parsed as a number is returned as is
        assert_eq!(
            ratio.format(&Value::Text("not a number".to_string())),
            "not a number"
        );
    }

    #[test]
    fn date_text_and_boolean_formats() {
        let (mut db, _path) = build_table();
        let stamp = formatter(&mut db, "Stamp", Some("yyyy-mm-dd"));
        let value = Value::DateTime(crate::datenums::LocalDateTime::new(
            2020, 3, 15, 12, 30, 45, 0,
        ));
        assert_eq!(stamp.format(&value), "2020-03-15");

        let label = formatter(&mut db, "Label", Some(">"));
        assert_eq!(label.format(&Value::Text("abc".to_string())), "ABC");

        // Java's `ColumnFormatter` cannot convert a boolean column value at all
        // (it throws a `ClassCastException`); only `try_format` reports it, the
        // non fallible form uses the plain `toString` fallback
        let flag = formatter(&mut db, "Flag", Some("Yes/No"));
        assert!(flag.try_format(&Value::Bool(true)).is_err());
        assert_eq!(flag.format(&Value::Bool(true)), "true");
        assert_eq!(flag.format(&Value::Bool(false)), "false");
    }

    #[test]
    fn no_format_returns_the_plain_string_form() {
        let (mut db, _path) = build_table();
        let note = formatter(&mut db, "Note", None);
        assert_eq!(note.get_format_string(), None);
        assert_eq!(note.format(&Value::Text("plain".to_string())), "plain");
        assert_eq!(note.format(&Value::Null), "");
        assert_eq!(note.format(&Value::Long(17)), "17");
    }

    #[test]
    fn row_values_and_named_columns() {
        let (mut db, _path) = build_table();
        let row = vec![
            Value::Long(1),
            Value::Decimal {
                unscaled: 999,
                scale: 2,
            },
            Value::Long(3),
            Value::Double(0.5),
            Value::Text("row".to_string()),
            Value::Bool(true),
            Value::DateTime(crate::datenums::LocalDateTime::new(2021, 1, 2, 3, 4, 5, 0)),
            Value::Text("memo".to_string()),
        ];
        db.add_rows("Fmt", std::slice::from_ref(&row)).unwrap();
        let maps = db.get_table_properties_by_name("Fmt").unwrap();
        let (price_col, qty_col, label_col) = {
            let table = db.get_table("Fmt").unwrap();
            (
                table.get_column("Price").unwrap().clone(),
                table.get_column("Qty").unwrap().clone(),
                table.get_column("Label").unwrap().clone(),
            )
        };

        let mut price = ColumnFormatter::with_format(&price_col, Some("0.00")).unwrap();
        assert_eq!(price.get_row_value(&row), "9.99");
        let mut prop_map = maps.clone();
        price
            .set_format_string(&mut db, &mut prop_map, Some("$0.00"))
            .unwrap();
        assert_eq!(price.get_format_string(), Some("$0.00"));
        assert_eq!(price.get_row_value(&row), "$9.99");

        let mut named = HashMap::new();
        named.insert(
            "Price".to_string(),
            Value::Decimal {
                unscaled: 5,
                scale: 2,
            },
        );
        assert_eq!(price.get_row_value_map(&named), "$0.05");
        assert_eq!(price.get_row_value_named(named.iter()), "$0.05");

        let columns: Vec<&Column> = vec![&qty_col, &label_col];
        let texts = format_columns(&columns, &maps, &row).unwrap();
        assert_eq!(texts, vec!["3".to_string(), "row".to_string()]);
    }

    #[test]
    fn invalid_formats_fail_when_built() {
        let (mut db, _path) = build_table();
        let maps = db.get_table_properties_by_name("Fmt").unwrap();
        let col = db
            .get_table("Fmt")
            .unwrap()
            .get_column("Qty")
            .unwrap()
            .clone();
        assert!(ColumnFormatter::with_format(&col, Some("\"unterminated")).is_err());
        // an empty format behaves like an unset property
        let empty = ColumnFormatter::with_format(&col, Some("")).unwrap();
        assert_eq!(empty.format(&Value::Long(5)), "5");
        assert!(ColumnFormatter::new(&col, &maps).is_ok());
    }

    /// `ColumnFormatter.setFormatString`: the `Format` property is saved
    /// through `PropertyMap.save()` (the catalog row's `LvProp` value), so it
    /// survives a reopen; an empty string or `None` removes it.
    #[test]
    fn set_format_string_saves_the_property_like_java() {
        let (mut db, path) = build_table();
        let mut maps = db.get_table_properties_by_name("Fmt").unwrap();
        let col = db
            .get_table("Fmt")
            .unwrap()
            .get_column("Qty")
            .unwrap()
            .clone();
        let mut fmt = ColumnFormatter::new(&col, &maps).unwrap();
        assert_eq!(fmt.get_format_string(), None);

        fmt.set_format_string(&mut db, &mut maps, Some("#,##0"))
            .unwrap();
        assert_eq!(fmt.get_format_string(), Some("#,##0"));
        assert_eq!(fmt.format(&Value::Long(1_234_567)), "1,234,567");

        // the property was written to the catalog row: a new handle sees it
        drop(db);
        let mut db = Database::open_path(&path).unwrap();
        let maps = db.get_table_properties_by_name("Fmt").unwrap();
        assert_eq!(
            maps.get("Qty").unwrap().get_value(FORMAT_PROP).cloned(),
            Some(Value::Text("#,##0".to_string()))
        );
        let col = db
            .get_table("Fmt")
            .unwrap()
            .get_column("Qty")
            .unwrap()
            .clone();
        let reloaded = ColumnFormatter::new(&col, &maps).unwrap();
        assert_eq!(reloaded.get_format_string(), Some("#,##0"));
        assert_eq!(reloaded.format(&Value::Long(42)), "42");

        // `None` removes the property (Java's `props.remove`)
        let mut maps = db.get_table_properties_by_name("Fmt").unwrap();
        let mut fmt = ColumnFormatter::new(&col, &maps).unwrap();
        fmt.set_format_string(&mut db, &mut maps, None).unwrap();
        assert_eq!(fmt.get_format_string(), None);
        drop(db);
        let mut db = Database::open_path(&path).unwrap();
        let maps = db.get_table_properties_by_name("Fmt").unwrap();
        assert_eq!(maps.get("Qty").unwrap().get_value(FORMAT_PROP), None);

        // an empty string removes it as well (`StringUtil.isEmpty`)
        let col = db
            .get_table("Fmt")
            .unwrap()
            .get_column("Qty")
            .unwrap()
            .clone();
        let mut maps = db.get_table_properties_by_name("Fmt").unwrap();
        let mut fmt = ColumnFormatter::new(&col, &maps).unwrap();
        fmt.set_format_string(&mut db, &mut maps, Some("0"))
            .unwrap();
        assert_eq!(fmt.get_format_string(), Some("0"));
        fmt.set_format_string(&mut db, &mut maps, Some("")).unwrap();
        assert_eq!(fmt.get_format_string(), None);
        assert_eq!(maps.get("Qty").unwrap().get_value(FORMAT_PROP), None);
        drop(db);
        let mut db = Database::open_path(&path).unwrap();
        let maps = db.get_table_properties_by_name("Fmt").unwrap();
        assert_eq!(maps.get("Qty").unwrap().get_value(FORMAT_PROP), None);
        let _ = std::fs::remove_file(&path);
    }

    /// The two error cases of `ColumnFormatter.setFormatString`, both from
    /// Java's method order (update the map, save, then reload):
    ///
    /// * an invalid pattern is *saved* before the reload rejects it (Java's
    ///   `ParseException`), and the formatter is left on the new string;
    /// * a read-only database fails the save with Java's
    ///   `NonWritableChannelException`, leaving the in-memory map updated but
    ///   the formatter on its old format (reload is not reached).
    #[test]
    fn set_format_string_error_cases_match_java() {
        let (mut db, path) = build_table();
        let mut maps = db.get_table_properties_by_name("Fmt").unwrap();
        let col = db
            .get_table("Fmt")
            .unwrap()
            .get_column("Qty")
            .unwrap()
            .clone();
        let mut fmt = ColumnFormatter::new(&col, &maps).unwrap();
        fmt.set_format_string(&mut db, &mut maps, Some("0"))
            .unwrap();
        drop(db);

        // a read-only database: the put changes the in-memory map, the save
        // fails, and the formatter keeps the format it had (`reload` is not
        // reached, exactly like Java's `setFormatString`)
        let mut ro = Database::open_path_read_only(&path).unwrap();
        let col = ro
            .get_table("Fmt")
            .unwrap()
            .get_column("Qty")
            .unwrap()
            .clone();
        let mut maps = ro.get_table_properties_by_name("Fmt").unwrap();
        let mut fmt = ColumnFormatter::new(&col, &maps).unwrap();
        assert_eq!(fmt.get_format_string(), Some("0"));
        let error = fmt
            .set_format_string(&mut ro, &mut maps, Some("0.00"))
            .unwrap_err();
        assert!(
            error.to_string().contains("NonWritableChannelException"),
            "unexpected error: {error}"
        );
        assert_eq!(
            error.java_to_string(),
            "java.nio.channels.NonWritableChannelException"
        );
        // `reload` was not reached, so the formatter still has the old format
        assert_eq!(fmt.get_format_string(), Some("0"));
        // but the in-memory property map holds the value Java's `put` stored
        assert_eq!(
            maps.get("Qty").unwrap().get_value(FORMAT_PROP).cloned(),
            Some(Value::Text("0.00".to_string()))
        );
        drop(ro);

        // the database on disk is unchanged by the failed save
        let mut db = Database::open_path(&path).unwrap();
        let maps = db.get_table_properties_by_name("Fmt").unwrap();
        assert_eq!(
            maps.get("Qty").unwrap().get_value(FORMAT_PROP).cloned(),
            Some(Value::Text("0".to_string()))
        );

        // an invalid pattern: the save happened, then the reload failed
        let col = db
            .get_table("Fmt")
            .unwrap()
            .get_column("Qty")
            .unwrap()
            .clone();
        let mut maps = db.get_table_properties_by_name("Fmt").unwrap();
        let mut fmt = ColumnFormatter::new(&col, &maps).unwrap();
        let error = fmt
            .set_format_string(&mut db, &mut maps, Some("\"unterminated"))
            .unwrap_err();
        // Java: `com.healthmarketscience.jackcess.expr.ParseException: Missing
        // closing '"' for quoted string [char 13] '"unterminated'`.  The port
        // has no ParseException error kind and its expression parser's message
        // is shorter ("Missing closing '\"' for quoted string in format
        // pattern"), but the failure is reported after the save like Java's.
        assert!(
            error
                .to_string()
                .contains("Missing closing '\"' for quoted string"),
            "unexpected error: {error}"
        );
        // `reload` assigned the format string before the parse failed
        assert_eq!(fmt.get_format_string(), Some("\"unterminated"));
        // a later format reports the parse failure instead of Java's
        // NullPointerException (the port has no parsed formatter to null out)
        assert!(fmt.try_format(&Value::Long(5)).is_err());
        // and the pattern itself cannot be used to build a new formatter
        // (Java's `new ColumnFormatter` throws the same parse exception)
        assert!(ColumnFormatter::new(&col, &maps).is_err());
        drop(db);

        // the property was saved before the reload failed
        let mut db = Database::open_path(&path).unwrap();
        let maps = db.get_table_properties_by_name("Fmt").unwrap();
        assert_eq!(
            maps.get("Qty").unwrap().get_value(FORMAT_PROP).cloned(),
            Some(Value::Text("\"unterminated".to_string()))
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
