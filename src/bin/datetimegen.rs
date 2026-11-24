//! `datetimegen` - verification harness for `Database.DateTimeType` and
//! `Table.ColumnOrder`.
//!
//! It mirrors `java-src/DateTimeGen.java` line for line, so the two programs
//! can be compared with a plain text diff:
//!
//! ```text
//! datetimegen dump <db> <DATE|LOCAL_DATE_TIME> <DATA|DISPLAY> <label>
//! datetimegen write <out> <V2003|V2007|V2010|V2016> <DATE|LOCAL_DATE_TIME> [--now-ms N] [--clean]
//! datetimegen order <out> <format> [--now-ms N]
//! datetimegen props <db> <DATE|LOCAL_DATE_TIME> <label>
//! datetimegen propblob <db> <DATE|LOCAL_DATE_TIME> <label>
//! datetimegen lookup <out> <format> <DATE|LOCAL_DATE_TIME> [--now-ms N]
//! datetimegen eq <out> <format> [--now-ms N]
//! ```
//!
//! **dump** prints, for every user table, the column list in the configured
//! `Table.ColumnOrder` (`name`, `number`, `id`, the data order `index` and the
//! `display` index, the data type) and every row value as
//! `kind=...,class=...,text=...`.
//!
//! The `class` field names the Java class the port's value stands for, which
//! is why both programs print the same text for the same stored value:
//! [`Value::DateBits`] is jackcess' `ColumnImpl.DateExt` (a `java.util.Date`
//! which keeps the stored date bits) and [`Value::DateTime`] is
//! `java.time.LocalDateTime`.  `kind` is the real cross-check: it is derived
//! from the value's representation (`date` for `DateBits`, `ldt` for
//! `DateTime`, ...), and the `text` of a date carries both the
//! `java.util.Date.getTime()` milliseconds and the exact stored bits, so the
//! comparison covers the conversion *and* the byte preserving round trip.
//!
//! **write** creates the database the Java oracle creates (same table, same
//! index, same rows, same catalog timestamps) so `tools/verify-datetime.ps1`
//! can compare the two files byte for byte.  It also prints the values read
//! back, which is compared as text.
//!
//! **order** creates a table whose columns carry a reversed `ColumnOrder`
//! column property.  jackcess 5.0.4 does not read that property for
//! `ColumnImpl.getDisplayIndex` (the display index is the load order), which
//! the dump of the two files demonstrates.
//!
//! **props** prints, for every user table, the catalog created/updated dates
//! and every `SHORT_DATE_TIME`/`EXT_DATE_TIME` property value, using the
//! database's property map plumbing.  **propblob** parses and re-encodes each
//! table's raw `LvProp` blob with the database's date/time context instead,
//! printing the date valued properties and the re-encoded bytes, so the codec
//! is compared with Java independently of the catalog plumbing.
//!
//! **lookup** creates an indexed date column in the given mode and searches
//! it with sub-millisecond `LocalDateTime` values, `java.util.Date` instants
//! and values read back from the table, under both `DateTimeType` settings,
//! printing the matching row ids.  It covers the index-key conversion of the
//! `DATE` factory (which truncates a sub-millisecond value) and the "noisy
//! stored double" where a row/index written in `LOCAL_DATE_TIME` mode holds
//! sub-millisecond bits which a `DATE` search key no longer matches.
//!
//! **eq** creates date values with different stored bits but the same
//! millisecond and compares the values read back in both modes, mirroring
//! `java.util.Date.equals` (which compares `getTime()`) against the
//! `LocalDateTime` representation.

use std::path::Path;

use jackcess::column::{self, ColumnSpec, DateContext};
use jackcess::cursor::{Cursor, IndexCursor, IndexRange};
use jackcess::database::{ColumnOrder, Database, DateTimeType};
use jackcess::datatype::DataType;
use jackcess::datenums::{LocalDateTime, TimeZone};
use jackcess::error::{Error, Result};
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

