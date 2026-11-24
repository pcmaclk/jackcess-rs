//! `msisam` - the Rust counterpart of `java-src/MsisamGen.java`: reads a
//! Microsoft Money (`.mny`, MSISAM) database, plain or password protected, and
//! prints the canonical text `tools/verify-msisam.ps1` compares against the
//! Java reference.
//!
//! Modes (the password `-` means "no password"):
//!
//! ```text
//!   msisam info       <file.mny> [password|-]
//!   msisam dump       <file.mny> [password|-]
//!   msisam checkpw    <file.mny> <password|->
//!   msisam default    <file.mny>
//!   msisam create     <path.mny>
//!   msisam writecheck <file.mny> [password|-]
//! ```
//!
//! `dump` mirrors `Jet3DumpGen`'s dump (see `src/bin/jet3dump.rs`): table
//! definition header, columns, index definitions and entries, owned pages and
//! every row, for every user and system table.  Java reads the table
//! definition header fields at the raw format offsets while this dumps the
//! port's parsed values, so a wrong offset in the port shows up in the
//! comparison.

use jackcess::column::Column;
use jackcess::cursor::{Cursor, TableScanCursor};
use jackcess::database::Database;
use jackcess::datenums::LocalDateTime;
use jackcess::format::FileFormat;
use jackcess::table::Table;
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: msisam <mode> <file.mny> [password|-]");
        std::process::exit(2);
    }
    let mode = args[1].as_str();
    let path = std::path::Path::new(&args[2]);
    let password = args.get(3).map(|p| p.as_str());
    let password = match password {
        Some("-") | None => None,
        Some(p) => Some(p),
    };

    let mut out = String::with_capacity(1 << 20);
    let result = match mode {
        "info" => open(path, password).and_then(|mut db| info(&mut db, &mut out)),
        "dump" => open(path, password).and_then(|mut db| dump(&mut db, &mut out)),
        "checkpw" => checkpw(path, password, &mut out),
        "default" => default_open(path, &mut out),
        "create" => create(path, &mut out),
        "writecheck" => writecheck(path, password, &mut out),
        other => {
            eprintln!("unknown mode {}", other);
            std::process::exit(2);
        }
    };
    match result {
        Ok(()) => print!("{}", out),
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}

/// `CryptCodecProvider` + `DatabaseBuilder.open` (read-only).
fn open(path: &std::path::Path, password: Option<&str>) -> jackcess::Result<Database> {
    Database::open_path_with_password(path, password)
}

/// The `Database.FileFormat.name()` of a database opened from a file.  Only
/// the MSISAM entry is exercised by this tool; the jet 4 formats need the
/// `AccessVersion` property to tell V2000 from V2003, which jackcess reads
/// through the property maps.
fn file_format_name(db: &Database) -> &'static str {
    match db.format.name {
        "MSISAM" => "MSISAM",
        "VERSION_3" => "V1997",
        "VERSION_4" => "GENERIC_JET4",
        "VERSION_2007" => "V2007",
        "VERSION_2010" => "V2010",
        "VERSION_2016" => "V2016",
        "VERSION_2019" => "V2019",
        _ => "UNKNOWN",
    }
}

/// The simple name of the Java exception class an error corresponds to.
fn error_class(e: &jackcess::Error) -> &'static str {
    match e {
        jackcess::Error::InvalidCredentials(_) => "InvalidCredentialsException",
        jackcess::Error::Unsupported(_) => "UnsupportedOperationException",
        jackcess::Error::IllegalState(_) => "IllegalStateException",
        jackcess::Error::IllegalArgument(_) => "IllegalArgumentException",
        jackcess::Error::ConstraintViolation(_) => "ConstraintViolationException",
        jackcess::Error::InvalidValue(_) => "InvalidValueException",
        jackcess::Error::BatchUpdate { .. } => "BatchUpdateException",
        jackcess::Error::BufferUnderflow(_) => "BufferUnderflowException",
        jackcess::Error::BufferOverflow(_) => "BufferOverflowException",
        jackcess::Error::InvalidMark => "InvalidMarkException",
        jackcess::Error::NumberFormat(_) => "NumberFormatException",
        jackcess::Error::Io(_) | jackcess::Error::Jackcess(_) => "IOException",
    }
}

