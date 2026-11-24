//! `rowapi` - the Rust half of the `Row` API verification harness: the
//! counterpart of `java-src/RowApiGen.java` (see `tools/verify-rowapi.ps1`).
//!
//! Usage:
//! ```text
//! rowapi read <database>
//! rowapi date <database>
//! rowapi gen <database> <format> [--now-ms N]
//! ```
//!
//! `read` opens the database and dumps the canonical text form of the [`Row`]
//! API of every user table: the table/column metadata, and for every row the
//! [`Row`] contents in iteration order, its `RowId`, the Java typed getters
//! (`get_string`, `get_boolean`, `get_byte`, `get_short`, `get_int`,
//! `get_big_decimal`, `get_float`, `get_double`, `get_local_date_time`,
//! `get_bytes`, `get_blob`, `get_foreign_key`, plus the legacy `get_date` which
//! fails in the default `LOCAL_DATE_TIME` mode, like Java's
//! `ClassCastException`), and the map equality.  The Java tool prints the same
//! lines from jackcess and the harness compares them line by line.
//!
//! `gen` creates a database with a table covering the scalar column types,
//! drives the [`Row`] based add/update/delete/iteration/cursor operations
//! (`add_row_from_map`, `add_rows_from_maps`, `update_row_object`,
//! `delete_row_object`, `Table::row_iter`, the cursor row getters, the
//! `IterableBuilder`, `as_row`/`as_update_row`) and prints the same operation
//! trace as the Java tool, then reopens the file and dumps it.  `--now-ms` is
//! the catalog `DateCreate` timestamp read from the Java file by `GetDates`,
//! so the two generated databases can also be compared byte for byte.

use std::collections::BTreeMap;
use std::path::Path;

use jackcess::column::ColumnSpec;
use jackcess::cursor::{Cursor, TableScanCursor};
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::datenums::{self, LocalDateTime, TimeZone};
use jackcess::format::FileFormat;
use jackcess::row::{guid_string, Row};
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