/// 2020-03-15T10:30:45Z, the instant the other harnesses use.
const DATE_BASE_MS: i64 = 1_584_246_645_000;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let result = match args[1].as_str() {
        "dump" => {
            if args.len() < 6 {
                usage();
            }
            dump(
                Path::new(&args[2]),
                DateTimeType::from_name(&args[3]),
                ColumnOrder::from_name(&args[4]),
                &args[5],
            )
        }
        "write" => {
            if args.len() < 5 {
                usage();
            }
            write(
                Path::new(&args[2]),
                FileFormat::from_name(&args[3]).unwrap_or(FileFormat::V2003),
                DateTimeType::from_name(&args[4]),
                now_ms(&args[5..]),
                args[5..].iter().any(|a| a == "clean" || a == "--clean"),
            )
        }
        "order" => {
            if args.len() < 4 {
                usage();
            }
            order(
                Path::new(&args[2]),
                FileFormat::from_name(&args[3]).unwrap_or(FileFormat::V2003),
                now_ms(&args[4..]),
            )
        }
        "props" => {
            if args.len() < 5 {
                usage();
            }
            props(
                Path::new(&args[2]),
                DateTimeType::from_name(&args[3]),
                &args[4],
            )
        }
        "propblob" => {
            if args.len() < 5 {
                usage();
            }
            propblob(
                Path::new(&args[2]),
                DateTimeType::from_name(&args[3]),
                &args[4],
            )
        }
        "lookup" => {
            if args.len() < 5 {
                usage();
            }
            lookup(
                Path::new(&args[2]),
                FileFormat::from_name(&args[3]).unwrap_or(FileFormat::V2003),
                DateTimeType::from_name(&args[4]),
                now_ms(&args[5..]),
            )
        }
        "eq" => {
            if args.len() < 4 {
                usage();
            }
            eq(
                Path::new(&args[2]),
                FileFormat::from_name(&args[3]).unwrap_or(FileFormat::V2003),
                now_ms(&args[4..]),
            )
        }
        other => Err(Error::IllegalArgument(format!("unknown command {other}"))),
    };
    match result {
        Ok(()) => {}
        Err(e) => {
            eprintln!("datetimegen failed: {e}");
            std::process::exit(1);
        }
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: datetimegen dump <db> <DATE|LOCAL_DATE_TIME> <DATA|DISPLAY> <label>\n\
         \x20      datetimegen write <out> <format> <DATE|LOCAL_DATE_TIME> [--now-ms N] [--clean]\n\
         \x20      datetimegen order <out> <format> [--now-ms N]\n\
         \x20      datetimegen props <db> <DATE|LOCAL_DATE_TIME> <label>\n\
         \x20      datetimegen propblob <db> <DATE|LOCAL_DATE_TIME> <label>\n\
         \x20      datetimegen lookup <out> <format> <DATE|LOCAL_DATE_TIME> [--now-ms N]\n\
         \x20      datetimegen eq <out> <format> [--now-ms N]"
    );
    std::process::exit(2);
}

fn now_ms(args: &[String]) -> Option<i64> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--now-ms" {
            return args.get(i + 1).and_then(|s| s.trim().parse::<i64>().ok());
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// dump
// ---------------------------------------------------------------------------

