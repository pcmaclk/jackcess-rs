//! `cursorrange` - runs the range/savepoint/iterable cursor scenario (the Rust
//! half of `tools/verify-cursor-range.ps1`), mirroring
//! `java-src/CursorRangeGen.java`.
//!
//! Usage: `cursorrange <out.mdb> <format> [--now-ms N]`
//!
//! The generator builds the table, then runs the same sequences as the Java
//! reference: builder index selection (`setIndexByColumns`/
//! `setIndexByColumnNames`/`setIndex`, including the partial (leading column)
//! and error cases, combined with ranges, and the static `CursorBuilder`
//! `createCursor`/`createPrimaryKeyCursor`/`findRow*` helpers), `findRow(RowId)`
//! edges (the first row of a non-unique run, rows before/inside/after a range,
//! invalid and deleted row ids), range-bounded traversal over the primary key, a
//! unique text index, ascending/descending non-unique indexes and a two column
//! index (inclusive/exclusive bounds, open ends, single rows, reversed and empty
//! ranges, bounds beyond the data), ranged finds, savepoints (including
//! cross-cursor restore, rejection for another index, builder restore and
//! restore around updates and deletes), iterables (forward/reverse, column
//! subsets, row/column match patterns, by-entry iteration, ranged cursors and
//! `remove`), ranged updates/deletes and iterable removals.  The trace printed
//! on stdout is compared with the Java trace by the harness, as are the
//! resulting database files.

use std::collections::BTreeMap;

use jackcess::column::ColumnSpec;
use jackcess::cursor::{
    Cursor, CursorBuilder, EntryIterableBuilder, IndexCursor, IterableBuilder, Savepoint,
    TableScanCursor,
};
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::error::{Error, Result};
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: cursorrange <out.mdb> <format> [--now-ms N]");
        std::process::exit(2);
    }
    let out = &args[1];
    let mut file_format = FileFormat::from_name(&args[2]).unwrap_or(FileFormat::V2003);
    let mut now_ms: Option<i64> = None;
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--now-ms" {
            now_ms = args.get(i + 1).and_then(|s| s.trim().parse::<i64>().ok());
            i += 2;
        } else {
            if let Some(f) = FileFormat::from_name(&args[i]) {
                file_format = f;
            }
            i += 1;
        }
    }

    let path = std::path::Path::new(out);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(file_format, path)?;
    db.now_ms = now_ms;

    make_table(&mut db)?;
    add_rows(&mut db)?;
    run_builder_index_selection(&mut db)?;
    run_find_row_edges(&mut db)?;
    run_savepoints(&mut db)?;
    run_primary_key_ranges(&mut db)?;
    run_name_ranges(&mut db)?;
    run_num_ranges(&mut db)?;
    run_desc_num_ranges(&mut db)?;
    run_num_id_ranges(&mut db)?;
    run_ranged_finds(&mut db)?;
    run_iterables(&mut db)?;
    run_ranged_mutations(&mut db)?;
    run_iterable_removes(&mut db)?;
    run_delete_and_restore(&mut db)?;
    run_find_row_deleted(&mut db)?;
    read_everything_back(&mut db)?;
    println!("DONE");
    db.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// scenario data
// ---------------------------------------------------------------------------

fn make_table(db: &mut Database) -> Result<()> {
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
    .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique())
    .with_index(IndexSpec::new("ByNum", vec![("Num".to_string(), true)]))
    .with_index(IndexSpec::new(
        "ByNumDesc",
        vec![("Num".to_string(), false)],
    ))
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
    db.create_table(&spec)?;
    Ok(())
}

fn name(i: i32) -> String {
    let prefix = if i % 5 == 0 { "NAME" } else { "name" };
    format!("{}{:02}", prefix, i)
}

fn num(i: i32) -> Option<i32> {
    if i % 7 == 0 {
        None
    } else {
        Some((i * 37) % 11)
    }
}

fn note(i: i32) -> Option<String> {
    if i % 4 == 0 {
        None
    } else {
        Some(format!("note {}", i))
    }
}

