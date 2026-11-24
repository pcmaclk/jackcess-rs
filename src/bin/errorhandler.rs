//! `errorhandler` - Rust half of `tools/verify-errorhandler.ps1`, mirroring
//! `java-src/ErrorHandlerGen.java`.
//!
//! Usage: `errorhandler <work-dir>`
//!
//! The Java reference prepares the corrupted fixture copies and prints the
//! canonical error handling trace; this binary reads the very same files and
//! prints the same trace.  The PowerShell script compares the two traces
//! exactly (and checks the error handler API does what Jackcess 5.0.4 does:
//! a handler returns a replacement value for the failed column, the row is
//! never skipped, and table level errors are not handled at all).
//!
//! The `corrupt_nullmask_*` fixtures are the corrupt null mask path: the
//! damaged row declares a null mask of 8192 bytes (`(0xFFFF + 7) / 8`), so
//! Java's `NullMask.read` positions the row buffer at `limit - 8192`, a
//! negative page offset, and throws
//! `IllegalArgumentException("newPosition < 0: (-4096 < 0)")`.  The mask is
//! read lazily per column and never cached after a failure, so every column
//! of the row gets its own `handleRowError` call with a null `columnData`.
//!
//! The `corrupt_shortmask_v2003` fixture declares a 4094 byte mask which still
//! fits on the page (it starts at page offset 2) but reaches in front of the
//! row start.  The mask read succeeds, but `Buffer.position(...)` discards the
//! row buffer mark, so the `rowBuffer.reset()` at the start of
//! `getRowColumnData` throws `java.nio.InvalidMarkException` for each
//! non-null, non-boolean column (the port's `ByteBuf::reset` error, rendered
//! as the NIO class here).
//!
//! Trace grammar:
//!
//! ```text
//! EH-TRACE 1
//! SCENARIO <name>
//! BLOCK <block>
//! CALL table=<t> column=<c> type=<TYPE> loc=<Location> bytes=<hex|null> err=<class> msg=<msg|null>
//! ROW id=<page>:<row> <column>=<value> ...
//! ERROR <class>: <msg|null>
//! END
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jackcess::column::Column;
use jackcess::cursor::{Cursor, IndexCursor, TableScanCursor};
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::error::Error;
use jackcess::table::{ErrorHandler, ErrorLocation, RowId};
use jackcess::value::Value;

const DATA: &str = "external/jackcess-test-resources/data";
const UNSUPPORTED_FIELDS: &str = "V2007/unsupportedFieldsV2007.accdb";
const UNSUPPORTED_SORT_ORDER: &str = "other/unsupportedSortOrder.accdb";
const LINKED_ODBC_V2007: &str = "V2007/linkedOdbcV2007.accdb";