fn dump(
    path: &Path,
    date_time_type: DateTimeType,
    column_order: ColumnOrder,
    label: &str,
) -> Result<()> {
    let mut db = Database::open_path(path)?;
    // both settings are applied before any table is loaded: the date type is
    // read from the database on every conversion and the column order is
    // applied while a table is constructed (Java's `TableImpl` constructor)
    db.set_date_time_type(date_time_type);
    db.set_column_order(column_order);
    let tz = db.tz;
    println!(
        "db label={label} type={} order={}",
        date_time_type.name(),
        column_order.name()
    );

    // `Database.getTableNames()`: the user tables, sorted with
    // `String.CASE_INSENSITIVE_ORDER`
    let mut names: Vec<String> = Vec::new();
    for name in db.get_table_names()? {
        // a linked table's rows live in the linkee database
        if db.linked_tables.contains_key(&name.to_uppercase()) {
            continue;
        }
        match db.find_table(&name) {
            Ok(Some(table)) if !table.is_system() && !names.contains(&table.name) => {
                names.push(table.name.clone());
            }
            _ => {}
        }
    }
    names.sort_by(|a, b| {
        a.to_uppercase()
            .cmp(&b.to_uppercase())
            .then_with(|| a.cmp(b))
    });

    for name in names {
        let page = match db.find_table(&name) {
            Ok(Some(table)) => table.table_def_page_number,
            _ => continue,
        };
        let row_count = db.tables.get(&page).map(|t| t.row_count).unwrap_or(0);
        let rows = db.read_table_rows(page)?;
        let table = db.tables.get(&page).unwrap();
        println!(
            "table={} rows={} cols={}",
            table.name,
            row_count,
            table.columns.len()
        );
        for (i, c) in table.get_columns().iter().enumerate() {
            println!(
                "column={},name={},number={},id={},index={},display={},type={}",
                i,
                escape(&c.name),
                c.column_number,
                c.column_id,
                c.column_index,
                c.display_index,
                c.data_type.get_name()
            );
        }
        for (row_number, row) in rows.iter().enumerate() {
            let values = match row {
                Some(values) => values,
                None => continue,
            };
            println!("row={row_number}");
            for c in table.get_columns() {
                let value = values.get(c.column_index).cloned().unwrap_or(Value::Null);
                println!("value={},{}", escape(&c.name), describe(&value, &tz));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

fn write(
    path: &Path,
    format: FileFormat,
    date_time_type: DateTimeType,
    now: Option<i64>,
    clean_only: bool,
) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(format, path)?;
    db.set_date_time_type(date_time_type);
    db.now_ms = now;
    db.now_queue = now.into_iter().collect();

    let spec = TableSpec::new(
        "DT",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("D1", DataType::ShortDateTime),
            ColumnSpec::new("D2", DataType::ShortDateTime),
            ColumnSpec::new("D3", DataType::ShortDateTime),
            ColumnSpec::new("Label", DataType::Text).with_length(20),
        ],
    )
    .with_index(IndexSpec::new("D1Index", vec![("D1".to_string(), true)]));
    let page = db.create_table(&spec)?;
    // custom date properties, mirroring `DateTimeGen.putDateProperties`
    put_date_properties(&mut db, clean_only)?;

    // the rows `java-src/DateTimeGen.java` writes, in the same order
    for row in first_rows(!clean_only) {
        db.add_rows("DT", std::slice::from_ref(&row))?;
    }

    // read values back and write one of each again: in DATE mode the read
    // value is the `DateExt` analogue and is written with its original bits, so
    // the third row reproduces the stored bytes exactly
    let read_back = db.read_table_rows(page)?;
    let row0 = read_back
        .first()
        .and_then(|r| r.clone())
        .ok_or_else(|| Error::Jackcess("row 0 missing".to_string()))?;
    let third_d1 = if clean_only {
        row0[3].clone()
    } else {
        read_back
            .get(1)
            .and_then(|r| r.clone())
            .and_then(|row| row.get(1).cloned())
            .ok_or_else(|| Error::Jackcess("row 1 missing".to_string()))?
    };
    let reread = vec![
        Value::Long(3),
        row0[1].clone(),
        row0[2].clone(),
        third_d1,
        Value::Text("reread".to_string()),
    ];
    db.add_rows("DT", &[reread])?;
    let nulls = vec![
        Value::Long(4),
        Value::Null,
        Value::BigInt(DATE_BASE_MS + 86_400_000),
        Value::BigInt(-1),
        Value::Text("nulls".to_string()),
    ];
    db.add_rows("DT", &[nulls])?;
    if !clean_only {
        // an ancient date, through the conversion each mode defines for it
        // (see `java-src/DateTimeGen.java`): a `java.util.Date` instant in DATE
        // mode, a `LocalDateTime` in LOCAL_DATE_TIME mode
        let ancient = match date_time_type {
            DateTimeType::Date => Value::BigInt(-12_219_292_800_000),
            DateTimeType::LocalDateTime => {
                Value::DateTime(LocalDateTime::new(1582, 10, 15, 0, 0, 0, 0))
            }
        };
        let ancient_row = vec![
            Value::Long(5),
            ancient,
            Value::Null,
            Value::Null,
            Value::Text("ancient".to_string()),
        ];
        db.add_rows("DT", &[ancient_row])?;
        // and read it back, so the original date bits are written again
        let read_back = db.read_table_rows(page)?;
        let ancient_read = read_back
            .get(4)
            .and_then(|r| r.clone())
            .and_then(|row| row.get(1).cloned())
            .ok_or_else(|| Error::Jackcess("ancient row missing".to_string()))?;
        let ancient_reread = vec![
            Value::Long(6),
            ancient_read,
            Value::Null,
            Value::Null,
            Value::Text("ancientre".to_string()),
        ];
        db.add_rows("DT", &[ancient_reread])?;
    }

    let tz = db.tz;
    let rows = db.read_table_rows(page)?;
    let table = db.tables.get(&page).unwrap();
    println!("write table=DT type={}", date_time_type.name());
    for (row_number, row) in rows.iter().enumerate() {
        let values = match row {
            Some(values) => values,
            None => continue,
        };
        println!("row={row_number}");
        for c in table.get_columns() {
            let value = values.get(c.column_index).cloned().unwrap_or(Value::Null);
            println!("value={},{}", escape(&c.name), describe(&value, &tz));
        }
    }
    // the Java oracle reads the catalog timestamp back; the port was given the
    // same value, so printing it makes the two outputs comparable
    let ms = now.unwrap_or(-1);
    println!("nowms={ms}");
    db.flush()
}

/// The rows the Java oracle writes, in column (data) order.  `full` adds the
/// sub-millisecond and ancient value rows; without them every value is a whole
/// millisecond inside the range of the fixed offset zone model, so the two
/// DateTimeType modes must produce the same bytes.
fn first_rows(full: bool) -> Vec<Vec<Value>> {
    let mut rows = vec![vec![
        Value::Long(1),
        Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0)),
        Value::BigInt(DATE_BASE_MS),
        Value::DateTime(LocalDateTime::new(1999, 12, 31, 23, 59, 59, 123_000_000)),
        Value::Text("clean".to_string()),
    ]];
    if full {
        rows.push(vec![
            Value::Long(2),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789)),
            Value::BigInt(0),
            // 1910-01-01T00:00:00Z as a java.util.Date instant: a negative
            // value inside the range where the fixed offset zone model agrees
            // with Java's ZoneId rules
            Value::BigInt(-1_893_456_000_000),
            Value::Text("subms".to_string()),
        ]);
    }
    rows
}

