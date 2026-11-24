//! `mdbgen` - generates an MS Access database using the Rust port.
//!
//! Scenarios mirror the Java reference generator (`java-src/Gen.java`).

use jackcess::column::ColumnSpec;
use jackcess::database::{Database, DatabaseProperties};
use jackcess::datatype::DataType;
use jackcess::format::FileFormat;
use jackcess::property::{
    ALLOW_ZERO_LEN_PROP, AUTHOR_PROP, DEFAULT_VALUE_PROP, DESCRIPTION_PROP, FORMAT_PROP,
    REPLICATE_PROJECT_PROP, REQUIRED_PROP, ROW_SOURCE_PROP, TITLE_PROP, VALIDATION_RULE_PROP,
    VALIDATION_TEXT_PROP,
};
use jackcess::relationship::RelationshipSpec;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: mdbgen <scenario> <out.mdb> [format] [--now-ms N] [--complex-names names]"
        );
        std::process::exit(2);
    }
    let scenario = args[1].clone();
    let out = args[2].clone();
    let mut file_format = FileFormat::V2003;
    let mut now_ms: Option<i64> = None;
    let mut now_queue: Vec<i64> = Vec::new();
    let mut complex_names: Vec<String> = Vec::new();
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--now-ms" {
            let raw = args.get(i + 1).cloned().unwrap_or_default();
            now_queue = raw
                .split(',')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect();
            now_ms = now_queue.first().copied();
            i += 2;
        } else if args[i] == "--complex-names" {
            let raw = args.get(i + 1).cloned().unwrap_or_default();
            complex_names = raw
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            i += 2;
        } else {
            if let Some(f) = FileFormat::from_name(&args[i]) {
                file_format = f;
            }
            i += 1;
        }
    }

    if let Err(e) = run(
        &scenario,
        &out,
        file_format,
        now_ms,
        &now_queue,
        &complex_names,
    ) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

/// Adds rows one at a time, which is what the Java reference generator does:
/// `Gen.java` always loops over `Table.addRowFromMap`, and `TableImpl.addRow`
/// runs the complete per-row write (`PageChannel.startWrite`, the data page
/// write, then `updateTableDefinition` -> `IndexData.update` ->
/// `IndexPageCache.write`, which promotes/splits/writes the index pages after
/// every single row).
///
/// `Database::add_rows` is the *batch* `TableImpl.addRows`: it keeps the index
/// pages in memory and only writes/splits them at the end of the batch.  Once
/// an index page crosses `maxPageEntrySize` the two histories are observable
/// byte for byte: the page's entry prefix is compressed (which shrinks the
/// used area, leaving the bytes of the previous, larger write in the reused
/// page buffer) and the split leaf is allocated at a different point in the
/// file (before the following data pages, or after all of them).  The harness
/// scenarios must therefore insert one row at a time to stay byte-identical
/// with `Gen.java` for any row count.
fn add_rows_one_by_one(
    db: &mut Database,
    table: &str,
    rows: &[Vec<Value>],
) -> jackcess::Result<()> {
    for row in rows {
        db.add_rows(table, std::slice::from_ref(row))?;
    }
    Ok(())
}

fn run(
    scenario: &str,
    out: &str,
    fmt: FileFormat,
    now_ms: Option<i64>,
    now_queue: &[i64],
    complex_names: &[String],
) -> jackcess::Result<()> {
    let path = std::path::Path::new(out);
    if scenario == "append" {
        let mut db = Database::open_path(path)?;
        db.now_ms = now_ms;
        let date_ms = std::env::var("JACKCESS_DATE_MS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(1584246645000);
        append_rows(&mut db, date_ms)?;
        db.flush()?;
        println!("appended to {} size={}", out, std::fs::metadata(out)?.len());
        return Ok(());
    }
    if scenario == "propupdate" {
        return propupdate(path, fmt, now_ms, now_queue);
    }
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = if scenario == "dbprops" {
        Database::create_with_properties(fmt, path, &database_properties())?
    } else {
        Database::create(fmt, path)?
    };
    db.now_ms = now_ms;
    db.now_queue = now_queue.to_vec();
    db.set_complex_names(complex_names.to_vec());
    // database level behaviour settings which affect the written bytes, set
    // after `Database::create` and before any table or row is created
    match scenario {
        // `DatabaseBuilder.setTimeZone(TimeZone.getTimeZone("UTC"))`
        "tzutc" => db.set_time_zone_offset_minutes(0),
        // `TimeZone.getTimeZone("GMT-05:00")`
        "tzneg" => db.set_time_zone_offset_minutes(-300),
        _ => {}
    }
    if scenario == "noexpr" || scenario == "noexprcalc" {
        // `db.setEvaluateExpressions(false)` (a Database method, not a
        // DatabaseBuilder one)
        db.set_evaluate_expressions(false);
    }
    match scenario {
        "empty" => {}
        "dbprops" => {}
        "tableprops" => {
            tableprops(&mut db)?;
        }
        "summaryprops" => {
            summaryprops(&mut db)?;
        }
        "defaults" => {
            defaults(&mut db)?;
        }
        "noexpr" => {
            noexpr(&mut db)?;
        }
        "noexprcalc" => {
            noexprcalc(&mut db)?;
        }
        "tzutc" | "tzneg" => {
            tz_rows(&mut db)?;
        }
        "required" => {
            required(&mut db)?;
        }
        "validate" => {
            validate(&mut db, out)?;
        }
        "validateupdate" => {
            validateupdate(&mut db, out)?;
        }
        "t1" => {
            make_table1(&mut db)?;
        }
        "rows" => {
            make_table1(&mut db)?;
            add_rows(&mut db)?;
        }
        "types" => {
            make_types(&mut db)?;
        }
        "types_rows" => {
            make_types(&mut db)?;
            add_type_rows(&mut db, 1584246645000)?;
        }
        "rows50" => {
            make_table1(&mut db)?;
            add_rows50(&mut db)?;
        }
        "twotables" => {
            make_table1(&mut db)?;
            make_table2(&mut db)?;
        }
        "unicode" => {
            make_unicode(&mut db)?;
            add_unicode_rows(&mut db)?;
        }
        "bigmemo" => {
            make_big_memo(&mut db)?;
            add_big_memo_rows(&mut db)?;
        }
        "twoindexes" => {
            make_two_indexes(&mut db)?;
            add_two_index_rows(&mut db)?;
        }
        "delete" => {
            make_delete_table(&mut db)?;
            add_delete_rows(&mut db)?;
            delete_rows(&mut db)?;
        }
        "update" => {
            make_update_table(&mut db)?;
            add_update_rows(&mut db, DATE_BASE_MS)?;
            update_rows(&mut db, DATE_BASE_MS)?;
        }
        "memoupdate" => {
            make_memo_update_table(&mut db)?;
            add_memo_update_rows(&mut db)?;
            update_memo_rows(&mut db)?;
        }
        "partialupdate" => {
            partialupdate(&mut db)?;
        }
        "diag200" => {
            diag200(&mut db)?;
        }
        "bigindex" => {
            bigindex(&mut db)?;
        }
        "indexchurn" => {
            indexchurn(&mut db)?;
        }
        "bigindex2" => {
            bigindex2(&mut db)?;
        }
        "keeplong" => {
            keeplong(&mut db)?;
        }
        "rowops" => {
            make_rowops_tables(&mut db)?;
            add_rowops_rows(&mut db)?;
            rowops(&mut db)?;
        }
        "addcol" => {
            addcol(&mut db)?;
        }
        "addindex" => {
            addindex(&mut db)?;
        }
        "addboth" => {
            addboth(&mut db)?;
        }
        "addmany" => {
            addmany(&mut db)?;
        }
        "relate" => {
            relate(&mut db)?;
        }
        "samerel" => {
            samerel(&mut db)?;
        }
        "selfrel" => {
            selfrel(&mut db)?;
        }
        "fkadd" => {
            fkadd(&mut db)?;
        }
        "fkdel" => {
            fkdel(&mut db)?;
        }
        "fkupd" => {
            fkupd(&mut db)?;
        }
        "linked" => {
            linked(&mut db)?;
        }
        "reclaim" => {
            reclaim(&mut db)?;
        }
        "reuse" => {
            reuse(&mut db)?;
        }
        "autonum" => {
            autonum(&mut db)?;
        }
        "guidx" => {
            guidx(&mut db)?;
        }
        "multivalue" => {
            multivalue(&mut db)?;
        }
        "attachment" => {
            attachment(&mut db)?;
        }
        "addcomplex" => {
            addcomplex(&mut db)?;
        }
        "versionhistory" => {
            versionhistory(&mut db)?;
        }
        "vhadd" => {
            vhadd(&mut db)?;
        }
        "calc" => {
            calc(&mut db)?;
        }
        "calcupdate" => {
            calcupdate(&mut db)?;
        }
        "calcfloat" => {
            calcfloat(&mut db)?;
        }
        other => {
            return Err(jackcess::Error::IllegalArgument(format!(
                "unknown scenario {}",
                other
            )))
        }
    }
    db.flush()?;
    println!("wrote {} size={}", out, std::fs::metadata(out)?.len());
    Ok(())
}

fn make_types(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Types",
        vec![
            ColumnSpec::new("BLong", DataType::Long),
            ColumnSpec::new("BByte", DataType::Byte),
            ColumnSpec::new("BInt", DataType::Int),
            ColumnSpec::new("BDouble", DataType::Double),
            ColumnSpec::new("BFloat", DataType::Float),
            ColumnSpec::new("BMoney", DataType::Money),
            ColumnSpec::new("BDate", DataType::ShortDateTime),
            ColumnSpec::new("BText", DataType::Text).with_length(100),
            ColumnSpec::new("BTextC", DataType::Text)
                .with_length(100)
                .with_compressed_unicode(true),
            ColumnSpec::new("BBin", DataType::Binary).with_length(50),
            ColumnSpec::new("BMemo", DataType::Memo),
            ColumnSpec::new("BGuid", DataType::Guid),
            ColumnSpec::new("BNumeric", DataType::Numeric)
                .with_precision(18)
                .with_scale(4),
            ColumnSpec::new("BBool", DataType::Boolean),
        ],
    );
    db.create_table(&spec)?;
    Ok(())
}

fn memo_text(i: i64) -> String {
    if i == 0 {
        "short memo".to_string()
    } else {
        "long memo ".repeat((i * 100) as usize)
    }
}

#[expect(
    clippy::approx_constant,
    reason = "the generated fixture must keep the exact literal 2.71828"
)]
fn append_rows(db: &mut Database, date_ms: i64) -> jackcess::Result<()> {
    let offset = db.tz.raw_offset_ms;
    let mut rows = Vec::new();
    for i in 0..3i64 {
        rows.push(vec![
            Value::Long(200000 + i as i32),
            Value::Byte((i + 5) as u8),
            Value::Int((-2000 - i) as i16),
            Value::Double(2.71828 * (i + 1) as f64),
            Value::Float(1.25 * (i + 1) as f32),
            Value::Decimal {
                unscaled: 777000 + i as i128 * 100,
                scale: 4,
            },
            Value::DateTime(jackcess::datenums::LocalDateTime::from_epoch_millis(
                date_ms + i * 86_400_000 + offset,
            )),
            Value::Text(format!("appended\u{00e9}{}", i)),
            Value::Text(format!("comp{}", i)),
            Value::Bytes(vec![9, 8, i as u8]),
            Value::Text(memo_text(i)),
            Value::Text("{89ABCDEF-0123-4567-89AB-CDEF01234567}".to_string()),
            Value::Decimal {
                unscaled: 1234567891 + i as i128 * 10000,
                scale: 4,
            },
            Value::Bool(i % 2 == 1),
        ]);
    }
    add_rows_one_by_one(db, "Types", &rows)?;
    Ok(())
}

#[expect(
    clippy::approx_constant,
    reason = "the generated fixture must keep the exact literal 3.14159"
)]
fn add_type_rows(db: &mut Database, date_ms: i64) -> jackcess::Result<()> {
    let offset = db.tz.raw_offset_ms;
    let mut rows = Vec::new();
    for i in 0..3i64 {
        rows.push(vec![
            Value::Long(100000 + i as i32),
            Value::Byte((i + 1) as u8),
            Value::Int((-1000 - i) as i16),
            Value::Double(3.14159 * (i + 1) as f64),
            Value::Float(2.5 * (i + 1) as f32),
            Value::Decimal {
                unscaled: 12345000 + i as i128 * 100,
                scale: 4,
            },
            Value::DateTime(jackcess::datenums::LocalDateTime::from_epoch_millis(
                date_ms + i * 86_400_000 + offset,
            )),
            Value::Text(format!("text\u{00e9}\u{4e2d}{}", i)),
            Value::Text(format!("compressed{}", i)),
            Value::Bytes(vec![1, 2, 3, i as u8]),
            if i == 0 {
                Value::Null
            } else {
                Value::Text(format!("memo value {}", i))
            },
            Value::Text("{01234567-89AB-CDEF-0123-456789ABCDEF}".to_string()),
            Value::Decimal {
                unscaled: -98765432 + i as i128 * 10000,
                scale: 4,
            },
            Value::Bool(i % 2 == 0),
        ]);
    }
    // a row of all nulls (`t.addRow(new Object[0])`)
    rows.push(vec![Value::Null; 14]);
    add_rows_one_by_one(db, "Types", &rows)?;
    Ok(())
}

fn make_unicode(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Uni",
        vec![
            ColumnSpec::new("Code", DataType::Text).with_length(40),
            ColumnSpec::new("Val", DataType::Long),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("Code".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn add_unicode_rows(db: &mut Database) -> jackcess::Result<()> {
    let codes = [
        "caf\u{00e9}",
        "caf\u{00e8}",
        "\u{65e5}\u{672c}\u{8a9e}",
        "Stra\u{00df}e",
        "\u{1f600}",
        "ABC",
        "xyz",
        "a b",
        "\u{00dc}n\u{00ef}cod\u{00e9}",
    ];
    let mut rows = Vec::new();
    for (i, code) in codes.iter().enumerate() {
        rows.push(vec![
            Value::Text(code.to_string()),
            Value::Long((i * 3) as i32),
        ]);
    }
    add_rows_one_by_one(db, "Uni", &rows)?;
    Ok(())
}

fn make_big_memo(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "BigMemo",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Body", DataType::Memo),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn add_big_memo_rows(db: &mut Database) -> jackcess::Result<()> {
    let sizes = [10usize, 1000, 3000, 6000, 12000];
    let mut rows = Vec::new();
    for size in sizes {
        let mut s = String::with_capacity(size);
        for j in 0..size {
            s.push((b'a' + (j % 26) as u8) as char);
        }
        rows.push(vec![Value::Null, Value::Text(s)]);
    }
    add_rows_one_by_one(db, "BigMemo", &rows)?;
    Ok(())
}

fn make_table2(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Second",
        vec![
            ColumnSpec::new("Code", DataType::Text).with_length(10),
            ColumnSpec::new("Amount", DataType::Money),
        ],
    );
    db.create_table(&spec)?;
    Ok(())
}

fn add_rows50(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 0..50 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("longer name number {}", i)),
            Value::Double(0.5 * i as f64),
        ]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

fn make_two_indexes(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Two",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(30),
            ColumnSpec::new("Note", DataType::Text).with_length(50),
            ColumnSpec::new("Num", DataType::Long),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
    .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique())
    .with_index(IndexSpec::new(
        "ByNumName",
        vec![("Num".to_string(), false), ("Name".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn add_two_index_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 0..20i32 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("name{}", i)),
            Value::Text(format!("note {}", i)),
            Value::Long((i * 37) % 11),
        ]);
    }
    add_rows_one_by_one(db, "Two", &rows)?;
    Ok(())
}

