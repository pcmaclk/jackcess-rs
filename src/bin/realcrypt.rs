//! `realcrypt` - verification tool for the **real** Access / jackcess-encrypt
//! test databases (`external/jackcessencrypt-test`).
//!
//! It is the Rust side of `java-src/RealCryptGen.java`; every mode mirrors the
//! Java mode of the same name so `tools/verify-realcrypt.ps1` can compare the
//! two.
//!
//! Modes:
//!   dump <db> <password|-> <out.txt>
//!        canonical text dump of every table (definition, columns, indexes,
//!        index entries in cursor order, rows), byte for byte the same layout
//!        as `Jet3DumpGen`'s dump
//!   checkpw <db> <password|->
//!        open through the crypt provider and report the stored page 0
//!        password field; a wrong Office password and a jet database password
//!        which does not match both exit 3
//!   info <db>
//!        page 0 metadata (format, encoding key, password field,
//!        EncryptionInfo version)
//!   decode <src> <dst>      remove the jet page encoding (jet 3 included)
//!   encode <src> <dst> <keyHex>   add the jet page encoding
//!   pwset <src> <dst> <password>  set the jet database password
//!   pwremove <src> <dst>          remove the jet database password
//!   office-decrypt <src> <dst> <password>
//!        remove the Office encryption
//!
//! Usage: `realcrypt <mode> ...`

use jackcess::crypt::office::CRYPT_STRUCTURE_OFFSET;
use jackcess::crypt::{
    apply_header_mask, decode_file, encode_file_with_key, recode_jet3, CryptCodecProvider,
    PasswordUtil,
};
use jackcess::cursor::{Cursor, IndexCursor};
use jackcess::database::Database;
use jackcess::datenums::LocalDateTime;
use jackcess::error::{Error, Result};
use jackcess::table::Table;
use jackcess::value::Value;
use std::io::{Read, Seek};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let mode = args[1].as_str();
    let result = match mode {
        "dump" => {
            if args.len() != 5 {
                usage();
                std::process::exit(2);
            }
            let password = password_arg(&args[3]);
            dump_to_file(Path::new(&args[2]), password, Path::new(&args[4]))
        }
        "checkpw" => {
            if args.len() != 4 {
                usage();
                std::process::exit(2);
            }
            check_password(Path::new(&args[2]), password_arg(&args[3]))
        }
        "info" => {
            if args.len() != 3 {
                usage();
                std::process::exit(2);
            }
            info(Path::new(&args[2]))
        }
        "sids" => {
            if args.len() != 3 {
                usage();
                std::process::exit(2);
            }
            print_sids(Path::new(&args[2]))
        }
        "pwfield" => {
            if args.len() != 3 {
                usage();
                std::process::exit(2);
            }
            print_pwfield(Path::new(&args[2]))
        }
        "plain-open" => {
            if args.len() != 3 {
                usage();
                std::process::exit(2);
            }
            // the default codec provider: a file which needs a codec is
            // refused, like Java's `DefaultCodecProvider`
            Database::open_path_read_only(Path::new(&args[2])).and_then(|mut db| {
                db.get_table_names()
                    .map(|names| println!("OPENED tables={}", names.len()))
            })
        }
        "decode" => {
            if args.len() != 4 {
                usage();
                std::process::exit(2);
            }
            recode(
                Path::new(&args[2]),
                Path::new(&args[3]),
                false,
                0,
                "DECODED",
            )
        }
        "encode" => {
            if args.len() != 5 {
                usage();
                std::process::exit(2);
            }
            let key = match i64::from_str_radix(&args[4], 16) {
                Ok(k) => k as i32,
                Err(e) => {
                    eprintln!("invalid key {}: {}", args[4], e);
                    std::process::exit(2);
                }
            };
            recode(
                Path::new(&args[2]),
                Path::new(&args[3]),
                true,
                key,
                "ENCODED",
            )
        }
        "pwset" => {
            if args.len() != 5 {
                usage();
                std::process::exit(2);
            }
            PasswordUtil::set_database_password(Path::new(&args[2]), Path::new(&args[3]), &args[4])
                .map(|()| println!("PASSWORD-SET {}", args[3]))
        }
        "pwremove" => {
            if args.len() != 4 {
                usage();
                std::process::exit(2);
            }
            PasswordUtil::remove_database_password(Path::new(&args[2]), Path::new(&args[3]))
                .map(|()| println!("PASSWORD-REMOVED {}", args[3]))
        }
        "office-decrypt" => {
            if args.len() != 5 {
                usage();
                std::process::exit(2);
            }
            jackcess::crypt::remove_office_encryption(
                Path::new(&args[2]),
                Path::new(&args[3]),
                &args[4],
            )
            .map(|()| println!("DECRYPTED {}", args[3]))
        }
        _ => {
            usage();
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        report_error(&e);
        std::process::exit(3);
    }
}

