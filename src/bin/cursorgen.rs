//! `cursorgen` - runs the cursor scenario (the Rust half of
//! `tools/verify-cursor.ps1`), mirroring `java-src/CursorGen.java`.
//!
//! Usage: `cursorgen <out.mdb> <format> [--now-ms N]`
//!
//! The generator builds the table, runs the same cursor operations as the Java
//! reference (table-scan lookups/updates/deletes and index-cursor
//! lookups/updates/deletes over the primary key, a unique text index and
//! ascending/descending non-unique indexes), then adds more rows through the
//! table and reads everything back.  The trace printed on stdout is compared
//! with the Java trace by the harness, as are the resulting database files.

use std::collections::BTreeMap;

use jackcess::column::ColumnSpec;
use jackcess::cursor::{ColumnMatcher, Cursor, IndexCursor, TableScanCursor};
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn run() -> jackcess::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: cursorgen <out.mdb> <format> [--now-ms N]");
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
    run_table_scan(&mut db)?;
    run_primary_key_cursor(&mut db)?;
    run_name_cursor(&mut db)?;
    run_num_cursor(&mut db)?;
    run_desc_num_cursor(&mut db)?;
    add_more_rows(&mut db)?;
    read_everything_back(&mut db)?;
    println!("DONE");
    db.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// scenario data
// ---------------------------------------------------------------------------

fn make_table(db: &mut Database) -> jackcess::Result<()> {
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

fn int_or_null(value: Option<i32>) -> Value {
    match value {
        Some(n) => Value::Long(n),
        None => Value::Null,
    }
}

fn text_or_null(value: Option<String>) -> Value {
    match value {
        Some(s) => Value::Text(s),
        None => Value::Null,
    }
}

/// Adds rows one at a time, like `Table.addRowFromMap` (see the note in
/// `mdbgen.rs`: the per-row index page write history is observable).
fn add_rows_one_by_one(db: &mut Database, rows: &[Vec<Value>]) -> jackcess::Result<()> {
    for row in rows {
        db.add_rows("T", std::slice::from_ref(row))?;
    }
    Ok(())
}

fn add_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 1..=20i32 {
        rows.push(vec![
            Value::Long(i),
            Value::Text(name(i)),
            int_or_null(num(i)),
            text_or_null(note(i)),
        ]);
    }
    add_rows_one_by_one(db, &rows)
}

fn add_more_rows(db: &mut Database) -> jackcess::Result<()> {
    let mut rows = Vec::new();
    for i in 21..=25i32 {
        rows.push(vec![
            Value::Long(i),
            Value::Text(name(i)),
            Value::Long((i * 7) % 5),
            if i % 2 == 0 {
                Value::Null
            } else {
                Value::Text(format!("added {}", i))
            },
        ]);
    }
    add_rows_one_by_one(db, &rows)
}

// ---------------------------------------------------------------------------
// trace formatting (kept identical to CursorGen.java)
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

fn pairs_string(items: &[String]) -> String {
    strings_string(items)
}