fn make_table1(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Value", DataType::Double),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn add_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 0..5 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("name{}", i)),
            Value::Double(1.5 * i as f64),
        ]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// row update / delete scenarios (`Gen.java` "delete", "update", "memoupdate"
// and "rowops")
// ---------------------------------------------------------------------------

/// The local date/time used by the row scenarios (2020-03-15 12:30:45 +08:00,
/// the same instant the "types" scenario uses).
const DATE_BASE_MS: i64 = 1584246645000;

/// A memo value long enough to live on long value pages.
fn long_memo(tag: i64) -> String {
    format!("L{}:{}", tag, "abcdefghij".repeat(300))
}

fn make_delete_table(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Note", DataType::Memo),
            ColumnSpec::new("Value", DataType::Double),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn add_delete_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 0..150i64 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("name{}", i)),
            Value::Text(if i % 7 == 0 {
                long_memo(i)
            } else {
                format!("memo {}", i)
            }),
            Value::Double(i as f64 * 1.5 + 0.25),
        ]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

/// Deletes a few rows (one holding a long MEMO value), then adds more rows so
/// that page re-use is exercised.
fn delete_rows(db: &mut Database) -> jackcess::Result<()> {
    let id_col = db.get_table("Test")?.column_index("ID").unwrap_or(0);
    let mut by_id = std::collections::HashMap::new();
    for (row_id, values) in db.read_rows_with_ids("Test")? {
        if let Some(values) = values {
            if let Some(Value::Long(id)) = values.get(id_col) {
                by_id.insert(*id, row_id);
            }
        }
    }

    for id in [3i32, 8, 40, 71, 99, 150] {
        let row_id = match by_id.get(&id) {
            Some(row_id) => *row_id,
            None => {
                return Err(jackcess::Error::Jackcess(format!(
                    "delete scenario: row {} not found",
                    id
                )))
            }
        };
        db.delete_row("Test", row_id)?;
    }

    let rows = vec![
        vec![
            Value::Null,
            Value::Text("after0".to_string()),
            Value::Text("after memo 0".to_string()),
            Value::Double(1000.5),
        ],
        vec![
            Value::Null,
            Value::Text("after1".to_string()),
            Value::Text(format!("LM:{}", "0123456789".repeat(120))),
            Value::Double(1001.5),
        ],
        vec![
            Value::Null,
            Value::Text("after2".to_string()),
            Value::Text("after memo 2".to_string()),
            Value::Double(1002.5),
        ],
    ];
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

fn make_update_table(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Value", DataType::Double),
            ColumnSpec::new("Count", DataType::Long),
            ColumnSpec::new("Flag", DataType::Boolean),
            ColumnSpec::new("Stamp", DataType::ShortDateTime),
            ColumnSpec::new("Note", DataType::Text).with_length(400),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn update_stamp(base_ms: i64, offset: i64, days: i64) -> Value {
    Value::DateTime(jackcess::datenums::LocalDateTime::from_epoch_millis(
        base_ms + days * 86_400_000 + offset,
    ))
}

fn add_update_rows(db: &mut Database, base_ms: i64) -> jackcess::Result<()> {
    let offset = db.tz.raw_offset_ms;
    let mut rows = Vec::new();
    for i in 0..150i64 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("row{}", i)),
            Value::Double(0.25 * i as f64 + 1.5),
            Value::Long(i as i32 * 7 - 50),
            Value::Bool(i % 2 == 0),
            update_stamp(base_ms, offset, i),
            Value::Text(format!("note-{}-{}", i, "x".repeat((i % 9) as usize))),
        ]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

/// Updates several rows (in a fixed id order): shorter text, longer text which
/// forces a relocation, and changed numeric/date/boolean/null values.
fn update_rows(db: &mut Database, base_ms: i64) -> jackcess::Result<()> {
    let (id_col, name_col, value_col, count_col, flag_col, stamp_col, note_col) = {
        let t = db.get_table("Test")?;
        (
            t.column_index("ID").unwrap_or(0),
            t.column_index("Name").unwrap_or(1),
            t.column_index("Value").unwrap_or(2),
            t.column_index("Count").unwrap_or(3),
            t.column_index("Flag").unwrap_or(4),
            t.column_index("Stamp").unwrap_or(5),
            t.column_index("Note").unwrap_or(6),
        )
    };
    let offset = db.tz.raw_offset_ms;

    let mut by_id = std::collections::HashMap::new();
    for (row_id, values) in db.read_rows_with_ids("Test")? {
        if let Some(values) = values {
            if let Some(Value::Long(id)) = values.get(id_col) {
                by_id.insert(*id, (row_id, values));
            }
        }
    }

    for id in [99i32, 50, 10, 3, 77, 25, 64] {
        let (row_id, mut values) = match by_id.get(&id) {
            Some(entry) => entry.clone(),
            None => {
                return Err(jackcess::Error::Jackcess(format!(
                    "update scenario: row {} not found",
                    id
                )))
            }
        };
        match id {
            // everything changes, and the row shrinks
            99 => {
                values[name_col] = Value::Text("short".to_string());
                values[value_col] = Value::Double(99.5);
                values[count_col] = Value::Null;
                values[flag_col] = Value::Bool(true);
                values[stamp_col] = update_stamp(base_ms, offset, 500);
                values[note_col] = Value::Text("s".to_string());
            }
            // only the double and the note change, and the note grows
            50 => {
                if let Some(value) = values[value_col].as_f64() {
                    values[value_col] = Value::Double(value * 2.0);
                }
                values[note_col] = Value::Text("y".repeat(180));
            }
            // a longer name and note force a relocation
            10 => {
                values[name_col] = Value::Text(format!("row9-{}", "z".repeat(18)));
                values[value_col] = Value::Double(-10.75);
                values[count_col] = Value::Null;
                values[flag_col] = Value::Bool(false);
                values[stamp_col] = update_stamp(base_ms, offset, 900);
                values[note_col] = Value::Text("w".repeat(120));
            }
            // only the double changes, so the row keeps its size
            3 => {
                values[value_col] = Value::Double(3.25);
            }
            // nulls for a long and a var length column, the row shrinks
            77 => {
                values[count_col] = Value::Null;
                values[flag_col] = Value::Bool(false);
                values[note_col] = Value::Null;
            }
            // a null double, a new name and a longer note
            25 => {
                values[name_col] = Value::Text("n25".to_string());
                values[value_col] = Value::Null;
                values[count_col] = Value::Long(123456);
                values[note_col] = Value::Text("q".repeat(60));
            }
            // a longer note and a different date
            64 => {
                values[note_col] = Value::Text("m".repeat(100));
                values[stamp_col] = update_stamp(base_ms, offset, 0);
            }
            _ => {}
        }
        db.update_row("Test", row_id, &values)?;
    }
    Ok(())
}

fn make_memo_update_table(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Body", DataType::Memo),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn memo_body(i: i64) -> String {
    if i % 5 == 0 {
        format!("L{}:{}", i, "abcdefghij".repeat(150))
    } else if i % 3 == 0 {
        format!("M{}:{}", i, "0123456789".repeat(20))
    } else {
        format!("memo {}", i)
    }
}

fn add_memo_update_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 0..60i64 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("m{}", i)),
            Value::Text(memo_body(i)),
        ]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

/// Updates MEMO values in every long value layout: inline to other page(s),
/// other/multi page back to inline, an unchanged value, a null and a value
/// which grows across several long value pages.
fn update_memo_rows(db: &mut Database) -> jackcess::Result<()> {
    let (id_col, body_col) = {
        let t = db.get_table("Test")?;
        (
            t.column_index("ID").unwrap_or(0),
            t.column_index("Body").unwrap_or(2),
        )
    };

    let mut by_id = std::collections::HashMap::new();
    for (row_id, values) in db.read_rows_with_ids("Test")? {
        if let Some(values) = values {
            if let Some(Value::Long(id)) = values.get(id_col) {
                by_id.insert(*id, (row_id, values));
            }
        }
    }

    for id in [1i32, 6, 4, 9, 12, 20] {
        let (row_id, mut values) = match by_id.get(&id) {
            Some(entry) => entry.clone(),
            None => {
                return Err(jackcess::Error::Jackcess(format!(
                    "memoupdate scenario: row {} not found",
                    id
                )))
            }
        };
        match id {
            // a long value on another page shrinks to an inline value
            1 => values[body_col] = Value::Text("replaced".to_string()),
            // a long value grows across several long value pages
            6 => {
                values[body_col] = Value::Text(format!("XL:{}", "qrstuvwxyz".repeat(500)));
            }
            // an unchanged value is written again
            4 => {}
            // a value becomes null
            9 => values[body_col] = Value::Null,
            // an inline value moves to another page
            12 => values[body_col] = Value::Text("z".repeat(100)),
            // a medium value shrinks to a short inline value
            20 => values[body_col] = Value::Text("S".to_string()),
            _ => {}
        }
        db.update_row("Test", row_id, &values)?;
    }

    let rows = vec![
        vec![
            Value::Null,
            Value::Text("after0".to_string()),
            Value::Text("tail memo 0".to_string()),
        ],
        vec![
            Value::Null,
            Value::Text("after1".to_string()),
            Value::Text(format!("TL:{}", "9876543210".repeat(400))),
        ],
    ];
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// KEEP_VALUE partial row updates (`Column.KEEP_VALUE` / `Value::Keep`):
// "partialupdate" and "keeplong"
// ---------------------------------------------------------------------------

/// A memo long enough to be stored on long value pages; with more than 4096
/// bytes it lives across several of them.
fn keep_memo(tag: &str, chars: usize) -> String {
    format!("{}:{}", tag, "abcdefghij".repeat(chars / 10))
}

/// The `Test` table of the "partialupdate" scenario: an auto number primary
/// key, a unique index on the text column and an index on the numeric column.
fn make_partial_update_table(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(255),
            ColumnSpec::new("Memo", DataType::Memo),
            ColumnSpec::new("N", DataType::Long),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
    .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique())
    .with_index(IndexSpec::new("ByN", vec![("N".to_string(), true)]));
    db.create_table(&spec)?;
    Ok(())
}

/// A row of the KEEP_VALUE scenarios (ID generated, then name, memo and the
/// numeric column; both tables use that column order).
fn keep_row_values(name: &str, memo: &str, n: i32) -> Vec<Value> {
    vec![
        Value::Null,
        Value::Text(name.to_string()),
        Value::Text(memo.to_string()),
        Value::Long(n),
    ]
}

fn row_id_of(
    by_id: &std::collections::HashMap<i32, (jackcess::table::RowId, Vec<Value>)>,
    id: i32,
    scenario: &str,
) -> jackcess::Result<jackcess::table::RowId> {
    by_id.get(&id).map(|entry| entry.0).ok_or_else(|| {
        jackcess::Error::Jackcess(format!("{} scenario: row {} not found", scenario, id))
    })
}

/// Diagnostic scenario ("diag200"): the "partialupdate" table shape (auto
/// number primary key, unique text index, numeric index) with 200 rows added
/// one at a time.  It pins the per-row write history which the reference
/// generator produces with `addRowFromMap` (see `add_rows_one_by_one`): once
/// the unique text index page crosses `maxPageEntrySize`, jackcess compresses
/// its entry prefix and then splits the leaf, and both events leave page
/// contents/allocation which depend on the number of writes per row.
fn diag200(db: &mut Database) -> jackcess::Result<()> {
    make_partial_update_table(db)?;
    let rows: Vec<Vec<Value>> = (0..200i32)
        .map(|i| {
            keep_row_values(
                &format!("filler{:03}", i),
                &format!("fill memo {}", i),
                1000 + i,
            )
        })
        .collect();
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// index stress: deep B-trees, many leaf and interior splits, compound,
// descending and unique indexes, and index churn (`Gen.java` "bigindex",
// "indexchurn" and "bigindex2").  Every row is added, updated and deleted one
// at a time, which pins the per-row index page write history (see
// `add_rows_one_by_one`).
// ---------------------------------------------------------------------------

/// number of rows in the "bigindex" scenario
const BIGINDEX_ROWS: usize = 3000;
/// number of rows inserted before the "indexchurn" mutations
const CHURN_ROWS: usize = 1500;
/// number of rows in the "bigindex2" scenario
const BIGINDEX2_ROWS: usize = 2000;

/// Deterministic permutation of `0..n` (`mul` must be coprime with `n`), the
/// same arithmetic as `Gen.scramble`.
fn scramble(i: usize, n: usize, mul: usize) -> usize {
    (i * mul) % n
}

/// The "bigindex" table: an auto number primary key, a unique TEXT index, a
/// descending compound index (`Num DESC, Name ASC`) and a non-unique index
/// (`Note`) whose keys are heavily duplicated.  `Pad` holds variable length
/// text so the data pages grow and rows relocate while the indexes are
/// written.
fn make_big_index(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Big",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(40),
            ColumnSpec::new("Num", DataType::Long),
            ColumnSpec::new("Note", DataType::Text).with_length(60),
            ColumnSpec::new("Pad", DataType::Text).with_length(255),
            ColumnSpec::new("Val", DataType::Long),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
    .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique())
    .with_index(IndexSpec::new(
        "ByNumName",
        vec![("Num".to_string(), false), ("Name".to_string(), true)],
    ))
    .with_index(IndexSpec::new("ByNote", vec![("Note".to_string(), true)]));
    db.create_table(&spec)?;
    Ok(())
}

/// one "bigindex" row; `k` drives every value (`Gen.bigIndexRow`)
fn big_index_row(k: usize) -> Vec<Value> {
    vec![
        Value::Null,
        Value::Text(format!("key{:05}", k)),
        Value::Long((k % 37) as i32),
        Value::Text(format!("note {}", k % 97)),
        Value::Text("p".repeat(10 + (k % 110))),
        Value::Long((k * 3) as i32),
    ]
}

fn add_big_index_rows(db: &mut Database, count: usize) -> jackcess::Result<()> {
    for i in 0..count {
        let row = big_index_row(scramble(i, count, 7919));
        add_rows_one_by_one(db, "Big", &[row])?;
    }
    Ok(())
}