fn add_rows(db: &mut Database) -> Result<()> {
    for i in 1..=20i32 {
        let row = vec![
            Value::Long(i),
            Value::Text(name(i)),
            match num(i) {
                Some(n) => Value::Long(n),
                None => Value::Null,
            },
            match note(i) {
                Some(n) => Value::Text(n),
                None => Value::Null,
            },
        ];
        db.add_rows("T", std::slice::from_ref(&row))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn value_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Text(s) => s.clone(),
        Value::Long(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Byte(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => format!("{:?}", other),
    }
}

fn row_string(row: &[Value]) -> String {
    format!(
        "ID={} Name={} Num={} Note={}",
        value_string(row.first().unwrap_or(&Value::Null)),
        value_string(row.get(1).unwrap_or(&Value::Null)),
        value_string(row.get(2).unwrap_or(&Value::Null)),
        value_string(row.get(3).unwrap_or(&Value::Null))
    )
}

fn ids_string(ids: &[i32]) -> String {
    format!(
        "[{}]",
        ids.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn strings_string(items: &[String]) -> String {
    format!("[{}]", items.join(", "))
}

fn pattern(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn long(n: i32) -> Value {
    Value::Long(n)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn id(db: &mut Database, cursor: &mut dyn Cursor) -> Result<i32> {
    Ok(cursor
        .get_current_row_value(db, "ID")?
        .as_i64()
        .unwrap_or(-1) as i32)
}

fn flags(cursor: &dyn Cursor) -> String {
    format!(
        "beforeFirst={} afterLast={}",
        cursor.is_before_first(),
        cursor.is_after_last()
    )
}

fn pos_id(cursor: &dyn Cursor) -> String {
    let (page, row) = cursor.get_savepoint().get_current_row_id();
    format!("{}:{}", page, row)
}

/// `CursorImpl.isCurrentRowValid` (the same test the Java harness uses).
fn valid(db: &mut Database, cursor: &mut dyn Cursor) -> Result<bool> {
    if cursor.is_before_first() || cursor.is_after_last() {
        return Ok(false);
    }
    if cursor.is_current_row_deleted(db)? {
        return Ok(false);
    }
    Ok(cursor.get_current_row(db).is_ok())
}

fn scan_ids(db: &mut Database, cursor: &mut dyn Cursor) -> Result<Vec<i32>> {
    let mut ids = Vec::new();
    cursor.before_first();
    while let Some(row) = cursor.get_next_row(db)? {
        ids.push(row[0].as_i64().unwrap_or(-1) as i32);
    }
    Ok(ids)
}

fn scan_ids_backwards(db: &mut Database, cursor: &mut dyn Cursor) -> Result<Vec<i32>> {
    let mut ids = Vec::new();
    cursor.after_last();
    while let Some(row) = cursor.get_previous_row(db)? {
        ids.push(row[0].as_i64().unwrap_or(-1) as i32);
    }
    Ok(ids)
}

/// The Java exception class of a failing operation, for the trace.
fn error(f: impl FnOnce() -> Result<()>) -> String {
    match f() {
        Ok(()) => "none".to_string(),
        Err(Error::IllegalArgument(_)) => "IllegalArgumentException".to_string(),
        Err(Error::IllegalState(_)) => "IllegalStateException".to_string(),
        Err(other) => format!("{:?}", other),
    }
}

fn check(cond: bool, what: &str) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(Error::Jackcess(format!("assertion failed: {}", what)))
    }
}

/// A ranged index cursor over an index of `T`
/// (`Table.newCursor().setIndex(...).toIndexCursor()`).
fn ranged(
    db: &mut Database,
    index_name: &str,
    configure: impl FnOnce(&mut CursorBuilder),
) -> Result<IndexCursor> {
    let mut builder = CursorBuilder::new("T");
    builder.set_index_by_name(index_name);
    configure(&mut builder);
    builder.to_index_cursor(db)
}

/// `CursorBuilder.createPrimaryKeyCursor(table)` / `createCursor(index)`.
fn index_cursor(db: &mut Database, index_name: &str) -> Result<IndexCursor> {
    IndexCursor::new(db, "T", index_name)
}

/// A ranged index cursor whose index is selected by its columns
/// (`t.newCursor().setIndexByColumnNames(...)`).
fn ranged_by_columns(
    db: &mut Database,
    columns: &[&str],
    configure: impl FnOnce(&mut CursorBuilder),
) -> Result<IndexCursor> {
    let mut builder = CursorBuilder::new("T");
    builder.set_index_by_column_names(columns);
    configure(&mut builder);
    builder.to_index_cursor(db)
}

/// The index of a cursor as `Name(Col+Col)` (the Java generator's `describe`).
fn describe(cursor: &IndexCursor, db: &mut Database) -> Result<String> {
    let name = cursor.get_index(db)?.name;
    let columns = cursor.get_index_columns(db)?;
    let names: Vec<String> = columns.iter().map(|c| c.column.name.clone()).collect();
    Ok(format!("{}({})", name, names.join("+")))
}

/// The ID of the current row, or `none` when the cursor is not on a row (the
/// Java generator's `curId`).
fn cur_id(db: &mut Database, cursor: &mut dyn Cursor) -> String {
    match cursor.get_current_row_value(db, "ID") {
        Ok(Value::Long(n)) => n.to_string(),
        Ok(other) => value_string(&other),
        Err(_) => "none".to_string(),
    }
}

/// A sparse table row array (for `createCursor(index, startRow, endRow)`): the
/// Java generator's `row(table, ...)`.
fn row_spec(values: &[(&str, Value)]) -> Vec<Value> {
    let mut row = vec![Value::Null; 4];
    for (name, value) in values {
        let index = match *name {
            "ID" => 0,
            "Name" => 1,
            "Num" => 2,
            "Note" => 3,
            other => panic!("unknown column {}", other),
        };
        row[index] = value.clone();
    }
    row
}

/// The index a column list selects, or the exception class it raises (the Java
/// generator's `selected`).
fn selected(db: &mut Database, cols: &[&str]) -> Result<String> {
    let mut builder = CursorBuilder::new("T");
    builder.set_index_by_column_names(cols);
    match builder.to_index_cursor(db) {
        Ok(cursor) => describe(&cursor, db),
        Err(Error::IllegalArgument(_)) => Ok("EX IllegalArgumentException".to_string()),
        Err(other) => Ok(format!("EX {:?}", other)),
    }
}

// ---------------------------------------------------------------------------
// builder index selection (`setIndexByColumns`/`setIndexByColumnNames`)
// ---------------------------------------------------------------------------

fn run_builder_index_selection(db: &mut Database) -> Result<()> {
    let cases: &[&[&str]] = &[
        &["Num"],
        &["num"],
        &["Name"],
        &["ID"],
        &["Num", "ID"],
        &["Num", "id"],
        &["ID", "Num"],
        &["Note"],
        &["Note", "Num"],
        &["Note", "ID"],
        &["Note", "Num", "ID"],
        &["Nonexistent"],
        &["Num", "Nonexistent"],
        &["ID", "Num", "Note"],
    ];
    for cols in cases {
        println!("SEL {} {}", cols.join(","), selected(db, cols)?);
    }

    // a column based selection orders the traversal and takes a range
    let mut c = ranged_by_columns(db, &["Num"], |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(5)]);
    })?;
    let label = describe(&c, db)?;
    println!(
        "SEL_RANGE_NUM {} {}",
        label,
        ids_string(&scan_ids(db, &mut c)?)
    );
    println!(
        "SEL_RANGE_NUM_B {}",
        ids_string(&scan_ids_backwards(db, &mut c)?)
    );
    let mut c = ranged_by_columns(db, &["Num", "ID"], |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(3)]);
    })?;
    let label = describe(&c, db)?;
    println!(
        "SEL_RANGE_NUMID {} {}",
        label,
        ids_string(&scan_ids(db, &mut c)?)
    );

    // exclusive bounds on the selected index
    let mut c = ranged_by_columns(db, &["Num"], |b| {
        b.set_start_entry(vec![long(3)])
            .set_start_row_inclusive(false)
            .set_end_entry(vec![long(5)])
            .set_end_row_inclusive(false);
    })?;
    let label = describe(&c, db)?;
    println!(
        "SEL_RANGE_X {} {}",
        label,
        ids_string(&scan_ids(db, &mut c)?)
    );

    // a partial (leading column) selection
    let mut c = ranged_by_columns(db, &["Note"], |b| {
        b.set_start_entry(vec![text("note 1")])
            .set_end_entry(vec![text("note 11")]);
    })?;
    let label = describe(&c, db)?;
    println!(
        "SEL_RANGE_NOTE {} {}",
        label,
        ids_string(&scan_ids(db, &mut c)?)
    );

    // a row based range on the selected index
    let mut c = ranged_by_columns(db, &["Num"], |b| {
        b.set_start_row_map(&pattern(&[("Num", long(3))]))
            .set_end_row_map(&pattern(&[("Num", long(4))]));
    })?;
    let label = describe(&c, db)?;
    println!(
        "SEL_ROW_RANGE {} {}",
        label,
        ids_string(&scan_ids(db, &mut c)?)
    );

    // `setIndex` replaces a column selection, and a column selection replaces
    // `setIndex`
    let by_name = CursorBuilder::create_index_cursor(db, "T", "ByName")?.get_index(db)?;
    let mut builder = CursorBuilder::new("T");
    builder.set_index_by_column_names(&["Num"]);
    builder.set_index(&by_name);
    builder
        .set_start_entry(vec![text("name03")])
        .set_end_entry(vec![text("name06")]);
    let mut c = builder.to_index_cursor(db)?;
    let label = describe(&c, db)?;
    println!(
        "SEL_REPLACED {} {}",
        label,
        ids_string(&scan_ids(db, &mut c)?)
    );
    let mut builder = CursorBuilder::new("T");
    builder.set_index_by_name("ByNum");
    builder.set_index_by_column_names(&["Num", "ID"]);
    builder
        .set_start_entry(vec![long(3)])
        .set_end_entry(vec![long(3)]);
    let mut c = builder.to_index_cursor(db)?;
    let label = describe(&c, db)?;
    println!(
        "SEL_REPLACED2 {} {}",
        label,
        ids_string(&scan_ids(db, &mut c)?)
    );

    // a column list no index matches
    println!(
        "SEL_ERROR {}",
        error(|| {
            let mut builder = CursorBuilder::new("T");
            builder.set_index_by_column_names(&["ID", "Num"]);
            builder.to_index_cursor(db)?;
            Ok(())
        })
    );

    // the static helpers
    let mut sc = CursorBuilder::create_cursor_with_range(
        db,
        "T",
        "ByNum",
        Some(row_spec(&[("Num", long(3))])),
        Some(row_spec(&[("Num", long(5))])),
    )?;
    let label = describe(&sc, db)?;
    println!(
        "STATIC_RANGE {} {}",
        label,
        ids_string(&scan_ids(db, &mut sc)?)
    );
    let mut sc = CursorBuilder::create_cursor_with_range(db, "T", "PrimaryKey", None, None)?;
    let label = describe(&sc, db)?;
    println!(
        "STATIC_ALL {} {}",
        label,
        ids_string(&scan_ids(db, &mut sc)?)
    );
    let mut sc = CursorBuilder::create_cursor_with_range_inclusive(
        db,
        "T",
        "ByNum",
        Some(row_spec(&[("Num", long(3))])),
        false,
        Some(row_spec(&[("Num", long(5))])),
        true,
    )?;
    let label = describe(&sc, db)?;
    println!(
        "STATIC_RANGE2 {} {}",
        label,
        ids_string(&scan_ids(db, &mut sc)?)
    );
    let mut sc = CursorBuilder::create_index_cursor(db, "T", "ByNum")?;
    let label = describe(&sc, db)?;
    println!(
        "STATIC_INDEX {} {}",
        label,
        ids_string(&scan_ids(db, &mut sc)?)
    );
    let mut pkc = CursorBuilder::create_primary_key_cursor(db, "T")?;
    let label = describe(&pkc, db)?;
    println!(
        "STATIC_PK {} {}",
        label,
        ids_string(&scan_ids(db, &mut pkc)?)
    );
    let mut scan = CursorBuilder::create_cursor(db, "T")?;
    println!("STATIC_SCAN {}", ids_string(&scan_ids(db, &mut *scan)?));

    // the `findRow*` convenience statics
    let row = CursorBuilder::find_row_by_primary_key(db, "T", &[long(6)])?;
    println!(
        "STATIC_FIND_PK {}",
        row.map(|r| value_string(&r[0]))
            .unwrap_or_else(|| "null".to_string())
    );
    let row = CursorBuilder::find_row_by_entry(db, "T", "ByNum", &[long(3)])?;
    println!(
        "STATIC_FIND_ENTRY {}",
        row.map(|r| value_string(&r[0]))
            .unwrap_or_else(|| "null".to_string())
    );
    let row = CursorBuilder::find_row_by_entry(db, "T", "ByNum", &[long(100)])?;
    println!(
        "STATIC_FIND_ENTRY_MISS {}",
        row.map(|r| value_string(&r[0]))
            .unwrap_or_else(|| "null".to_string())
    );
    let row = CursorBuilder::find_row(db, "T", &pattern(&[("Name", text("name04"))]))?;
    println!(
        "STATIC_FIND_MAP {}",
        row.map(|r| value_string(&r[0]))
            .unwrap_or_else(|| "null".to_string())
    );
    let row =
        CursorBuilder::find_row_by_index(db, "T", "ByName", &pattern(&[("Name", text("name04"))]))?;
    println!(
        "STATIC_FIND_INDEX_MAP {}",
        row.map(|r| value_string(&r[0]))
            .unwrap_or_else(|| "null".to_string())
    );
    let row = CursorBuilder::find_row(db, "T", &pattern(&[("Name", text("nope"))]))?;
    println!(
        "STATIC_FIND_MAP_MISS {}",
        row.map(|r| value_string(&r[0]))
            .unwrap_or_else(|| "null".to_string())
    );
    let value = CursorBuilder::find_value(db, "T", "Note", "Num", &long(3))?;
    println!(
        "STATIC_FIND_VALUE {}",
        value
            .map(|v| value_string(&v))
            .unwrap_or_else(|| "null".to_string())
    );
    let value = CursorBuilder::find_value_by_index(db, "T", "ByNum", "Note", "Num", &long(3))?;
    println!(
        "STATIC_FIND_VALUE_INDEX {}",
        value
            .map(|v| value_string(&v))
            .unwrap_or_else(|| "null".to_string())
    );
    let value = CursorBuilder::find_value(db, "T", "Note", "Num", &long(100))?;
    println!(
        "STATIC_FIND_VALUE_MISS {}",
        value
            .map(|v| value_string(&v))
            .unwrap_or_else(|| "null".to_string())
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// findRow(RowId) edges (`IndexCursorImpl.getRowPosition`)
// ---------------------------------------------------------------------------

fn run_find_row_edges(db: &mut Database) -> Result<()> {
    // the row id of every row, keyed by its ID value
    let mut by_id: BTreeMap<i32, (i32, i32)> = BTreeMap::new();
    let mut pk = CursorBuilder::create_primary_key_cursor(db, "T")?;
    pk.before_first();
    while let Some(r) = pk.get_next_row(db)? {
        let id = r[0].as_i64().unwrap_or(-1) as i32;
        by_id.insert(id, pk.get_savepoint().get_current_row_id());
    }
    println!("FR_IDS {}", by_id.len());

    // a non-unique index: the run of Num = 3
    let mut by_num = CursorBuilder::create_index_cursor(db, "T", "ByNum")?;
    let mut run = Vec::new();
    let mut iter = EntryIterableBuilder::new(vec![long(3)]).rows();
    while let Some(r) = iter.next_row(&mut by_num, db)? {
        run.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("FR_RUN_IDS {}", ids_string(&run));
    for want in &run {
        let mut c = CursorBuilder::create_index_cursor(db, "T", "ByNum")?;
        let found = c.find_row(db, by_id[want])?;
        let current = cur_id(db, &mut c);
        println!("FR_RUN {} {} {} {}", want, found, current, pos_id(&c));
    }

    // from the middle of the run of Num = 4
    let mut mid = CursorBuilder::create_index_cursor(db, "T", "ByNum")?;
    mid.find_first_row_by_entry(db, &[long(4)])?;
    mid.move_next_rows(db, 1)?;
    let current = cur_id(db, &mut mid);
    println!("FR_MID_PRE {} {}", current, pos_id(&mid));
    let last = *run.last().unwrap();
    let found = mid.find_row(db, by_id[&last])?;
    let current = cur_id(db, &mut mid);
    println!("FR_MID {} {} {}", found, current, pos_id(&mid));
    let moved = mid.move_to_next_row(db)?;
    let current = cur_id(db, &mut mid);
    println!("FR_MID_NEXT {} {}", moved, current);

    // inside / before / after a range on the primary key
    let mut ranged_cursor = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_end_entry(vec![long(15)]);
    })?;
    for want in [9, 2, 5, 15, 17, 1, 20] {
        let found = ranged_cursor.find_row(db, by_id[&want])?;
        let state = flags(&ranged_cursor);
        let current = cur_id(db, &mut ranged_cursor);
        println!(
            "FR_RANGE {} {} {} {} {}",
            want,
            found,
            state,
            current,
            pos_id(&ranged_cursor)
        );
    }

    // ... and on the non-unique index: a row before the range lands on the
    // first row of the range, a row after it is not found
    let mut num_ranged = ranged(db, "ByNum", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(5)]);
    })?;
    let mut low = Vec::new();
    let mut in_range = Vec::new();
    let mut high = Vec::new();
    let mut all = CursorBuilder::create_index_cursor(db, "T", "ByNum")?;
    all.before_first();
    while let Some(r) = all.get_next_row(db)? {
        let id = r[0].as_i64().unwrap_or(-1) as i32;
        match &r[2] {
            Value::Null => low.push(id),
            value => {
                let n = value.as_i64().unwrap_or(0);
                if n < 3 {
                    low.push(id);
                } else if n > 5 {
                    high.push(id);
                } else {
                    in_range.push(id);
                }
            }
        }
    }
    println!(
        "FR_NUM_SPLIT {} {} {}",
        ids_string(&low),
        ids_string(&in_range),
        ids_string(&high)
    );
    for want in low.iter().chain(in_range.iter()).chain(high.iter()) {
        let low_row = low.contains(want);
        let high_row = high.contains(want);
        let label = if low_row {
            "FR_NUM_LOW"
        } else if high_row {
            "FR_NUM_HIGH"
        } else {
            "FR_NUM_IN"
        };
        let found = num_ranged.find_row(db, by_id[want])?;
        let current = cur_id(db, &mut num_ranged);
        println!(
            "{} {} {} {} {}",
            label,
            want,
            found,
            current,
            pos_id(&num_ranged)
        );
    }

    // an invalid row id is not an error (`TableImpl.positionAtRowHeader`)
    let found = ranged_cursor.find_row(db, (999, 0))?;
    let state = flags(&ranged_cursor);
    let current = cur_id(db, &mut ranged_cursor);
    println!(
        "FR_INVALID {} {} {} {}",
        found,
        state,
        current,
        pos_id(&ranged_cursor)
    );
    let found = ranged_cursor.find_row(db, (1, 99))?;
    let current = cur_id(db, &mut ranged_cursor);
    println!("FR_INVALID2 {} {}", found, current);
    let found = mid.find_row(db, (-1, -1))?;
    let current = cur_id(db, &mut mid);
    println!("FR_INVALID3 {} {}", found, current);

    // a reversed range: a row inside the data but outside the (empty) range is
    // not found
    let mut reversed_cursor = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(10)])
            .set_end_entry(vec![long(5)]);
    })?;
    let found = reversed_cursor.find_row(db, by_id[&7])?;
    let state = flags(&reversed_cursor);
    let current = cur_id(db, &mut reversed_cursor);
    println!(
        "FR_REVERSED {} {} {} {}",
        found,
        state,
        current,
        pos_id(&reversed_cursor)
    );
    let found = reversed_cursor.find_row(db, by_id[&3])?;
    let state = flags(&reversed_cursor);
    let current = cur_id(db, &mut reversed_cursor);
    println!(
        "FR_REVERSED2 {} {} {} {}",
        found,
        state,
        current,
        pos_id(&reversed_cursor)
    );
    Ok(())
}