const TEXT_DB: &str = "corrupt_text_v2003.mdb";
const MEMO_DB: &str = "corrupt_memo_v2003.mdb";
const NULLMASK_DB: &str = "corrupt_nullmask_v2003.mdb";
const NULLMASK_FIXED_DB: &str = "corrupt_nullmask_fixed_v2003.mdb";
const NULLMASK_V2007: &str = "corrupt_nullmask_v2007.accdb";
const SHORTMASK_DB: &str = "corrupt_shortmask_v2003.mdb";

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> jackcess::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let out_dir = PathBuf::from(args.get(1).map(String::as_str).unwrap_or("out/eh"));
    let data_dir = PathBuf::from(DATA);

    println!("EH-TRACE 1");

    // ---- unsupported column types: raw data, no handler callbacks
    scenario("unsupported_fields", || {
        block("ITER default");
        with_database(&data_dir.join(UNSUPPORTED_FIELDS), None, |db| {
            dump_table_rows(db, "test")
        });
        block("ITER replacement");
        with_database(
            &data_dir.join(UNSUPPORTED_FIELDS),
            Some(db_handler(Value::Null)),
            |db| dump_table_rows(db, "test"),
        );
    });

    // ---- unsupported text sort order: unicode text, no handler callbacks
    scenario("unsupported_sort_order", || {
        block("ITER default");
        with_database(&data_dir.join(UNSUPPORTED_SORT_ORDER), None, |db| {
            dump_table_rows(db, "t_icelandic")
        });
    });

    // ---- a text row whose variable length offset reaches past the row
    scenario("corrupt_text", || {
        let file = out_dir.join(TEXT_DB);
        block("ITER default");
        with_database(&file, None, |db| dump_table_rows(db, "Table1"));

        block("ITER db_null");
        with_database(&file, Some(db_handler(Value::Null)), |db| {
            dump_table_rows(db, "Table1")
        });

        block("ITER table_replaced");
        with_database(&file, None, |db| {
            let table = db.get_table("Table1").expect("table");
            table.set_error_handler(Some(Arc::new(TraceHandler::new(
                "table",
                Value::Text("REPLACED".to_string()),
                false,
            ))));
            dump_table_rows(db, "Table1");
        });

        block("ITER handler_throws");
        with_database(&file, Some(db_handler_throwing()), |db| {
            dump_table_rows(db, "Table1")
        });

        block("SCAN default");
        with_database(&file, None, |db| dump_scan_rows(db, "Table1", None));
        block("SCAN db_null");
        with_database(&file, Some(db_handler(Value::Null)), |db| {
            dump_scan_rows(db, "Table1", None)
        });
        block("SCAN handler_throws");
        with_database(&file, Some(db_handler_throwing()), |db| {
            dump_scan_rows(db, "Table1", None)
        });
        block("SCAN cursor_replaced");
        with_database(&file, None, |db| {
            dump_scan_rows(
                db,
                "Table1",
                Some(Arc::new(TraceHandler::new(
                    "cursor",
                    Value::Text("CURSOR".to_string()),
                    false,
                ))),
            )
        });

        block("INDEX default");
        with_database(&file, None, |db| dump_index_rows(db, "Table1", None));
        block("INDEX db_null");
        with_database(&file, Some(db_handler(Value::Null)), |db| {
            dump_index_rows(db, "Table1", None)
        });
        block("INDEX cursor_replaced");
        with_database(&file, None, |db| {
            dump_index_rows(
                db,
                "Table1",
                Some(Arc::new(TraceHandler::new(
                    "cursor",
                    Value::Text("CURSOR".to_string()),
                    false,
                ))),
            )
        });

        block("ITER debug_handler");
        with_database(
            &file,
            Some(Arc::new(jackcess::table::DebugErrorHandler::new())),
            |db| dump_table_rows(db, "Table1"),
        );
    });

    // ---- a MEMO row whose long value points at an invalid page
    scenario("corrupt_memo", || {
        let file = out_dir.join(MEMO_DB);
        block("ITER default");
        with_database(&file, None, |db| dump_table_rows(db, "test"));
        block("ITER db_null");
        with_database(&file, Some(db_handler(Value::Null)), |db| {
            dump_table_rows(db, "test")
        });
        block("SCAN db_null");
        with_database(&file, Some(db_handler(Value::Null)), |db| {
            dump_scan_rows(db, "test", None)
        });
        block("SCAN default");
        with_database(&file, None, |db| dump_scan_rows(db, "test", None));
        block("SCAN handler_throws");
        with_database(&file, Some(db_handler_throwing()), |db| {
            dump_scan_rows(db, "test", None)
        });
    });

    // ---- rows whose null mask cannot be read: every column retries the mask
    //      read and gets its own callback with a null columnData
    null_mask_scenarios(
        "corrupt_nullmask_v2003",
        &out_dir.join(NULLMASK_DB),
        "Table1",
        true,
    );
    null_mask_scenarios(
        "corrupt_nullmask_fixed_v2003",
        &out_dir.join(NULLMASK_FIXED_DB),
        "test",
        false,
    );
    null_mask_scenarios(
        "corrupt_nullmask_v2007",
        &out_dir.join(NULLMASK_V2007),
        "Table1",
        true,
    );
    // the declared mask still fits on the page but starts in front of the row,
    // which invalidates the row buffer mark (`java.nio.InvalidMarkException`
    // for every non-null, non-boolean column)
    null_mask_scenarios(
        "corrupt_shortmask_v2003",
        &out_dir.join(SHORTMASK_DB),
        "Table1",
        true,
    );

    // ---- the mutation paths of the corrupt rows.  `update_row` reads the old
    //      values lazily (a failed raw value read is swallowed) and `delete_row`
    //      decodes only the index columns; every block mutates its own copy, and
    //      the copies are compared byte for byte with the Java reference's.
    mutation_scenarios(
        "corrupt_nullmask_v2003",
        &out_dir.join(NULLMASK_DB),
        "Table1",
        (27, 0),
        ".mdb",
        9,
        3,
        Value::Int(5),
        Some((0, Value::Text("A1".to_string()))),
    );
    mutation_scenarios(
        "corrupt_nullmask_fixed_v2003",
        &out_dir.join(NULLMASK_FIXED_DB),
        "test",
        (28, 0),
        ".mdb",
        7,
        1,
        Value::Decimal {
            unscaled: 9,
            scale: 0,
        },
        None,
    );
    mutation_scenarios(
        "corrupt_nullmask_v2007",
        &out_dir.join(NULLMASK_V2007),
        "Table1",
        (64, 0),
        ".accdb",
        9,
        3,
        Value::Int(5),
        Some((0, Value::Text("A1".to_string()))),
    );
    mutation_scenarios(
        "corrupt_shortmask_v2003",
        &out_dir.join(SHORTMASK_DB),
        "Table1",
        (27, 0),
        ".mdb",
        9,
        3,
        Value::Int(5),
        Some((0, Value::Text("A1".to_string()))),
    );
    mutation_scenarios(
        "corrupt_text_v2003",
        &out_dir.join(TEXT_DB),
        "Table1",
        (27, 0),
        ".mdb",
        9,
        3,
        Value::Int(5),
        Some((0, Value::Text("A1".to_string()))),
    );
    mutation_scenarios(
        "corrupt_memo_v2003",
        &out_dir.join(MEMO_DB),
        "TEST",
        (28, 0),
        ".mdb",
        7,
        1,
        Value::Decimal {
            unscaled: 9,
            scale: 0,
        },
        None,
    );

    // ---- a table level error: no table error handler exists, so it surfaces
    scenario("linked_odbc_table_error", || {
        let file = data_dir.join(LINKED_ODBC_V2007);
        block("ITER default");
        with_database(&file, None, |db| dump_table_rows(db, "Ordrar"));
        block("ITER table_replaced");
        with_database(&file, None, |db| {
            let table = db.get_table("Ordrar").expect("table");
            table.set_error_handler(Some(Arc::new(TraceHandler::new(
                "table",
                Value::Text("REPLACED".to_string()),
                false,
            ))));
            dump_table_rows(db, "Ordrar");
        });
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// corrupt row mutations (update / delete)
// ---------------------------------------------------------------------------

/// The mutation blocks of one corrupt row fixture.  Each block works on its own
/// copy of the prepared fixture (`rust/mut_<label>_<tag><ext>`); the Java half
/// writes identically named copies below `java/`, so the verification script
/// compares the two directories file by file (the file *name* is part of the
/// Java error context).
#[allow(clippy::too_many_arguments)]
fn mutation_scenarios(
    label: &str,
    prepared: &Path,
    table_name: &'static str,
    row_id: RowId,
    extension: &str,
    column_count: usize,
    given_column: usize,
    given_value: Value,
    index_given: Option<(usize, Value)>,
) {
    scenario(&format!("{label}_mutations"), || {
        block("UPDATE keep_all_dbh");
        mutate(
            prepared,
            label,
            "upd_keep_all",
            table_name,
            extension,
            true,
            |db| db.update_row(table_name, row_id, &vec![Value::Keep; column_count]),
        );

        block("UPDATE keep_one_dbh");
        mutate(
            prepared,
            label,
            "upd_keep_one",
            table_name,
            extension,
            true,
            |db| {
                let mut row = vec![Value::Keep; column_count];
                row[given_column] = given_value.clone();
                db.update_row(table_name, row_id, &row)
            },
        );

        if let Some((index_column, index_value)) = &index_given {
            block("UPDATE one_index_dbh");
            mutate(
                prepared,
                label,
                "upd_one_index",
                table_name,
                extension,
                true,
                |db| {
                    let mut row = vec![Value::Keep; column_count];
                    row[*index_column] = index_value.clone();
                    db.update_row(table_name, row_id, &row)
                },
            );
        }

        block("UPDATE keep_all_default");
        mutate(
            prepared,
            label,
            "upd_keep_all_default",
            table_name,
            extension,
            false,
            |db| db.update_row(table_name, row_id, &vec![Value::Keep; column_count]),
        );

        block("DELETE dbh");
        mutate(
            prepared,
            label,
            "del_dbh",
            table_name,
            extension,
            true,
            |db| db.delete_row(table_name, row_id),
        );

        block("DELETE default");
        mutate(
            prepared,
            label,
            "del_default",
            table_name,
            extension,
            false,
            |db| db.delete_row(table_name, row_id),
        );
    });
}

/// Copies the prepared fixture and runs one mutation on it, printing the same
/// `OK rows=<n>` / `ERROR ...` lines as the Java half.
#[allow(clippy::too_many_arguments)]
fn mutate<F>(
    prepared: &Path,
    label: &str,
    tag: &str,
    table_name: &str,
    extension: &str,
    handler: bool,
    action: F,
) where
    F: FnOnce(&mut Database) -> jackcess::Result<()>,
{
    let dir = prepared
        .parent()
        .map(|parent| parent.join("rust"))
        .unwrap_or_else(|| PathBuf::from("rust"));
    let dst = dir.join(format!("mut_{label}_{tag}{extension}"));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::remove_file(&dst);
    if let Err(e) = std::fs::copy(prepared, &dst) {
        println!("ERROR java.io.IOException: {e}");
        println!("END");
        return;
    }
    match Database::open_path(&dst) {
        Ok(mut db) => {
            if handler {
                db.set_error_handler(Some(Arc::new(TraceHandler::typed("db"))));
            }
            match action(&mut db) {
                Ok(()) => {
                    let rows = db
                        .get_table(table_name)
                        .map(|table| table.row_count)
                        .unwrap_or(-1);
                    println!("OK rows={rows}");
                }
                Err(error) => println!("ERROR {}", canonical_error(&error)),
            }
        }
        Err(error) => println!("ERROR {}", canonical_error(&error)),
    }
    println!("END");
}

// ---------------------------------------------------------------------------
// trace plumbing
// ---------------------------------------------------------------------------

fn scenario<F: FnOnce()>(name: &str, body: F) {
    println!("SCENARIO {name}");
    body();
}

fn block(name: &str) {
    println!("BLOCK {name}");
}

/// The blocks of a corrupt null mask fixture: the mask read fails for every
/// column of the damaged row, so every column is reported through
/// `handleRowError` (with a null `columnData`) and, with a replacing handler,
/// replaced in an otherwise normally returned row.
fn null_mask_scenarios(name: &str, file: &Path, table_name: &'static str, with_index: bool) {
    scenario(name, || {
        block("ITER default");
        with_database(file, None, |db| dump_table_rows(db, table_name));

        block("ITER db_null");
        with_database(file, Some(db_handler(Value::Null)), |db| {
            dump_table_rows(db, table_name)
        });

        block("ITER table_replaced");
        with_database(file, None, |db| {
            let table = db.get_table(table_name).expect("table");
            table.set_error_handler(Some(Arc::new(TraceHandler::new(
                "table",
                Value::Text("REPLACED".to_string()),
                false,
            ))));
            dump_table_rows(db, table_name);
        });

        block("ITER handler_throws");
        with_database(file, Some(db_handler_throwing()), |db| {
            dump_table_rows(db, table_name)
        });

        block("SCAN default");
        with_database(file, None, |db| dump_scan_rows(db, table_name, None));
        block("SCAN db_null");
        with_database(file, Some(db_handler(Value::Null)), |db| {
            dump_scan_rows(db, table_name, None)
        });
        block("SCAN cursor_replaced");
        with_database(file, None, |db| {
            dump_scan_rows(
                db,
                table_name,
                Some(Arc::new(TraceHandler::new(
                    "cursor",
                    Value::Text("CURSOR".to_string()),
                    false,
                ))),
            )
        });

        if with_index {
            block("INDEX default");
            with_database(file, None, |db| dump_index_rows(db, table_name, None));
            block("INDEX db_null");
            with_database(file, Some(db_handler(Value::Null)), |db| {
                dump_index_rows(db, table_name, None)
            });
            block("INDEX cursor_replaced");
            with_database(file, None, |db| {
                dump_index_rows(
                    db,
                    table_name,
                    Some(Arc::new(TraceHandler::new(
                        "cursor",
                        Value::Text("CURSOR".to_string()),
                        false,
                    ))),
                )
            });
        }

        block("ITER debug_handler");
        with_database(
            file,
            Some(Arc::new(jackcess::table::DebugErrorHandler::new())),
            |db| dump_table_rows(db, table_name),
        );
    });
}

fn db_handler(replacement: Value) -> Arc<dyn ErrorHandler> {
    Arc::new(TraceHandler::new("db", replacement, false))
}

fn db_handler_throwing() -> Arc<dyn ErrorHandler> {
    Arc::new(TraceHandler::new("db", Value::Null, true))
}

/// Opens the database, optionally installs a database level handler, runs the
/// action and closes the database (the Java half opens a fresh `Database` per
/// block as well).
fn with_database<F>(file: &Path, handler: Option<Arc<dyn ErrorHandler>>, action: F)
where
    F: FnOnce(&mut Database),
{
    match Database::open_path(file) {
        Ok(mut db) => {
            if let Some(handler) = handler {
                db.set_error_handler(Some(handler));
            }
            action(&mut db);
        }
        Err(e) => println!("ERROR {}", canonical_error(&e)),
    }
    println!("END");
}

/// What a [`TraceHandler`] returns for a failed row: a fixed value, or one the
/// failed column's writer accepts (the mutation blocks need the latter: a fixed
/// text value in a numeric column is rejected by the writer, and `null` would
/// violate the fixture's required primary key).
enum Replacement {
    Fixed(Value),
    Typed,
}

/// A handler which records the callback in the trace and returns (or throws).
struct TraceHandler {
    block: &'static str,
    replacement: Replacement,
    throwing: bool,
}

impl TraceHandler {
    fn new(block: &'static str, replacement: Value, throwing: bool) -> TraceHandler {
        TraceHandler {
            block,
            replacement: Replacement::Fixed(replacement),
            throwing,
        }
    }

    /// The handler the mutation blocks use (`ErrorHandlerGen.TraceHandler
    /// .typed`): every failed column is replaced by a value its own type can
    /// write.
    fn typed(block: &'static str) -> TraceHandler {
        TraceHandler {
            block,
            replacement: Replacement::Typed,
            throwing: false,
        }
    }
}

/// The typed replacement of `ErrorHandlerGen.TraceHandler.typedValue`.
fn typed_value(column: &Column) -> Value {
    match column.data_type {
        DataType::Text | DataType::Memo => Value::Text("R".to_string()),
        DataType::Boolean => Value::Bool(false),
        DataType::Byte | DataType::Int | DataType::Long | DataType::ComplexType => Value::Long(0),
        DataType::BigInt => Value::BigInt(0),
        DataType::Float => Value::Float(0.0),
        DataType::Double => Value::Double(0.0),
        DataType::Money | DataType::Numeric => Value::Decimal {
            unscaled: 0,
            scale: 0,
        },
        DataType::ShortDateTime | DataType::ExtDateTime => Value::DateTime(
            jackcess::datenums::LocalDateTime::new(2020, 1, 1, 0, 0, 0, 0),
        ),
        DataType::Binary | DataType::Ole => Value::Bytes(Vec::new()),
        DataType::Guid => Value::Uuid([0u8; 16]),
        _ => Value::Null,
    }
}

impl ErrorHandler for TraceHandler {
    fn handle_row_error(
        &self,
        column: &Column,
        column_data: Option<&[u8]>,
        location: &dyn ErrorLocation,
        error: Error,
    ) -> jackcess::Result<Value> {
        println!(
            "CALL table={} column={} type={} loc={} bytes={} err={} handler={}",
            location.get_table().name,
            column.name,
            column.data_type.get_name(),
            location,
            match column_data {
                Some(data) => hex(data),
                None => "null".to_string(),
            },
            canonical_error(&error),
            self.block
        );
        if self.throwing {
            return Err(Error::Jackcess("handler says no".to_string()));
        }
        Ok(match &self.replacement {
            Replacement::Fixed(value) => value.clone(),
            Replacement::Typed => typed_value(column),
        })
    }
}

// ---------------------------------------------------------------------------
// row dumps
// ---------------------------------------------------------------------------

fn dump_table_rows(db: &mut Database, table_name: &str) {
    // `Table.iterator` walks the default cursor, whose `BaseIterator` wraps an
    // IOException in an UncheckedIOException; a RuntimeException (like a
    // BufferUnderflowException) is propagated unchanged.
    let columns = match db.get_table(table_name) {
        Ok(table) => table.columns.clone(),
        Err(e) => {
            println!("ERROR {}", canonical_error(&e));
            return;
        }
    };
    match db.read_rows_with_ids(table_name) {
        Ok(rows) => {
            for (row_id, row) in rows {
                if let Some(values) = row {
                    println!("{}", format_row(&columns, row_id, &values));
                }
            }
        }
        Err(e) => println!("ERROR {}", iteration_error(&e)),
    }
}

fn dump_scan_rows(db: &mut Database, table_name: &str, handler: Option<Arc<dyn ErrorHandler>>) {
    let columns = match db.get_table(table_name) {
        Ok(table) => table.columns.clone(),
        Err(e) => {
            println!("ERROR {}", canonical_error(&e));
            return;
        }
    };
    let result = (|| -> jackcess::Result<()> {
        let mut cursor = TableScanCursor::new(db, table_name)?;
        cursor.set_error_handler(handler);
        while let Some(values) = cursor.get_next_row(db)? {
            let row_id = cursor.get_current_row_id();
            println!("{}", format_row(&columns, row_id, &values));
        }
        Ok(())
    })();
    if let Err(e) = result {
        println!("ERROR {}", canonical_error(&e));
    }
}

fn dump_index_rows(db: &mut Database, table_name: &str, handler: Option<Arc<dyn ErrorHandler>>) {
    let columns = match db.get_table(table_name) {
        Ok(table) => table.columns.clone(),
        Err(e) => {
            println!("ERROR {}", canonical_error(&e));
            return;
        }
    };
    let result = (|| -> jackcess::Result<()> {
        let mut cursor = IndexCursor::new(db, table_name, "PrimaryKey")?;
        cursor.set_error_handler(handler);
        while let Some(values) = cursor.get_next_row(db)? {
            let row_id = cursor.get_current_row_id();
            println!("{}", format_row(&columns, row_id, &values));
        }
        Ok(())
    })();
    if let Err(e) = result {
        println!("ERROR {}", canonical_error(&e));
    }
}

fn format_row(columns: &[Column], row_id: RowId, values: &[Value]) -> String {
    let mut out = format!("ROW id={}:{}", row_id.0, row_id.1);
    for (i, column) in columns.iter().enumerate() {
        out.push(' ');
        out.push_str(&column.name);
        out.push('=');
        out.push_str(&format_value(column, values.get(i).unwrap_or(&Value::Null)));
    }
    out
}

// ---------------------------------------------------------------------------
// canonical value formatting (mirrors ErrorHandlerGen.formatValue)
// ---------------------------------------------------------------------------

fn format_value(column: &Column, value: &Value) -> String {
    // `ComplexValueForeignKeyImpl` is a `Number`; the port keeps the foreign
    // key id itself in a `Value::Long`, so the canonical form is `complex(id)`
    if column.data_type == DataType::ComplexType {
        if let Value::Long(id) = value {
            return format!("complex({id})");
        }
    }
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("bool({b})"),
        Value::Byte(b) => format!("byte({})", *b as i8),
        Value::Int(v) => format!("int({v})"),
        Value::Long(v) => format!("long({v})"),
        Value::BigInt(v) => format!("bigint({v})"),
        Value::Decimal { unscaled, scale } => format!("decimal({unscaled},{scale})"),
        Value::Float(v) => format!("float({:08x})", v.to_bits()),
        Value::Double(v) => format!("double({:016x})", v.to_bits()),
        Value::Text(text) => format!("text({})", escape(text)),
        Value::Bytes(data) => {
            // Java's UnsupportedColumnImpl returns a RawData wrapper for an
            // unknown column type; the trace prints its raw bytes
            if column.data_type.is_unsupported() {
                format!("raw({})", hex(data))
            } else {
                format!("bytes({})", hex(data))
            }
        }
        Value::DateTime(date) => format!(
            "date({:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09})",
            date.year, date.month, date.day, date.hour, date.minute, date.second, date.nano
        ),
        Value::DateBits(bits) => {
            let date = jackcess::column::ldt_from_local_date_double(*bits);
            format!(
                "date({:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09})",
                date.year, date.month, date.day, date.hour, date.minute, date.second, date.nano
            )
        }
        Value::Uuid(uuid) => format!("uuid({})", hex(uuid)),
        Value::Keep => "keep".to_string(),
    }
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// Same escaping as the Java half: printable ASCII, C escapes, else \u{XXXX}.
fn escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{20}'..='\u{7e}' => out.push(c),
            other => out.push_str(&format!("\\u{{{:04x}}}", other as u32)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// canonical error formatting (mirrors the Java exception classes)
// ---------------------------------------------------------------------------

fn canonical_error(error: &Error) -> String {
    let (class, message) = match error {
        Error::Io(e) => ("java.io.IOException", e.to_string()),
        Error::Jackcess(message) => ("java.io.IOException", message.clone()),
        Error::IllegalArgument(message) => ("java.lang.IllegalArgumentException", message.clone()),
        // `ByteBuf::reset` models `java.nio.Buffer.reset`, which throws
        // `java.nio.InvalidMarkException` (no message) when the mark is unset;
        // the port has a dedicated error kind for it, so no text based
        // canonicalisation is needed
        Error::InvalidMark => ("java.nio.InvalidMarkException", "null".to_string()),
        Error::NumberFormat(message) => ("java.lang.NumberFormatException", message.clone()),
        Error::IllegalState(message) => ("java.lang.IllegalStateException", message.clone()),
        Error::Unsupported(message) => ("java.lang.UnsupportedOperationException", message.clone()),
        Error::InvalidCredentials(message) => (
            "com.healthmarketscience.jackcess.crypt.InvalidCredentialsException",
            message.clone(),
        ),
        Error::ConstraintViolation(message) => (
            "com.healthmarketscience.jackcess.ConstraintViolationException",
            message.clone(),
        ),
        Error::InvalidValue(message) => (
            "com.healthmarketscience.jackcess.InvalidValueException",
            message.clone(),
        ),
        Error::BatchUpdate { message, .. } => (
            "com.healthmarketscience.jackcess.BatchUpdateException",
            message.clone(),
        ),
        // java.nio exceptions have no message
        Error::BufferUnderflow(_) => ("java.nio.BufferUnderflowException", "null".to_string()),
        Error::BufferOverflow(_) => ("java.nio.BufferOverflowException", "null".to_string()),
    };
    format!("{class}: {message}")
}

/// `CursorImpl.BaseIterator` wraps an `IOException` in an
/// `UncheckedIOException` (whose message is the cause's `toString`), while a
/// `RuntimeException` escapes unchanged.
fn iteration_error(error: &Error) -> String {
    match error {
        Error::Jackcess(message) => {
            format!("java.io.UncheckedIOException: java.io.IOException: {message}")
        }
        Error::Io(e) => format!("java.io.UncheckedIOException: java.io.IOException: {e}"),
        _ => canonical_error(error),
    }
}