fn bigindex(db: &mut Database) -> jackcess::Result<()> {
    make_big_index(db)?;
    add_big_index_rows(db, BIGINDEX_ROWS)
}

/// Churn on the "bigindex" table: move indexed key values, rewrite only
/// non-key columns (the rows grow and relocate, so the index entries must
/// follow the new row ids), delete every third row, re-insert rows and add
/// more rows at the end.
fn indexchurn(db: &mut Database) -> jackcess::Result<()> {
    make_big_index(db)?;
    add_big_index_rows(db, CHURN_ROWS)?;

    let (name_col, num_col, note_col, pad_col, val_col) = {
        let t = db.get_table("Big")?;
        (
            t.column_index("Name").unwrap_or(1),
            t.column_index("Num").unwrap_or(2),
            t.column_index("Note").unwrap_or(3),
            t.column_index("Pad").unwrap_or(4),
            t.column_index("Val").unwrap_or(5),
        )
    };

    // (1) change indexed key values: every updated entry moves across the
    // leaf pages (the unique Name to a fresh key range, Num/Note to other
    // duplicates)
    let mut by_id = rows_by_id(db, "Big")?;
    let mut id = 1i32;
    while id <= CHURN_ROWS as i32 {
        let (row_id, mut values) = by_id.remove(&id).ok_or_else(|| {
            jackcess::Error::Jackcess(format!("indexchurn scenario: row {} not found", id))
        })?;
        values[name_col] = Value::Text(format!("move{:05}", 5000 + id));
        values[num_col] = Value::Long((id * 17) % 53);
        values[note_col] = Value::Text(format!("moved {}", id % 31));
        db.update_row("Big", row_id, &values)?;
        id += 5;
    }

    // (2) only non-key columns change: the rows grow and relocate on the data
    // pages while every index entry is rewritten with the new row id
    by_id = rows_by_id(db, "Big")?;
    let mut id = 2i32;
    while id <= CHURN_ROWS as i32 {
        let (row_id, mut values) = by_id.remove(&id).ok_or_else(|| {
            jackcess::Error::Jackcess(format!("indexchurn scenario: row {} not found", id))
        })?;
        values[pad_col] = Value::Text("g".repeat(30 + (id as usize % 90)));
        values[val_col] = Value::Long(-id);
        db.update_row("Big", row_id, &values)?;
        id += 7;
    }

    // (3) delete every third row: many leaf pages empty out completely
    by_id = rows_by_id(db, "Big")?;
    let mut id = 3i32;
    while id <= CHURN_ROWS as i32 {
        let (row_id, _values) = by_id.remove(&id).ok_or_else(|| {
            jackcess::Error::Jackcess(format!("indexchurn scenario: row {} not found", id))
        })?;
        db.delete_row("Big", row_id)?;
        id += 3;
    }

    // (4) re-insert rows (the freed data pages are reused) ...
    for i in 0..300usize {
        let k = scramble(i, 300, 7);
        let mut row = big_index_row(k);
        row[1] = Value::Text(format!("re{:05}", k));
        add_rows_one_by_one(db, "Big", &[row])?;
    }
    // (5) ... and add more rows at the end
    for i in 0..200usize {
        let k = scramble(i, 200, 71);
        let mut row = big_index_row(k);
        row[1] = Value::Text(format!("tail{:05}", k));
        add_rows_one_by_one(db, "Big", &[row])?;
    }
    Ok(())
}

/// The "bigindex2" table: a three column compound index, an index with two
/// descending columns, a non-unique index on a column which is null for whole
/// groups of rows (an all null index entry) and two TEXT columns whose values
/// share a 120 character prefix, so a leaf page holds few entries and its
/// entry prefix is long.
fn make_big_index2(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Big2",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("A", DataType::Text).with_length(255),
            ColumnSpec::new("B", DataType::Long),
            ColumnSpec::new("C", DataType::Text).with_length(255),
            ColumnSpec::new("D", DataType::Text).with_length(20),
            ColumnSpec::new("E", DataType::Long),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
    .with_index(IndexSpec::new(
        "ByABC",
        vec![
            ("A".to_string(), true),
            ("B".to_string(), false),
            ("C".to_string(), true),
        ],
    ))
    .with_index(IndexSpec::new(
        "ByED",
        vec![("E".to_string(), false), ("D".to_string(), false)],
    ))
    .with_index(IndexSpec::new("ByC", vec![("C".to_string(), true)]));
    db.create_table(&spec)?;
    Ok(())
}

/// one "bigindex2" row; an all-null key row for every 37th `k`
/// (`Gen.runBigIndex2Scenario`)
fn big_index2_row(k: usize) -> Vec<Value> {
    if k.is_multiple_of(37) {
        // every indexed column is null: all null index entries in every index
        return vec![Value::Null; 6];
    }
    let prefix_a = "ab".repeat(60);
    let prefix_c = "cd".repeat(60);
    vec![
        Value::Null,
        Value::Text(format!("{}{:06}", prefix_a, k)),
        Value::Long((k % 41) as i32 - 20),
        if k.is_multiple_of(13) {
            Value::Null
        } else {
            Value::Text(format!("{}{:06}", prefix_c, k))
        },
        Value::Text(format!("d{}", k % 29)),
        if k.is_multiple_of(9) {
            Value::Null
        } else {
            Value::Long(((k * 11) % 400) as i32 - 200)
        },
    ]
}

fn bigindex2(db: &mut Database) -> jackcess::Result<()> {
    make_big_index2(db)?;
    for i in 0..BIGINDEX2_ROWS {
        let row = big_index2_row(scramble(i, BIGINDEX2_ROWS, 997));
        add_rows_one_by_one(db, "Big2", &[row])?;
    }
    Ok(())
}

/// Partial updates which pass `Value::Keep` (`Column.KEEP_VALUE`) for the
/// columns which must not change: a fixed length column keeps its old value
/// and a variable length (long) value keeps its raw on-page bytes, so a memo
/// keeps its existing long value pages while other columns (including indexed
/// ones) change.  Also exercises `updateValue`, `asUpdateRow`, a relocation of
/// the row and an update of the memo itself.
fn partialupdate(db: &mut Database) -> jackcess::Result<()> {
    make_partial_update_table(db)?;

    // short inline memo, multi page memo, other page memo, multi page memo
    let mut rows = vec![
        keep_row_values("alpha", "short memo alpha", 10),
        keep_row_values("beta", &keep_memo("B", 4200), 20),
        keep_row_values("gamma", &keep_memo("G", 300), 30),
        keep_row_values("delta", &keep_memo("D", 2500), 40),
        keep_row_values("epsilon", "short memo epsilon", 50),
    ];
    // fillers: page the table out so the growing update relocates the row to a
    // later data page
    for i in 0..60i32 {
        rows.push(keep_row_values(
            &format!("filler{:03}", i),
            &format!("fill memo {}", i),
            1000 + i,
        ));
    }
    add_rows_one_by_one(db, "Test", &rows)?;

    let by_id = rows_by_id(db, "Test")?;
    let id1 = row_id_of(&by_id, 1, "partialupdate")?;
    let id2 = row_id_of(&by_id, 2, "partialupdate")?;
    let id3 = row_id_of(&by_id, 3, "partialupdate")?;
    let id4 = row_id_of(&by_id, 4, "partialupdate")?;
    let id5 = row_id_of(&by_id, 5, "partialupdate")?;
    let keep = Value::Keep;

    // keep the inline memo and the auto number, change the indexed Name and N
    db.update_row_partial(
        "Test",
        id1,
        &[
            keep.clone(),
            Value::Text("alpha2".to_string()),
            keep.clone(),
            Value::Long(11),
        ],
    )?;
    // keep the multi page memo, change only the indexed N
    db.update_row_partial(
        "Test",
        id2,
        &[keep.clone(), keep.clone(), keep.clone(), Value::Long(21)],
    )?;
    // every column kept (the row bytes are written again unchanged)
    db.update_row_partial(
        "Test",
        id3,
        &[keep.clone(), keep.clone(), keep.clone(), keep.clone()],
    )?;
    // `Table.updateValue`
    db.update_value("Test", id4, "N", Value::Long(41))?;
    // `Table.asUpdateRow`: the columns the map does not name are kept
    let mut map = std::collections::BTreeMap::new();
    map.insert("Name".to_string(), Value::Text("epsilon2".to_string()));
    db.update_row_from_map("Test", id5, &map)?;
    // the Name grows, so the row relocates to another data page, while the
    // multi page memo keeps the pages it already has
    db.update_row_partial(
        "Test",
        id2,
        &[
            keep.clone(),
            Value::Text(format!("relocated-{}", "zyxwv".repeat(18))),
            keep.clone(),
            Value::Long(22),
        ],
    )?;
    // the memo itself changes (new long value pages), the rest is kept
    db.update_row_partial(
        "Test",
        id1,
        &[
            keep.clone(),
            keep.clone(),
            Value::Text(keep_memo("R", 2500)),
            keep.clone(),
        ],
    )?;

    // two rows afterwards (the auto number and the index pages still work)
    add_rows_one_by_one(
        db,
        "Test",
        &[
            keep_row_values("omega", &keep_memo("O", 1500), 60),
            keep_row_values("zeta", "tail memo", 70),
        ],
    )?;
    Ok(())
}

/// Keeps a multi page MEMO value through several updates of the other columns
/// (so no new long value pages may be allocated) and then updates the memo.
fn keeplong(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Body", DataType::Memo),
            ColumnSpec::new("Num", DataType::Long),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
    .with_index(IndexSpec::new("ByNum", vec![("Num".to_string(), true)]));
    db.create_table(&spec)?;

    let rows = vec![
        keep_row_values("one", &keep_memo("K1", 4500), 1),
        keep_row_values("two", &keep_memo("K2", 2500), 2),
        keep_row_values("three", "short body", 3),
    ];
    add_rows_one_by_one(db, "Test", &rows)?;

    let by_id = rows_by_id(db, "Test")?;
    let id1 = row_id_of(&by_id, 1, "keeplong")?;
    let id2 = row_id_of(&by_id, 2, "keeplong")?;
    let keep = Value::Keep;

    // several updates which keep the multi page memo while the other columns
    // (including the indexed Num) change
    for i in 0..5 {
        db.update_row_partial(
            "Test",
            id1,
            &[
                keep.clone(),
                Value::Text(format!("one-{}", i)),
                keep.clone(),
                Value::Long(10 + i),
            ],
        )?;
    }
    db.update_row_partial(
        "Test",
        id1,
        &[
            keep.clone(),
            Value::Text("one-final".to_string()),
            keep.clone(),
            Value::Long(19),
        ],
    )?;
    db.update_value("Test", id1, "Num", Value::Long(20))?;

    let mut map = std::collections::BTreeMap::new();
    map.insert("Name".to_string(), Value::Text("two-b".to_string()));
    db.update_row_from_map("Test", id2, &map)?;
    db.update_row_partial(
        "Test",
        id2,
        &[keep.clone(), keep.clone(), keep.clone(), Value::Long(21)],
    )?;

    add_rows_one_by_one(db, "Test", &[keep_row_values("four", "tail body", 4)])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// rowops: two indexes, repeated updates, delete of a relocated row and an
// index-less table
// ---------------------------------------------------------------------------

fn make_rowops_tables(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(30),
            ColumnSpec::new("Num", DataType::Long),
            ColumnSpec::new("Note", DataType::Text).with_length(120),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
    .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique());
    db.create_table(&spec)?;

    let plain = TableSpec::new(
        "NoIdx",
        vec![
            ColumnSpec::new("ID", DataType::Long),
            ColumnSpec::new("Label", DataType::Text).with_length(20),
        ],
    );
    db.create_table(&plain)?;
    Ok(())
}

fn add_rowops_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 0..60i32 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("n{}", i)),
            Value::Long((i * 13) % 7),
            Value::Text(format!("note{}", i)),
        ]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;

    let mut plain = Vec::new();
    for i in 0..5i32 {
        plain.push(vec![Value::Long(i), Value::Text(format!("l{}", i))]);
    }
    add_rows_one_by_one(db, "NoIdx", &plain)?;
    Ok(())
}

/// Builds an id -> (row id, values) map for the named table.
fn rows_by_id(
    db: &mut Database,
    table_name: &str,
) -> jackcess::Result<std::collections::HashMap<i32, (jackcess::table::RowId, Vec<Value>)>> {
    let id_col = db.get_table(table_name)?.column_index("ID").unwrap_or(0);
    let mut by_id = std::collections::HashMap::new();
    for (row_id, values) in db.read_rows_with_ids(table_name)? {
        if let Some(values) = values {
            if let Some(Value::Long(id)) = values.get(id_col) {
                by_id.insert(*id, (row_id, values));
            }
        }
    }
    Ok(by_id)
}

fn rowops(db: &mut Database) -> jackcess::Result<()> {
    let (name_col, num_col, note_col) = {
        let t = db.get_table("Test")?;
        (
            t.column_index("Name").unwrap_or(1),
            t.column_index("Num").unwrap_or(2),
            t.column_index("Note").unwrap_or(3),
        )
    };

    // first pass: grow rows (relocating them) and change indexed values
    let mut by_id = rows_by_id(db, "Test")?;
    for id in [5i32, 20, 40] {
        let (row_id, mut values) = match by_id.get(&id) {
            Some(entry) => entry.clone(),
            None => {
                return Err(jackcess::Error::Jackcess(format!(
                    "rowops scenario: row {} not found",
                    id
                )))
            }
        };
        match id {
            5 => {
                values[name_col] = Value::Text("renamed5".to_string());
                values[num_col] = Value::Long(99);
                values[note_col] = Value::Text("g".repeat(50));
            }
            20 => {
                values[num_col] = Value::Long(12345);
                values[note_col] = Value::Text("h".repeat(55));
            }
            _ => {
                values[name_col] = Value::Text("other40".to_string());
                values[note_col] = Value::Text("x".to_string());
            }
        }
        db.update_row("Test", row_id, &values)?;
    }

    // second pass: update an already relocated row in place, grow one again and
    // delete a relocated row
    by_id = rows_by_id(db, "Test")?;
    for id in [20i32, 5] {
        let (row_id, mut values) = match by_id.get(&id) {
            Some(entry) => entry.clone(),
            None => {
                return Err(jackcess::Error::Jackcess(format!(
                    "rowops scenario: row {} not found",
                    id
                )))
            }
        };
        match id {
            20 => values[note_col] = Value::Text("hh".to_string()),
            _ => values[note_col] = Value::Text("k".repeat(58)),
        }
        db.update_row("Test", row_id, &values)?;
    }
    if let Some((row_id, _)) = by_id.get(&5) {
        db.delete_row("Test", *row_id)?;
    }

    // delete from a table which has no index at all
    let plain = rows_by_id(db, "NoIdx")?;
    for id in [2i32, 4] {
        if let Some((row_id, _)) = plain.get(&id) {
            db.delete_row("NoIdx", *row_id)?;
        }
    }

    // more rows afterwards
    let rows = vec![
        vec![
            Value::Null,
            Value::Text("n60".to_string()),
            Value::Long(60),
            Value::Text("note60".to_string()),
        ],
        vec![
            Value::Null,
            Value::Text("n61".to_string()),
            Value::Long(61),
            Value::Text("note61".to_string()),
        ],
    ];
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// table mutation scenarios: adding columns and indexes to an existing table
// (`ColumnBuilder.addToTable` / `IndexBuilder.addToTable`).  jackcess 5.0.4 has
// no drop-column or drop-index API, so only adding is exercised.
// ---------------------------------------------------------------------------

fn make_addcol_table(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "T",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Value", DataType::Double),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn add_addcol_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 0..5 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("name{}", i)),
            Value::Double(1.5 * i as f64),
        ]);
    }
    add_rows_one_by_one(db, "T", &rows)?;
    Ok(())
}