fn pattern(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn check(cond: bool, what: &str) -> jackcess::Result<()> {
    if cond {
        Ok(())
    } else {
        Err(jackcess::Error::IllegalState(format!(
            "assertion failed: {}",
            what
        )))
    }
}

fn id(c: &mut dyn Cursor, db: &mut Database) -> jackcess::Result<i32> {
    Ok(c.get_current_row_value(db, "ID")?.as_i64().unwrap_or(-1) as i32)
}

fn scan_ids(c: &mut dyn Cursor, db: &mut Database) -> jackcess::Result<Vec<i32>> {
    let mut ids = Vec::new();
    c.before_first();
    while c.get_next_row(db)?.is_some() {
        ids.push(id(c, db)?);
    }
    Ok(ids)
}

fn scan_ids_backwards(c: &mut dyn Cursor, db: &mut Database) -> jackcess::Result<Vec<i32>> {
    let mut ids = Vec::new();
    c.after_last();
    while c.get_previous_row(db)?.is_some() {
        ids.push(id(c, db)?);
    }
    Ok(ids)
}

fn valid(c: &mut dyn Cursor, db: &mut Database) -> jackcess::Result<bool> {
    c.is_current_row_valid(db)
}

/// The run of rows a by-entry lookup finds, the equivalent of iterating
/// `IndexCursor.newEntryIterable` (EntryIterator.findNext).
fn entry_ids(
    c: &mut IndexCursor,
    db: &mut Database,
    values: &[Value],
) -> jackcess::Result<Vec<i32>> {
    let mut ids = Vec::new();
    if c.find_first_row_by_entry(db, values)? {
        ids.push(id(c, db)?);
        while c.find_next_row_by_entry(db, values)? {
            ids.push(id(c, db)?);
        }
    }
    Ok(ids)
}

fn reverse(ids: &[i32]) -> Vec<i32> {
    let mut out = ids.to_vec();
    out.reverse();
    out
}

// ---------------------------------------------------------------------------
// table scan cursor
// ---------------------------------------------------------------------------

fn run_table_scan(db: &mut Database) -> jackcess::Result<()> {
    let mut c = TableScanCursor::new(db, "T")?;

    let forward = scan_ids(&mut c, db)?;
    println!("SCAN_F {}", ids_string(&forward));
    let back = scan_ids_backwards(&mut c, db)?;
    println!("SCAN_B {}", ids_string(&back));
    check(back == reverse(&forward), "reverse scan order")?;

    check(
        c.find_first_row(db, &pattern(&[("Name", Value::Text("NAME05".into()))]))?,
        "find name05",
    )?;
    println!("F1 {}", id(&mut c, db)?);

    check(
        c.find_first_row_by_column(db, "Num", &Value::Long(5))?,
        "find num 5",
    )?;
    println!("F2 {}", id(&mut c, db)?);

    // update through the table scan cursor: change Name and Num, then a no-op
    // updateCurrentRow which keeps every column
    c.set_current_row_value(db, "Name", Value::Text("scanned04".into()))?;
    c.set_current_row_value(db, "Num", Value::Long(55))?;
    c.update_current_row(db, &[Value::Keep, Value::Keep, Value::Keep, Value::Keep])?;
    println!("U1 {}", row_string(&c.get_current_row(db)?));

    check(
        c.find_first_row(db, &pattern(&[("ID", Value::Long(10))]))?,
        "find id 10",
    )?;
    c.update_current_row_from_map(
        db,
        &pattern(&[
            ("Note", Value::Text("updated ten".into())),
            ("Num", Value::Long(123)),
        ]),
    )?;
    println!("U2 {}", row_string(&c.get_current_row(db)?));

    check(
        c.find_first_row(db, &pattern(&[("ID", Value::Long(11))]))?,
        "find id 11",
    )?;
    c.delete_current_row(db)?;
    println!(
        "D1 valid={} deleted={}",
        valid(&mut c, db)?,
        c.is_current_row_deleted(db)?
    );

    check(
        !c.find_first_row(db, &pattern(&[("ID", Value::Long(9999))]))?,
        "find missing id",
    )?;
    println!("NF1 deleted={}", c.is_current_row_deleted(db)?);

    check(
        c.find_first_row_by_column(db, "Num", &Value::Long(3))?,
        "find num 3",
    )?;
    println!("F3 {}", id(&mut c, db)?);
    check(
        c.find_next_row_by_column(db, "Num", &Value::Long(3))?,
        "next num 3",
    )?;
    println!("F4 {}", id(&mut c, db)?);
    check(
        !c.find_next_row_by_column(db, "Num", &Value::Long(3))?,
        "no third num 3",
    )?;
    check(
        c.find_last_row(db, &pattern(&[("Num", Value::Long(3))]))?,
        "last num 3",
    )?;
    println!("F5 {}", id(&mut c, db)?);

    c.set_column_matcher(ColumnMatcher::CaseInsensitive);
    check(
        c.find_first_row(db, &pattern(&[("Name", Value::Text("name05".into()))]))?,
        "ci find name05",
    )?;
    println!("F6 {}", id(&mut c, db)?);
    c.set_column_matcher(ColumnMatcher::Simple);

    println!("COUNT1 {}", c.get_row_count(db)?);
    println!("SCAN_F2 {}", ids_string(&scan_ids(&mut c, db)?));
    Ok(())
}

// ---------------------------------------------------------------------------
// primary key index cursor
// ---------------------------------------------------------------------------

fn run_primary_key_cursor(db: &mut Database) -> jackcess::Result<()> {
    let mut c = IndexCursor::new(db, "T", "PrimaryKey")?;

    let forward = scan_ids(&mut c, db)?;
    println!("PK_F {}", ids_string(&forward));
    println!("PK_B {}", ids_string(&scan_ids_backwards(&mut c, db)?));

    check(
        c.find_first_row_by_entry(db, &[Value::Long(7)])?,
        "pk entry 7",
    )?;
    println!("PK_E7 {}", id(&mut c, db)?);

    // change the indexed key (the primary key) and a non-indexed column
    c.set_current_row_value(db, "Note", Value::Text("via pk".into()))?;
    c.set_current_row_value(db, "ID", Value::Long(30))?;
    c.update_current_row(db, &[Value::Keep, Value::Keep, Value::Keep, Value::Keep])?;
    println!("PK_U {}", row_string(&c.get_current_row(db)?));
    check(
        c.find_first_row_by_entry(db, &[Value::Long(30)])?,
        "pk entry 30",
    )?;
    println!("PK_E30 {}", row_string(&c.get_current_row(db)?));
    check(
        !c.find_first_row_by_entry(db, &[Value::Long(7)])?,
        "pk entry 7 gone",
    )?;

    c.find_closest_row_by_entry(db, &[Value::Long(13)])?;
    println!("PK_C13 {}", id(&mut c, db)?);

    check(
        c.find_first_row_by_entry(db, &[Value::Long(12)])?,
        "pk entry 12",
    )?;
    c.delete_current_row(db)?;
    println!(
        "PK_D12 valid={} deleted={}",
        valid(&mut c, db)?,
        c.is_current_row_deleted(db)?
    );
    println!("PK_F2 {}", ids_string(&scan_ids(&mut c, db)?));
    Ok(())
}

// ---------------------------------------------------------------------------
// unique text index cursor
// ---------------------------------------------------------------------------

fn run_name_cursor(db: &mut Database) -> jackcess::Result<()> {
    let mut c = IndexCursor::new(db, "T", "ByName")?;

    let mut names = Vec::new();
    c.before_first();
    while let Some(row) = c.get_next_row(db)? {
        names.push(value_string(row.get(1).unwrap_or(&Value::Null)));
    }
    println!("NAME_F {}", strings_string(&names));

    // the unique text index folds case, so a lookup with other case matches
    check(
        c.find_first_row_by_entry(db, &[Value::Text("name05".into())])?,
        "name entry name05",
    )?;
    println!("NAME_E5 {}", id(&mut c, db)?);

    // update the indexed Name key and the other columns
    c.set_current_row_value(db, "Name", Value::Text("renamed05".into()))?;
    c.set_current_row_value(db, "Num", Value::Long(77))?;
    c.set_current_row_value(db, "Note", Value::Text("by name".into()))?;
    c.update_current_row(db, &[Value::Keep, Value::Keep, Value::Keep, Value::Keep])?;
    println!("NAME_U {}", row_string(&c.get_current_row(db)?));
    check(
        c.find_first_row_by_entry(db, &[Value::Text("RENAMED05".into())])?,
        "renamed entry",
    )?;
    println!("NAME_E5b {}", row_string(&c.get_current_row(db)?));

    // the run of a unique index holds one row
    println!(
        "NAME_RUN {}",
        ids_string(&entry_ids(&mut c, db, &[Value::Text("renamed05".into())])?)
    );

    check(
        c.find_first_row_by_entry(db, &[Value::Text("name18".into())])?,
        "name entry name18",
    )?;
    c.delete_current_row(db)?;
    println!(
        "NAME_D18 valid={} deleted={}",
        valid(&mut c, db)?,
        c.is_current_row_deleted(db)?
    );

    check(
        !c.find_first_row_by_entry(db, &[Value::Text("name21".into())])?,
        "name21 not added yet",
    )?;
    println!("NAME_E21 false");
    Ok(())
}

// ---------------------------------------------------------------------------
// non-unique index cursor (ascending, nulls first)
// ---------------------------------------------------------------------------

fn run_num_cursor(db: &mut Database) -> jackcess::Result<()> {
    let mut c = IndexCursor::new(db, "T", "ByNum")?;

    let mut pairs = Vec::new();
    c.before_first();
    while let Some(row) = c.get_next_row(db)? {
        pairs.push(format!(
            "{}:{}",
            value_string(row.first().unwrap_or(&Value::Null)),
            value_string(row.get(2).unwrap_or(&Value::Null))
        ));
    }
    println!("NUM_F {}", pairs_string(&pairs));

    // two rows share Num 3 (ids 9 and 20): the by-entry iteration walks the
    // run (the Java side uses newEntryIterable, which is the same iteration)
    println!(
        "NUM_RUN3 {}",
        ids_string(&entry_ids(&mut c, db, &[Value::Long(3)])?)
    );

    // update the first row of the run (id 9), changing the indexed key
    check(
        c.find_first_row_by_entry(db, &[Value::Long(3)])?,
        "num entry 3",
    )?;
    println!("NUM_E3 {}", id(&mut c, db)?);
    c.update_current_row_from_map(
        db,
        &pattern(&[
            ("Num", Value::Long(88)),
            ("Note", Value::Text("run 09".into())),
        ]),
    )?;
    println!("NUM_U {}", row_string(&c.get_current_row(db)?));
    println!(
        "NUM_RUN3B {}",
        ids_string(&entry_ids(&mut c, db, &[Value::Long(3)])?)
    );

    c.find_closest_row_by_entry(db, &[Value::Long(6)])?;
    println!(
        "NUM_C6 {}:{}",
        value_string(&c.get_current_row_value(db, "ID")?),
        value_string(&c.get_current_row_value(db, "Num")?)
    );

    // delete the only remaining row with Num 3 (id 20) through this cursor
    check(
        c.find_first_row_by_entry(db, &[Value::Long(3)])?,
        "num entry 3 for delete",
    )?;
    c.delete_current_row(db)?;
    println!(
        "NUM_D20 valid={} deleted={}",
        valid(&mut c, db)?,
        c.is_current_row_deleted(db)?
    );
    println!(
        "NUM_RUN3C {}",
        ids_string(&entry_ids(&mut c, db, &[Value::Long(3)])?)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// descending index cursor (nulls last)
// ---------------------------------------------------------------------------

fn run_desc_num_cursor(db: &mut Database) -> jackcess::Result<()> {
    let mut c = IndexCursor::new(db, "T", "ByNumDesc")?;

    let mut pairs = Vec::new();
    c.before_first();
    while let Some(row) = c.get_next_row(db)? {
        pairs.push(format!(
            "{}:{}",
            value_string(row.first().unwrap_or(&Value::Null)),
            value_string(row.get(2).unwrap_or(&Value::Null))
        ));
    }
    println!("DESC_F {}", pairs_string(&pairs));

    check(
        c.find_first_row_by_entry(db, &[Value::Long(8)])?,
        "desc entry 8",
    )?;
    println!(
        "DESC_E8 {}:{}",
        value_string(&c.get_current_row_value(db, "ID")?),
        value_string(&c.get_current_row_value(db, "Num")?)
    );
    c.set_current_row_value(db, "Note", Value::Text("desc".into()))?;
    println!("DESC_U {}", row_string(&c.get_current_row(db)?));

    c.find_closest_row_by_entry(db, &[Value::Long(9)])?;
    println!(
        "DESC_C9 {}:{}",
        value_string(&c.get_current_row_value(db, "ID")?),
        value_string(&c.get_current_row_value(db, "Num")?)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// final read back
// ---------------------------------------------------------------------------

fn read_everything_back(db: &mut Database) -> jackcess::Result<()> {
    let mut c = TableScanCursor::new(db, "T")?;
    c.before_first();
    let mut rows = Vec::new();
    while let Some(row) = c.get_next_row(db)? {
        rows.push(row_string(&row));
    }
    println!("FINAL {}", strings_string(&rows));
    println!("FINAL_COUNT {}", c.get_row_count(db)?);

    check(
        c.find_first_row(db, &pattern(&[("Name", Value::Text("name22".into()))]))?,
        "final find name22",
    )?;
    println!("FINAL_F22 {}", row_string(&c.get_current_row(db)?));

    let mut pk = IndexCursor::new(db, "T", "PrimaryKey")?;
    check(
        pk.find_first_row_by_entry(db, &[Value::Long(22)])?,
        "final pk 22",
    )?;
    println!("FINAL_PK22 {}", row_string(&pk.get_current_row(db)?));

    let mut by_name = IndexCursor::new(db, "T", "ByName")?;
    check(
        by_name.find_first_row_by_entry(db, &[Value::Text("name23".into())])?,
        "final name23",
    )?;
    println!("FINAL_NAME23 {}", row_string(&by_name.get_current_row(db)?));
    Ok(())
}