/// 2020-03-15T12:30:45 in the database zone (the instant the existing
/// generators use, `DATE_BASE_MS` in `Gen.java`).
const DATE_BASE_MS: i64 = 1584246645000;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: rowapi read <database>");
        eprintln!("       rowapi date <database>");
        eprintln!("       rowapi gen <database> <format> [--now-ms N]");
        std::process::exit(2);
    }
    let result = match args[1].as_str() {
        "read" => read_mode(Path::new(&args[2])),
        "date" => date_mode(Path::new(&args[2])),
        "gen" => {
            let mut now_ms = None;
            let mut i = 4;
            while i < args.len() {
                if args[i] == "--now-ms" {
                    now_ms = args.get(i + 1).and_then(|s| s.trim().parse::<i64>().ok());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            gen_mode(Path::new(&args[2]), &args[3], now_ms)
        }
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn read_mode(path: &Path) -> jackcess::Result<()> {
    let mut db = Database::open_path(path)?;
    dump(&mut db)
}

/// The legacy `DateTimeType.DATE` mode (`Row.getDate` succeeds there, the
/// `LocalDateTime` getters fail, like jackcess).
fn date_mode(path: &Path) -> jackcess::Result<()> {
    let mut db = Database::open_path(path)?;
    db.set_date_time_type(jackcess::database::DateTimeType::Date);
    dump(&mut db)
}

// ---------------------------------------------------------------------------
// the canonical dump
// ---------------------------------------------------------------------------

fn dump(db: &mut Database) -> jackcess::Result<()> {
    let tz = *db.get_time_zone();
    println!("FORMAT={}", format_name(db.get_file_format()?));
    let mut names = db.get_table_names()?;
    names.sort();
    println!("TABLES={}:{}", names.len(), names.join(","));
    for name in &names {
        let (row_count, columns) = {
            let table = db.get_table(name)?;
            let columns: Vec<(String, DataType)> = table
                .get_columns()
                .iter()
                .map(|column| (column.name.clone(), column.data_type))
                .collect();
            (table.get_row_count(), columns)
        };
        println!("TABLE={}|rows={}|cols={}", name, row_count, columns.len());
        for (i, (column_name, data_type)) in columns.iter().enumerate() {
            println!(
                "COL={}|{}|{}|{}",
                name,
                i,
                column_name,
                data_type.get_name()
            );
        }
        let rows = db.rows(name)?;
        let mut iterated = 0;
        for row in &rows {
            iterated += 1;
            let id = row_id_text(row);
            let mut line = format!("ROW={}|{}", name, id);
            for (column_name, data_type) in &columns {
                line.push('|');
                line.push_str(column_name);
                line.push('=');
                line.push_str(&format_value(
                    *data_type,
                    row.get(column_name).unwrap_or(&Value::Null),
                    &tz,
                ));
            }
            println!("{line}");
            println!(
                "KEYS={}|{}|{}",
                name,
                id,
                row.keys().collect::<Vec<_>>().join(",")
            );
            let mut line = format!("GET={}|{}", name, id);
            for (column_name, data_type) in &columns {
                line.push('|');
                line.push_str(column_name);
                line.push('=');
                line.push_str(&typed_value(row, column_name, *data_type, &tz));
            }
            println!("{line}");
            for (column_name, data_type) in &columns {
                if *data_type == DataType::Ole {
                    println!(
                        "BLOB={}|{}|{}={}",
                        name,
                        id,
                        column_name,
                        blob_text(row, column_name)
                    );
                }
            }
            let mut line = format!("DATE={}|{}", name, id);
            for (column_name, data_type) in &columns {
                if matches!(data_type, DataType::ShortDateTime | DataType::ExtDateTime) {
                    line.push('|');
                    line.push_str(column_name);
                    line.push('=');
                    line.push_str(&date_text(row, column_name, &tz));
                }
            }
            println!("{line}");
            println!("EQ={}|{}|equals={}", name, id, row == &row.clone());
        }
        println!("ROWS={}|iterated={}", name, iterated);
    }
    Ok(())
}

fn format_name(format: FileFormat) -> &'static str {
    match format {
        FileFormat::V1997 => "V1997",
        FileFormat::GenericJet4 => "GENERIC_JET4",
        FileFormat::V2000 => "V2000",
        FileFormat::V2003 => "V2003",
        FileFormat::V2007 => "V2007",
        FileFormat::V2010 => "V2010",
        FileFormat::V2016 => "V2016",
        FileFormat::V2019 => "V2019",
        FileFormat::Msisam => "MSISAM",
    }
}

fn row_id_text(row: &Row) -> String {
    match row.get_id() {
        Some((page, number)) => format!("{page}:{number}"),
        None => "null".to_string(),
    }
}

// ---------------------------------------------------------------------------
// canonical value formatting (must match RowApiGen.java)
// ---------------------------------------------------------------------------

fn format_value(data_type: DataType, value: &Value, tz: &TimeZone) -> String {
    match data_type {
        DataType::ComplexType => match value.as_i64() {
            Some(number) => format!("complex({number})"),
            None if value.is_null() => "null".to_string(),
            None => value_text(value, tz),
        },
        DataType::Binary | DataType::Ole | DataType::BigBinary => match value {
            Value::Bytes(bytes) => format!("bytes({})", hex(bytes)),
            Value::Null => "null".to_string(),
            other => value_text(other, tz),
        },
        DataType::Money | DataType::Numeric => match value {
            Value::Decimal { .. } => decimal_text(value),
            Value::Null => "null".to_string(),
            other => value_text(other, tz),
        },
        DataType::Guid => match value {
            Value::Uuid(bytes) => format!("text({})", escape(&guid_string(bytes))),
            Value::Text(text) => format!("text({})", escape(text)),
            Value::Null => "null".to_string(),
            other => value_text(other, tz),
        },
        _ => value_text(value, tz),
    }
}

fn value_text(value: &Value, tz: &TimeZone) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => format!("bool({value})"),
        Value::Byte(value) => format!("byte({})", *value as i8),
        Value::Int(value) => format!("int({value})"),
        Value::Long(value) => format!("long({value})"),
        Value::BigInt(value) => format!("bigint({value})"),
        Value::Decimal { .. } => decimal_text(value),
        Value::Float(value) => format!("float({:08x})", value.to_bits()),
        Value::Double(value) => format!("double({:016x})", value.to_bits()),
        Value::Text(text) => format!("text({})", escape(text)),
        Value::Bytes(bytes) => format!("bytes({})", hex(bytes)),
        Value::DateTime(ldt) => date_time_text(ldt),
        Value::DateBits(bits) => {
            format!("datemillis({})", datenums::from_date_double(*bits, tz))
        }
        Value::Uuid(bytes) => format!("text({})", escape(&guid_string(bytes))),
        Value::Keep => "keep".to_string(),
    }
}

fn decimal_text(value: &Value) -> String {
    match value {
        Value::Decimal { unscaled, scale } => format!("decimal({unscaled},{scale})"),
        Value::Null => "null".to_string(),
        other => value_text(other, &TimeZone::utc()),
    }
}