/// `addcol`: add a LONG column to a table which already holds rows.
fn addcol(db: &mut Database) -> jackcess::Result<()> {
    make_addcol_table(db)?;
    add_addcol_rows(db)?;

    db.add_column("T", ColumnSpec::new("Extra", DataType::Long))?;

    let rows = vec![
        vec![
            Value::Null,
            Value::Text("extra0".to_string()),
            Value::Double(100.5),
            Value::Long(42),
        ],
        vec![
            Value::Null,
            Value::Text("extra1".to_string()),
            Value::Double(101.5),
            Value::Null,
        ],
        vec![
            Value::Null,
            Value::Text("extra2".to_string()),
            Value::Double(102.5),
            Value::Long(-7),
        ],
    ];
    add_rows_one_by_one(db, "T", &rows)?;
    Ok(())
}

fn make_addindex_table(db: &mut Database) -> jackcess::Result<()> {
    // no index at all, so adding the first one creates its backing index data
    let spec = TableSpec::new(
        "T",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(30),
            ColumnSpec::new("Num", DataType::Long),
            ColumnSpec::new("Note", DataType::Text).with_length(50),
        ],
    );
    db.create_table(&spec)?;
    Ok(())
}

fn add_addindex_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 0..30i32 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("name{}", i)),
            Value::Long((i * 37) % 11),
            Value::Text(format!("note {}", i)),
        ]);
    }
    add_rows_one_by_one(db, "T", &rows)?;
    Ok(())
}

/// `addindex`: a unique text index, a second unique index over the same column
/// (which shares the first one's backing index data) and a non-unique
/// descending numeric index.
fn addindex(db: &mut Database) -> jackcess::Result<()> {
    make_addindex_table(db)?;
    add_addindex_rows(db)?;

    db.add_index(
        "T",
        IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique(),
    )?;
    db.add_index(
        "T",
        IndexSpec::new("ByNameCopy", vec![("Name".to_string(), true)]).unique(),
    )?;
    db.add_index(
        "T",
        IndexSpec::new("ByNum", vec![("Num".to_string(), false)]),
    )?;
    db.add_index(
        "T",
        IndexSpec::primary_key("PrimaryKey", vec![("ID".to_string(), true)]),
    )?;

    let rows = vec![
        vec![
            Value::Null,
            Value::Text("after0".to_string()),
            Value::Long(500),
            Value::Text("last but one".to_string()),
        ],
        vec![
            Value::Null,
            Value::Text("after1".to_string()),
            Value::Long(500),
            Value::Text("last".to_string()),
        ],
    ];
    add_rows_one_by_one(db, "T", &rows)?;
    Ok(())
}

