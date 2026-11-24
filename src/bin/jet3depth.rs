//! `jet3depth` - depth/exception parity dump of a Jet 3 database, the mirror
//! of `java-src/Jet3DepthGen.java`.
//!
//! The canonical dump (`jet3dump`) compares the reading surfaces.  This dump
//! compares the parts which must *fail*: every write entry point is attempted
//! on a fresh, read-only database instance and the resulting error is recorded
//! together with:
//!   * whether the file bytes stayed unchanged,
//!   * whether the database can be reopened and read afterwards,
//!   * a before/after structural fingerprint (table count, per table row and
//!     owned page counts, the first row of the first user table).
//!
//! Java's exception class/message is canonicalised to the *root cause* of its
//! cause chain (the exception the read-only channel throws).  This side maps
//! its error to the same Java class/message:
//!   * any error whose text names `NonWritableChannelException` is
//!     `java.nio.channels.NonWritableChannelException` with a null message;
//!   * the port emits Java NIO's `Buffer.position` wording
//!     (`newPosition > limit: (N > M)`, `newPosition < 0: (N < 0)`)
//!     directly, so a position error is Java's `java.lang.IllegalArgumentException`
//!     with exactly that message;
//!   * otherwise the port's error variant maps to the Java exception class
//!     named by [`crate::error`].
//!
//! The *top level* exception (what a caller sees, e.g. the
//! `BatchUpdateException` `Table.addRows` wraps a failure in) is recorded too,
//! with Java's `getUpdateCount()` for the batch exception.
//!
//! The `api` lines dump the read-side surface the depth audit covers: the
//! catalog dates of every table (the system catalog included), the sort ids of
//! the textual columns, the referenced index of every index, the `MSysACEs`
//! rows and the (cached) name map of every table.
//!
//! Usage: `jet3depth <file.mdb>` (stdout is the dump, stderr diagnostics).

use jackcess::column::ColumnSpec;
use jackcess::database::{Database, TABLE_SYSTEM_ACES};
use jackcess::datatype::DataType;
use jackcess::error::Error;
use jackcess::table::RowId;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;
use sha2::{Digest, Sha256};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: jet3depth <file.mdb>");
        std::process::exit(2);
    }
    let file = args[1].clone();
    let mut out = String::with_capacity(1 << 18);
    match run(&file, &mut out) {
        Ok(()) => print!("{}", out),
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}