// ---------------------------------------------------------------------------
// modes
// ---------------------------------------------------------------------------

fn info(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    out.push_str(&format!("format {}\n", db.format.name));
    out.push_str(&format!("fileformat {}\n", file_format_name(db)));
    out.push_str(&format!("readonly {}\n", db.is_read_only()));
    out.push_str(&format!("charset {}\n", db.charset.name()));
    let user_tables = sort_names(db.get_table_names()?);
    let system_tables = sort_names(db.get_system_table_names()?);
    out.push_str(&format!("usertables {} ", user_tables.len()));
    push_joined_names(out, &user_tables);
    out.push_str(&format!("systemtables {} ", system_tables.len()));
    push_joined_names(out, &system_tables);
    Ok(())
}

fn dump(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    out.push_str("version 1\n");
    out.push_str(&format!("format {}\n", db.format.name));
    out.push_str(&format!("fileformat {}\n", file_format_name(db)));
    out.push_str(&format!("readonly {}\n", db.is_read_only()));
    out.push_str(&format!("charset {}\n", db.charset.name()));

    let user_tables = sort_names(db.get_table_names()?);
    let system_tables = sort_names(db.get_system_table_names()?);
    out.push_str(&format!("usertables {} ", user_tables.len()));
    push_joined_names(out, &user_tables);
    out.push_str(&format!("systemtables {} ", system_tables.len()));
    push_joined_names(out, &system_tables);

    for name in user_tables.iter().chain(system_tables.iter()) {
        dump_table(db, name, out)?;
    }
    Ok(())
}

fn checkpw(
    path: &std::path::Path,
    password: Option<&str>,
    out: &mut String,
) -> jackcess::Result<()> {
    match open(path, password) {
        Ok(mut db) => {
            let format_name = db.format.name;
            let file_format = file_format_name(&db);
            let read_only = db.is_read_only();
            let table_count = db.get_table_names()?.len();
            out.push_str(&format!(
                "ok {} {} readonly={} tables={}\n",
                format_name, file_format, read_only, table_count
            ));
            // read the first row so the page encoding is really exercised
            let mut cursor = TableScanCursor::new(&mut db, "CRNC")?;
            match cursor.get_next_row(&mut db)? {
                Some(values) => {
                    out.push_str("row ");
                    for v in &values {
                        out.push_str(&val(v));
                        out.push(' ');
                    }
                    out.push('\n');
                }
                None => out.push_str("row none\n"),
            }
            Ok(())
        }
        Err(e) => {
            out.push_str(&format!(
                "error {} {}\n",
                error_class(&e),
                error_message(&e)
            ));
            Ok(())
        }
    }
}

/// The Java `getMessage()` of the corresponding error: the jackcess message
/// without the Rust error variant decoration.
fn error_message(e: &jackcess::Error) -> String {
    match e {
        jackcess::Error::Io(io) => io.to_string(),
        other => other.to_string(),
    }
}

fn default_open(path: &std::path::Path, out: &mut String) -> jackcess::Result<()> {
    match Database::open_path_with_provider(path, &jackcess::crypt::DefaultCodecProvider::INSTANCE)
    {
        Ok(db) => {
            out.push_str(&format!("ok {}\n", db.format.name));
            Ok(())
        }
        Err(e) => {
            // the class differs between the two implementations for this
            // refusal; the message is the `UnsupportedHandler` one in both.
            out.push_str(&format!("error {}\n", error_message(&e)));
            Ok(())
        }
    }
}

fn create(path: &std::path::Path, out: &mut String) -> jackcess::Result<()> {
    match Database::create(FileFormat::Msisam, path) {
        Ok(mut db) => {
            let _ = db.flush();
            out.push_str("ok\n");
        }
        Err(e) => {
            out.push_str(&format!(
                "error {} {}\n",
                error_class(&e),
                error_message(&e)
            ));
        }
    }
    Ok(())
}