/// `addboth`: add a LONG column, index it, then add a MEMO column (whose usage
/// maps grow the definition a second time) and add rows, all inside one open of
/// the file.
fn addboth(db: &mut Database) -> jackcess::Result<()> {
    make_table1(db)?;
    add_rows(db)?;

    db.add_column("Test", ColumnSpec::new("Extra", DataType::Long))?;
    db.add_index(
        "Test",
        IndexSpec::new("ByExtra", vec![("Extra".to_string(), true)]),
    )?;
    db.add_column("Test", ColumnSpec::new("Note", DataType::Memo))?;

    let long_memo = format!("LM:{}", "abcdefghij".repeat(40));
    let rows = vec![
        vec![
            Value::Null,
            Value::Text("both0".to_string()),
            Value::Double(10.5),
            Value::Long(7),
            Value::Text("short memo".to_string()),
        ],
        vec![
            Value::Null,
            Value::Text("both1".to_string()),
            Value::Double(11.5),
            Value::Null,
            Value::Text(long_memo),
        ],
        vec![
            Value::Null,
            Value::Text("both2".to_string()),
            Value::Double(12.5),
            Value::Long(-1),
            Value::Null,
        ],
        vec![
            Value::Null,
            Value::Text("both3".to_string()),
            Value::Double(13.5),
            Value::Long(100),
            Value::Text("memo 3".to_string()),
        ],
    ];
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// relationship scenarios (`Gen.java` "relate" and "samerel"): creating a
// relationship writes the `MSysRelationships` rows, a catalog row and the
// supporting foreign key indexes
// ---------------------------------------------------------------------------

/// The primary side of the `relate` relationship.
fn make_relate_p(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "P",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(40),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

/// The secondary (foreign key) side of the `relate` relationship.
fn make_relate_c(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
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
    db.create_table(&spec)?;
    Ok(())
}

fn add_relate_rows(db: &mut Database, from: i32, to: i32) -> jackcess::Result<()> {
    let mut p_rows = Vec::new();
    let mut c_rows = Vec::new();
    for i in from..to {
        p_rows.push(vec![Value::Null, Value::Text(format!("p{}", i))]);
        c_rows.push(vec![
            Value::Null,
            Value::Long((i % 3) + 1),
            Value::Text(format!("note {}", i)),
        ]);
    }
    add_rows_one_by_one(db, "P", &p_rows)?;
    add_rows_one_by_one(db, "C", &c_rows)?;
    Ok(())
}

/// `relate`: one relationship with referential integrity between two tables
/// which already hold rows, then more rows.
fn relate(db: &mut Database) -> jackcess::Result<()> {
    make_relate_p(db)?;
    make_relate_c(db)?;
    add_relate_rows(db, 0, 5)?;

    db.add_relationship(
        RelationshipSpec::new("P", "C")
            .add_columns("ID", "P_ID")
            .set_referential_integrity(),
    )?;

    add_relate_rows(db, 5, 8)?;
    Ok(())
}

/// The primary side of the one-to-one relationship (`D` has a unique index on
/// its foreign key column).
fn make_relate_n(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "N",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Code", DataType::Text).with_length(20),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn make_relate_d(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "D",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("N_ID", DataType::Long),
            ColumnSpec::new("Memo", DataType::Text).with_length(60),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
    .with_index(IndexSpec::new("ByNId", vec![("N_ID".to_string(), true)]).unique());
    db.create_table(&spec)?;
    Ok(())
}

/// `samerel`: two relationships in one open, the first cascading updates and
/// deletes, the second one-to-one.
fn samerel(db: &mut Database) -> jackcess::Result<()> {
    make_relate_p(db)?;
    make_relate_c(db)?;
    make_relate_n(db)?;
    make_relate_d(db)?;
    add_relate_rows(db, 0, 4)?;

    let mut n_rows = Vec::new();
    let mut d_rows = Vec::new();
    for i in 0..4i32 {
        n_rows.push(vec![Value::Null, Value::Text(format!("code{}", i))]);
        d_rows.push(vec![
            Value::Null,
            Value::Long(i + 1),
            Value::Text(format!("memo {}", i)),
        ]);
    }
    add_rows_one_by_one(db, "N", &n_rows)?;
    add_rows_one_by_one(db, "D", &d_rows)?;

    db.add_relationship(
        RelationshipSpec::new("P", "C")
            .add_columns("ID", "P_ID")
            .set_referential_integrity()
            .set_cascade_updates()
            .set_cascade_deletes(),
    )?;

    db.add_relationship(
        RelationshipSpec::new("N", "D")
            .add_columns("ID", "N_ID")
            .set_referential_integrity(),
    )?;

    add_relate_rows(db, 4, 6)?;
    add_rows_one_by_one(
        db,
        "N",
        &[vec![Value::Null, Value::Text("code4".to_string())]],
    )?;
    add_rows_one_by_one(
        db,
        "D",
        &[vec![
            Value::Null,
            Value::Long(5),
            Value::Text("memo 4".to_string()),
        ]],
    )?;
    Ok(())
}

/// `selfrel`: a table related to itself, so both supporting indexes live on the
/// same table (one index number apart).
fn selfrel(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "E",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Parent_ID", DataType::Long),
            ColumnSpec::new("Name", DataType::Text).with_length(30),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;

    let mut rows = Vec::new();
    for i in 0..4i32 {
        rows.push(vec![
            Value::Null,
            if i > 0 { Value::Long(i) } else { Value::Null },
            Value::Text(format!("e{}", i)),
        ]);
    }
    add_rows_one_by_one(db, "E", &rows)?;

    db.add_relationship(
        RelationshipSpec::new("E", "E")
            .add_columns("ID", "Parent_ID")
            .set_referential_integrity(),
    )?;

    add_rows_one_by_one(
        db,
        "E",
        &[vec![
            Value::Null,
            Value::Long(2),
            Value::Text("e4".to_string()),
        ]],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// linked tables (`Gen.java` "linked"): a catalog object of type 6 with the
// Database and ForeignName columns, access control entries and a synthetic
// object id
// ---------------------------------------------------------------------------

/// `linked`: a relationship (which uses the first synthetic object id) followed
/// by two linked tables (whose ids have to continue after it) and a normal
/// table.
fn linked(db: &mut Database) -> jackcess::Result<()> {
    make_relate_p(db)?;
    make_relate_c(db)?;

    db.add_relationship(
        RelationshipSpec::new("P", "C")
            .add_columns("ID", "P_ID")
            .set_referential_integrity(),
    )?;

    db.create_linked_table("L1", r"C:\data\one.mdb", "Remote1")?;
    db.create_linked_table("L2", r"C:\data\two.accdb", "Remote2")?;

    let spec = TableSpec::new(
        "After",
        vec![
            ColumnSpec::new("ID", DataType::Long),
            ColumnSpec::new("Label", DataType::Text).with_length(20),
        ],
    );
    db.create_table(&spec)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// free page handling (`Gen.java` "reclaim" and "reuse"):
// `Database.reclaimFreeSpace` / `Database.setReuseFreePages`
//
// Jet 4 / ACE pages are 4096 bytes and a primary key entry is a few bytes, so
// a leaf page holds ~300 entries: the row counts are chosen so that the index
// fills several leaf pages and so that the *last* page allocated by the
// inserts is an index leaf (which the deletions then empty, leaving a run of
// free pages at the end of the file).
// ---------------------------------------------------------------------------

/// Rows of the reclaim/reuse table.
const RECLAIM_ROWS: i32 = 905;
/// Rows which survive the deletions (the low primary keys).
const RECLAIM_KEEP: i32 = 10;
/// Rows added after the deletions by the reuse scenario.
const REUSE_NEW_ROWS: i32 = 300;

fn make_reclaim_table(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn add_reclaim_rows(db: &mut Database) -> jackcess::Result<()> {
    // one `addRowFromMap` per row on the Java side, so one add per row here
    for i in 0..RECLAIM_ROWS {
        add_rows_one_by_one(
            db,
            "Test",
            &[vec![Value::Null, Value::Text(format!("name{}", i))]],
        )?;
    }
    Ok(())
}

/// Deletes the rows `RECLAIM_KEEP+1..=RECLAIM_ROWS` through the primary key
/// (the Rust side has no index cursor, so the rows are looked up by their `ID`
/// value and deleted by id).
fn delete_reclaim_rows(db: &mut Database) -> jackcess::Result<()> {
    let by_id = rows_by_id(db, "Test")?;
    for id in (RECLAIM_KEEP + 1)..=RECLAIM_ROWS {
        match by_id.get(&id) {
            Some((row_id, _)) => db.delete_row("Test", *row_id)?,
            None => {
                return Err(jackcess::Error::Jackcess(format!(
                    "reclaim scenario: row {} not found",
                    id
                )))
            }
        }
    }
    Ok(())
}

fn reclaim(db: &mut Database) -> jackcess::Result<()> {
    make_reclaim_table(db)?;
    add_reclaim_rows(db)?;
    delete_reclaim_rows(db)?;
    let reclaimed = db.reclaim_free_space()?;
    println!("reclaimed {} pages", reclaimed);
    Ok(())
}

fn reuse(db: &mut Database) -> jackcess::Result<()> {
    make_reclaim_table(db)?;
    add_reclaim_rows(db)?;
    delete_reclaim_rows(db)?;

    db.set_reuse_free_pages(true);

    // one `addRowFromMap` per row on the Java side, so one add per row here
    for i in 0..REUSE_NEW_ROWS {
        add_rows_one_by_one(
            db,
            "Test",
            &[vec![Value::Null, Value::Text(format!("new{}", i))]],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// explicit auto-number insertion (`Gen.java` "autonum"):
// `Database.setAllowAutoNumberInsert` and `handleAutoNumbersForAdd`
// ---------------------------------------------------------------------------

fn autonum(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;

    db.set_allow_auto_number_insert(true);

    // explicit values, including one below the highest one used
    for id in [5i32, 100, 3, 7] {
        add_rows_one_by_one(
            db,
            "Test",
            &[vec![
                Value::Long(id),
                Value::Text(format!("explicit{}", id)),
            ]],
        )?;
    }

    // generated values continue after the highest explicit one (100)
    let mut rows = Vec::new();
    for i in 0..4 {
        rows.push(vec![Value::Null, Value::Text(format!("auto{}", i))]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;

    // deleting a row does not move the high water mark
    let by_id = rows_by_id(db, "Test")?;
    match by_id.get(&102) {
        Some((row_id, _)) => db.delete_row("Test", *row_id)?,
        None => {
            return Err(jackcess::Error::Jackcess(
                "autonum scenario: row 102 not found".into(),
            ))
        }
    }

    add_rows_one_by_one(
        db,
        "Test",
        &[vec![Value::Null, Value::Text("after".to_string())]],
    )?;

    // the same without any index on the auto-number column
    let nokey = TableSpec::new(
        "NoKey",
        vec![
            ColumnSpec::new("Num", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Label", DataType::Text).with_length(20),
        ],
    );
    db.create_table(&nokey)?;
    let rows = vec![
        vec![Value::Long(10), Value::Text("ten".to_string())],
        vec![Value::Null, Value::Text("auto0".to_string())],
        vec![Value::Long(4), Value::Text("four".to_string())],
        vec![Value::Null, Value::Text("auto1".to_string())],
    ];
    add_rows_one_by_one(db, "NoKey", &rows)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// explicit GUID auto-number insertion (`Gen.java` "guidx"):
// `Database.setAllowAutoNumberInsert` + `GuidAutoNumberGenerator.handleInsert`.
// A GUID auto number is random, so the byte identical scenario can only insert
// explicit values; the generated values are covered behaviourally instead
// (`src/bin/guidauto.rs` and `tools/verify-guid.ps1`).
// ---------------------------------------------------------------------------

/// The page layout of a canonical GUID literal (the first three components are
/// integer fields and are swapped for the little endian Jet4/ACE page).
fn stored_guid(literal: &str) -> [u8; 16] {
    let mut bytes = jackcess::column::parse_guid(literal).expect("canonical GUID");
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    bytes
}

fn guidx(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Guid).with_auto_number(true),
            ColumnSpec::new("Ref", DataType::Guid),
            ColumnSpec::new("Plain", DataType::Guid),
            ColumnSpec::new("Label", DataType::Text).with_length(50),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;

    // explicit GUID insertion is off by default
    db.set_allow_auto_number_insert(true);

    // fixed values: braced / unbraced, upper / lower case, a raw byte GUID (the
    // value a read row holds) and an explicit null in the plain GUID column
    let rows = vec![
        vec![
            Value::Text("{00112233-4455-6677-8899-AABBCCDDEEFF}".to_string()),
            Value::Text("FFEEDDCC-BBAA-9988-7766-554433221100".to_string()),
            Value::Text("{12345678-90AB-CDEF-1234-567890ABCDEF}".to_string()),
            Value::Text("r1".to_string()),
        ],
        vec![
            Value::Text("aabbccdd-eeff-0011-2233-445566778899".to_string()),
            Value::Uuid(stored_guid("ABCDEF01-2345-6789-ABCD-EF0123456789")),
            Value::Text("00000000-0000-0000-0000-000000000000".to_string()),
            Value::Text("r2".to_string()),
        ],
        vec![
            Value::Text("DEADBEEF-DEAD-BEEF-DEAD-BEEFDEADBEEF".to_string()),
            Value::Text("{FFFFFFFF-EEEE-DDDD-CCCC-BBBBBBBBBBBB}".to_string()),
            Value::Null,
            Value::Text("r3".to_string()),
        ],
        vec![
            Value::Text("01234567-89AB-CDEF-0123-456789ABCDEF".to_string()),
            Value::Text("FEDCBA98-7654-3210-FEDC-BA9876543210".to_string()),
            Value::Text("A1B2C3D4-E5F6-7890-ABCD-EF0123456789".to_string()),
            Value::Text("r4".to_string()),
        ],
    ];
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

/// `addmany`: add enough columns to push the table definition onto a second
/// page (the update path has to split and re-use the definition pages).
fn addmany(db: &mut Database) -> jackcess::Result<()> {
    make_addcol_table(db)?;
    add_addcol_rows(db)?;

    for i in 0..120 {
        let name = format!("C{:03}", i);
        db.add_column("T", ColumnSpec::new(name, DataType::Long))?;
    }

    let mut row = vec![Value::Null; 123];
    row[1] = Value::Text("m1".to_string());
    row[2] = Value::Double(50.5);
    row[3] = Value::Long(1); // C000
    row[3 + 119] = Value::Long(2); // C119
    let mut row2 = vec![Value::Null; 123];
    row2[1] = Value::Text("m2".to_string());
    row2[2] = Value::Double(51.5);
    row2[3 + 59] = Value::Long(-60); // C059
    add_rows_one_by_one(db, "T", &[row, row2])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// foreign key enforcement scenarios (`Gen.java` "fkadd", "fkdel" and "fkupd"):
// the relationships carry referential integrity and optionally cascade, so a
// row modification is checked, cascaded or rejected.  A rejected modification
// must leave the file unchanged and the next modification must still work.
// ---------------------------------------------------------------------------

/// The primary side of the foreign key scenarios (the key is not auto
/// numbered, so an update can change it).
fn make_fk_p(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
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
    db.create_table(&spec)?;
    Ok(())
}

/// The secondary side (ID is auto numbered, P_ID is the foreign key).
fn make_fk_c(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
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
    db.create_table(&spec)?;
    Ok(())
}

/// The primary side of the second (restricted) relationship.
fn make_fk_n(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "N",
        vec![
            ColumnSpec::new("ID", DataType::Long),
            ColumnSpec::new("Code", DataType::Text).with_length(20),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

/// The secondary side of the second relationship (N_ID is not unique, so two D
/// rows may name the same N row).
fn make_fk_d(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "D",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("N_ID", DataType::Long),
            ColumnSpec::new("Memo", DataType::Text).with_length(60),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn add_fk_p_row(db: &mut Database, id: i32) -> jackcess::Result<()> {
    add_rows_one_by_one(
        db,
        "P",
        &[vec![Value::Long(id), Value::Text(format!("p{}", id))]],
    )
}

fn add_fk_c_row(db: &mut Database, p_id: Option<i32>, note: &str) -> jackcess::Result<()> {
    add_rows_one_by_one(
        db,
        "C",
        &[vec![
            Value::Null,
            p_id.map(Value::Long).unwrap_or(Value::Null),
            Value::Text(note.to_string()),
        ]],
    )
}

fn add_fk_n_row(db: &mut Database, id: i32) -> jackcess::Result<()> {
    add_rows_one_by_one(
        db,
        "N",
        &[vec![Value::Long(id), Value::Text(format!("n{}", id))]],
    )
}

fn add_fk_d_row(db: &mut Database, n_id: i32, memo: &str) -> jackcess::Result<()> {
    add_rows_one_by_one(
        db,
        "D",
        &[vec![
            Value::Null,
            Value::Long(n_id),
            Value::Text(memo.to_string()),
        ]],
    )
}

/// `fkadd`: a referential integrity relationship, valid rows, an insert whose
/// foreign key has no parent (rejected), then more valid rows.
fn fkadd(db: &mut Database) -> jackcess::Result<()> {
    make_fk_p(db)?;
    make_fk_c(db)?;

    for i in 1..=4 {
        add_fk_p_row(db, i)?;
    }
    add_fk_c_row(db, Some(1), "c0")?;
    add_fk_c_row(db, Some(1), "c1")?;
    add_fk_c_row(db, Some(2), "c2")?;
    add_fk_c_row(db, Some(3), "c3")?;

    db.add_relationship(
        RelationshipSpec::new("P", "C")
            .add_columns("ID", "P_ID")
            .set_referential_integrity(),
    )?;

    add_fk_c_row(db, Some(4), "c4")?;
    add_fk_p_row(db, 5)?;

    // the foreign key names no parent row, so the insert is rejected
    match add_fk_c_row(db, Some(99), "bad") {
        Err(e) if e.is_constraint_violation() => println!("fkadd rejected"),
        Err(e) => return Err(e),
        Ok(()) => {
            return Err(jackcess::Error::IllegalState(
                "fkadd: insert was not rejected".into(),
            ))
        }
    }

    // the following valid rows still work, and the rejected insert consumed no
    // auto number
    add_fk_p_row(db, 6)?;
    add_fk_c_row(db, Some(6), "c6")?;
    add_fk_c_row(db, None, "c7")?;
    Ok(())
}

/// `fkdel`: a cascade delete relationship and a restricted one.  The cascade
/// removes the children of a deleted parent, the restricted one rejects the
/// delete, and a parent without children still goes away.
fn fkdel(db: &mut Database) -> jackcess::Result<()> {
    make_fk_p(db)?;
    make_fk_c(db)?;
    make_fk_n(db)?;
    make_fk_d(db)?;

    for i in 1..=4 {
        add_fk_p_row(db, i)?;
    }
    add_fk_c_row(db, Some(1), "c0")?;
    add_fk_c_row(db, Some(2), "c1")?;
    add_fk_c_row(db, Some(3), "c2")?;
    add_fk_c_row(db, Some(1), "c3")?;
    for i in 1..=4 {
        add_fk_n_row(db, i)?;
    }
    add_fk_d_row(db, 1, "d1")?;
    add_fk_d_row(db, 2, "d2")?;
    add_fk_d_row(db, 2, "d3")?;

    db.add_relationship(
        RelationshipSpec::new("P", "C")
            .add_columns("ID", "P_ID")
            .set_referential_integrity()
            .set_cascade_deletes(),
    )?;
    db.add_relationship(
        RelationshipSpec::new("N", "D")
            .add_columns("ID", "N_ID")
            .set_referential_integrity(),
    )?;

    // deleting the parent cascades to its two children
    let row_id = fk_row_id(db, "P", 1)?;
    db.delete_row("P", row_id)?;

    // a parent which a D row names cannot be deleted
    let row_id = fk_row_id(db, "N", 1)?;
    match db.delete_row("N", row_id) {
        Err(e) if e.is_constraint_violation() => println!("fkdel rejected"),
        Err(e) => return Err(e),
        Ok(()) => {
            return Err(jackcess::Error::IllegalState(
                "fkdel: delete was not rejected".into(),
            ))
        }
    }

    // a parent without children goes away
    let row_id = fk_row_id(db, "N", 3)?;
    db.delete_row("N", row_id)?;

    // a child row itself can be deleted freely
    let row_id = fk_row_id(db, "C", 2)?;
    db.delete_row("C", row_id)?;

    // and the database still accepts rows afterwards
    add_fk_p_row(db, 5)?;
    add_fk_c_row(db, Some(5), "c5")?;
    Ok(())
}

/// `fkupd`: a cascade update relationship and a restricted one.  The cascade
/// follows a parent key change into the children, the restricted one rejects
/// it, and a child may never name a missing parent.
fn fkupd(db: &mut Database) -> jackcess::Result<()> {
    make_fk_p(db)?;
    make_fk_c(db)?;
    make_fk_n(db)?;
    make_fk_d(db)?;

    for i in 1..=4 {
        add_fk_p_row(db, i)?;
    }
    add_fk_c_row(db, Some(1), "c0")?;
    add_fk_c_row(db, Some(1), "c1")?;
    add_fk_c_row(db, Some(2), "c2")?;
    add_fk_c_row(db, Some(3), "c3")?;
    for i in 1..=4 {
        add_fk_n_row(db, i)?;
    }
    add_fk_d_row(db, 1, "d1")?;
    add_fk_d_row(db, 2, "d2")?;
    add_fk_d_row(db, 2, "d3")?;
    add_fk_d_row(db, 4, "d4")?;

    db.add_relationship(
        RelationshipSpec::new("P", "C")
            .add_columns("ID", "P_ID")
            .set_referential_integrity()
            .set_cascade_updates(),
    )?;
    db.add_relationship(
        RelationshipSpec::new("N", "D")
            .add_columns("ID", "N_ID")
            .set_referential_integrity(),
    )?;

    // the parent key change follows into the two children
    let (row_id, mut values) = fk_row(db, "P", 1)?;
    values[0] = Value::Long(100);
    db.update_row("P", row_id, &values)?;

    // a child may not name a missing parent
    let (row_id, mut values) = fk_row(db, "C", 1)?;
    let p_id_col = db.get_table("C")?.column_index("P_ID").unwrap_or(1);
    values[p_id_col] = Value::Long(999);
    match db.update_row("C", row_id, &values) {
        Err(e) if e.is_constraint_violation() => println!("fkupd rejected child"),
        Err(e) => return Err(e),
        Ok(()) => {
            return Err(jackcess::Error::IllegalState(
                "fkupd: child update was not rejected".into(),
            ))
        }
    }

    // the restricted relationship refuses to change a named parent key
    let (row_id, mut values) = fk_row(db, "N", 1)?;
    values[0] = Value::Long(50);
    match db.update_row("N", row_id, &values) {
        Err(e) if e.is_constraint_violation() => println!("fkupd rejected parent"),
        Err(e) => return Err(e),
        Ok(()) => {
            return Err(jackcess::Error::IllegalState(
                "fkupd: parent update was not rejected".into(),
            ))
        }
    }

    // a parent key no child names may change
    let (row_id, mut values) = fk_row(db, "N", 3)?;
    values[0] = Value::Long(30);
    db.update_row("N", row_id, &values)?;

    // and so may a P key without children
    let (row_id, mut values) = fk_row(db, "P", 4)?;
    values[0] = Value::Long(40);
    db.update_row("P", row_id, &values)?;

    // more rows afterwards
    add_fk_p_row(db, 45)?;
    add_fk_c_row(db, Some(45), "c45")?;
    add_fk_c_row(db, Some(40), "c40")?;
    add_fk_n_row(db, 31)?;
    add_fk_d_row(db, 31, "d31")?;
    add_fk_d_row(db, 30, "d30")?;
    Ok(())
}

/// The id of the row whose `ID` column holds the given value.
fn fk_row_id(db: &mut Database, table: &str, id: i32) -> jackcess::Result<jackcess::table::RowId> {
    fk_row(db, table, id).map(|(row_id, _)| row_id)
}

/// The id and values of the row whose `ID` column holds the given value.
fn fk_row(
    db: &mut Database,
    table: &str,
    id: i32,
) -> jackcess::Result<(jackcess::table::RowId, Vec<Value>)> {
    let by_id = rows_by_id(db, table)?;
    match by_id.get(&id) {
        Some(entry) => Ok(entry.clone()),
        None => Err(jackcess::Error::Jackcess(format!(
            "row {} not found in {}",
            id, table
        ))),
    }
}

// ---------------------------------------------------------------------------
// column default values and property driven validators (`Gen.java`
// "defaults" and "required")
// ---------------------------------------------------------------------------

/// `defaults`: a table whose columns carry `DefaultValue` properties, rows
/// which omit the columns (defaults applied), rows with explicit values
/// (defaults ignored), rows with explicit nulls (defaults applied on add) and
/// an update which leaves most columns unchanged (an update never applies a
/// default).
fn defaults(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("L", DataType::Long)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("7".to_string()))?,
            // 1.5 in a LONG column is rounded half even to 2 by the Java
            // expression engine
            ColumnSpec::new("L15", DataType::Long)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("1.5".to_string()))?,
            ColumnSpec::new("D", DataType::Double)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("1.5".to_string()))?,
            ColumnSpec::new("T", DataType::Text)
                .with_length(20)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("\"abc\"".to_string()))?,
            ColumnSpec::new("U", DataType::Text)
                .with_length(20)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("unq".to_string()))?,
            ColumnSpec::new("M", DataType::Memo).put_property(
                DEFAULT_VALUE_PROP,
                Value::Text("\"memo default\"".to_string()),
            )?,
            ColumnSpec::new("B", DataType::Boolean)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("TRUE".to_string()))?,
            ColumnSpec::new("N", DataType::Long)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("-1".to_string()))?,
        ],
    );
    db.create_table(&spec)?;

    let rows = vec![
        // every default value
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
        ],
        // explicit values, the defaults are ignored
        vec![
            Value::Null,
            Value::Long(42),
            Value::Long(42),
            Value::Double(2.25),
            Value::Text("xyz".to_string()),
            Value::Text("u2".to_string()),
            Value::Text("custom".to_string()),
            Value::Bool(false),
            Value::Long(5),
        ],
        // explicit nulls, the defaults apply on add
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
        ],
    ];
    add_rows_one_by_one(db, "Test", &rows)?;

    // update the row with ID 2: an explicit null stays null (no default is
    // generated on update), one text value changes and every other column
    // keeps the value it already holds
    let rows = db.read_rows_with_ids("Test")?;
    for (row_id, values) in rows {
        let mut values = match values {
            Some(values) => values,
            None => continue,
        };
        if values[0] != Value::Long(2) {
            continue;
        }
        values[1] = Value::Null;
        values[4] = Value::Text("changed".to_string());
        db.update_row("Test", row_id, &values)?;
        break;
    }
    Ok(())
}

/// `noexpr`: the same kind of table as `defaults`/`required`, created after
/// `Database.setEvaluateExpressions(false)`.  Java does not apply the
/// `DefaultValue` properties (they are expressions) but still enforces the
/// `Required` and `AllowZeroLength` column validators, which are installed
/// before `ColumnImpl.reloadPropertiesValidators` checks the flag.
///
/// The rows store the explicit values exactly as given (nulls stay null),
/// while a default-enabled database would fill the `L`, `T` and `U` columns.
fn noexpr(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("L", DataType::Long)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("7".to_string()))?,
            ColumnSpec::new("Req", DataType::Long)
                .put_property(REQUIRED_PROP, Value::Bool(true))?,
            ColumnSpec::new("T", DataType::Text)
                .with_length(20)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("\"abc\"".to_string()))?
                .put_property(ALLOW_ZERO_LEN_PROP, Value::Bool(false))?,
            ColumnSpec::new("U", DataType::Text)
                .with_length(20)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("unq".to_string()))?,
            ColumnSpec::new("V", DataType::Text).with_length(20),
        ],
    );
    db.create_table(&spec)?;

    // row 1: only the required column; the default values are not applied
    add_rows_one_by_one(
        db,
        "Test",
        &[vec![
            Value::Null,
            Value::Null,
            Value::Long(1),
            Value::Null,
            Value::Null,
            Value::Null,
        ]],
    )?;
    // row 2: explicit values, including empty strings (which are stored as
    // such: the empty U must not be replaced by its default value)
    add_rows_one_by_one(
        db,
        "Test",
        &[vec![
            Value::Null,
            Value::Long(42),
            Value::Long(2),
            Value::Text("xyz".to_string()),
            Value::Text(String::new()),
            Value::Text(String::new()),
        ]],
    )?;
    // row 3: explicit nulls stay null (a default-enabled database would fill
    // L, T and U on insert)
    add_rows_one_by_one(
        db,
        "Test",
        &[vec![
            Value::Null,
            Value::Null,
            Value::Long(3),
            Value::Null,
            Value::Null,
            Value::Null,
        ]],
    )?;

    // the property driven validators are not gated by the expression policy
    let row_count = db.read_rows_with_ids("Test")?.len();
    let err = db
        .add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text("bad".to_string()),
                Value::Null,
                Value::Null,
            ]],
        )
        .expect_err("a null required value must still be rejected");
    assert!(
        err.to_string()
            .contains("Missing value for required column"),
        "unexpected required message: {}",
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
                Value::Null,
            ]],
        )
        .expect_err("an empty string must still be rejected");
    assert!(
        err.to_string()
            .contains("Zero length string is not allowed"),
        "unexpected zero length message: {}",
        err
    );
    assert_eq!(
        row_count,
        db.read_rows_with_ids("Test")?.len(),
        "the rejected inserts must not add a row"
    );
    Ok(())
}