/// `findRow` of a deleted row (index cursors raise, scans report "no row").
fn run_find_row_deleted(db: &mut Database) -> Result<()> {
    let mut c = CursorBuilder::create_primary_key_cursor(db, "T")?;
    check(c.find_first_row_by_entry(db, &[long(18)])?, "find 18")?;
    let deleted = c.get_current_row_id();
    c.delete_current_row(db)?;
    println!(
        "FR_DELETED {}",
        error(|| {
            let mut probe = CursorBuilder::create_primary_key_cursor(db, "T")?;
            probe.find_row(db, deleted)?;
            Ok(())
        })
    );
    let mut scan = CursorBuilder::create_cursor(db, "T")?;
    println!("FR_DELETED_SCAN {}", scan.find_row(db, deleted)?);
    let mut after = CursorBuilder::create_primary_key_cursor(db, "T")?;
    println!(
        "FR_DELETED_ENTRY {}",
        after.find_first_row_by_entry(db, &[long(18)])?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// savepoints
// ---------------------------------------------------------------------------

fn run_savepoints(db: &mut Database) -> Result<()> {
    let mut c = index_cursor(db, "PrimaryKey")?;
    check(c.find_first_row_by_entry(db, &[long(6)])?, "find 6")?;
    let sp = c.get_savepoint();
    println!("SP_AT {} {}", id(db, &mut c)?, pos_id(&c));
    c.move_next_rows(db, 4)?;
    println!("SP_MOVED {}", id(db, &mut c)?);
    c.restore_savepoint(&sp)?;
    println!("SP_RESTORED {} {}", id(db, &mut c)?, flags(&c));

    // a savepoint around an update: the cursor position is restored, the data
    // change stays
    c.set_current_row_value(db, "Note", text("via savepoint"))?;
    c.set_current_row_value(db, "Num", long(55))?;
    c.move_next_rows(db, 2)?;
    c.restore_savepoint(&sp)?;
    println!("SP_AFTER_UPDATE {}", row_string(&c.get_current_row(db)?));

    // a boundary position savepoint
    c.before_first();
    let before = c.get_savepoint();
    println!("SP_BEFORE {} {}", flags(&c), pos_id(&c));
    c.move_next_rows(db, 3)?;
    c.restore_savepoint(&before)?;
    println!("SP_BEFORE_RESTORED {} {}", flags(&c), pos_id(&c));
    c.after_last();
    let after = c.get_savepoint();
    println!("SP_AFTER {} {}", flags(&c), pos_id(&c));
    c.move_previous_rows(db, 3)?;
    c.restore_savepoint(&after)?;
    println!("SP_AFTER_RESTORED {} {}", flags(&c), pos_id(&c));
    println!(
        "SP_AFTER_MOVED {} {}",
        c.move_to_previous_row(db)?,
        id(db, &mut c)?
    );

    // savepoints may be restored into another cursor instance of the same index
    let mut other = index_cursor(db, "PrimaryKey")?;
    other.restore_savepoint(&sp)?;
    println!("SP_OTHER_CURSOR {} {}", id(db, &mut other)?, pos_id(&other));

    // ... but not into a cursor of another index
    let mut by_name = index_cursor(db, "ByName")?;
    println!(
        "SP_WRONG_INDEX {}",
        error(|| by_name.restore_savepoint(&sp))
    );

    // a table scan savepoint
    let mut scan = TableScanCursor::new(db, "T")?;
    check(
        scan.find_first_row(db, &pattern(&[("ID", long(5))]))?,
        "find 5",
    )?;
    let scan_sp = scan.get_savepoint();
    scan.move_next_rows(db, 3)?;
    println!("SP_SCAN_MOVED {}", id(db, &mut scan)?);
    scan.restore_savepoint(&scan_sp)?;
    println!("SP_SCAN_RESTORED {}", id(db, &mut scan)?);

    // a savepoint used as the initial position of a builder
    let built = {
        let mut builder = CursorBuilder::new("T");
        builder
            .set_index_by_name("PrimaryKey")
            .restore_savepoint(&sp)
            .after_last();
        builder.to_index_cursor(db)?
    };
    println!("SP_BUILDER {} {}", flags(&built), pos_id(&built));
    let mut built2 = {
        let mut builder = CursorBuilder::new("T");
        builder
            .set_index_by_name("PrimaryKey")
            .restore_savepoint(&sp);
        builder.to_index_cursor(db)?
    };
    println!("SP_BUILDER2 {}", id(db, &mut built2)?);

    // a savepoint on a ranged cursor
    let mut ranged_cursor = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_end_entry(vec![long(10)]);
    })?;
    check(
        ranged_cursor.find_first_row_by_entry(db, &[long(8)])?,
        "find 8",
    )?;
    let ranged_sp = ranged_cursor.get_savepoint();
    ranged_cursor.move_next_rows(db, 1)?;
    println!("SP_RANGED_MOVED {}", id(db, &mut ranged_cursor)?);
    ranged_cursor.restore_savepoint(&ranged_sp)?;
    println!(
        "SP_RANGED_RESTORED {} {}",
        id(db, &mut ranged_cursor)?,
        flags(&ranged_cursor)
    );
    println!(
        "SP_RANGED_MOVED2 {} {}",
        ranged_cursor.move_to_next_row(db)?,
        id(db, &mut ranged_cursor)?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// primary key ranges
// ---------------------------------------------------------------------------

fn run_primary_key_ranges(db: &mut Database) -> Result<()> {
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_end_entry(vec![long(10)]);
    })?;
    println!("PK_5_10_F {}", ids_string(&scan_ids(db, &mut c)?));
    println!("PK_5_10_B {}", ids_string(&scan_ids_backwards(db, &mut c)?));
    println!("PK_5_10_FLAGS {} {}", flags(&c), pos_id(&c));

    // inclusive start, exclusive end
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_start_row_inclusive(true)
            .set_end_entry(vec![long(10)])
            .set_end_row_inclusive(false);
    })?;
    println!("PK_5_10X_F {}", ids_string(&scan_ids(db, &mut c)?));
    println!("PK_5_10X_POS {}", pos_id(&c));

    // exclusive start, inclusive end
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_start_row_inclusive(false)
            .set_end_entry(vec![long(10)])
            .set_end_row_inclusive(true);
    })?;
    println!("PK_X5_10_F {}", ids_string(&scan_ids(db, &mut c)?));
    println!("PK_X5_10_START {} {}", flags(&c), pos_id(&c));

    // exclusive both
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_start_row_inclusive(false)
            .set_end_entry(vec![long(10)])
            .set_end_row_inclusive(false);
    })?;
    println!("PK_X5_10X_F {}", ids_string(&scan_ids(db, &mut c)?));
    println!(
        "PK_X5_10X_B {}",
        ids_string(&scan_ids_backwards(db, &mut c)?)
    );

    // open start / open end
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_end_entry(vec![long(3)]);
    })?;
    println!("PK_0_3_F {}", ids_string(&scan_ids(db, &mut c)?));
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(18)]);
    })?;
    println!("PK_18_N_F {}", ids_string(&scan_ids(db, &mut c)?));

    // a single row
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_specific_entry(vec![long(7)]);
    })?;
    println!("PK_7_F {}", ids_string(&scan_ids(db, &mut c)?));
    println!("PK_7_B {}", ids_string(&scan_ids_backwards(db, &mut c)?));

    // a row based range
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_row_map(&pattern(&[("ID", long(4))]))
            .set_end_row_map(&pattern(&[("ID", long(6))]));
    })?;
    println!("PK_ROW_4_6_F {}", ids_string(&scan_ids(db, &mut c)?));

    // a reversed range
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(10)])
            .set_end_entry(vec![long(5)]);
    })?;
    println!("PK_REV_INIT {} {}", flags(&c), pos_id(&c));
    let moved = c.move_to_next_row(db)?;
    println!("PK_REV_NEXT {} {} {}", moved, flags(&c), pos_id(&c));
    println!("PK_REV_F {}", ids_string(&scan_ids(db, &mut c)?));
    let moved = c.move_to_previous_row(db)?;
    println!("PK_REV_PREV {} {}", moved, flags(&c));
    println!("PK_REV_B {}", ids_string(&scan_ids_backwards(db, &mut c)?));

    // an empty range
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(10)])
            .set_start_row_inclusive(false)
            .set_end_entry(vec![long(10)])
            .set_end_row_inclusive(false);
    })?;
    println!("PK_EMPTY_INIT {} {}", flags(&c), pos_id(&c));
    let moved = c.move_to_next_row(db)?;
    println!("PK_EMPTY_NEXT {} {}", moved, flags(&c));
    println!("PK_EMPTY_F {}", ids_string(&scan_ids(db, &mut c)?));
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_specific_entry(vec![long(10)]);
    })?;
    println!("PK_10_10_F {}", ids_string(&scan_ids(db, &mut c)?));

    // bounds beyond the data
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(100)]);
    })?;
    println!("PK_BEYOND_INIT {} {}", flags(&c), pos_id(&c));
    let moved = c.move_to_next_row(db)?;
    println!("PK_BEYOND_NEXT {} {}", moved, flags(&c));
    println!("PK_BEYOND_F {}", ids_string(&scan_ids(db, &mut c)?));
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_end_entry(vec![long(0)]);
    })?;
    println!("PK_BEFORE_F {}", ids_string(&scan_ids(db, &mut c)?));

    // stepping within a range
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_end_entry(vec![long(10)]);
    })?;
    println!(
        "PK_STEP_NEXT {} {}",
        c.move_next_rows(db, 3)?,
        id(db, &mut c)?
    );
    println!(
        "PK_STEP_PREV {} {}",
        c.move_previous_rows(db, 2)?,
        id(db, &mut c)?
    );
    println!(
        "PK_STEP_TO_END {} {} {}",
        c.move_next_rows(db, 10)?,
        flags(&c),
        pos_id(&c)
    );
    println!("PK_STEP_RESET {}", flags(&c));
    c.reset();
    println!(
        "PK_STEP_RESET2 {} {} {}",
        flags(&c),
        c.move_to_next_row(db)?,
        id(db, &mut c)?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// unique text index ranges
// ---------------------------------------------------------------------------

fn run_name_ranges(db: &mut Database) -> Result<()> {
    let mut c = ranged(db, "ByName", |b| {
        b.set_start_entry(vec![text("name05")])
            .set_end_entry(vec![text("name10")]);
    })?;
    println!("NAME_5_10_F {}", ids_string(&scan_ids(db, &mut c)?));
    println!(
        "NAME_5_10_B {}",
        ids_string(&scan_ids_backwards(db, &mut c)?)
    );

    // the entries fold case, so an upper case bound selects the same rows
    let mut c = ranged(db, "ByName", |b| {
        b.set_start_entry(vec![text("NAME06")])
            .set_end_entry(vec![text("NAME09")]);
    })?;
    println!("NAME_CI_F {}", ids_string(&scan_ids(db, &mut c)?));

    // exclusive bounds
    let mut c = ranged(db, "ByName", |b| {
        b.set_start_entry(vec![text("name05")])
            .set_start_row_inclusive(false)
            .set_end_entry(vec![text("name10")])
            .set_end_row_inclusive(false);
    })?;
    println!("NAME_X_F {}", ids_string(&scan_ids(db, &mut c)?));

    // a range which holds no row
    let mut c = ranged(db, "ByName", |b| {
        b.set_start_entry(vec![text("name05a")])
            .set_end_entry(vec![text("name05z")]);
    })?;
    println!("NAME_NONE_F {}", ids_string(&scan_ids(db, &mut c)?));

    // entry lookup inside the range
    let mut c = ranged(db, "ByName", |b| {
        b.set_start_entry(vec![text("name05")])
            .set_end_entry(vec![text("name10")]);
    })?;
    let found = c.find_first_row_by_entry(db, &[text("name07")])?;
    println!("NAME_LOOKUP_IN {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row_by_entry(db, &[text("name12")])?;
    println!("NAME_LOOKUP_OUT {} {}", found, id(db, &mut c)?);
    c.find_closest_row_by_entry(db, &[text("name09")])?;
    println!("NAME_CLOSEST {}", id(db, &mut c)?);
    c.find_closest_row_by_entry(db, &[text("name15")])?;
    let is_valid = valid(db, &mut c)?;
    println!("NAME_CLOSEST_END {} {}", flags(&c), is_valid);
    Ok(())
}

// ---------------------------------------------------------------------------
// non-unique index ranges (ascending, nulls first)
// ---------------------------------------------------------------------------

fn run_num_ranges(db: &mut Database) -> Result<()> {
    let mut c = index_cursor(db, "ByNum")?;
    let mut pairs = Vec::new();
    c.before_first();
    while let Some(r) = c.get_next_row(db)? {
        pairs.push(format!("{}:{}", value_string(&r[0]), value_string(&r[2])));
    }
    println!("NUM_F {}", strings_string(&pairs));

    let mut c = ranged(db, "ByNum", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(6)]);
    })?;
    println!("NUM_3_6_F {}", ids_string(&scan_ids(db, &mut c)?));
    println!("NUM_3_6_B {}", ids_string(&scan_ids_backwards(db, &mut c)?));

    let mut c = ranged(db, "ByNum", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(3)]);
    })?;
    println!("NUM_3_3_F {}", ids_string(&scan_ids(db, &mut c)?));

    // a null start row sorts before every entry of an ascending index
    let mut c = ranged(db, "ByNum", |b| {
        b.set_start_row_map(&pattern(&[("Num", Value::Null)]))
            .set_end_entry(vec![long(1)]);
    })?;
    println!("NUM_NULL_1_F {}", ids_string(&scan_ids(db, &mut c)?));

    // an exclusive start inside the null run
    let mut c = ranged(db, "ByNum", |b| {
        b.set_start_row_map(&pattern(&[("Num", Value::Null)]))
            .set_start_row_inclusive(false)
            .set_end_entry(vec![long(0)]);
    })?;
    println!("NUM_XNULL_0_F {}", ids_string(&scan_ids(db, &mut c)?));

    // a row based range on a non-unique index
    let mut c = ranged(db, "ByNum", |b| {
        b.set_start_row_map(&pattern(&[("Num", long(4))]))
            .set_end_row_map(&pattern(&[("Num", long(5))]));
    })?;
    println!("NUM_ROW_4_5_F {}", ids_string(&scan_ids(db, &mut c)?));

    // partitioned finds inside a range
    let mut c = ranged(db, "ByNum", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(5)]);
    })?;
    let found = c.find_first_row_by_entry(db, &[long(3)])?;
    println!("NUM_FIND3 {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row_by_entry(db, &[long(3)])?;
    println!("NUM_FIND3B {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row_by_entry(db, &[long(4)])?;
    println!("NUM_FIND4 {} {}", found, id(db, &mut c)?);
    c.find_closest_row_by_entry(db, &[long(2)])?;
    println!("NUM_CLOSEST2 {}", id(db, &mut c)?);
    c.find_closest_row_by_entry(db, &[long(6)])?;
    println!("NUM_CLOSEST6 {}", flags(&c));
    let is_valid = valid(db, &mut c)?;
    println!("NUM_CLOSEST6_VALID {}", is_valid);

    // entry iteration inside a range
    let mut c = ranged(db, "ByNum", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(5)]);
    })?;
    let mut run = Vec::new();
    let mut iter = EntryIterableBuilder::new(vec![long(3)]).rows();
    while let Some(r) = iter.next_row(&mut c, db)? {
        run.push(format!("{}:{}", value_string(&r[0]), value_string(&r[2])));
    }
    println!("NUM_ENTRY_3 {}", strings_string(&run));
    Ok(())
}