fn usage() {
    eprintln!("usage: realcrypt dump <db> <password|-> <out.txt>");
    eprintln!("       realcrypt checkpw <db> <password|->");
    eprintln!("       realcrypt info <db>");
    eprintln!("       realcrypt sids <db>");
    eprintln!("       realcrypt pwfield <db>");
    eprintln!("       realcrypt plain-open <db>");
    eprintln!("       realcrypt decode <src> <dst>");
    eprintln!("       realcrypt encode <src> <dst> <keyHex>");
    eprintln!("       realcrypt pwset <src> <dst> <password>");
    eprintln!("       realcrypt pwremove <src> <dst>");
    eprintln!("       realcrypt office-decrypt <src> <dst> <password>");
}

fn password_arg(arg: &str) -> Option<&str> {
    if arg == "-" {
        None
    } else {
        Some(arg)
    }
}

/// Reports an error the way the Java reference prints an exception: the class
/// name the Rust error corresponds to, then the message.
fn report_error(e: &Error) {
    match e {
        Error::InvalidCredentials(m) => println!("InvalidCredentialsException: {}", m),
        Error::IllegalState(m) => println!("IllegalStateException: {}", m),
        Error::IllegalArgument(m) => println!("IllegalArgumentException: {}", m),
        Error::Unsupported(m) => println!("UnsupportedOperationException: {}", m),
        _ => println!("IOException: {}", e),
    }
}

fn open(db_file: &Path, password: Option<&str>) -> Result<Database> {
    let provider = CryptCodecProvider::from_password(password);
    Database::open_path_read_only_with_provider(db_file, &provider)
}

// ---------------------------------------------------------------------------
// dump
// ---------------------------------------------------------------------------

fn dump_to_file(db_file: &Path, password: Option<&str>, out_file: &Path) -> Result<()> {
    let mut db = open(db_file, password)?;
    let mut out = String::with_capacity(1 << 20);
    dump(&mut db, &mut out)?;
    std::fs::write(out_file, out.as_bytes())?;
    println!("wrote {} bytes={}", out_file.display(), out.len());
    Ok(())
}

fn dump(db: &mut Database, out: &mut String) -> Result<()> {
    out.push_str("version 1\n");
    out.push_str(&format!("format {}\n", db.format.name));
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

fn push_joined_names(out: &mut String, names: &[String]) {
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&esc(name));
    }
    out.push('\n');
}