/// `noexprcalc` (V2010+): calculated columns created after
/// `Database.setEvaluateExpressions(false)`.  Java builds no calculated column
/// evaluator, so a calculated column is never computed: a row which does not
/// mention it stores a null, and an explicitly supplied value is written as
/// given (still inside the calculated value wrapper).  This is the opposite of
/// the default policy, where the expression would overwrite the input.
fn noexprcalc(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("A", DataType::Long),
            ColumnSpec::new("C", DataType::Long).set_calculated_info("[A]+1"),
            ColumnSpec::new("S", DataType::Text).with_length(10),
            ColumnSpec::new("CS", DataType::Text)
                .with_length(20)
                .set_calculated_info("[S] & \"x\""),
        ],
    );
    db.create_table(&spec)?;

    add_rows_one_by_one(
        db,
        "Test",
        &[
            // C and CS would be 6 and "abcx" when evaluated
            vec![
                Value::Null,
                Value::Long(5),
                Value::Null,
                Value::Text("abc".to_string()),
                Value::Null,
            ],
            // the empty S is allowed (no AllowZeroLength property) and CS is
            // not evaluated to "x"
            vec![
                Value::Null,
                Value::Long(-3),
                Value::Null,
                Value::Text(String::new()),
                Value::Null,
            ],
            // an all-null row
            vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            // explicit values of the calculated columns are stored as given
            vec![
                Value::Null,
                Value::Long(7),
                Value::Long(99),
                Value::Text("q".to_string()),
                Value::Text("explicit".to_string()),
            ],
        ],
    )?;
    Ok(())
}

/// `tzutc` / `tzneg`: a table with two SHORT_DATE_TIME columns whose values
/// are `java.util.Date` instants.  Java writes the wall clock time of the
/// database zone (`LocalDateTime.ofInstant(instant, getZoneId())`), so the
/// Rust generator converts the same epoch milliseconds with the database
/// offset.  The catalog `DateCreate`/`DateUpdate` values of the new table use
/// the same zone (`Database::now_local_datetime`).
fn tz_rows(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("D1", DataType::ShortDateTime),
            ColumnSpec::new("D2", DataType::ShortDateTime),
        ],
    );
    db.create_table(&spec)?;
    let offset_ms = db.time_zone_offset_minutes() as i64 * 60_000;
    let base: i64 = 1_584_246_645_000; // 2020-03-15T04:30:45Z
    let rows: Vec<Vec<Value>> = [
        (base, base + 86_400_000),
        (base + 3_661_000, base + 86_399_999),
        (base - 86_400_000, base + 1),
    ]
    .iter()
    .map(|(d1, d2)| {
        vec![
            Value::Null,
            Value::DateTime(jackcess::datenums::LocalDateTime::from_epoch_millis(
                *d1 + offset_ms,
            )),
            Value::DateTime(jackcess::datenums::LocalDateTime::from_epoch_millis(
                *d2 + offset_ms,
            )),
        ]
    })
    .collect();
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

/// `required`: a table with a `Required` LONG column and a TEXT column with
/// `AllowZeroLength` turned off.  Valid rows are added, then an insert with a
/// null required value and one with an empty string are rejected with the
/// Java messages, leaving the table (and the auto number counter) unchanged;
/// more valid rows follow.
fn required(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Req", DataType::Long)
                .put_property(REQUIRED_PROP, Value::Bool(true))?,
            ColumnSpec::new("Name", DataType::Text)
                .with_length(30)
                .put_property(ALLOW_ZERO_LEN_PROP, Value::Bool(false))?,
        ],
    );
    db.create_table(&spec)?;

    add_rows_one_by_one(
        db,
        "Test",
        &[
            vec![Value::Null, Value::Long(1), Value::Text("a".to_string())],
            vec![Value::Null, Value::Long(2), Value::Text("b".to_string())],
        ],
    )?;

    let row_count = db.read_rows_with_ids("Test")?.len();

    // a null value in the required column is rejected
    let err = db
        .add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Null,
                Value::Text("bad".to_string()),
            ]],
        )
        .expect_err("a null required value must be rejected");
    let msg = err.to_string();
    println!("required: {}", msg);
    assert!(
        msg.contains("Missing value for required column") && msg.contains("Table=Test;Column=Req"),
        "unexpected required message: {}",
        msg
    );
    assert_eq!(
        row_count,
        db.read_rows_with_ids("Test")?.len(),
        "the rejected insert must not add a row"
    );

    // an empty string in the no-zero-length column is rejected
    let err = db
        .add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Long(99),
                Value::Text(String::new()),
            ]],
        )
        .expect_err("an empty string must be rejected");
    let msg = err.to_string();
    println!("zero length: {}", msg);
    assert!(
        msg.contains("Zero length string is not allowed") && msg.contains("Table=Test;Column=Name"),
        "unexpected zero length message: {}",
        msg
    );
    assert_eq!(
        row_count,
        db.read_rows_with_ids("Test")?.len(),
        "the rejected insert must not add a row"
    );

    // more valid rows (the failed inserts consumed no auto number)
    add_rows_one_by_one(
        db,
        "Test",
        &[
            vec![Value::Null, Value::Long(3), Value::Text("c".to_string())],
            vec![Value::Null, Value::Long(4), Value::Text("d".to_string())],
        ],
    )?;
    let ids: Vec<i64> = db
        .read_rows_with_ids("Test")?
        .into_iter()
        .filter_map(|(_, values)| values)
        .filter_map(|values| values.first().and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(ids, vec![1, 2, 3, 4], "unexpected auto numbers: {:?}", ids);
    Ok(())
}

// ---------------------------------------------------------------------------
// expression based validation rules (`Gen.java` "validate" and
// "validateupdate")
// ---------------------------------------------------------------------------

/// The table of the `validate` / `validateupdate` scenarios: a numeric field
/// rule with a `ValidationText`, a function based field rule without text (the
/// default Java message) and a two column row rule with its own text.
fn validate_table_spec() -> jackcess::Result<TableSpec> {
    TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Num", DataType::Long)
                .put_property(VALIDATION_RULE_PROP, Value::Text("[Num] >= 0".into()))?
                .put_property(VALIDATION_TEXT_PROP, Value::Text("Num must be >= 0".into()))?,
            ColumnSpec::new("Code", DataType::Text)
                .with_length(10)
                .put_property(VALIDATION_RULE_PROP, Value::Text("Len([Code]) = 3".into()))?,
            ColumnSpec::new("Start", DataType::ShortDateTime),
            ColumnSpec::new("End", DataType::ShortDateTime),
        ],
    )
    .put_property(VALIDATION_RULE_PROP, Value::Text("[Start] <= [End]".into()))?
    .put_property(
        VALIDATION_TEXT_PROP,
        Value::Text("Start must not be after End".into()),
    )
}

fn validate_date(month: u32, day: u32) -> Value {
    Value::DateTime(jackcess::datenums::LocalDateTime::new(
        2020, month, day, 0, 0, 0, 0,
    ))
}

fn validate_row(num: i32, code: &str, start: Value, end: Value) -> Vec<Value> {
    vec![
        Value::Null,
        Value::Long(num),
        Value::Text(code.to_string()),
        start,
        end,
    ]
}

fn validate_find_row(
    db: &mut Database,
    id: i32,
) -> jackcess::Result<(jackcess::table::RowId, Vec<Value>)> {
    for (row_id, values) in db.read_rows_with_ids("Test")? {
        if let Some(values) = values {
            if values.first() == Some(&Value::Long(id)) {
                return Ok((row_id, values));
            }
        }
    }
    Err(jackcess::Error::IllegalState(format!(
        "validate: row {} not found",
        id
    )))
}

fn validate_column_message(db: &Database, text: &str, column: &str) -> String {
    format!("{} (Db={};Table=Test;Column={})", text, db.name, column)
}

fn validate_table_message(db: &Database, text: &str) -> String {
    format!("{} (Db={};Table=Test)", text, db.name)
}

/// Snapshots the database file after flushing every dirty page, so a rejected
/// operation can be shown to leave it byte for byte identical.
fn validate_snapshot(db: &mut Database, path: &str) -> jackcess::Result<Vec<u8>> {
    db.flush()?;
    Ok(std::fs::read(path)?)
}