// ---------------------------------------------------------------------------
// descending index ranges
// ---------------------------------------------------------------------------

fn run_desc_num_ranges(db: &mut Database) -> Result<()> {
    let mut c = ranged(db, "ByNumDesc", |b| {
        b.set_start_entry(vec![long(6)])
            .set_end_entry(vec![long(3)]);
    })?;
    let mut pairs = Vec::new();
    c.before_first();
    while let Some(r) = c.get_next_row(db)? {
        pairs.push(format!("{}:{}", value_string(&r[0]), value_string(&r[2])));
    }
    println!("DESC_6_3_F {}", strings_string(&pairs));

    // the reversed range on a descending index holds nothing
    let mut c = ranged(db, "ByNumDesc", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(6)]);
    })?;
    println!("DESC_3_6_F {}", ids_string(&scan_ids(db, &mut c)?));

    // exclusive bounds on a descending index
    let mut c = ranged(db, "ByNumDesc", |b| {
        b.set_start_entry(vec![long(5)])
            .set_start_row_inclusive(false)
            .set_end_entry(vec![long(4)])
            .set_end_row_inclusive(false);
    })?;
    println!("DESC_X5_4X_F {}", ids_string(&scan_ids(db, &mut c)?));

    // nulls sort last on a descending index
    let mut c = ranged(db, "ByNumDesc", |b| {
        b.set_start_entry(vec![long(0)]);
    })?;
    println!("DESC_0_N_F {}", ids_string(&scan_ids(db, &mut c)?));
    Ok(())
}