fn writecheck(
    path: &std::path::Path,
    password: Option<&str>,
    out: &mut String,
) -> jackcess::Result<()> {
    use jackcess::column::ColumnSpec;
    use jackcess::datatype::DataType;
    use jackcess::tablecreator::TableSpec;

    let mut db = match open(path, password) {
        Ok(db) => db,
        Err(e) => {
            out.push_str(&format!(
                "open error {} {}\n",
                error_class(&e),
                error_message(&e)
            ));
            return Ok(());
        }
    };
    let spec = TableSpec::new(
        "MsisamWriteProbe",
        vec![ColumnSpec::new("ID", DataType::Long)],
    );
    match db.create_table(&spec) {
        Ok(_) => out.push_str("write ok\n"),
        Err(e) => {
            // the page channel wraps a failed write in its operation context;
            // the read-only channel's `NonWritableChannelException` text is
            // still in the message
            if e.to_string().contains("NonWritableChannelException") {
                out.push_str("write blocked\n");
            } else {
                out.push_str(&format!("write error {}\n", error_class(&e)));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// dump
// ---------------------------------------------------------------------------

/// Java's `String.CASE_INSENSITIVE_ORDER`, spelled out the same way on both
/// sides: compare the lower cased names (Java's comparator lowercases before
/// comparing, so `ADV_SUM` sorts before `Advisor`), then the exact names.
fn sort_names(mut names: Vec<String>) -> Vec<String> {
    names.sort_by(|a, b| {
        let al = a.to_lowercase();
        let bl = b.to_lowercase();
        al.cmp(&bl).then_with(|| a.cmp(b))
    });
    names
}

fn push_joined_names(out: &mut String, names: &[String]) {
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&esc(name));
    }
    out.push('\n');
}

fn dump_table(db: &mut Database, name: &str, out: &mut String) -> jackcess::Result<()> {
    let (page, flags) = match db.table_lookup.get(&name.to_uppercase()) {
        Some(v) => *v,
        None => {
            return Err(jackcess::Error::Jackcess(format!(
                "Table {} does not exist",
                name
            )))
        }
    };
    db.load_table(page, name, flags)?;

    let (index_names, pages): (Vec<String>, Vec<i32>) = {
        let table = db.tables.get(&page).unwrap();
        out.push_str(&format!(
            "table {} tdef {} type {} rows {} nextAuto {} maxCols {} varCols {} cols {} logicalIndexes {} indexes {}\n",
            esc(&table.name),
            table.table_def_page_number,
            table.table_type,
            table.row_count,
            table.last_long_auto_number,
            table.max_column_count,
            table.max_var_column_count,
            table.columns.len(),
            table.logical_index_count,
            table.index_count,
        ));
        for col in &table.columns {
            dump_column(table, col, out);
        }
        let mut pages: Vec<i32> = table
            .owned_pages
            .as_ref()
            .map(|p| p.iter_pages().collect())
            .unwrap_or_default();
        pages.sort_unstable();
        (
            table.indexes.iter().map(|i| i.name.clone()).collect(),
            pages,
        )
    };

    // index definitions and their entries, in index cursor order (the Java
    // side dumps the two together per index, so the lines interleave)
    for idx_name in &index_names {
        {
            let table = db.tables.get(&page).unwrap();
            if let Some(idx) = table.indexes.iter().find(|i| &i.name == idx_name) {
                dump_index_definition(table, idx, out);
            }
        }
        let mut ids: Vec<(i32, i32)> = Vec::new();
        let mut error: Option<jackcess::Error> = None;
        match jackcess::cursor::IndexCursor::new(db, name, idx_name) {
            Ok(mut cursor) => loop {
                match cursor.get_next_row(db) {
                    Ok(Some(_)) => {
                        let id = cursor.get_current_row_id();
                        ids.push(id);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            },
            Err(e) => error = Some(e),
        }
        if let Some(e) = error {
            eprintln!("index {} entries error: {}", idx_name, e);
            out.push_str(&format!("indexentries {} readerror\n", esc(idx_name)));
            continue;
        }
        out.push_str(&format!("indexentries {} {} ", esc(idx_name), ids.len()));
        for (i, (p, r)) in ids.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("{}/{}", p, r));
        }
        out.push('\n');
    }

    out.push_str(&format!("ownedpages {} ", pages.len()));
    for (i, page_num) in pages.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&page_num.to_string());
    }
    out.push('\n');

    // rows
    let table = db.tables.get(&page).unwrap();
    for page_num in pages {
        let mut page_buf = db.channel.create_page_buffer();
        db.channel.read_page(&mut page_buf, page_num)?;
        if page_buf.get_u8_at(0)? != jackcess::page_types::DATA {
            continue;
        }
        let format = db.format.clone();
        if page_buf.get_i32_at(4)? != table.table_def_page_number {
            continue;
        }
        let rows_on_page = page_buf.get_u16_at(format.offset_num_rows_on_data_page)? as usize;
        for row_num in 0..rows_on_page {
            let row_id = (page_num, row_num as i32);
            let raw_start =
                page_buf.get_i16_at(jackcess::table::get_row_start_offset(row_num, &format))?;
            out.push_str(&format!("row {}/{} ", page_num, row_num));
            if jackcess::table::is_deleted_row(raw_start) {
                out.push_str("deleted\n");
                continue;
            }
            let loc = table.position_at_row_data(&mut db.channel, row_id);
            let loc = match loc {
                Ok(loc) => loc,
                Err(e) => {
                    eprintln!("row {}/{} read error: {}", page_num, row_num, e);
                    out.push_str("readerror\n");
                    continue;
                }
            };
            if loc.deleted {
                out.push_str("deleted\n");
                continue;
            }
            if (loc.final_page_number, loc.final_row_number) != row_id {
                out.push_str(&format!(
                    "relocated {}/{} ",
                    loc.final_page_number, loc.final_row_number
                ));
            }
            match table.read_row_at(&mut db.channel, row_id) {
                Ok(values) => {
                    for v in &values {
                        out.push_str(&val(v));
                        out.push(' ');
                    }
                    out.push('\n');
                }
                Err(e) => {
                    eprintln!("row {}/{} read error: {}", page_num, row_num, e);
                    out.push_str("readerror\n");
                }
            }
        }
    }
    Ok(())
}