/// `validate`: valid inserts, rejected inserts and updates (the field rules
/// and the row rule), then more valid rows.  The rejected operations must
/// leave the file byte identical and consume no auto number.
fn validate(db: &mut Database, path: &str) -> jackcess::Result<()> {
    let spec = validate_table_spec()?;
    db.create_table(&spec)?;

    let d1 = validate_date(1, 1);
    let d2 = validate_date(1, 2);
    let d5 = validate_date(1, 5);
    add_rows_one_by_one(
        db,
        "Test",
        &[
            validate_row(1, "abc", d1.clone(), d2.clone()),
            validate_row(0, "xyz", d5.clone(), d5.clone()),
        ],
    )?;

    let before = validate_snapshot(db, path)?;

    // field rule (with ValidationText): a negative Num
    let err = db
        .add_rows("Test", &[validate_row(-5, "abc", d1.clone(), d2.clone())])
        .expect_err("a negative Num must be rejected");
    let msg = err.to_string();
    println!("validate num: {}", msg);
    assert_eq!(
        msg,
        validate_column_message(db, "Num must be >= 0", "Num"),
        "unexpected Num rule message"
    );

    // field rule with a function (default message): Code is 2 characters
    let err = db
        .add_rows("Test", &[validate_row(5, "ab", d1.clone(), d2.clone())])
        .expect_err("a 2 character Code must be rejected");
    let msg = err.to_string();
    println!("validate code: {}", msg);
    assert_eq!(
        msg,
        validate_column_message(db, "Invalid column value 'ab'", "Code"),
        "unexpected Code rule message"
    );

    // row rule (with ValidationText): Start is after End
    let err = db
        .add_rows("Test", &[validate_row(5, "abc", d2.clone(), d1.clone())])
        .expect_err("Start after End must be rejected");
    let msg = err.to_string();
    println!("validate row add: {}", msg);
    assert_eq!(
        msg,
        validate_table_message(db, "Start must not be after End"),
        "unexpected row rule message"
    );

    // a rejected update of the first row's Num
    let (row_id, mut row) = validate_find_row(db, 1)?;
    row[1] = Value::Long(-1);
    let err = db
        .update_row("Test", row_id, &row)
        .expect_err("a negative Num update must be rejected");
    let msg = err.to_string();
    println!("validate num update: {}", msg);
    assert_eq!(
        msg,
        validate_column_message(db, "Num must be >= 0", "Num"),
        "unexpected Num update message"
    );

    // a rejected update which only the row rule rejects (Start > End)
    let (row_id, mut row) = validate_find_row(db, 2)?;
    row[3] = validate_date(6, 1);
    row[4] = validate_date(1, 1);
    let err = db
        .update_row("Test", row_id, &row)
        .expect_err("a Start after End update must be rejected");
    let msg = err.to_string();
    println!("validate row update: {}", msg);
    assert_eq!(
        msg,
        validate_table_message(db, "Start must not be after End"),
        "unexpected row update message"
    );

    let after = validate_snapshot(db, path)?;
    assert_eq!(
        before, after,
        "a rejected operation changed the database file"
    );

    // more valid rows (the rejected inserts consumed no auto number)
    add_rows_one_by_one(
        db,
        "Test",
        &[
            validate_row(2, "def", validate_date(4, 1), validate_date(4, 1)),
            validate_row(3, "ghi", validate_date(4, 2), validate_date(4, 3)),
        ],
    )?;

    // valid updates
    let (row_id, mut row) = validate_find_row(db, 1)?;
    row[2] = Value::Text("new".into());
    db.update_row("Test", row_id, &row)?;
    let (row_id, mut row) = validate_find_row(db, 1)?;
    row[1] = Value::Long(7);
    db.update_row("Test", row_id, &row)?;

    let ids: Vec<i64> = db
        .read_rows_with_ids("Test")?
        .into_iter()
        .filter_map(|(_, values)| values)
        .filter_map(|values| values.first().and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(ids, vec![1, 2, 3, 4], "unexpected auto numbers: {:?}", ids);
    let (_, row1) = validate_find_row(db, 1)?;
    assert_eq!(row1[1], Value::Long(7), "valid Num update not stored");
    assert_eq!(
        row1[2],
        Value::Text("new".into()),
        "valid Code update not stored"
    );
    Ok(())
}

/// `validateupdate`: valid updates, rejected field rule and row rule updates
/// (the row rule violation depends on two columns), then more valid updates
/// and a final insert.
fn validateupdate(db: &mut Database, path: &str) -> jackcess::Result<()> {
    let spec = validate_table_spec()?;
    db.create_table(&spec)?;
    add_rows_one_by_one(
        db,
        "Test",
        &[
            validate_row(1, "abc", validate_date(1, 1), validate_date(1, 2)),
            validate_row(2, "def", validate_date(2, 1), validate_date(2, 2)),
            validate_row(3, "ghi", validate_date(3, 1), validate_date(3, 2)),
        ],
    )?;

    // valid updates: the numeric rule, the Len rule and the row rule all
    // accept these
    let (row_id, mut row) = validate_find_row(db, 1)?;
    row[1] = Value::Long(10);
    db.update_row("Test", row_id, &row)?;
    let (row_id, mut row) = validate_find_row(db, 2)?;
    row[2] = Value::Text("xyz".into());
    db.update_row("Test", row_id, &row)?;
    let (row_id, mut row) = validate_find_row(db, 3)?;
    row[3] = validate_date(3, 5);
    row[4] = validate_date(3, 10);
    db.update_row("Test", row_id, &row)?;

    let before = validate_snapshot(db, path)?;

    // field rule: a negative Num
    let (row_id, mut row) = validate_find_row(db, 1)?;
    row[1] = Value::Long(-10);
    let err = db
        .update_row("Test", row_id, &row)
        .expect_err("a negative Num update must be rejected");
    let msg = err.to_string();
    println!("validateupdate num: {}", msg);
    assert_eq!(
        msg,
        validate_column_message(db, "Num must be >= 0", "Num"),
        "unexpected Num update message"
    );

    // field rule: a Code which is not 3 characters (default message)
    let (row_id, mut row) = validate_find_row(db, 2)?;
    row[2] = Value::Text("toolong".into());
    let err = db
        .update_row("Test", row_id, &row)
        .expect_err("a long Code update must be rejected");
    let msg = err.to_string();
    println!("validateupdate code: {}", msg);
    assert_eq!(
        msg,
        validate_column_message(db, "Invalid column value 'toolong'", "Code"),
        "unexpected Code update message"
    );

    // row rule: Start > End, a violation which depends on two columns
    let (row_id, mut row) = validate_find_row(db, 3)?;
    row[3] = validate_date(3, 20);
    row[4] = validate_date(3, 15);
    let err = db
        .update_row("Test", row_id, &row)
        .expect_err("a Start after End update must be rejected");
    let msg = err.to_string();
    println!("validateupdate row: {}", msg);
    assert_eq!(
        msg,
        validate_table_message(db, "Start must not be after End"),
        "unexpected row update message"
    );

    let after = validate_snapshot(db, path)?;
    assert_eq!(before, after, "a rejected update changed the database file");

    // more valid updates and one valid insert
    let (row_id, mut row) = validate_find_row(db, 1)?;
    row[1] = Value::Long(11);
    row[2] = Value::Text("zzz".into());
    db.update_row("Test", row_id, &row)?;
    add_rows_one_by_one(
        db,
        "Test",
        &[validate_row(
            4,
            "jkl",
            validate_date(4, 1),
            validate_date(4, 2),
        )],
    )?;

    let (_, row1) = validate_find_row(db, 1)?;
    assert_eq!(row1[1], Value::Long(11), "valid Num update not stored");
    assert_eq!(row1[2], Value::Text("zzz".into()));
    let (_, row3) = validate_find_row(db, 3)?;
    assert_eq!(row3[3], validate_date(3, 5));
    assert_eq!(row3[4], validate_date(3, 10));
    assert_eq!(db.read_rows_with_ids("Test")?.len(), 4);
    Ok(())
}

// ---------------------------------------------------------------------------
// property persistence scenarios (`Gen.java` "dbprops", "tableprops" and
// "propupdate"): database/summary/user defined properties applied at create
// time, table and column creation time properties, and property updates and
// removals which are saved back into the catalog row's property blob
// ---------------------------------------------------------------------------

/// The database/summary/user defined properties of the `dbprops` scenario,
/// the equivalent of `DatabaseBuilder.putDatabaseProperty` and friends.
fn database_properties() -> DatabaseProperties {
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

/// `tableprops`: a table with a table property and column properties, rows,
/// then a changed table property, a removed column property and a changed
/// column property, saved again.
fn tableprops(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text)
                .with_length(50)
                .put_property(DESCRIPTION_PROP, Value::Text("the name column".to_string()))?
                .put_property(FORMAT_PROP, Value::Text("@".to_string()))?,
            ColumnSpec::new("Value", DataType::Double)
                .put_property(DEFAULT_VALUE_PROP, Value::Text("1.5".to_string()))?,
            ColumnSpec::new("Note", DataType::Text)
                .with_length(30)
                .put_property(DESCRIPTION_PROP, Value::Text("the note column".to_string()))?,
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
    .put_property(DESCRIPTION_PROP, Value::Text("the test table".to_string()))?
    // a MEMO property whose value does not fit into one long value row, so the
    // table's property blob is stored across several long value pages
    .put_property_typed(
        ROW_SOURCE_PROP,
        DataType::Memo,
        Value::Text("abcdefghij".repeat(600)),
    );
    db.create_table(&spec)?;

    let mut rows = Vec::new();
    for i in 0..3i64 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("name{}", i)),
            Value::Double(1.5 + i as f64),
            Value::Text(format!("note {}", i)),
        ]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;

    // change a table property, remove a column property, change another column
    // property and save the whole group again
    let mut maps = db.get_table_properties_by_name("Test")?;
    maps.get_default().put_value(
        DESCRIPTION_PROP,
        Value::Text("the changed table".to_string()),
    )?;
    maps.get_mut("Name").remove(FORMAT_PROP);
    maps.get_mut("Note")
        .put_value(DESCRIPTION_PROP, Value::Text("note changed".to_string()))?;
    maps.save(db)?;
    Ok(())
}

/// `summaryprops`: change and remove properties of the database, summary and
/// user defined groups of an existing database, all inside one open of the
/// file (the groups live in the template's `MSysDb`, `SummaryInfo` and
/// `UserDefined` catalog rows).
fn summaryprops(db: &mut Database) -> jackcess::Result<()> {
    // database properties: change one value and remove another
    let mut props = db.get_database_properties()?;
    props
        .get_default()
        .put_value(TITLE_PROP, Value::Text("Changed DB title".to_string()))?;
    props.get_default().remove("NavPane Width");
    props.save(db)?;

    // summary properties: change the title and remove the author
    let mut props = db.get_summary_properties()?;
    props
        .get_default()
        .put_value(TITLE_PROP, Value::Text("The summary title".to_string()))?;
    props.get_default().remove(AUTHOR_PROP);
    props.save(db)?;

    // user defined properties: drop the template's property and add two
    let mut props = db.get_user_defined_properties()?;
    props.get_default().remove(REPLICATE_PROJECT_PROP);
    props.get_default().put_value("MyFlag", Value::Bool(true))?;
    props
        .get_default()
        .put_value("MyText", Value::Text("user text".to_string()))?;
    props.save(db)?;
    Ok(())
}

/// The table of the `propupdate` scenario (no properties yet).
fn make_propupdate_table(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Value", DataType::Double),
            ColumnSpec::new("Note", DataType::Text).with_length(30),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    Ok(())
}

fn add_propupdate_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 0..3i64 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("name{}", i)),
            Value::Double(1.5 + i as f64),
            Value::Text(format!("note {}", i)),
        ]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;
    Ok(())
}

/// `propupdate`: create a plain table with rows and close, reopen and set a
/// table and a column description, save, then reopen again and change/remove
/// them and save.
fn propupdate(
    path: &std::path::Path,
    fmt: FileFormat,
    now_ms: Option<i64>,
    now_queue: &[i64],
) -> jackcess::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    {
        let mut db = Database::create(fmt, path)?;
        db.now_ms = now_ms;
        db.now_queue = now_queue.to_vec();
        make_propupdate_table(&mut db)?;
        add_propupdate_rows(&mut db)?;
        db.flush()?;
    }
    {
        let mut db = Database::open_path(path)?;
        let mut maps = db.get_table_properties_by_name("Test")?;
        maps.get_default().put_value(
            DESCRIPTION_PROP,
            Value::Text("a table description".to_string()),
        )?;
        maps.get_mut("Name")
            .put_value(DESCRIPTION_PROP, Value::Text("the name column".to_string()))?;
        maps.save(&mut db)?;
        db.flush()?;
    }
    {
        let mut db = Database::open_path(path)?;
        let mut maps = db.get_table_properties_by_name("Test")?;
        maps.get_default().put_value(
            DESCRIPTION_PROP,
            Value::Text("the new table description".to_string()),
        )?;
        maps.get_mut("Name").remove(DESCRIPTION_PROP);
        maps.save(&mut db)?;
        db.flush()?;
    }
    println!(
        "wrote {} size={}",
        path.display(),
        std::fs::metadata(path)?.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// complex column scenarios (`Gen.java` "multivalue" and "attachment")
// ---------------------------------------------------------------------------

/// Adds one row whose complex columns have no values yet and returns its row
/// id (the physical position of its header row).
fn add_complex_row(db: &mut Database, table: &str, name: &str) -> jackcess::Result<(i32, i32)> {
    add_rows_one_by_one(
        db,
        table,
        &[vec![Value::Null, Value::Text(name.to_string())]],
    )?;
    let rows = db.read_rows_with_ids(table)?;
    rows.last()
        .map(|(row_id, _)| *row_id)
        .ok_or_else(|| jackcess::Error::IllegalState("row was not added".into()))
}

fn row_id_by_id(db: &mut Database, table: &str, id: i32) -> jackcess::Result<(i32, i32)> {
    let rows = db.read_rows_with_ids(table)?;
    for (row_id, values) in rows {
        if values
            .as_ref()
            .and_then(|v| v.first())
            .and_then(|v| v.as_i64())
            == Some(id as i64)
        {
            return Ok(row_id);
        }
    }
    Err(jackcess::Error::IllegalState(format!(
        "row with id {} not found",
        id
    )))
}

fn multivalue(db: &mut Database) -> jackcess::Result<()> {
    // the row source of the second multi-value column
    let lookup = TableSpec::new(
        "Lookup",
        vec![ColumnSpec::new("Code", DataType::Text).with_length(20)],
    );
    db.create_table(&lookup)?;
    for code in ["x", "y", "z"] {
        add_rows_one_by_one(db, "Lookup", &[vec![Value::Text(code.to_string())]])?;
    }

    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(40),
            ColumnSpec::new("Tags", DataType::Text)
                .set_multi_value(DataType::Text)
                .set_value_list(&["a", "b", "c"]),
            ColumnSpec::new("Codes", DataType::Text)
                .set_multi_value(DataType::Text)
                .set_row_source("Lookup", 1),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;

    let mut row_ids: Vec<(String, (i32, i32))> = Vec::new();
    for name in ["zero", "one", "three"] {
        let row_id = add_complex_row(db, "Test", name)?;
        row_ids.push((name.to_string(), row_id));
    }

    // the table is walked in row order, so the values are added in that order
    // (the flat table's ids depend on it)
    for (name, row_id) in row_ids.clone() {
        if name == "one" {
            let mut tags = db.complex_value_fk("Test", row_id, "Tags")?;
            tags.add_multi_value(Value::Text("a".to_string()))?;
            let mut codes = db.complex_value_fk("Test", row_id, "Codes")?;
            codes.add_multi_value(Value::Text("x".to_string()))?;
        } else if name == "three" {
            let mut tags = db.complex_value_fk("Test", row_id, "Tags")?;
            for v in ["a", "b", "c"] {
                tags.add_multi_value(Value::Text(v.to_string()))?;
            }
            let mut codes = db.complex_value_fk("Test", row_id, "Codes")?;
            for v in ["x", "y"] {
                codes.add_multi_value(Value::Text(v.to_string()))?;
            }
        }
    }

    // change the first value of the row with three values, remove the second
    // and add a fourth
    if let Some((_, (p, r))) = row_ids.iter().find(|(name, _)| name == "three") {
        let mut tags = db.complex_value_fk("Test", (*p, *r), "Tags")?;
        let values = tags.multi_values()?;
        let mut first = values[0].clone();
        first.value = Value::Text("A".to_string());
        tags.update_multi_value(&first)?;
        tags.delete_value(&values[1])?;
        tags.add_multi_value(Value::Text("d".to_string()))?;
    }

    // delete one row (its values stay behind, like jackcess)
    let row_id = row_id_by_id(db, "Test", 2)?;
    db.delete_row("Test", row_id)?;
    Ok(())
}

fn attachment_bytes(len: usize, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32 * 7 + seed) & 0xFF) as u8)
        .collect()
}