// ---------------------------------------------------------------------------
// two column index ranges (prefix keys)
// ---------------------------------------------------------------------------

fn run_num_id_ranges(db: &mut Database) -> Result<()> {
    // a prefix entry (Num only) holds every row with that Num
    let mut c = ranged(db, "ByNumId", |b| {
        b.set_start_entry(vec![long(3)]);
    })?;
    println!("NID_3_N_F {}", ids_string(&scan_ids(db, &mut c)?));
    let mut c = ranged(db, "ByNumId", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(3)]);
    })?;
    println!("NID_3_3_F {}", ids_string(&scan_ids(db, &mut c)?));

    // a full entry inside the prefix run
    let mut c = ranged(db, "ByNumId", |b| {
        b.set_start_entry(vec![long(3), long(9)])
            .set_end_entry(vec![long(3)]);
    })?;
    println!("NID_3_9_3_F {}", ids_string(&scan_ids(db, &mut c)?));

    // a range across runs, bounded by a prefix entry
    let mut c = ranged(db, "ByNumId", |b| {
        b.set_start_entry(vec![long(3), long(20)])
            .set_end_entry(vec![long(5)]);
    })?;
    println!("NID_3_20_5_F {}", ids_string(&scan_ids(db, &mut c)?));

    // the prefix run of the entry iterator
    let mut c = index_cursor(db, "ByNumId")?;
    let mut run = Vec::new();
    let mut iter = EntryIterableBuilder::new(vec![long(3)]).rows();
    while let Some(r) = iter.next_row(&mut c, db)? {
        run.push(format!("{}:{}", value_string(&r[0]), value_string(&r[2])));
    }
    println!("NID_ENTRY_3 {}", strings_string(&run));

    // prefix entry lookups
    let mut c = index_cursor(db, "ByNumId")?;
    let found = c.find_first_row_by_entry(db, &[long(3), long(20)])?;
    println!("NID_LOOKUP {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row_by_entry(db, &[long(9)])?;
    println!("NID_LOOKUP_PREFIX {} {}", found, id(db, &mut c)?);

    // a two column row based range
    let mut c = ranged(db, "ByNumId", |b| {
        b.set_start_row_map(&pattern(&[("Num", long(3)), ("ID", long(9))]))
            .set_end_row_map(&pattern(&[("Num", long(4)), ("ID", long(12))]));
    })?;
    println!("NID_ROW_F {}", ids_string(&scan_ids(db, &mut c)?));

    // an exclusive end on the second column of the index
    let mut c = ranged(db, "ByNumId", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(4), long(12)])
            .set_end_row_inclusive(false);
    })?;
    println!("NID_XEND_F {}", ids_string(&scan_ids(db, &mut c)?));

    // exclusive bounds built from a prefix entry: the trailing columns are
    // filled with MIN_VALUE/MAX_VALUE, so the run of the prefix key is still
    // included (the bound sorts outside of it)
    let mut c = ranged(db, "ByNumId", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(4)])
            .set_end_row_inclusive(false);
    })?;
    println!("NID_XEND_PREFIX {}", ids_string(&scan_ids(db, &mut c)?));
    let mut c = ranged(db, "ByNumId", |b| {
        b.set_start_entry(vec![long(3)])
            .set_start_row_inclusive(false)
            .set_end_entry(vec![long(5)]);
    })?;
    println!("NID_XSTART_PREFIX {}", ids_string(&scan_ids(db, &mut c)?));

    // an entry iterable started mid-cursor is repositioned absolutely
    let mut c = index_cursor(db, "ByNum")?;
    check(c.find_first_row_by_entry(db, &[long(9)])?, "find num 9")?;
    let mut mid = Vec::new();
    let mut iter = EntryIterableBuilder::new(vec![long(4)]).rows();
    while let Some(r) = iter.next_row(&mut c, db)? {
        mid.push(value_string(&r[0]));
    }
    println!("NID_ENTRY_MID {}", strings_string(&mid));

    // the entry values of an EntryIterableBuilder can be replaced
    let mut c = index_cursor(db, "ByNum")?;
    let mut builder = EntryIterableBuilder::new(vec![long(9)]);
    builder.set_entry_values(vec![long(3)]);
    let mut set = Vec::new();
    let mut iter = builder.rows();
    while let Some(r) = iter.next_row(&mut c, db)? {
        set.push(value_string(&r[0]));
    }
    println!("NID_ENTRY_SET {}", strings_string(&set));
    Ok(())
}