// ---------------------------------------------------------------------------
// order
// ---------------------------------------------------------------------------

fn order(path: &Path, format: FileFormat, now: Option<i64>) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let names = ["A", "B", "C", "D", "E"];
    {
        let mut db = Database::create(format, path)?;
        db.now_ms = now;
        db.now_queue = now.into_iter().collect();
        let mut columns = Vec::new();
        for (i, name) in names.iter().enumerate() {
            columns.push(
                ColumnSpec::new(*name, DataType::Long)
                    .put_property("ColumnOrder", Value::Int((names.len() - 1 - i) as i16))?,
            );
        }
        db.create_table(&TableSpec::new("Ord", columns))?;
        db.flush()?;
        println!("order table=Ord type={:?}", format);
    }
    // read the properties back, so the dump which follows shows that the
    // reversed order really is stored in the column property
    let mut db = Database::open_path(path)?;
    let maps = db.get_table_properties_by_name("Ord")?;
    for name in names {
        let value = maps.get(name).and_then(|map| map.get_value("ColumnOrder"));
        let text = match value {
            Some(Value::Int(v)) => v.to_string(),
            Some(Value::Byte(v)) => (*v as i8).to_string(),
            Some(Value::Long(v)) => v.to_string(),
            Some(Value::BigInt(v)) => v.to_string(),
            Some(Value::Null) | None => "null".to_string(),
            Some(other) => format!("{other:?}"),
        };
        println!("prop={name}={text}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// props
// ---------------------------------------------------------------------------

/// A date valued table and column property, in the order
/// `java-src/DateTimeGen.java#putDateProperties` adds them.
///
/// `get_table_properties_by_name` creates the (empty) column maps in column
/// order first, like Java's `TableImpl` does while loading the columns, so the
/// map order of the blob matches the Java oracle.
fn put_date_properties(db: &mut Database, clean_only: bool) -> Result<()> {
    let mut maps = db.get_table_properties_by_name("DT")?;
    {
        let default = maps.get_default();
        default.put_value(
            "CleanDate",
            Value::DateTime(LocalDateTime::new(1999, 12, 31, 23, 59, 59, 123_000_000)),
        )?;
        default.put_value_typed(
            "InstantDate",
            DataType::ShortDateTime,
            Value::BigInt(DATE_BASE_MS),
        );
        if !clean_only {
            default.put_value(
                "SubMsDate",
                Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789)),
            )?;
        }
    }
    {
        let column = maps.get_mut("D1");
        column.put_value(
            "ColCleanDate",
            Value::DateTime(LocalDateTime::new(2001, 2, 3, 4, 5, 6, 7_000_000)),
        )?;
        column.put_value_typed(
            "ColInstantDate",
            DataType::ShortDateTime,
            Value::BigInt(DATE_BASE_MS + 1),
        );
        if !clean_only {
            column.put_value(
                "ColSubMsDate",
                Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 987_654_321)),
            )?;
        }
    }
    maps.save(db)
}