fn attachment(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(40),
            ColumnSpec::new("Files", DataType::Text).set_attachment(),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;

    let mut row_ids = std::collections::HashMap::new();
    for name in ["zero", "small", "big", "third"] {
        let row_id = add_complex_row(db, "Test", name)?;
        row_ids.insert(name.to_string(), row_id);
    }

    let ts_small = jackcess::datenums::LocalDateTime::new(2021, 5, 4, 12, 34, 56, 0);
    let ts_big = jackcess::datenums::LocalDateTime::new(2021, 6, 7, 8, 9, 10, 0);
    let ts_two = jackcess::datenums::LocalDateTime::new(2021, 7, 8, 9, 10, 11, 0);
    let ts_third = jackcess::datenums::LocalDateTime::new(2021, 8, 9, 10, 11, 12, 0);

    if let Some(&(p, r)) = row_ids.get("small") {
        let mut files = db.complex_value_fk("Test", (p, r), "Files")?;
        files.add_attachment(
            None,
            Some("small.png"),
            Some("png"),
            &attachment_bytes(40, 1),
            Some(ts_small),
            Some(0),
        )?;
    }
    let mut big_attachment = None;
    if let Some(&(p, r)) = row_ids.get("big") {
        let mut files = db.complex_value_fk("Test", (p, r), "Files")?;
        big_attachment = Some(files.add_attachment(
            None,
            Some("big.png"),
            Some("png"),
            &attachment_bytes(6000, 2),
            Some(ts_big),
            Some(0),
        )?);
        files.add_attachment(
            None,
            Some("two.jpg"),
            Some("jpg"),
            &attachment_bytes(300, 3),
            Some(ts_two),
            Some(0),
        )?;
    }
    if let Some(&(p, r)) = row_ids.get("third") {
        let mut files = db.complex_value_fk("Test", (p, r), "Files")?;
        files.add_attachment(
            None,
            Some("third.zip"),
            Some("zip"),
            &attachment_bytes(10, 4),
            Some(ts_third),
            Some(0),
        )?;
    }

    // rename and rewrite the big attachment
    if let (Some(&(p, r)), Some(mut att)) = (row_ids.get("big"), big_attachment) {
        att.name = Some("renamed.png".to_string());
        att.data = Some(attachment_bytes(7000, 5));
        let mut files = db.complex_value_fk("Test", (p, r), "Files")?;
        files.update_attachment(&att)?;
    }

    // delete the small attachment
    if let Some(&(p, r)) = row_ids.get("small") {
        let mut files = db.complex_value_fk("Test", (p, r), "Files")?;
        let attachments = files.attachments()?;
        if let Some(a) = attachments.first().and_then(|v| v.attachment.as_ref()) {
            files.delete_attachment(a)?;
        }
    }

    // delete the row which holds the third attachment
    let row_id = row_id_by_id(db, "Test", 4)?;
    db.delete_row("Test", row_id)?;
    Ok(())
}

/// `addcomplex`: adds a multi-value and an attachment column to a table which
/// already holds rows (`ColumnBuilder.addToTable`).
fn addcomplex(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(40),
        ],
    );
    db.create_table(&spec)?;
    for name in ["a", "b", "c"] {
        add_rows_one_by_one(
            db,
            "Test",
            &[vec![Value::Null, Value::Text(name.to_string())]],
        )?;
    }

    db.add_column(
        "Test",
        ColumnSpec::new("Tags", DataType::Text)
            .set_multi_value(DataType::Text)
            .set_value_list(&["a", "b"]),
    )?;
    let rows = db.read_rows_with_ids("Test")?;
    for (row_id, values) in rows {
        let values = match values {
            Some(v) => v,
            None => continue,
        };
        let id = values[0].as_i64().unwrap_or(0);
        let mut tags = db.complex_value_fk("Test", row_id, "Tags")?;
        if id == 1 {
            tags.add_multi_value(Value::Text("a".to_string()))?;
        } else if id == 3 {
            tags.add_multi_value(Value::Text("b".to_string()))?;
            tags.add_multi_value(Value::Text("a".to_string()))?;
        }
    }

    db.add_column(
        "Test",
        ColumnSpec::new("Files", DataType::Text).set_attachment(),
    )?;
    let rows = db.read_rows_with_ids("Test")?;
    for (row_id, values) in rows {
        let values = match values {
            Some(v) => v,
            None => continue,
        };
        if values[0].as_i64() != Some(2) {
            continue;
        }
        let mut files = db.complex_value_fk("Test", row_id, "Files")?;
        files.add_attachment(
            None,
            Some("added.png"),
            Some("png"),
            &attachment_bytes(64, 9),
            Some(jackcess::datenums::LocalDateTime::new(
                2022, 1, 2, 3, 4, 5, 0,
            )),
            Some(0),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// version history (append-only memo) scenarios (`Gen.java` "versionhistory"
// and "vhadd")
// ---------------------------------------------------------------------------

/// The explicit modified date of a version (the scenario uses fixed dates so
/// the files are comparable).
fn version_date(
    year: i32,
    month: u32,
    day: u32,
    h: u32,
    m: u32,
    s: u32,
) -> jackcess::datenums::LocalDateTime {
    jackcess::datenums::LocalDateTime::new(year, month, day, h, m, s, 0)
}

/// `versionhistory`: an append-only MEMO column, three versions of one row, an
/// update (which records a new version), the row's deletion and a second row.
fn versionhistory(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(40),
            ColumnSpec::new("Body", DataType::Memo).set_append_only(true),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;

    add_rows_one_by_one(
        db,
        "Test",
        &[vec![
            Value::Null,
            Value::Text("first".to_string()),
            Value::Text("initial body".to_string()),
        ]],
    )?;

    let row_id = row_id_by_id(db, "Test", 1)?;
    {
        let mut vh = db.version_history_fk("Test", row_id, "Body")?;
        vh.add_version("v1 body", version_date(2021, 1, 2, 3, 4, 5))?;
        vh.add_version("v2 body", version_date(2021, 2, 3, 4, 5, 6))?;
        vh.add_version("v3 body", version_date(2021, 3, 4, 5, 6, 7))?;
    }

    // updating the memo column records another version
    let mut values = db
        .read_rows_with_ids("Test")?
        .into_iter()
        .find(|(rid, _)| *rid == row_id)
        .and_then(|(_, values)| values)
        .ok_or_else(|| jackcess::Error::IllegalState("row was not found".into()))?;
    values[2] = Value::Text("updated body".to_string());
    db.update_row("Test", row_id, &values)?;
    {
        let mut vh = db.version_history_fk("Test", row_id, "Body")?;
        vh.add_version("updated body", version_date(2021, 4, 5, 6, 7, 8))?;
    }

    // jackcess does not delete the versions of a deleted row
    db.delete_row("Test", row_id)?;

    add_rows_one_by_one(
        db,
        "Test",
        &[vec![
            Value::Null,
            Value::Text("second".to_string()),
            Value::Text("second body".to_string()),
        ]],
    )?;
    let row_id = row_id_by_id(db, "Test", 2)?;
    let mut vh = db.version_history_fk("Test", row_id, "Body")?;
    vh.add_version("second version", version_date(2021, 5, 6, 7, 8, 9))?;
    Ok(())
}

/// `vhadd`: adds an append-only MEMO column to a table which already holds
/// rows (`ColumnBuilder.addToTable`), gives every existing row a version and
/// adds a row with a body afterwards.
fn vhadd(db: &mut Database) -> jackcess::Result<()> {
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
    db.create_table(&spec)?;
    for i in 0..5 {
        add_rows_one_by_one(
            db,
            "T",
            &[vec![Value::Null, Value::Text(format!("name{}", i))]],
        )?;
    }

    db.add_column(
        "T",
        ColumnSpec::new("Body", DataType::Memo).set_append_only(true),
    )?;

    // the rows written before the column was added got the next complex ids
    let rows = db.read_rows_with_ids("T")?;
    let mut i: u32 = 0;
    for (row_id, values) in rows {
        let values = match values {
            Some(v) => v,
            None => continue,
        };
        let name = match values.get(1) {
            Some(Value::Text(s)) => s.clone(),
            _ => String::new(),
        };
        let mut vh = db.version_history_fk("T", row_id, "Body")?;
        vh.add_version(
            &format!("version of {}", name),
            version_date(2021, 6, 1 + i, 1, 2, 3),
        )?;
        i += 1;
    }

    add_rows_one_by_one(
        db,
        "T",
        &[vec![
            Value::Null,
            Value::Text("after".to_string()),
            Value::Text("after body".to_string()),
        ]],
    )?;
    let row_id = row_id_by_id(db, "T", 6)?;
    let mut vh = db.version_history_fk("T", row_id, "Body")?;
    vh.add_version("after version", version_date(2022, 1, 2, 3, 4, 5))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// calculated column scenarios (`ColumnBuilder.setCalculatedInfo`,
// `CalculatedColumnUtil`, `TableImpl.CalcColEvaluator`).  See `Gen.java` for
// the reference table; the scenarios avoid BOOLEAN input columns because
// jackcess 5.0.4 throws a ClassCastException when an expression reads one.
// ---------------------------------------------------------------------------

fn calc_ldt(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> Value {
    Value::DateTime(jackcess::datenums::LocalDateTime::new(
        year, month, day, hour, min, sec, 0,
    ))
}

fn make_calc_table(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("A", DataType::Long),
            ColumnSpec::new("B", DataType::Double),
            ColumnSpec::new("S", DataType::Text).with_length(50),
            ColumnSpec::new("D", DataType::ShortDateTime),
            ColumnSpec::new("C", DataType::Long).set_calculated_info("[A]+1"),
            ColumnSpec::new("E", DataType::Double).set_calculated_info("[B]*2"),
            ColumnSpec::new("F", DataType::Text)
                .with_length(30)
                .set_calculated_info("[S] & \"x\""),
            ColumnSpec::new("G", DataType::Text)
                .with_length(10)
                .set_calculated_info("IIf([A]>0,\"pos\",\"neg\")"),
            // H references two calculated columns
            ColumnSpec::new("H", DataType::Double).set_calculated_info("[C]+[E]"),
            ColumnSpec::new("J", DataType::Text)
                .with_length(40)
                .set_calculated_info("\"v=\" & [A]"),
            // a calculated MEMO: the wrapped (encoded) text is written as a
            // long value, so a longer value lives on long value pages
            ColumnSpec::new("M", DataType::Memo).set_calculated_info("[S] & [S]"),
            // a calculated NUMERIC (its own 16 byte value layout) and an
            // identity calculated date
            ColumnSpec::new("N", DataType::Numeric).set_calculated_info("[A]*2"),
            ColumnSpec::new("K", DataType::ShortDateTime).set_calculated_info("[D]"),
            // a calculated BOOLEAN is not stored in the null mask; a null
            // result is written as no data and reads back as FALSE
            ColumnSpec::new("P", DataType::Boolean).set_calculated_info("[A]>0"),
        ],
    );
    db.create_table(&spec)?;
    Ok(())
}

/// One row of the calculated column table: the input columns A/B/S/D, with the
/// calculated columns left null (they are computed on insert).
fn calc_row(a: Value, b: Value, s: Value, d: Value) -> Vec<Value> {
    let mut row = vec![Value::Null; 15];
    row[0] = Value::Null; // ID (auto number)
    row[1] = a;
    row[2] = b;
    row[3] = s;
    row[4] = d;
    row
}

fn add_calc_rows(db: &mut Database) -> jackcess::Result<()> {
    let rows = vec![
        calc_row(
            Value::Long(5),
            Value::Double(2.5),
            Value::Text("abc".to_string()),
            calc_ldt(2020, 3, 15, 12, 30, 45),
        ),
        calc_row(
            Value::Long(-3),
            Value::Double(-1.5),
            Value::Text(String::new()),
            calc_ldt(2021, 1, 2, 3, 4, 5),
        ),
        calc_row(Value::Null, Value::Null, Value::Null, Value::Null),
        calc_row(
            Value::Long(0),
            Value::Double(0.25),
            Value::Text("z".to_string()),
            Value::Null,
        ),
        calc_row(
            Value::Long(100),
            Value::Double(-2.125),
            Value::Text("rowfive longer".to_string()),
            calc_ldt(2022, 6, 7, 8, 9, 10),
        ),
        calc_row(
            Value::Long(-1000),
            Value::Double(123.5),
            Value::Text("six".to_string()),
            calc_ldt(2023, 12, 31, 23, 59, 59),
        ),
    ];
    add_rows_one_by_one(db, "Test", &rows)
}

fn calc_rows_by_id(
    db: &mut Database,
) -> jackcess::Result<std::collections::HashMap<i32, (jackcess::table::RowId, Vec<Value>)>> {
    let mut by_id = std::collections::HashMap::new();
    for (row_id, values) in db.read_rows_with_ids("Test")? {
        if let Some(values) = values {
            if let Some(Value::Long(id)) = values.first() {
                by_id.insert(*id, (row_id, values));
            }
        }
    }
    Ok(by_id)
}

fn calc(db: &mut Database) -> jackcess::Result<()> {
    make_calc_table(db)?;
    add_calc_rows(db)
}

fn calcupdate(db: &mut Database) -> jackcess::Result<()> {
    make_calc_table(db)?;
    add_calc_rows(db)?;
    let by_id = calc_rows_by_id(db)?;

    // change only A; the calculated columns keep their raw values and are
    // computed again
    let (row_id, _) = by_id.get(&1).cloned().expect("row 1");
    let mut row = vec![Value::Keep; 15];
    row[1] = Value::Long(10);
    db.update_row("Test", row_id, &row)?;

    // a full row update through the read values: only B changes
    let (row_id, mut values) = by_id.get(&2).cloned().expect("row 2");
    values[2] = Value::Double(7.5);
    db.update_row("Test", row_id, &values)?;

    // only S changes (`asUpdateRow` keeps every other column)
    let (row_id, _) = by_id.get(&4).cloned().expect("row 4");
    let mut map = std::collections::BTreeMap::new();
    map.insert("S".to_string(), Value::Text("changed".to_string()));
    db.update_row_from_map("Test", row_id, &map)?;

    // A and S become null, the calculated columns are recomputed
    let (row_id, _) = by_id.get(&5).cloned().expect("row 5");
    let mut row = vec![Value::Keep; 15];
    row[1] = Value::Null;
    row[3] = Value::Null;
    db.update_row("Test", row_id, &row)?;

    let rows = vec![
        calc_row(
            Value::Long(7),
            Value::Double(0.5),
            Value::Text("after".to_string()),
            calc_ldt(2024, 5, 6, 7, 8, 9),
        ),
        calc_row(
            Value::Long(-7),
            Value::Double(-0.5),
            Value::Text("tail".to_string()),
            Value::Null,
        ),
    ];
    add_rows_one_by_one(db, "Test", &rows)
}

/// Calculated MONEY/FLOAT/DOUBLE result types (V14/2010+ only).
fn calcfloat(db: &mut Database) -> jackcess::Result<()> {
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("A", DataType::Long),
            ColumnSpec::new("M", DataType::Money).set_calculated_info("[A]/4"),
            ColumnSpec::new("M2", DataType::Money).set_calculated_info("[A]*2"),
            ColumnSpec::new("F", DataType::Float).set_calculated_info("[A]/3"),
            ColumnSpec::new("D", DataType::Double).set_calculated_info("[A]/8"),
        ],
    );
    db.create_table(&spec)?;
    let mut rows = Vec::new();
    for v in [-2i32, -1, 0, 1, 2, 3] {
        rows.push(vec![Value::Long(v)]);
    }
    rows.push(vec![Value::Null]);
    add_rows_one_by_one(db, "Test", &rows)
}