fn dump_table(db: &mut Database, name: &str, out: &mut String) -> Result<()> {
    let (page, flags) = match db.table_lookup.get(&name.to_uppercase()) {
        Some(v) => *v,
        None => return Err(Error::Jackcess(format!("Table {} does not exist", name))),
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
        let mut error: Option<Error> = None;
        match IndexCursor::new(db, name, idx_name) {
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
            // an overflow row holds only a pointer; the data lives in another
            // row (`RowState.getFinalRowId`)
            if (loc.final_page_number, loc.final_row_number) != row_id {
                out.push_str(&format!(
                    "relocated {}/{} ",
                    loc.final_page_number, loc.final_row_number
                ));
            }
            match table.read_row_at(&mut db.channel, row_id) {
                Ok(values) => {
                    for (col, v) in table.columns.iter().zip(values.iter()) {
                        out.push_str(&val_for(col, v));
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

fn dump_column(table: &Table, col: &jackcess::column::Column, out: &mut String) {
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

fn val_for(col: &jackcess::column::Column, v: &Value) -> String {
    // Java's `ColumnImpl.read` returns a `ComplexValueForeignKey` object for a
    // complex type column; the dump prints it the way `toString` does
    // (`String.valueOf(get())`), tagged with the class name.
    if col.data_type == jackcess::datatype::DataType::ComplexType {
        match v {
            Value::Long(n) => return format!("other:ComplexValueForeignKeyImpl:{}", n),
            Value::Int(n) => return format!("other:ComplexValueForeignKeyImpl:{}", n),
            Value::BigInt(n) => return format!("other:ComplexValueForeignKeyImpl:{}", n),
            _ => {}
        }
    }
    val(v)
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
        // Java's `ColumnImpl.read` returns a GUID as the Access GUID string
        // (`{8E39276A-...}`); this port keeps the value as `Value::Uuid` in
        // *stored* byte order, so render it the way Java does (swap the three
        // integer components, brace it, upper case) to compare the same value
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
/// backslash-u escape of the UTF-16 code unit.
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

// ---------------------------------------------------------------------------
// checkpw / info
// ---------------------------------------------------------------------------

fn esc_password(pwd: Option<&str>) -> String {
    match pwd {
        None => "null".to_string(),
        Some(pwd) => pwd
            .encode_utf16()
            .map(|u| format!("\\u{:04x}", u))
            .collect(),
    }
}

fn check_password(db_file: &Path, password: Option<&str>) -> Result<()> {
    let mut db = open(db_file, password)?;
    let stored = db.get_database_password()?;
    println!("format {}", db.format.name);
    println!("stored {}", esc_password(stored.as_deref()));
    let jet = db.format.name == "VERSION_3" || db.format.name == "VERSION_4";
    if jet && password.is_some() && stored.as_deref() != password {
        println!(
            "PASSWORD-REJECTED stored={}",
            esc_password(stored.as_deref())
        );
        std::process::exit(3);
    }
    println!("PASSWORD-ACCEPTED");
    Ok(())
}

/// Every masked SID in the database as hex, sorted, like
/// `PasswordUtilTest.readSids`.
fn print_sids(db_file: &Path) -> Result<()> {
    let mut db = Database::open_path_read_only(db_file)?;
    let mut sids: Vec<String> = Vec::new();
    for (table_name, col_name) in [("MSysACEs", "SID"), ("MSysObjects", "Owner")] {
        let Some((page, flags)) = db.table_lookup.get(&table_name.to_uppercase()).copied() else {
            continue;
        };
        db.load_table(page, table_name, flags)?;
        let ci = {
            let table = db.tables.get(&page).unwrap();
            table
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col_name))
        };
        let Some(ci) = ci else {
            continue;
        };
        let name = db.tables.get(&page).unwrap().name.clone();
        for (_id, values) in db.read_rows_with_ids(&name)? {
            let Some(values) = values else {
                continue;
            };
            if let Some(Value::Bytes(sid)) = values.get(ci) {
                if !sid.is_empty() {
                    sids.push(format!("{}:{}", table_name, hex(sid)));
                }
            }
        }
    }
    sids.sort();
    for sid in sids {
        println!("{}", sid);
    }
    Ok(())
}

/// The raw (still masked) page 0 password field as hex.
fn print_pwfield(db_file: &Path) -> Result<()> {
    let mut file = std::fs::File::open(db_file)?;
    let format = jackcess::crypt::rewriter::read_file_format(&mut file)?;
    let mut page = vec![0u8; format.page_size];
    jackcess::crypt::read_page(&mut file, &mut page, 0, format.page_size)?;
    let start = format.offset_password;
    let end = start + format.size_password;
    println!("{}", hex(&page[start..end]));
    Ok(())
}

fn info(db_file: &Path) -> Result<()> {
    let mut file = std::fs::File::open(db_file)?;
    let format = jackcess::crypt::rewriter::read_file_format(&mut file)?;
    let page_size = format.page_size;
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut page = vec![0u8; page_size];
    file.read_exact(&mut page)?;
    let mut raw = jackcess::bytebuf::ByteBuf::wrap_le(page.clone());
    let raw_key = raw.get_i32_at(format.offset_encoding_key)?;
    apply_header_mask(&mut raw, &format)?;
    let key = raw.get_i32_at(format.offset_encoding_key)?;
    let info_len = raw.get_u16_at(CRYPT_STRUCTURE_OFFSET)?;
    // Java only reads the version words when the length says the structure
    // holds them, otherwise it prints -1
    let v_major: i32 = if info_len >= 2 {
        raw.get_u16_at(CRYPT_STRUCTURE_OFFSET + 2)? as i32
    } else {
        -1
    };
    let v_minor: i32 = if info_len >= 4 {
        raw.get_u16_at(CRYPT_STRUCTURE_OFFSET + 4)? as i32
    } else {
        -1
    };
    println!("format {}", format.name);
    println!("encodingKeyRaw {:08x}", raw_key);
    println!("encodingKey {:08x}", key);
    println!("cryptInfoLen {}", info_len);
    println!("cryptVersion {}.{}", v_major, v_minor);
    println!("size {}", std::fs::metadata(db_file)?.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// jet encoding recode
// ---------------------------------------------------------------------------

fn recode(src: &Path, dst: &Path, add: bool, key: i32, verb: &str) -> Result<()> {
    let mut file = std::fs::File::open(src)?;
    let format = jackcess::crypt::rewriter::read_file_format(&mut file)?;
    match format.name {
        "VERSION_3" => recode_jet3(src, dst, add, key)?,
        "VERSION_4" => {
            if add {
                encode_file_with_key(src, dst, key)?;
            } else {
                decode_file(src, dst)?;
            }
        }
        _ => {
            return Err(Error::Unsupported(format!(
                "Jet encoding is only supported for jet 4 databases, not {}",
                format.name
            )))
        }
    }
    println!("{} {}", verb, dst.display());
    Ok(())
}