fn map_kind(map_type: i16) -> String {
    match map_type {
        0 => "default".to_string(),
        1 => "column".to_string(),
        2 => "index".to_string(),
        other => format!("other{other}"),
    }
}

fn date_text(value: Option<LocalDateTime>) -> String {
    match value {
        Some(ldt) => column::java_local_date_time_text(&ldt),
        None => "-".to_string(),
    }
}

/// Dumps the created/updated dates and every date valued property of every
/// user table, like `DateTimeGen.props`.
fn props(path: &Path, date_time_type: DateTimeType, label: &str) -> Result<()> {
    let mut db = Database::open_path(path)?;
    db.set_date_time_type(date_time_type);
    let tz = db.tz;
    println!("props label={label} type={}", date_time_type.name());

    let mut names: Vec<String> = Vec::new();
    for name in db.get_table_names()? {
        if db.linked_tables.contains_key(&name.to_uppercase()) {
            continue;
        }
        match db.find_table(&name) {
            Ok(Some(table)) if !table.is_system() && !names.contains(&table.name) => {
                names.push(table.name.clone());
            }
            _ => {}
        }
    }
    names.sort_by(|a, b| {
        a.to_uppercase()
            .cmp(&b.to_uppercase())
            .then_with(|| a.cmp(b))
    });

    for name in names {
        let page = match db.find_table(&name) {
            Ok(Some(table)) => table.table_def_page_number,
            _ => continue,
        };
        let (created, updated, table_name) = {
            let table = match db.tables.get(&page) {
                Some(table) => table,
                None => continue,
            };
            (
                table.get_created_date(),
                table.get_updated_date(),
                table.name.clone(),
            )
        };
        println!(
            "table={} created={} updated={}",
            escape(&table_name),
            date_text(created),
            date_text(updated)
        );
        let maps = db.get_table_properties_by_name(&name)?;
        for map in maps.iter() {
            let kind = map_kind(map.map_type());
            for prop in map.iter() {
                if matches!(
                    prop.data_type,
                    DataType::ShortDateTime | DataType::ExtDateTime
                ) {
                    println!(
                        "prop={}|{}|{}|{}|{}|{}",
                        escape(&table_name),
                        kind,
                        escape(map.name()),
                        escape(&prop.name),
                        prop.data_type.get_name(),
                        describe(&prop.value, &tz)
                    );
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// propblob: parse and re-encode each table's LvProp blob in the current
// DateTimeType mode (independent of the table property map plumbing)
// ---------------------------------------------------------------------------

/// Reads the raw `LvProp` blob of every user table, parses it with the
/// database's [`DateContext`] and re-encodes it, printing the date valued
/// properties and the re-encoded bytes (`DateTimeGen.propblob`).
fn propblob(path: &Path, date_time_type: DateTimeType, label: &str) -> Result<()> {
    let mut db = Database::open_path(path)?;
    db.set_date_time_type(date_time_type);
    let context = DateContext::new(db.date_time_type, db.tz);
    let tz = db.tz;
    println!("propblob label={label} type={}", date_time_type.name());

    let rows = db.read_rows_with_ids("MSysObjects")?;
    let (idx_name, idx_props) = {
        let cat = db
            .tables
            .get(&jackcess::database::PAGE_SYSTEM_CATALOG)
            .ok_or_else(|| Error::Jackcess("system catalog is not loaded".into()))?;
        (cat.column_index("Name"), cat.column_index("LvProp"))
    };
    let mut blobs: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for (_, values) in rows {
        let values = match values {
            Some(values) => values,
            None => continue,
        };
        let name = idx_name
            .and_then(|i| values.get(i))
            .and_then(|v| v.as_text());
        let blob = idx_props
            .and_then(|i| values.get(i))
            .and_then(|v| v.as_bytes());
        if let (Some(name), Some(blob)) = (name, blob) {
            blobs.insert(name.to_string(), blob.to_vec());
        }
    }

    let mut names: Vec<String> = Vec::new();
    for name in db.get_table_names()? {
        if db.linked_tables.contains_key(&name.to_uppercase()) {
            continue;
        }
        match db.find_table(&name) {
            Ok(Some(table)) if !table.is_system() && !names.contains(&table.name) => {
                names.push(table.name.clone());
            }
            _ => {}
        }
    }
    names.sort_by(|a, b| {
        a.to_uppercase()
            .cmp(&b.to_uppercase())
            .then_with(|| a.cmp(b))
    });

    for name in names {
        let table_name = match db.find_table(&name) {
            Ok(Some(table)) => table.name.clone(),
            _ => continue,
        };
        let orig = match blobs.get(&table_name) {
            Some(blob) => blob,
            None => {
                println!("blob={}|roundtrip=-|hex=-", escape(&table_name));
                continue;
            }
        };
        let maps = jackcess::property::PropertyMaps::read_with_context(
            orig, &db.format, db.charset, context,
        )?;
        let re = maps.write_with_context(&db.format, db.charset, context)?;
        let same = re == *orig;
        println!(
            "blob={}|roundtrip={}|hex={}",
            escape(&table_name),
            same,
            if re.is_empty() {
                "-".to_string()
            } else {
                hex(&re)
            }
        );
        for map in maps.iter() {
            for prop in map.iter() {
                if matches!(
                    prop.data_type,
                    DataType::ShortDateTime | DataType::ExtDateTime
                ) {
                    println!(
                        "prop={}|{}|{}|{}|{}|{}",
                        escape(&table_name),
                        map_kind(map.map_type()),
                        escape(map.name()),
                        escape(&prop.name),
                        prop.data_type.get_name(),
                        describe(&prop.value, &tz)
                    );
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// lookup: index search keys under both DateTimeType settings
// ---------------------------------------------------------------------------

/// The ids of the rows of the given index range, in index order
/// (`CursorBuilder.setStartRow`/`setEndRow` on the single column index).
fn lookup_ids(db: &mut Database, key: &Value) -> Result<Vec<i64>> {
    let mut row = vec![Value::Null; 2];
    row[1] = key.clone();
    let mut range = IndexRange::new();
    range.set_start_row(row.clone());
    range.set_end_row(row);
    db.find_table("DTL")?;
    let mut cursor = IndexCursor::new_with_range(db, "DTL", "D1Index", &range)?;
    cursor.before_first();
    let mut ids = Vec::new();
    while let Some(values) = cursor.get_next_row(db)? {
        if let Some(id) = values.first().and_then(|v| v.as_i64()) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

fn join_ids(ids: &[i64]) -> String {
    if ids.is_empty() {
        return "-".to_string();
    }
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Creates the lookup fixture and searches it under both DateTimeType
/// settings, like `DateTimeGen.lookup`.
fn lookup(
    path: &Path,
    format: FileFormat,
    date_time_type: DateTimeType,
    now: Option<i64>,
) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(format, path)?;
    db.set_date_time_type(date_time_type);
    db.now_ms = now;
    db.now_queue = now.into_iter().collect();

    let spec = TableSpec::new(
        "DTL",
        vec![
            ColumnSpec::new("ID", DataType::Long),
            ColumnSpec::new("D1", DataType::ShortDateTime),
        ],
    )
    .with_index(IndexSpec::new("D1Index", vec![("D1".to_string(), true)]));
    let page = db.create_table(&spec)?;

    let sub_ms = LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789);
    let sub_ms_700 = LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_700);
    let whole_ms = LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000);
    let next_ms = LocalDateTime::new(2020, 3, 15, 12, 30, 45, 124_000_000);
    for row in [
        vec![Value::Long(1), Value::DateTime(sub_ms)],
        vec![Value::Long(2), Value::DateTime(whole_ms)],
        vec![Value::Long(3), Value::DateTime(next_ms)],
        vec![Value::Long(4), Value::BigInt(DATE_BASE_MS)],
        vec![Value::Long(5), Value::Null],
    ] {
        db.add_rows("DTL", &[row])?;
    }
    let reread = row_value_by_id(&mut db, page, 1)?;
    db.add_rows("DTL", &[vec![Value::Long(6), reread]])?;

    let tz = db.tz;
    println!("lookup table=DTL type={}", date_time_type.name());
    print_table_values(&mut db, page, &tz)?;

    let other = match date_time_type {
        DateTimeType::Date => DateTimeType::LocalDateTime,
        DateTimeType::LocalDateTime => DateTimeType::Date,
    };
    for search_type in [date_time_type, other] {
        db.set_date_time_type(search_type);
        let reread_probe = row_value_by_id(&mut db, page, 1)?;
        let probes = [
            ("subms", Value::DateTime(sub_ms)),
            ("subms700", Value::DateTime(sub_ms_700)),
            ("whole123", Value::DateTime(whole_ms)),
            ("ms124", Value::DateTime(next_ms)),
            ("instant", Value::BigInt(DATE_BASE_MS)),
            ("instant1", Value::BigInt(DATE_BASE_MS + 1)),
            ("reread", reread_probe),
        ];
        for (probe, value) in probes {
            println!(
                "find={},{},{}",
                search_type.name(),
                probe,
                join_ids(&lookup_ids(&mut db, &value)?)
            );
        }
    }
    println!("nowms={},{}", now.unwrap_or(-1), now.unwrap_or(-1));
    db.flush()
}

/// The D1 value of the row whose ID is the given one, read under the table's
/// current DateTimeType.
fn row_value_by_id(db: &mut Database, page: i32, id: i64) -> Result<Value> {
    let rows = db.read_table_rows(page)?;
    for row in rows.into_iter().flatten() {
        if row.first().and_then(|v| v.as_i64()) == Some(id) {
            return Ok(row.get(1).cloned().unwrap_or(Value::Null));
        }
    }
    Err(Error::Jackcess(format!("no row with ID {id}")))
}

fn print_table_values(db: &mut Database, page: i32, tz: &TimeZone) -> Result<()> {
    let rows = db.read_table_rows(page)?;
    for row in rows.into_iter().flatten() {
        let id = row.first().and_then(|v| v.as_i64()).unwrap_or(-1);
        let value = row.get(1).cloned().unwrap_or(Value::Null);
        println!("value={},{}", id, describe(&value, tz));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// eq: java.util.Date.equals vs DateExt equality
// ---------------------------------------------------------------------------

fn bits_text(value: &Value) -> String {
    match value {
        Value::DateBits(bits) => format!("{:016x}", bits.to_bits()),
        _ => "-".to_string(),
    }
}

fn eq_case(label: &str, a: &Value, b: &Value, tz: &TimeZone) {
    let bits_a = bits_text(a);
    let bits_b = bits_text(b);
    let bits_equal = if bits_a == "-" || bits_b == "-" {
        "-".to_string()
    } else {
        (bits_a == bits_b).to_string()
    };
    println!(
        "eq pair={},a={},b={},bitsEqual={},equals={}",
        label,
        describe(a, tz),
        describe(b, tz),
        bits_equal,
        a == b
    );
}

/// Creates the equality fixture, like `DateTimeGen.eq`.
fn eq(path: &Path, format: FileFormat, now: Option<i64>) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(format, path)?;
    db.set_date_time_type(DateTimeType::LocalDateTime);
    db.now_ms = now;
    db.now_queue = now.into_iter().collect();

    let spec = TableSpec::new(
        "EQ",
        vec![
            ColumnSpec::new("ID", DataType::Long),
            ColumnSpec::new("D1", DataType::ShortDateTime),
            ColumnSpec::new("D2", DataType::ShortDateTime),
            ColumnSpec::new("D3", DataType::ShortDateTime),
            ColumnSpec::new("D4", DataType::ShortDateTime),
        ],
    );
    let page = db.create_table(&spec)?;
    db.add_rows(
        "EQ",
        &[vec![
            Value::Long(1),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789)),
            // 123.4ms: the same millisecond as D1, a different OLE double
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_400_000)),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000)),
            Value::BigInt(DATE_BASE_MS),
        ]],
    )?;
    db.add_rows(
        "EQ",
        &[vec![
            Value::Long(2),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 124_456_789)),
            Value::Null,
            Value::Null,
            Value::Null,
        ]],
    )?;

    println!("eq table=EQ");
    let tz = db.tz;
    for date_type in [DateTimeType::Date, DateTimeType::LocalDateTime] {
        db.set_date_time_type(date_type);
        let rows = db.read_table_rows(page)?;
        let row1 = rows.first().and_then(|r| r.clone()).unwrap_or_default();
        let row2 = rows.get(1).and_then(|r| r.clone()).unwrap_or_default();
        println!("eq under={}", date_type.name());
        eq_case("row1.D1==row1.D2", &row1[1], &row1[2], &tz);
        eq_case("row1.D1==row1.D3", &row1[1], &row1[3], &tz);
        eq_case("row1.D1==row2.D1", &row1[1], &row2[1], &tz);
        eq_case("row1.D3==row2.D1", &row1[3], &row2[1], &tz);
        eq_case("row1.D1==row1.D1", &row1[1], &row1[1], &tz);
        eq_case("row2.D2==row2.D3(null)", &row2[2], &row2[3], &tz);
        eq_case("row1.D4==row1.D4", &row1[4], &row1[4], &tz);
    }
    db.set_date_time_type(DateTimeType::Date);
    let date_rows = db.read_table_rows(page)?;
    db.set_date_time_type(DateTimeType::LocalDateTime);
    let ldt_rows = db.read_table_rows(page)?;
    let date_row1 = date_rows
        .first()
        .and_then(|r| r.clone())
        .unwrap_or_default();
    let ldt_row1 = ldt_rows.first().and_then(|r| r.clone()).unwrap_or_default();
    eq_case(
        "date.row1.D1==ldt.row1.D1",
        &date_row1[1],
        &ldt_row1[1],
        &tz,
    );
    println!("nowms={},{}", now.unwrap_or(-1), now.unwrap_or(-1));
    db.flush()
}

fn describe(value: &Value, tz: &TimeZone) -> String {
    match value {
        Value::Null => "kind=null,class=-,text=null".to_string(),
        Value::DateBits(bits) => format!(
            "kind=date,class=java.util.Date,text=date(millis={};bits={:016x})",
            column::date_ext_millis(*bits, tz),
            bits.to_bits()
        ),
        Value::DateTime(ldt) => format!(
            "kind=ldt,class=java.time.LocalDateTime,text={}",
            column::java_local_date_time_text(ldt)
        ),
        Value::Text(text) => format!(
            "kind=string,class=java.lang.String,text=\"{}\"",
            escape(text)
        ),
        Value::Bool(b) => format!("kind=bool,class=java.lang.Boolean,text={b}"),
        Value::Decimal { unscaled, scale } => {
            format!("kind=decimal,class=java.math.BigDecimal,text=dec({unscaled},{scale})")
        }
        Value::Byte(v) => format!("kind=number,class=java.lang.Byte,text={}", *v as i8),
        Value::Int(v) => format!("kind=number,class=java.lang.Short,text={v}"),
        Value::Long(v) => format!("kind=number,class=java.lang.Integer,text={v}"),
        Value::BigInt(v) => format!("kind=number,class=java.lang.Long,text={v}"),
        Value::Float(v) => format!(
            "kind=number,class=java.lang.Float,text={}",
            java_float_text(*v)
        ),
        Value::Double(v) => format!(
            "kind=number,class=java.lang.Double,text={}",
            java_double_text(*v)
        ),
        Value::Bytes(data) => format!("kind=bytes,class=[B,text=bytes({})", hex(data)),
        Value::Uuid(uuid) => format!(
            "kind=uuid,class=java.util.UUID,text=uuid({})",
            uuid_text(uuid)
        ),
        Value::Keep => "kind=other,class=KEEP_VALUE,text=-".to_string(),
    }
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `java.util.UUID.toString` (lower case, braced-free canonical form).
fn uuid_text(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `Double.toString` for the values this harness writes (it prints Java's plain
/// decimal form for every fixture value; a value which Java would print in
/// scientific notation is not used here).
fn java_double_text(v: f64) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    let text = format!("{v}");
    if text.contains('.') || text.contains('e') || text.contains('E') {
        text
    } else {
        format!("{text}.0")
    }
}

fn java_float_text(v: f32) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    let text = format!("{v}");
    if text.contains('.') || text.contains('e') || text.contains('E') {
        text
    } else {
        format!("{text}.0")
    }
}

/// `DateTimeGen.escape`: printable ASCII, every other character as `\uXXXX`.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{20}'..='\u{7e}' => out.push(c),
            other => out.push_str(&format!("\\u{:04x}", other as u32)),
        }
    }
    out
}