fn dump_column(table: &Table, col: &Column, out: &mut String) {
    let sql_type = match table.column_sql_type(col) {
        Some(t) => t.to_string(),
        None => "-".to_string(),
    };
    let sort = match &col.sort_order {
        None => "-".to_string(),
        Some(s) => format!("{}/{}/{}", s.value, s.variant, s.version),
    };
    out.push_str(&format!(
        "column {} {} type {} sqlType {} len {} units {} prec {} scale {} id {} var {} auto {} hidden {} calc {} append {} security {} fixedOff {} varIdx {} codePage {} sortOrder {}\n",
        col.column_number,
        esc(&col.name),
        col.data_type.get_name(),
        sql_type,
        col.get_length(),
        table.column_length_in_units(col),
        col.get_precision(),
        col.get_scale(),
        col.column_id,
        col.is_variable_length(),
        col.auto_number,
        col.hidden,
        col.calculated,
        col.append_only,
        col.security_identifier,
        col.fixed_data_offset,
        col.var_len_table_index,
        col.code_page,
        sort,
    ));
}

fn dump_index_definition(table: &Table, idx: &jackcess::index::IndexImpl, out: &mut String) {
    let data = table.index_data_for(idx);
    let mut cols = String::new();
    if let Some(data) = data {
        for (i, col) in data.columns.iter().enumerate() {
            if i > 0 {
                cols.push(',');
            }
            cols.push_str(&esc(&col.column.name));
            cols.push(':');
            cols.push(if col.is_ascending() { 'A' } else { 'D' });
        }
    }
    out.push_str(&format!(
        "index {} {} data {} primary {} foreign {} unique {} required {} ignoreNulls {} uniqEntries {} flags {} columns {}\n",
        idx.index_number,
        esc(&idx.name),
        data.map(|d| d.number).unwrap_or(-1),
        idx.is_primary_key(),
        idx.is_foreign_key(),
        data.map(|d| d.is_unique()).unwrap_or(false),
        data.map(|d| d.is_required()).unwrap_or(false),
        data.map(|d| d.should_ignore_nulls()).unwrap_or(false),
        data.map(|d| d.unique_entry_count).unwrap_or(0),
        data.map(|d| d.index_flags).unwrap_or(0),
        cols,
    ));
}

// ---------------------------------------------------------------------------
// values
// ---------------------------------------------------------------------------

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

/// `ColumnImpl.toGUIDString` from the stored (little endian component) bytes.
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

fn iso(d: &LocalDateTime) -> String {
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

/// The shared text escaping: everything outside printable ASCII becomes a
/// backslash-u escape of the UTF-16 code unit (exactly like the Java side,
/// which iterates `String.charAt`).
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