fn run(file: &str, out: &mut String) -> jackcess::Result<()> {
    let jet3 = {
        let db = Database::open_path(std::path::Path::new(file))?;
        (db.format.name, db.charset.name(), db.is_read_only())
    };
    out.push_str("depth 2\n");
    out.push_str(&format!("format {}\n", jet3.0));
    out.push_str(&format!("charset {}\n", jet3.1));
    out.push_str(&format!("readonly {}\n", jet3.2));
    let initial_digest = digest(file).map_err(Error::from)?;
    out.push_str(&format!("filedigest {}\n", initial_digest));

    dump_state(file, "before", out)?;
    dump_api_surface(file, out)?;

    let probes: &[(&str, ProbeFn)] = &[
        ("addRow", probe_add_row),
        ("addRows", probe_add_rows),
        ("updateRow", probe_update_row),
        ("updateValue", probe_update_value),
        ("deleteRow", probe_delete_row),
        ("addColumn", probe_add_column),
        ("addIndex", probe_add_index),
        ("createTable", probe_create_table),
        ("createLinkedTable", probe_create_linked_table),
        ("saveTableProperties", probe_save_table_properties),
        ("saveDatabaseProperties", probe_save_database_properties),
        ("saveSystemProperties", probe_save_system_properties),
        ("createRelationship", probe_create_relationship),
        (
            "setAllowAutoNumberInsert",
            probe_set_allow_auto_number_insert,
        ),
        (
            "setTableAllowAutoNumberInsert",
            probe_set_table_allow_auto_number_insert,
        ),
        ("reclaimFreeSpace", probe_reclaim_free_space),
        ("flush", probe_flush),
    ];
    for (name, probe) in probes {
        write_probe(file, &initial_digest, name, *probe, out)?;
    }

    dump_state(file, "afterreopen", out)?;
    out.push_str(&format!(
        "afterfiledigest {}\n",
        digest(file).map_err(Error::from)?
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// structural fingerprint
// ---------------------------------------------------------------------------

fn dump_state(file: &str, prefix: &str, out: &mut String) -> jackcess::Result<()> {
    let mut db = Database::open_path(std::path::Path::new(file))?;
    let user_tables = sort_names(db.get_table_names()?);
    let system_tables = sort_names(db.get_system_table_names()?);
    out.push_str(&format!(
        "{} tables {} systemtables {}\n",
        prefix,
        user_tables.len(),
        system_tables.len()
    ));
    let all: Vec<String> = user_tables
        .iter()
        .chain(system_tables.iter())
        .cloned()
        .collect();
    for name in &all {
        let table = db.get_system_table(name)?.expect("table exists");
        let owned = table
            .owned_pages
            .as_ref()
            .map(|p| p.iter_pages().count())
            .unwrap_or(0);
        out.push_str(&format!(
            "{} {} rows {} owned {}\n",
            prefix,
            esc(name),
            table.row_count,
            owned
        ));
    }
    if let Some(first) = user_tables.first() {
        let rows = db.read_rows_with_ids(first)?;
        match rows.into_iter().find(|(_, v)| v.is_some()) {
            Some((id, values)) => {
                out.push_str(&format!(
                    "{} firstrow {} {}/{}",
                    prefix,
                    esc(first),
                    id.0,
                    id.1
                ));
                for v in &values.unwrap() {
                    out.push(' ');
                    out.push_str(&val(v));
                }
                out.push('\n');
            }
            None => out.push_str(&format!("{} firstrow {} null\n", prefix, esc(first))),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// read-side API surface
// ---------------------------------------------------------------------------

/// The `api ...` lines of the depth audit (see the module docs): every table's
/// catalog dates, metadata system flag, column sort ids, index references, the
/// `MSysACEs` rows and the table name maps.
fn dump_api_surface(file: &str, out: &mut String) -> jackcess::Result<()> {
    let mut db = Database::open_path(std::path::Path::new(file))?;
    let user_tables = sort_names(db.get_table_names()?);
    let system_tables = sort_names(db.get_system_table_names()?);
    for name in user_tables.iter().chain(system_tables.iter()) {
        let (created, updated, sort_orders, index_names) = {
            let table = db.get_system_table(name)?.expect("table exists");
            let sort_orders: Vec<(String, jackcess::format::SortOrder)> = table
                .columns
                .iter()
                .filter_map(|column| column.sort_order.map(|sort| (column.name.clone(), sort)))
                .collect();
            (
                table.get_created_date(),
                table.get_updated_date(),
                sort_orders,
                table
                    .indexes
                    .iter()
                    .map(|i| i.name.clone())
                    .collect::<Vec<_>>(),
            )
        };
        out.push_str(&format!(
            "api tablecreated {} {}\n",
            esc(name),
            date_val(created)
        ));
        out.push_str(&format!(
            "api tableupdated {} {}\n",
            esc(name),
            date_val(updated)
        ));
        let is_system = db
            .get_table_meta_data(name)?
            .map(|meta| meta.is_system())
            .unwrap_or(false);
        out.push_str(&format!("api metasystem {} {}\n", esc(name), is_system));

        for (column, sort_order) in sort_orders {
            out.push_str(&format!("api sortid {} {} ", esc(name), esc(&column)));
            match sort_order.get_sort_id() {
                Ok(sort_id) => out.push_str(&format!("id {}", sort_id)),
                Err(error) => {
                    let (class, message, _) = canonical_top_error(&error);
                    out.push_str(&format!("throws {} {}", esc(&class), esc(&message)));
                }
            }
            out.push('\n');
        }

        for index in index_names {
            out.push_str(&format!("api refindex {} {} ", esc(name), esc(&index)));
            let reference_page = db
                .get_system_table(name)?
                .and_then(|table| table.get_index(&index).ok())
                .and_then(|index| index.reference.map(|r| r.other_table_page_number));
            match db.get_referenced_index(name, &index) {
                Ok(None) => out.push_str("none"),
                Ok(Some(referenced)) => {
                    let referenced_table = reference_page
                        .and_then(|page| db.table_by_page(page).ok())
                        .map(|table| table.name.clone())
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "{} {}",
                        esc(&referenced_table),
                        esc(&referenced.name)
                    ));
                }
                Err(error) => {
                    let (class, message, _) = canonical_top_error(&error);
                    out.push_str(&format!("throws {} {}", esc(&class), esc(&message)));
                }
            }
            out.push('\n');
        }

        // `Table.getNameMap()` reads the `NameMap` property of any table; the
        // database level call also caches the value on an uninitialised table
        // (the system catalog), and the table level call then returns the same
        // map from the cache
        let from_database = db.get_table_name_map(name)?;
        out.push_str(&format!(
            "api namemap {} {}\n",
            esc(name),
            name_map_val(from_database.as_ref())
        ));
        let cached = db
            .get_system_table(name)?
            .and_then(|table| table.get_name_map().ok().flatten());
        out.push_str(&format!(
            "api namemapcached {} {}\n",
            esc(name),
            name_map_val(cached.as_ref())
        ));
    }

    let (aces_name, aces_row_count) = {
        let table = db.get_access_control_entries()?;
        (table.name.clone(), table.row_count)
    };
    out.push_str(&format!(
        "api aces {} {}\n",
        esc(&aces_name),
        aces_row_count
    ));
    let entries = db.get_access_control_entries_rows()?;
    for (i, entry) in entries.iter().enumerate() {
        out.push_str(&format!(
            "api acerow {} {} {} {} {}\n",
            i,
            val(&Value::Long(entry.object_id)),
            val(&Value::Bytes(entry.sid.clone())),
            val(&Value::Long(entry.acm)),
            val(&Value::Bool(entry.f_inheritable))
        ));
    }
    out.push_str(&format!("api acecount {}\n", entries.len()));
    Ok(())
}

fn date_val(date: Option<jackcess::datenums::LocalDateTime>) -> String {
    match date {
        Some(date) => format!("dt:{}", iso(&date)),
        None => "null".to_string(),
    }
}

/// `api namemap`'s value: Java's `TableNameMap` (`present <records> <guid>`)
/// or `absent`.
fn name_map_val(name_map: Option<&jackcess::namemap::TableNameMap>) -> String {
    match name_map {
        None => "absent".to_string(),
        Some(name_map) => format!(
            "present {} {}",
            name_map.name_map().records().len(),
            match name_map.get_table_record() {
                Some(record) => nullable_string(record.get_guid().as_deref()),
                None => "null".to_string(),
            }
        ),
    }
}

// ---------------------------------------------------------------------------
// write probes
// ---------------------------------------------------------------------------

type ProbeFn = fn(&mut Database, &str, Option<RowId>, &[Value]) -> jackcess::Result<Option<String>>;

fn write_probe(
    file: &str,
    initial_digest: &str,
    name: &str,
    probe: ProbeFn,
    out: &mut String,
) -> jackcess::Result<()> {
    let before = digest(file).map_err(Error::from)?;
    let mut outcome: Option<(String, String)> = None;
    let mut top: Option<(String, String, Option<i32>)> = None;
    let mut probe_value: Option<String> = None;
    {
        let mut db = Database::open_path(std::path::Path::new(file))?;
        let user_tables = sort_names(db.get_table_names()?);
        let table_name = user_tables
            .first()
            .cloned()
            .ok_or_else(|| Error::Jackcess("no user table".into()))?;
        let (row_id, row) = write_row(&mut db, &table_name)?;
        if row_id.is_none() && needs_live_row(name) {
            // Java's `Table.updateRow(null, ...)` throws a NullPointerException;
            // the port's row operations take a `RowId`, not a nullable one, so
            // there is no equivalent call to make.  Both dumps record the
            // empty-table case explicitly instead.
            out.push_str(&format!(
                "write {} skipped no-live-row top - topmsg - topcount -\n",
                name
            ));
            return Ok(());
        }
        match probe(&mut db, &table_name, row_id, &row) {
            Ok(value) => probe_value = value,
            Err(e) => {
                // the exception the caller sees (`Table.addRows(List)` wraps a
                // failure in a `BatchUpdateException`) ...
                top = Some(canonical_top_error(&e));
                // ... and the root cause
                outcome = Some(canonical_error(&e));
            }
        }
    }
    let after = digest(file).map_err(Error::from)?;
    let unchanged = before == after && before == initial_digest;
    let reopen = reopen_summary(file);
    let refused = outcome.is_some();
    let (class, msg) = match &outcome {
        Some((c, m)) => (esc(c), esc(m)),
        None => ("-".to_string(), "-".to_string()),
    };
    let (top_class, top_msg, top_count) = match &top {
        Some((class, msg, count)) => (
            esc(class),
            esc(msg),
            count
                .map(|count| count.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
        None => ("-".to_string(), "-".to_string(), "-".to_string()),
    };
    out.push_str(&format!(
        "write {} refused {} class {} msg {} unchanged {} reopen {} top {} topmsg {} topcount {}\n",
        name, refused, class, msg, unchanged, reopen, top_class, top_msg, top_count
    ));
    if name == "reclaimFreeSpace" {
        out.push_str(&format!(
            "writevalue reclaimFreeSpace {}\n",
            probe_value.unwrap_or_else(|| "-".into())
        ));
    }
    Ok(())
}

fn needs_live_row(name: &str) -> bool {
    matches!(name, "updateRow" | "updateValue" | "deleteRow")
}

fn reopen_summary(file: &str) -> String {
    match Database::open_path(std::path::Path::new(file)) {
        Ok(mut db) => match (db.get_table_names(), db.get_system_table_names()) {
            (Ok(user), Ok(system)) => format!("ok:{}:{}", user.len(), system.len()),
            _ => "error:names".to_string(),
        },
        Err(e) => format!("error:{}", e),
    }
}

/// The row a write probe inserts: the first row with a fresh value in every
/// unique/primary indexed column, or a default row for an empty table.
fn write_row(db: &mut Database, table_name: &str) -> jackcess::Result<(Option<RowId>, Vec<Value>)> {
    let rows = db.read_rows_with_ids(table_name)?;
    let base = rows.into_iter().find(|(_, v)| v.is_some());
    let table = db.get_table(table_name)?;
    let mut row = match &base {
        Some((_, Some(values))) => values.clone(),
        _ => table
            .columns
            .iter()
            .map(|c| default_value(c.data_type))
            .collect(),
    };
    let mut edits: Vec<(usize, DataType)> = Vec::new();
    for idx in &table.indexes {
        let unique = idx.is_primary_key()
            || table
                .index_data_for(idx)
                .map(|d| d.is_unique())
                .unwrap_or(false);
        if !unique {
            continue;
        }
        if let Some(data) = table.index_data_for(idx) {
            for ic in &data.columns {
                if let Some(ci) = table.columns.iter().position(|c| c.name == ic.column.name) {
                    edits.push((ci, ic.column.data_type));
                }
            }
        }
    }
    for (ci, dt) in edits {
        row[ci] = fresh(dt, &row[ci]);
    }
    Ok((base.map(|(id, _)| id), row))
}

fn default_value(dt: DataType) -> Value {
    match dt {
        DataType::Text | DataType::Memo => Value::Text("depthprobe".into()),
        DataType::Byte => Value::Byte(1),
        DataType::Int => Value::Int(1),
        DataType::Long => Value::Long(1),
        DataType::BigInt => Value::BigInt(1),
        DataType::Double => Value::Double(1.5),
        DataType::Float => Value::Float(1.5),
        DataType::Boolean => Value::Bool(false),
        DataType::ShortDateTime => Value::DateTime(jackcess::datenums::LocalDateTime {
            year: 2000,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            nano: 0,
        }),
        DataType::Money => Value::Decimal {
            unscaled: 100,
            scale: 4,
        },
        DataType::Numeric => Value::Decimal {
            unscaled: 100,
            scale: 0,
        },
        DataType::Binary | DataType::Ole | DataType::BigBinary => Value::Bytes(Vec::new()),
        _ => Value::Null,
    }
}

fn fresh(dt: DataType, v: &Value) -> Value {
    match dt {
        DataType::Text | DataType::Memo => {
            let s = match v {
                Value::Text(s) => s.clone(),
                _ => String::new(),
            };
            Value::Text(format!("{}zz", s))
        }
        DataType::Byte => Value::Byte(match v {
            Value::Byte(x) => x.wrapping_add(100),
            _ => 100,
        }),
        DataType::Int => Value::Int(match v {
            Value::Int(x) => x.wrapping_add(1000),
            _ => 1000,
        }),
        DataType::Long => Value::Long(match v {
            Value::Long(x) => x.wrapping_add(100000),
            _ => 100000,
        }),
        DataType::BigInt => Value::BigInt(match v {
            Value::BigInt(x) => x.wrapping_add(100000),
            _ => 100000,
        }),
        DataType::Double => Value::Double(match v {
            Value::Double(x) => x + 123456.0,
            _ => 123456.0,
        }),
        DataType::Float => Value::Float(match v {
            Value::Float(x) => x + 1234.0,
            _ => 1234.0,
        }),
        DataType::Boolean => Value::Bool(match v {
            Value::Bool(b) => !*b,
            _ => true,
        }),
        DataType::ShortDateTime => {
            let mut d = match v {
                Value::DateTime(d) => *d,
                _ => jackcess::datenums::LocalDateTime {
                    year: 1970,
                    month: 1,
                    day: 1,
                    hour: 0,
                    minute: 0,
                    second: 0,
                    nano: 0,
                },
            };
            d.year += 40;
            if d.month == 2 && d.day == 29 {
                d.day = 28;
            }
            Value::DateTime(d)
        }
        _ => v.clone(),
    }
}

fn probe_add_row(
    db: &mut Database,
    table_name: &str,
    _row_id: Option<RowId>,
    row: &[Value],
) -> jackcess::Result<Option<String>> {
    let (page, flags) = *db
        .table_lookup
        .get(&table_name.to_uppercase())
        .expect("table exists");
    db.load_table(page, table_name, flags)?;
    let tz = *db.get_time_zone();
    let table = db.tables.get_mut(&page).unwrap();
    table.add_row(&mut db.channel, row, &tz)?;
    Ok(None)
}

fn probe_add_rows(
    db: &mut Database,
    table_name: &str,
    _row_id: Option<RowId>,
    row: &[Value],
) -> jackcess::Result<Option<String>> {
    db.add_rows(table_name, &[row.to_vec()])?;
    Ok(None)
}

fn probe_update_row(
    db: &mut Database,
    table_name: &str,
    row_id: Option<RowId>,
    row: &[Value],
) -> jackcess::Result<Option<String>> {
    db.update_row(table_name, row_id.expect("row id"), row)?;
    Ok(None)
}

fn probe_update_value(
    db: &mut Database,
    table_name: &str,
    row_id: Option<RowId>,
    row: &[Value],
) -> jackcess::Result<Option<String>> {
    let column = db.get_table(table_name)?.columns[0].name.clone();
    db.update_value(table_name, row_id.expect("row id"), &column, row[0].clone())?;
    Ok(None)
}

fn probe_delete_row(
    db: &mut Database,
    table_name: &str,
    row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    db.delete_row(table_name, row_id.expect("row id"))?;
    Ok(None)
}

fn probe_add_column(
    db: &mut Database,
    table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    db.add_column(table_name, ColumnSpec::new("NewCol", DataType::Text))?;
    Ok(None)
}

fn probe_add_index(
    db: &mut Database,
    table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    let column = db.get_table(table_name)?.columns[0].name.clone();
    db.add_index(table_name, IndexSpec::new("NewIdx", vec![(column, true)]))?;
    Ok(None)
}

fn probe_create_table(
    db: &mut Database,
    _table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    db.create_table(&TableSpec::new(
        "NewTable",
        vec![ColumnSpec::new("a", DataType::Text)],
    ))?;
    Ok(None)
}

fn probe_create_linked_table(
    db: &mut Database,
    _table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    db.create_linked_table("NewLink", "other.mdb", "Other")?;
    Ok(None)
}

fn probe_save_table_properties(
    db: &mut Database,
    table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    let mut maps = db.get_table_properties_by_name(table_name)?;
    maps.get_default()
        .put_value("DepthProbe", Value::Text("v".into()))?;
    db.save_properties(&mut maps)?;
    Ok(None)
}

fn probe_save_database_properties(
    db: &mut Database,
    _table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    let mut maps = db.get_database_properties()?;
    maps.get_default()
        .put_value("DepthProbe", Value::Text("v".into()))?;
    db.save_properties(&mut maps)?;
    Ok(None)
}

fn probe_save_system_properties(
    db: &mut Database,
    _table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    let mut maps = db.get_table_properties_by_name(TABLE_SYSTEM_ACES)?;
    maps.get_default()
        .put_value("DepthProbe", Value::Text("v".into()))?;
    db.save_properties(&mut maps)?;
    Ok(None)
}

fn probe_create_relationship(
    db: &mut Database,
    table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    let column = db.get_table(table_name)?.columns[0].name.clone();
    let spec = jackcess::relationship::RelationshipSpec::new(table_name, table_name)
        .add_columns(column.clone(), column);
    db.add_relationship(spec)?;
    Ok(None)
}

fn probe_set_allow_auto_number_insert(
    db: &mut Database,
    _table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    db.set_allow_auto_number_insert(true);
    Ok(None)
}

fn probe_set_table_allow_auto_number_insert(
    db: &mut Database,
    table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    db.set_table_allow_auto_number_insert(table_name, Some(true))?;
    Ok(None)
}

fn probe_reclaim_free_space(
    db: &mut Database,
    _table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    let freed = db.reclaim_free_space()?;
    Ok(Some(freed.to_string()))
}

fn probe_flush(
    db: &mut Database,
    _table_name: &str,
    _row_id: Option<RowId>,
    _row: &[Value],
) -> jackcess::Result<Option<String>> {
    db.flush()?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// error canonicalisation
// ---------------------------------------------------------------------------

/// The Java exception class/message a port error corresponds to (the root
/// cause of the Java cause chain the dump records).
fn canonical_error(e: &Error) -> (String, String) {
    let text = e.to_string();
    if text.contains("NonWritableChannelException") {
        return (
            "java.nio.channels.NonWritableChannelException".to_string(),
            "<null>".to_string(),
        );
    }
    if let Some(message) = java_position_message(&text) {
        return ("java.lang.IllegalArgumentException".to_string(), message);
    }
    if let Some(marker) = text.find("illegal argument: ") {
        return (
            "java.lang.IllegalArgumentException".to_string(),
            text[marker + "illegal argument: ".len()..].to_string(),
        );
    }
    match e {
        Error::IllegalArgument(m) => ("java.lang.IllegalArgumentException".to_string(), m.clone()),
        Error::IllegalState(m) => ("java.lang.IllegalStateException".to_string(), m.clone()),
        Error::Unsupported(m) => (
            "java.lang.UnsupportedOperationException".to_string(),
            m.clone(),
        ),
        Error::InvalidCredentials(m) => (
            "com.healthmarketscience.jackcess.crypt.InvalidCredentialsException".to_string(),
            m.clone(),
        ),
        Error::ConstraintViolation(m) => (
            "com.healthmarketscience.jackcess.ConstraintViolationException".to_string(),
            m.clone(),
        ),
        Error::InvalidValue(m) => (
            "com.healthmarketscience.jackcess.InvalidValueException".to_string(),
            m.clone(),
        ),
        Error::BatchUpdate { cause, .. } => canonical_error(cause),
        Error::Jackcess(m) => ("java.io.IOException".to_string(), m.clone()),
        Error::Io(io) => ("java.io.IOException".to_string(), io.to_string()),
        Error::BufferUnderflow(m) => ("java.nio.BufferUnderflowException".to_string(), m.clone()),
        Error::BufferOverflow(m) => ("java.nio.BufferOverflowException".to_string(), m.clone()),
        // `InvalidMarkException` has no message (see `canonical_top_error`)
        Error::InvalidMark => (
            "java.nio.InvalidMarkException".to_string(),
            "null".to_string(),
        ),
        Error::NumberFormat(m) => ("java.lang.NumberFormatException".to_string(), m.clone()),
    }
}

/// The *top level* Java exception a port error corresponds to (what the caller
/// of the write entry point sees), with Java's `getUpdateCount()` for a
/// `BatchUpdateException`.
///
/// The port now emits NIO's `Buffer.position` wording itself, so a position
/// error is Java's `IllegalArgumentException` with the identical message; the
/// legacy-prefix canonicalisation in [`java_position_message`] is kept so an
/// older spelling still compares.
fn canonical_top_error(e: &Error) -> (String, String, Option<i32>) {
    match e {
        Error::BatchUpdate {
            update_count,
            message,
            ..
        } => (
            "com.healthmarketscience.jackcess.BatchUpdateException".to_string(),
            message.clone(),
            Some(*update_count),
        ),
        Error::Io(io) if io.to_string().contains("NonWritableChannelException") => (
            "java.nio.channels.NonWritableChannelException".to_string(),
            "<null>".to_string(),
            None,
        ),
        Error::Io(io) => ("java.io.IOException".to_string(), io.to_string(), None),
        Error::Jackcess(m) => ("java.io.IOException".to_string(), m.clone(), None),
        Error::IllegalArgument(m) => (
            "java.lang.IllegalArgumentException".to_string(),
            m.clone(),
            None,
        ),
        Error::IllegalState(m) => (
            "java.lang.IllegalStateException".to_string(),
            m.clone(),
            None,
        ),
        Error::Unsupported(m) => (
            "java.lang.UnsupportedOperationException".to_string(),
            m.clone(),
            None,
        ),
        Error::InvalidCredentials(m) => (
            "com.healthmarketscience.jackcess.crypt.InvalidCredentialsException".to_string(),
            m.clone(),
            None,
        ),
        Error::ConstraintViolation(m) => (
            "com.healthmarketscience.jackcess.ConstraintViolationException".to_string(),
            m.clone(),
            None,
        ),
        Error::InvalidValue(m) => (
            "com.healthmarketscience.jackcess.InvalidValueException".to_string(),
            m.clone(),
            None,
        ),
        // java.nio exceptions have no message
        Error::BufferUnderflow(_) => (
            "java.nio.BufferUnderflowException".to_string(),
            "<null>".to_string(),
            None,
        ),
        Error::BufferOverflow(_) => (
            "java.nio.BufferOverflowException".to_string(),
            "<null>".to_string(),
            None,
        ),
        Error::InvalidMark => (
            "java.nio.InvalidMarkException".to_string(),
            "<null>".to_string(),
            None,
        ),
        Error::NumberFormat(m) => (
            "java.lang.NumberFormatException".to_string(),
            m.clone(),
            None,
        ),
    }
}

/// The port's (now historical) spellings of Java NIO's buffer position
/// exceptions, turned back into Java's exact messages:
///   * `invalid position N (limit M)` -> `newPosition > limit: (N > M)` or,
///     for a negative position, `newPosition < 0: (N < 0)`;
///   * `position > limit: N > M` -> `newPosition > limit: (N > M)`;
///   * `position < 0: N < 0` -> `newPosition < 0: (N < 0)`.
///
/// The exact Java form (`newPosition > limit: (N > M)`) is deliberately left
/// alone (it contains `position > limit: ` but the numbers are parenthesised,
/// so the parse fails and `None` is returned).
fn java_position_message(text: &str) -> Option<String> {
    const INVALID: &str = "invalid position ";
    if let Some(start) = text.find(INVALID) {
        let rest = &text[start + INVALID.len()..];
        let pos: i64 = rest
            .chars()
            .take_while(|c| *c == '-' || c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()?;
        if pos < 0 {
            return Some(format!("newPosition < 0: ({} < 0)", pos));
        }
        const LIMIT: &str = "(limit ";
        let limit_start = rest.find(LIMIT)? + LIMIT.len();
        let limit: i64 = rest[limit_start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()?;
        return Some(format!("newPosition > limit: ({} > {})", pos, limit));
    }
    for (marker, negative) in [("position > limit: ", false), ("position < 0: ", true)] {
        if let Some(start) = text.find(marker) {
            let rest = &text[start + marker.len()..];
            let numbers: Vec<i64> = rest
                .split_whitespace()
                .filter_map(|token| token.parse::<i64>().ok())
                .collect();
            if let Some(pos) = numbers.first().copied() {
                if negative || pos < 0 {
                    return Some(format!("newPosition < 0: ({} < 0)", pos));
                }
                if let Some(limit) = numbers.get(1).copied() {
                    return Some(format!("newPosition > limit: ({} > {})", pos, limit));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn digest(file: &str) -> std::io::Result<String> {
    let bytes = std::fs::read(file)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{}:{}", bytes.len(), hex(&hasher.finalize())))
}

/// Java's `String.CASE_INSENSITIVE_ORDER`, spelled out the same way on both
/// sides: compare the upper cased names, then the exact names.
fn sort_names(mut names: Vec<String>) -> Vec<String> {
    names.sort_by(|a, b| {
        let au = a.to_uppercase();
        let bu = b.to_uppercase();
        au.cmp(&bu).then_with(|| a.cmp(b))
    });
    names
}

/// A nullable string the way both dumps render it (`"..."` or `null`).
fn nullable_string(value: Option<&str>) -> String {
    match value {
        Some(value) => esc(value),
        None => "null".to_string(),
    }
}

fn val(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Text(s) => format!("s:{}", esc(s)),
        Value::Bool(b) => format!("b:{}", b),
        Value::Byte(x) => format!("i8:{}", x),
        Value::Int(x) => format!("i16:{}", x),
        Value::Long(x) => format!("i32:{}", x),
        Value::BigInt(x) => format!("i64:{}", x),
        Value::Decimal { unscaled, scale } => format!("dec:{}/{}", unscaled, scale),
        Value::Float(f) => format!("f32:{:08x}", f.to_bits()),
        Value::Double(d) => format!("f64:{:016x}", d.to_bits()),
        Value::Bytes(b) => format!("bin:{}", hex(b)),
        Value::DateTime(d) => format!("dt:{}", iso(d)),
        Value::DateBits(bits) => format!(
            "dt:{}",
            iso(&jackcess::column::ldt_from_local_date_double(*bits))
        ),
        Value::Uuid(u) => format!("s:{}", esc(&guid_string(u))),
        Value::Keep => "keep".to_string(),
    }
}

fn guid_string(stored: &[u8; 16]) -> String {
    let mut bytes = *stored;
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    let h: Vec<String> = bytes.iter().map(|b| format!("{:02X}", b)).collect();
    format!(
        "{{{}-{}-{}-{}-{}}}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

fn iso(d: &jackcess::datenums::LocalDateTime) -> String {
    let mut s = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        d.year, d.month, d.day, d.hour, d.minute, d.second
    );
    if d.nano != 0 {
        s.push_str(&format!(".{:09}", d.nano));
    }
    s
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for unit in s.encode_utf16() {
        match unit {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            c if !(0x20..=0x7e).contains(&c) => out.push_str(&format!("\\u{:04x}", c)),
            c => out.push(c as u8 as char),
        }
    }
    out.push('"');
    out
}