// ---------------------------------------------------------------------------
// ranged finds
// ---------------------------------------------------------------------------

fn run_ranged_finds(db: &mut Database) -> Result<()> {
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_end_entry(vec![long(15)]);
    })?;

    let found = c.find_first_row(db, &pattern(&[("ID", long(9))]))?;
    println!("F_IN {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row(db, &pattern(&[("ID", long(2))]))?;
    println!("F_BEFORE {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row(db, &pattern(&[("ID", long(17))]))?;
    println!("F_AFTER {} {}", found, id(db, &mut c)?);
    let found = c.find_next_row(db, &pattern(&[("ID", long(11))]))?;
    println!("F_NEXT_IN {} {}", found, id(db, &mut c)?);
    let found = c.find_next_row(db, &pattern(&[("ID", long(17))]))?;
    println!("F_NEXT_OUT {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row_by_entry(db, &[long(12)])?;
    println!("F_BY_ENTRY_IN {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row_by_entry(db, &[long(17)])?;
    println!("F_BY_ENTRY_OUT {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row_by_entry(db, &[long(3)])?;
    println!("F_BY_ENTRY_BEFORE {} {}", found, id(db, &mut c)?);
    c.find_closest_row_by_entry(db, &[long(4)])?;
    println!("F_CLOSEST_BEFORE {}", id(db, &mut c)?);
    c.find_closest_row_by_entry(db, &[long(11)])?;
    println!("F_CLOSEST_IN {}", id(db, &mut c)?);
    c.find_closest_row_by_entry(db, &[long(16)])?;
    let is_valid = valid(db, &mut c)?;
    println!("F_CLOSEST_AFTER {} {}", flags(&c), is_valid);
    let found = c.find_row_by_entry(db, &[long(14)])?.is_some();
    let is_valid = valid(db, &mut c)?;
    println!("F_ROW_BY_ENTRY {} {}", found, is_valid);
    let found = c.find_first_row_by_entry(db, &[long(13)])?;
    let matches13 = c.current_row_matches_entry(db, &[long(13)])?;
    let matches14 = c.current_row_matches_entry(db, &[long(14)])?;
    println!("F_MATCHES_ENTRY {} {} {}", found, matches13, matches14);

    // a row id lookup inside and after the range
    let mut all = index_cursor(db, "PrimaryKey")?;
    check(all.find_first_row_by_entry(db, &[long(9)])?, "find 9")?;
    let in_range = all.get_current_row_id();
    check(all.find_first_row_by_entry(db, &[long(18)])?, "find 18")?;
    let out_of_range = all.get_current_row_id();
    let found = c.find_row(db, in_range)?;
    println!("F_ROW_ID_IN {} {}", found, id(db, &mut c)?);
    let found = c.find_row(db, out_of_range)?;
    println!("F_ROW_ID_OUT {} {}", found, flags(&c));

    // findFirstRow(Column,Object) on a ranged index cursor
    c.before_first();
    let found = c.find_first_row_by_column(db, "ID", &long(8))?;
    println!("F_COLUMN {} {}", found, id(db, &mut c)?);

    // a backwards scan inside a range (the Rust port's findLastRow equivalent)
    c.after_last();
    let mut back = Vec::new();
    while let Some(r) = c.get_previous_row(db)? {
        if c.current_row_matches(db, &pattern(&[("Num", long(3))]))? {
            back.push(r[0].as_i64().unwrap_or(-1) as i32);
        }
    }
    println!("F_BACKWARDS {}", ids_string(&back));

    // a pattern which cannot use the index falls back to a scan inside the
    // range
    c.before_first();
    let found = c.find_first_row(db, &pattern(&[("Name", text("name12"))]))?;
    println!("F_SCAN_PATTERN {} {}", found, id(db, &mut c)?);
    let found = c.find_first_row(db, &pattern(&[("Name", text("name01"))]))?;
    println!("F_SCAN_OUT {} {}", found, id(db, &mut c)?);

    // a null key lookup inside a range of the non-unique index
    let mut nulls = ranged(db, "ByNum", |b| {
        b.set_start_row_map(&pattern(&[("Num", Value::Null)]))
            .set_end_entry(vec![long(1)]);
    })?;
    let found = nulls.find_first_row(db, &pattern(&[("Num", Value::Null)]))?;
    println!("F_NULL_PATTERN {} {}", found, id(db, &mut nulls)?);
    let found = nulls.find_first_row_by_entry(db, &[Value::Null])?;
    println!("F_NULL_ENTRY {} {}", found, id(db, &mut nulls)?);

    // a table scan cursor ignores the range (jackcess 5.0.4 only passes it to
    // the index cursor)
    let mut scanned = {
        let mut builder = CursorBuilder::new("T");
        builder
            .set_start_row_map(&pattern(&[("ID", long(5))]))
            .set_end_row_map(&pattern(&[("ID", long(7))]));
        builder.to_cursor(db)?
    };
    let mut scanned_ids = Vec::new();
    scanned.before_first();
    while let Some(r) = scanned.get_next_row(db)? {
        scanned_ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("F_SCAN_RANGE {}", ids_string(&scanned_ids));
    Ok(())
}

// ---------------------------------------------------------------------------
// iterables
// ---------------------------------------------------------------------------

fn run_iterables(db: &mut Database) -> Result<()> {
    // simple forward over a table scan
    let mut c = TableScanCursor::new(db, "T")?;
    let mut iter = IterableBuilder::new().rows();
    let mut ids = Vec::new();
    while let Some(r) = iter.next_row(&mut c, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_SCAN_F {}", ids_string(&ids));

    // simple reverse
    ids.clear();
    let mut iter = IterableBuilder::new().reverse().rows();
    while let Some(r) = iter.next_row(&mut c, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_SCAN_B {}", ids_string(&ids));

    // a column subset
    let mut subset = Vec::new();
    let mut iter = IterableBuilder::new()
        .set_column_names(vec!["ID".to_string(), "Note".to_string()])
        .rows();
    while let Some(r) = iter.next_row(&mut c, db)? {
        subset.push(format!(
            "{}:{}:{}",
            value_string(&r[0]),
            value_string(&r[1]),
            r.len()
        ));
    }
    println!("IT_SUBSET {}", strings_string(&subset));

    // reverse with a column subset over an index cursor
    let mut by_num = index_cursor(db, "ByNum")?;
    subset.clear();
    let mut iter = IterableBuilder::new()
        .reverse()
        .set_column_names(vec!["ID".to_string(), "Num".to_string()])
        .rows();
    while let Some(r) = iter.next_row(&mut by_num, db)? {
        subset.push(format!("{}:{}", value_string(&r[0]), value_string(&r[1])));
    }
    println!("IT_INDEX_B {}", strings_string(&subset));

    // without reset the current row is the first row of the iteration
    let mut pk = index_cursor(db, "PrimaryKey")?;
    check(pk.find_first_row_by_entry(db, &[long(18)])?, "find 18")?;
    ids.clear();
    let mut iter = IterableBuilder::new().reset(false).rows();
    while let Some(r) = iter.next_row(&mut pk, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_NO_RESET {}", ids_string(&ids));

    // row match patterns
    ids.clear();
    let mut builder = IterableBuilder::new();
    builder.set_match_pattern_row(pattern(&[("Num", long(3))]));
    let mut iter = builder.rows();
    while let Some(r) = iter.next_row(&mut c, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_MATCH_F {}", ids_string(&ids));
    ids.clear();
    let mut builder = IterableBuilder::new().reverse();
    builder.set_match_pattern_row(pattern(&[("Num", long(3))]));
    let mut iter = builder.rows();
    while let Some(r) = iter.next_row(&mut c, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_MATCH_B {}", ids_string(&ids));

    // row match over an index cursor without a usable key (plain scan)
    ids.clear();
    let mut builder = IterableBuilder::new().reverse();
    builder.set_match_pattern_row(pattern(&[("Num", long(3))]));
    let mut iter = builder.rows();
    while let Some(r) = iter.next_row(&mut pk, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_MATCH_PK_B {}", ids_string(&ids));
    // ... and with a usable key (the index assisted search)
    ids.clear();
    let mut builder = IterableBuilder::new().reverse();
    builder.set_match_pattern_row(pattern(&[("Num", long(3))]));
    let mut iter = builder.rows();
    while let Some(r) = iter.next_row(&mut by_num, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_MATCH_NUM_B {}", ids_string(&ids));
    ids.clear();
    let mut builder = IterableBuilder::new();
    builder.set_match_pattern_row(pattern(&[("Num", long(3))]));
    let mut iter = builder.rows();
    while let Some(r) = iter.next_row(&mut by_num, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_MATCH_NUM_F {}", ids_string(&ids));

    // column match patterns
    ids.clear();
    let mut builder = IterableBuilder::new();
    builder.set_match_pattern_column("Num", long(4));
    let mut iter = builder.rows();
    while let Some(r) = iter.next_row(&mut c, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_COLMATCH {}", ids_string(&ids));
    ids.clear();
    let mut builder = IterableBuilder::new().reverse();
    builder.set_match_pattern_column("Num", long(4));
    let mut iter = builder.rows();
    while let Some(r) = iter.next_row(&mut by_num, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_COLMATCH_B {}", ids_string(&ids));

    // the iterator interface directly (hasNext/next)
    let mut iter = IterableBuilder::new()
        .set_column_names(vec!["ID".to_string()])
        .rows();
    let mut first = Vec::new();
    for _ in 0..3 {
        if !iter.has_next(&mut c, db)? {
            break;
        }
        first.push(value_string(
            &iter.next_row(&mut c, db)?.map(|r| r[0].clone()).unwrap(),
        ));
    }
    println!("IT_HASNEXT {}", strings_string(&first));

    // entry iterables
    let mut run = Vec::new();
    let mut iter = EntryIterableBuilder::new(vec![long(3)]).rows();
    while let Some(r) = iter.next_row(&mut by_num, db)? {
        run.push(format!("{}:{}", value_string(&r[0]), value_string(&r[2])));
    }
    println!("IT_ENTRY_3 {}", strings_string(&run));
    run.clear();
    let mut iter = EntryIterableBuilder::new(vec![long(4)])
        .set_column_names(vec!["ID".to_string(), "Num".to_string()])
        .rows();
    while let Some(r) = iter.next_row(&mut by_num, db)? {
        run.push(format!(
            "{}:{}:{}",
            value_string(&r[0]),
            value_string(&r[1]),
            r.len()
        ));
    }
    println!("IT_ENTRY_4 {}", strings_string(&run));

    let mut by_name = index_cursor(db, "ByName")?;
    ids.clear();
    let mut iter = EntryIterableBuilder::new(vec![text("NAME05")]).rows();
    while let Some(r) = iter.next_row(&mut by_name, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_ENTRY_NAME {}", ids_string(&ids));

    // an entry iterable whose key is outside the cursor's range
    let mut ranged_cursor = ranged(db, "ByNum", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(5)]);
    })?;
    ids.clear();
    let mut iter = EntryIterableBuilder::new(vec![long(2)]).rows();
    while let Some(r) = iter.next_row(&mut ranged_cursor, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_ENTRY_OUT {}", ids_string(&ids));
    ids.clear();
    let mut iter = EntryIterableBuilder::new(vec![long(4)]).rows();
    while let Some(r) = iter.next_row(&mut ranged_cursor, db)? {
        ids.push(r[0].as_i64().unwrap_or(-1) as i32);
    }
    println!("IT_ENTRY_IN {}", ids_string(&ids));

    // updating the row the iterator is on, through the same cursor
    let mut upd = index_cursor(db, "PrimaryKey")?;
    let mut updated = Vec::new();
    let mut iter = IterableBuilder::new()
        .set_column_names(vec!["ID".to_string()])
        .rows();
    while let Some(r) = iter.next_row(&mut upd, db)? {
        let row_id = r[0].as_i64().unwrap_or(-1) as i32;
        if row_id % 3 == 0 {
            upd.set_current_row_value(db, "Note", text(&format!("it {}", row_id)))?;
            updated.push(row_id.to_string());
        }
    }
    println!("IT_UPDATE {}", strings_string(&updated));
    Ok(())
}

// ---------------------------------------------------------------------------
// ranged updates and deletes
// ---------------------------------------------------------------------------

fn run_ranged_mutations(db: &mut Database) -> Result<()> {
    // update a non-indexed column through a ranged cursor
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_end_entry(vec![long(10)]);
    })?;
    check(c.find_first_row_by_entry(db, &[long(6)])?, "find 6")?;
    c.set_current_row_value(db, "Note", text("ranged"))?;
    println!("MUT_UPDATE {}", row_string(&c.get_current_row(db)?));
    c.update_current_row_from_map(db, &pattern(&[("Num", long(56))]))?;
    println!("MUT_UPDATE_MAP {}", row_string(&c.get_current_row(db)?));

    // delete a row of the range and keep iterating
    check(c.find_first_row_by_entry(db, &[long(7)])?, "find 7")?;
    c.delete_current_row(db)?;
    println!("MUT_DELETE {} {}", flags(&c), c.is_current_row_deleted(db)?);
    let moved = c.move_to_next_row(db)?;
    println!("MUT_AFTER_DELETE {} {}", moved, id(db, &mut c)?);
    println!("MUT_RANGE_NOW {}", ids_string(&scan_ids(db, &mut c)?));

    // update the indexed key of another index through the ranged cursor
    check(c.find_first_row_by_entry(db, &[long(8)])?, "find 8")?;
    c.set_current_row_value(db, "Num", long(77))?;
    println!("MUT_KEY {}", row_string(&c.get_current_row(db)?));
    let found = c.find_first_row_by_entry(db, &[long(8)])?;
    println!("MUT_KEY_FIND {} {}", found, id(db, &mut c)?);

    // change the indexed key of the cursor's own index, moving the row out of
    // the range
    let mut num_cursor = ranged(db, "ByNum", |b| {
        b.set_start_entry(vec![long(3)])
            .set_end_entry(vec![long(5)]);
    })?;
    check(
        num_cursor.find_first_row_by_entry(db, &[long(3)])?,
        "find num 3",
    )?;
    println!(
        "MUT_NUM_BEFORE {}",
        row_string(&num_cursor.get_current_row(db)?)
    );
    num_cursor.set_current_row_value(db, "Num", long(99))?;
    println!(
        "MUT_NUM_AFTER {}",
        row_string(&num_cursor.get_current_row(db)?)
    );
    println!(
        "MUT_NUM_RANGE {}",
        ids_string(&scan_ids(db, &mut num_cursor)?)
    );
    num_cursor.find_closest_row_by_entry(db, &[long(3)])?;
    println!(
        "MUT_NUM_CLOSEST {}",
        row_string(&num_cursor.get_current_row(db)?)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// iterable removals
// ---------------------------------------------------------------------------

fn ids_ranges(db: &mut Database, label: &str) -> Result<()> {
    let mut c = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(1)])
            .set_end_entry(vec![long(10)]);
    })?;
    println!("{} {}", label, ids_string(&scan_ids(db, &mut c)?));
    Ok(())
}

fn run_iterable_removes(db: &mut Database) -> Result<()> {
    // delete the rows of an entry run through the entry iterator
    let mut by_num = index_cursor(db, "ByNum")?;
    let mut iter = EntryIterableBuilder::new(vec![long(4)]).rows();
    let mut removed = Vec::new();
    while iter.has_next(&mut by_num, db)? {
        let row = iter.next_row(&mut by_num, db)?.unwrap();
        removed.push(value_string(&row[0]));
        iter.remove(&mut by_num, db)?;
    }
    println!("RM_ENTRY {}", strings_string(&removed));
    println!(
        "RM_ENTRY_AGAIN {}",
        by_num.find_first_row_by_entry(db, &[long(4)])?
    );
    ids_ranges(db, "RM_AFTER_ENTRY")?;

    // remove() twice for the same row is rejected
    let mut pk = index_cursor(db, "PrimaryKey")?;
    let mut iter = EntryIterableBuilder::new(vec![long(3)]).rows();
    let has = iter.has_next(&mut pk, db)?;
    let row = iter.next_row(&mut pk, db)?.unwrap();
    println!("RM_FIRST {} {}", has, value_string(&row[0]));
    iter.remove(&mut pk, db)?;
    println!("RM_TWICE {}", error(|| iter.remove(&mut pk, db)));

    // delete the first rows of a ranged simple iteration
    let mut ranged_cursor = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(2)])
            .set_end_entry(vec![long(5)]);
    })?;
    let mut iter = IterableBuilder::new().rows();
    let mut removed2 = Vec::new();
    for _ in 0..2 {
        if !iter.has_next(&mut ranged_cursor, db)? {
            break;
        }
        let row = iter.next_row(&mut ranged_cursor, db)?.unwrap();
        removed2.push(value_string(&row[0]));
        iter.remove(&mut ranged_cursor, db)?;
    }
    println!("RM_RANGE {}", strings_string(&removed2));
    ids_ranges(db, "RM_AFTER_RANGE")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// delete and restore
// ---------------------------------------------------------------------------

fn run_delete_and_restore(db: &mut Database) -> Result<()> {
    let mut c = index_cursor(db, "PrimaryKey")?;
    check(c.find_first_row_by_entry(db, &[long(20)])?, "find 20")?;
    let sp: Savepoint = c.get_savepoint();
    c.delete_current_row(db)?;
    println!(
        "DEL_RESTORE_DELETED {} {}",
        flags(&c),
        c.is_current_row_deleted(db)?
    );
    c.restore_savepoint(&sp)?;
    let is_valid = valid(db, &mut c)?;
    println!(
        "DEL_RESTORE_RESTORED {} {} {} {}",
        pos_id(&c),
        flags(&c),
        is_valid,
        c.is_current_row_deleted(db)?
    );
    let moved = c.move_to_next_row(db)?;
    println!("DEL_RESTORE_NEXT {} {}", moved, flags(&c));
    Ok(())
}

// ---------------------------------------------------------------------------
// final read back
// ---------------------------------------------------------------------------

fn read_everything_back(db: &mut Database) -> Result<()> {
    let mut c = TableScanCursor::new(db, "T")?;
    let mut rows = Vec::new();
    c.before_first();
    while let Some(r) = c.get_next_row(db)? {
        rows.push(row_string(&r));
    }
    println!("FINAL {}", strings_string(&rows));
    // `Table.getRowCount`
    let count = c.get_row_count(db)?;
    println!("FINAL_COUNT {}", count);

    let mut by_num = index_cursor(db, "ByNum")?;
    let mut pairs = Vec::new();
    by_num.before_first();
    while let Some(r) = by_num.get_next_row(db)? {
        pairs.push(format!("{}:{}", value_string(&r[0]), value_string(&r[2])));
    }
    println!("FINAL_NUM {}", strings_string(&pairs));

    let mut by_num_id = index_cursor(db, "ByNumId")?;
    pairs.clear();
    by_num_id.before_first();
    while let Some(r) = by_num_id.get_next_row(db)? {
        pairs.push(format!("{}:{}", value_string(&r[0]), value_string(&r[2])));
    }
    println!("FINAL_NUMID {}", strings_string(&pairs));

    let mut ranged_cursor = ranged(db, "PrimaryKey", |b| {
        b.set_start_entry(vec![long(5)])
            .set_end_entry(vec![long(10)]);
    })?;
    println!(
        "FINAL_RANGE {}",
        ids_string(&scan_ids(db, &mut ranged_cursor)?)
    );
    println!(
        "FINAL_RANGE_B {}",
        ids_string(&scan_ids_backwards(db, &mut ranged_cursor)?)
    );
    Ok(())
}