fn date_time_text(ldt: &LocalDateTime) -> String {
    format!(
        "date({:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09})",
        ldt.year, ldt.month, ldt.day, ldt.hour, ldt.minute, ldt.second, ldt.nano
    )
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Same escaping as the Java half: printable ASCII, C escapes, else `\u{XXXX}`.
fn escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (' '..='~').contains(&c) => out.push(c),
            c => out.push_str(&format!("\\u{{{:04x}}}", c as u32)),
        }
    }
    out
}

fn typed_value(row: &Row, name: &str, data_type: DataType, tz: &TimeZone) -> String {
    match data_type {
        DataType::Boolean => opt_text(row.get_boolean(name), |value| format!("bool({value})")),
        DataType::Byte => opt_text(row.get_byte(name), |value| format!("byte({value})")),
        DataType::Int => opt_text(row.get_short(name), |value| format!("int({value})")),
        DataType::Long => opt_text(row.get_int(name), |value| format!("long({value})")),
        DataType::Money | DataType::Numeric => opt_text(row.get_big_decimal(name), |value| {
            let (unscaled, scale) = value.as_bigint_and_exponent();
            format!("decimal({unscaled},{scale})")
        }),
        DataType::Float => opt_text(row.get_float(name), |value| {
            format!("float({:08x})", value.to_bits())
        }),
        DataType::Double => opt_text(row.get_double(name), |value| {
            format!("double({:016x})", value.to_bits())
        }),
        DataType::ShortDateTime | DataType::ExtDateTime => {
            opt_text(row.get_local_date_time(name), |value| {
                date_time_text(&value)
            })
        }
        DataType::Text | DataType::Memo | DataType::Guid => {
            opt_text(row.get_string(name), |value| {
                format!("text({})", escape(&value))
            })
        }
        DataType::Binary | DataType::Ole | DataType::BigBinary => {
            opt_text(row.get_bytes(name), |value| {
                format!("bytes({})", hex(&value))
            })
        }
        DataType::ComplexType => opt_text(row.get_foreign_key(name), |value| {
            format!("complex({value})")
        }),
        _ => format_value(data_type, row.get(name).unwrap_or(&Value::Null), tz),
    }
}

fn opt_text<T>(result: jackcess::Result<Option<T>>, format: impl FnOnce(T) -> String) -> String {
    match result {
        Ok(Some(value)) => format(value),
        Ok(None) => "null".to_string(),
        Err(_) => "err".to_string(),
    }
}

fn blob_text(row: &Row, name: &str) -> String {
    match row.get_blob(name) {
        Ok(Some(blob)) => match blob.content() {
            Ok(content) => format!("blob({},{})", blob.length(), content.content_type().name()),
            Err(_) => "err".to_string(),
        },
        Ok(None) => "null".to_string(),
        Err(_) => "err".to_string(),
    }
}

fn date_text(row: &Row, name: &str, tz: &TimeZone) -> String {
    match row.get_date(name) {
        Ok(Some(bits)) => format!("datemillis({})", datenums::from_date_double(bits, tz)),
        Ok(None) => "null".to_string(),
        Err(_) => "err".to_string(),
    }
}

// ---------------------------------------------------------------------------
// the generated scenario
// ---------------------------------------------------------------------------

fn gen_mode(path: &Path, format_name: &str, now_ms: Option<i64>) -> jackcess::Result<()> {
    let format = FileFormat::from_name(format_name)
        .ok_or_else(|| jackcess::Error::IllegalArgument(format!("unknown format {format_name}")))?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(format, path)?;
    db.now_ms = now_ms;
    db.now_queue = now_ms.into_iter().collect();
    let base_ms = DATE_BASE_MS;
    let offset = db.tz.raw_offset_ms;

    make_row_test(&mut db)?;
    for i in 0..3 {
        db.add_row_from_map("RowTest", &type_row(i, base_ms, offset))?;
    }

    // update a row returned by the table iterator (Table.updateRow(Row))
    let mut alpha = find_by_name(&mut db, "RowTest", "alpha")?;
    alpha.insert("Name", Value::Text("alpha2".to_string()));
    alpha.insert("Note", Value::Text(repeat("long note ", 40)));
    alpha.insert("Flag", Value::Bool(false));
    let updated = db.update_row_object("RowTest", alpha)?;
    println!(
        "GEN=UPDATE|{}|{}|note={}",
        row_id_text(&updated),
        updated.get_string("Name")?.unwrap_or_default(),
        updated
            .get_string("Note")?
            .map(|text| text.len())
            .unwrap_or(0)
    );

    // delete a row returned by the table iterator (Table.deleteRow(Row))
    let beta = find_by_name(&mut db, "RowTest", "beta")?;
    let beta_id = row_id_text(&beta);
    db.delete_row_object("RowTest", beta)?;
    println!("GEN=DELETE|{}|beta", beta_id);

    // two rows through addRowsFromMaps (the batch map path)
    let mut delta = type_row(3, base_ms, offset);
    delta.insert("Name".to_string(), Value::Text("delta".to_string()));
    let mut epsilon = type_row(4, base_ms, offset);
    epsilon.insert("Name".to_string(), Value::Text("epsilon".to_string()));
    let added = db.add_rows_from_maps("RowTest", &[delta, epsilon])?;
    let added_text = added
        .iter()
        .map(|row| {
            format!(
                "{}:{}",
                row_id_text(row),
                row.get_string("Name").ok().flatten().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    println!("GEN=ADDMAPS={}", added_text);

    // cursor iteration (Cursor implements Iterable<Row>)
    let mut cursor = TableScanCursor::new(&mut db, "RowTest")?;
    let iterated = cursor.stream(&mut db)?;
    println!("GEN=CURSORITER={}", id_name_list(&iterated));

    cursor.before_first();
    let next = cursor
        .get_next_row_object(&mut db)?
        .ok_or_else(|| missing_row("next"))?;
    println!(
        "GEN=NEXT={}:{}",
        row_id_text(&next),
        next.get_string("Name")?.unwrap_or_default()
    );
    let next_cols = cursor
        .get_next_row_object_columns(&mut db, &["Name"])?
        .ok_or_else(|| missing_row("next columns"))?;
    println!(
        "GEN=NEXTCOLS={}",
        next_cols.get_string("Name")?.unwrap_or_default()
    );
    let current = cursor.get_current_row_object(&mut db)?;
    println!(
        "GEN=CUR={}:{}",
        row_id_text(&current),
        current.get_string("Name")?.unwrap_or_default()
    );
    let current_cols = cursor.get_current_row_object_columns(&mut db, &["Name"])?;
    println!(
        "GEN=CURCOLS={}:{}",
        current_cols.len(),
        current_cols.get_string("Name")?.unwrap_or_default()
    );

    let mut pattern = BTreeMap::new();
    pattern.insert("Name".to_string(), Value::Text("delta".to_string()));
    let found = cursor.find_first_row(&mut db, &pattern)?;
    if found {
        let found_row = cursor.get_current_row_object(&mut db)?;
        println!(
            "GEN=FIND=true|{}:{}",
            row_id_text(&found_row),
            found_row.get_string("Name")?.unwrap_or_default()
        );
    } else {
        println!("GEN=FIND=false|null");
    }
    println!(
        "GEN=MATCH={}",
        found && cursor.current_row_matches(&mut db, &pattern)?
    );

    let mut change = Row::empty();
    change.insert("Note", Value::Text("cursor note".to_string()));
    let changed = cursor.update_current_row_object(&mut db, &change)?;
    println!(
        "GEN=CURUPD={}:{}",
        row_id_text(&changed),
        changed.get_string("Note")?.unwrap_or_default()
    );

    let builder = cursor
        .new_iterable()
        .set_column_names(vec!["Name".to_string()]);
    let iterated = builder.rows_object(&mut cursor, &mut db)?;
    println!(
        "GEN=ITER={}",
        iterated
            .iter()
            .map(|row| row.get_string("Name").ok().flatten().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(",")
    );
    println!("GEN=STREAM={}", cursor.stream(&mut db)?.len());

    // asRow / asUpdateRow (positional conversions)
    let mut as_row_map = BTreeMap::new();
    as_row_map.insert("Name".to_string(), Value::Text("asrow".to_string()));
    as_row_map.insert("Small".to_string(), Value::Int(7));
    let (columns, values) = {
        let table = db.get_table("RowTest")?;
        let columns: Vec<(String, DataType)> = table
            .get_columns()
            .iter()
            .map(|column| (column.name.clone(), column.data_type))
            .collect();
        (columns, table.as_row(&as_row_map)?)
    };
    println!("GEN=ASROW={}", positional_dump(&columns, &values, &tz(&db)));
    let (columns, values) = {
        let table = db.get_table("RowTest")?;
        let columns: Vec<(String, DataType)> = table
            .get_columns()
            .iter()
            .map(|column| (column.name.clone(), column.data_type))
            .collect();
        (columns, table.as_update_row(&as_row_map)?)
    };
    println!(
        "GEN=ASUPDROW={}",
        positional_dump(&columns, &values, &tz(&db))
    );

    // table iteration after all modifications
    let rows = db.rows("RowTest")?;
    println!("GEN=TITER={}", id_name_list(&rows));

    db.flush()?;
    drop(db);

    let mut reopened = Database::open_path(path)?;
    dump(&mut reopened)
}

fn tz(db: &Database) -> TimeZone {
    *db.get_time_zone()
}

fn missing_row(what: &str) -> jackcess::Error {
    jackcess::Error::IllegalState(format!("the {what} row does not exist"))
}

fn make_row_test(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "RowTest",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(40),
            ColumnSpec::new("Tiny", DataType::Byte),
            ColumnSpec::new("Small", DataType::Int),
            ColumnSpec::new("Single", DataType::Float),
            ColumnSpec::new("Double", DataType::Double),
            ColumnSpec::new("Amount", DataType::Numeric)
                .with_precision(18)
                .with_scale(4),
            ColumnSpec::new("Flag", DataType::Boolean),
            ColumnSpec::new("When", DataType::ShortDateTime),
            ColumnSpec::new("Data", DataType::Binary).with_length(10),
            ColumnSpec::new("Blob", DataType::Ole),
            ColumnSpec::new("Note", DataType::Memo),
            ColumnSpec::new("Guid", DataType::Guid),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
    .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique());
    db.create_table(&spec)?;
    Ok(())
}

fn type_row(i: usize, base_ms: i64, offset: i64) -> BTreeMap<String, Value> {
    let names = ["alpha", "beta", "gamma", "rows4", "rows5"];
    let mut row = BTreeMap::new();
    row.insert("Name".to_string(), Value::Text(names[i].to_string()));
    row.insert("Tiny".to_string(), Value::Byte(7));
    row.insert("Small".to_string(), Value::Int(100 + i as i16));
    row.insert("Single".to_string(), Value::Float(1.25 * (i + 1) as f32));
    row.insert("Double".to_string(), Value::Double(2.5 * (i + 1) as f64));
    row.insert(
        "Amount".to_string(),
        Value::Decimal {
            unscaled: 123_4500 + i as i128 * 10000,
            scale: 4,
        },
    );
    row.insert("Flag".to_string(), Value::Bool(i.is_multiple_of(2)));
    row.insert(
        "When".to_string(),
        Value::DateTime(LocalDateTime::from_epoch_millis(
            base_ms + i as i64 * 86_400_000 + offset,
        )),
    );
    row.insert("Data".to_string(), Value::Bytes(vec![1, 2, 3, i as u8]));
    row.insert("Blob".to_string(), Value::Bytes(blob_bytes(i)));
    row.insert(
        "Note".to_string(),
        if i == 2 {
            Value::Null
        } else {
            Value::Text(note_text(i))
        },
    );
    row.insert(
        "Guid".to_string(),
        Value::Text("{01234567-89AB-CDEF-0123-456789ABCDEF}".to_string()),
    );
    row
}

fn blob_bytes(i: usize) -> Vec<u8> {
    (0..16).map(|j| ((i * 16 + j) & 0xFF) as u8).collect()
}

fn note_text(i: usize) -> String {
    if i == 0 {
        "memo alpha".to_string()
    } else {
        repeat(&format!("memo {} ", i), 80)
    }
}

fn repeat(text: &str, count: usize) -> String {
    text.repeat(count)
}

fn find_by_name(db: &mut Database, table: &str, name: &str) -> jackcess::Result<Row> {
    for row in db.rows(table)? {
        if row.get_string("Name")?.as_deref() == Some(name) {
            return Ok(row);
        }
    }
    Err(jackcess::Error::IllegalState(format!(
        "row {name} not found in table {table}"
    )))
}

fn id_name_list(rows: &[Row]) -> String {
    rows.iter()
        .map(|row| {
            format!(
                "{}:{}",
                row_id_text(row),
                row.get_string("Name").ok().flatten().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn positional_dump(columns: &[(String, DataType)], values: &[Value], tz: &TimeZone) -> String {
    columns
        .iter()
        .enumerate()
        .map(|(i, (_, data_type))| {
            let value = values.get(i).unwrap_or(&Value::Null);
            if value.is_keep() {
                "keep".to_string()
            } else {
                format_value(*data_type, value, tz)
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}
