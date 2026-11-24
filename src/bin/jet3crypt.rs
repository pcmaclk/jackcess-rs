//! `jet3crypt` - the Rust counterpart of `java-src/Jet3CryptGen.java`: jet 3
//! (Access 97) page encoding and database password, for the verification
//! harness `tools/verify-jet3-crypt.ps1`.
//!
//! Neither jackcess-encrypt nor this port writes jet 3 files through its
//! public API, so the harness has to produce the encoded and password
//! protected Access 97 files itself.  This tool provides the two writes the
//! public API refuses (through the explicitly test-side
//! [`jackcess::crypt::recode_jet3`] and
//! [`jackcess::crypt::set_jet3_password_file`]) and the canonical dump used to
//! compare the reads with the Java reference.
//!
//! Modes:
//!   encode  <src> <dst> <keyHex>       jet 3 encode (mirrors JetEncodingHandler.recode)
//!   decode  <src> <dst>                jet 3 decode
//!   pw-set  <src> <dst> <password>     write the page 0 password field directly
//!   pw-remove <src> <dst>              zero the page 0 password field
//!   pw-get  <db> [password|-]          print the stored database password
//!   checkpw <db> <password>            open with the password, report the stored one (exit 3 on mismatch)
//!   dump    <db> [password|-]          canonical dump, opened through CryptCodecProvider
//!   key     <db>                       print the page 0 encoding key
//!   fixtures                           print the page transform fixture values
//!
//! The dump is the same canonical text `src/bin/jet3dump.rs` (and
//! `java-src/Jet3DumpGen.java`) produce, so the encoded file's dump can also be
//! compared with the plain file's dump: opening the encoded file through the
//! codec provider has to read exactly what opening the plain file reads.

use jackcess::bytebuf::ByteBuf;
use jackcess::crypt::{apply_header_mask, CryptCodecProvider};
use jackcess::cursor::Cursor;
use jackcess::database::Database;
use jackcess::datenums::LocalDateTime;
use jackcess::error::{Error, Result};
use jackcess::table::Table;
use jackcess::value::Value;
use std::path::{Path, PathBuf};

const DEFAULT_KEY: i32 = 0x1234_abcd;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let result = match args[1].as_str() {
        "encode" => {
            let (src, dst) = two_paths(&args, 4);
            let key = match parse_key(&args[4]) {
                Some(k) => k,
                None => {
                    eprintln!("error: invalid key {}", args[4]);
                    std::process::exit(2);
                }
            };
            jackcess::crypt::recode_jet3(&src, &dst, true, key)
        }
        "decode" => {
            let (src, dst) = two_paths(&args, 4);
            jackcess::crypt::recode_jet3(&src, &dst, false, 0)
        }
        "pw-set" | "pwset" => {
            let (src, dst) = two_paths(&args, 5);
            jackcess::crypt::set_jet3_password_file(&src, &dst, Some(&args[4]))
        }
        "pw-remove" | "pwremove" => {
            let (src, dst) = two_paths(&args, 4);
            jackcess::crypt::set_jet3_password_file(&src, &dst, None)
        }
        "pw-get" | "pwget" => {
            let db = PathBuf::from(&args[2]);
            let password = args.get(3).map(|s| s.as_str()).filter(|s| *s != "-");
            get_password(&db, password)
        }
        "checkpw" => {
            if args.len() < 4 {
                usage();
                std::process::exit(2);
            }
            let db = PathBuf::from(&args[2]);
            let password = args[3].as_str();
            check_password(&db, password)
        }
        "dump" => {
            let db = PathBuf::from(&args[2]);
            let password = args.get(3).map(|s| s.as_str()).filter(|s| *s != "-");
            dump_file(&db, password)
        }
        "key" => {
            let db = PathBuf::from(&args[2]);
            print_key(&db)
        }
        "fixtures" => {
            print_fixtures();
            Ok(())
        }
        // the public API must refuse jet 3; these modes print the refusal the
        // same way `Jet3CryptGen` prints the Java exception
        "refuse-encode" => {
            let (src, dst) = two_paths(&args, 4);
            let key = parse_key(&args[4]).unwrap_or(DEFAULT_KEY);
            match jackcess::crypt::JetEncodingHandler::add_encoding_with_key(&src, &dst, key) {
                Ok(()) => {
                    println!("ENCODED {}", dst.display());
                    Ok(())
                }
                Err(e) => {
                    println!("{}", e);
                    std::process::exit(3);
                }
            }
        }
        "refuse-decode" => {
            let (src, dst) = two_paths(&args, 4);
            match jackcess::crypt::JetEncodingHandler::remove_encoding(&src, &dst) {
                Ok(()) => {
                    println!("DECODED {}", dst.display());
                    Ok(())
                }
                Err(e) => {
                    println!("{}", e);
                    std::process::exit(3);
                }
            }
        }
        "refuse-pwset" => {
            let (src, dst) = two_paths(&args, 5);
            match jackcess::crypt::PasswordUtil::set_database_password(&src, &dst, &args[4]) {
                Ok(()) => {
                    println!("PASSWORD-SET {}", dst.display());
                    Ok(())
                }
                Err(e) => {
                    println!("{}", e);
                    std::process::exit(3);
                }
            }
        }
        _ => {
            usage();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn two_paths(args: &[String], needed: usize) -> (PathBuf, PathBuf) {
    if args.len() < needed {
        usage();
        std::process::exit(2);
    }
    (PathBuf::from(&args[2]), PathBuf::from(&args[3]))
}

fn parse_key(value: &str) -> Option<i32> {
    i64::from_str_radix(value.trim_start_matches("0x"), 16)
        .ok()
        .map(|v| v as i32)
}

fn usage() {
    eprintln!("usage: jet3crypt encode <src> <dst> <keyHex>");
    eprintln!("       jet3crypt decode <src> <dst>");
    eprintln!("       jet3crypt pw-set <src> <dst> <password>");
    eprintln!("       jet3crypt pw-remove <src> <dst>");
    eprintln!("       jet3crypt pw-get <db> [password|-]");
    eprintln!("       jet3crypt checkpw <db> <password>");
    eprintln!("       jet3crypt dump <db> [password|-]");
    eprintln!("       jet3crypt key <db>");
    eprintln!("       jet3crypt fixtures");
    eprintln!("       jet3crypt refuse-encode <src> <dst> <keyHex>");
    eprintln!("       jet3crypt refuse-decode <src> <dst>");
    eprintln!("       jet3crypt refuse-pwset <src> <dst> <password>");
}

// ---------------------------------------------------------------------------
// opening
// ---------------------------------------------------------------------------

/// Opens a database read-only through the crypt codec provider, the same way
/// the Java reference does (`DatabaseBuilder.setReadOnly(true)
/// .setCodecProvider(new CryptCodecProvider(password))`).  The jet page
/// encoding needs no password; the argument is accepted for parity with the
/// Java tool (and because the Java API asks for one to select the provider).
fn open_with_provider(db_file: &Path, password: Option<&str>) -> Result<Database> {
    let provider = CryptCodecProvider::from_password(password);
    Database::open_path_read_only_with_provider(db_file, &provider)
}

fn get_password(db_file: &Path, password: Option<&str>) -> Result<()> {
    let mut db = open_with_provider(db_file, password)?;
    match db.get_database_password()? {
        Some(p) => println!("password={}", p),
        None => println!("password=null"),
    }
    Ok(())
}

fn check_password(db_file: &Path, password: &str) -> Result<()> {
    let mut db = open_with_provider(db_file, Some(password))?;
    let stored = db.get_database_password()?.unwrap_or_else(|| "null".into());
    println!("opened-with-wrong-password password={}", stored);
    if stored == password {
        println!("PASSWORD-ACCEPTED");
        Ok(())
    } else {
        println!("PASSWORD-REJECTED stored={}", stored);
        std::process::exit(3);
    }
}

/// The page 0 encoding key, after the header mask is removed (the position
/// `JetCryptCodecHandler.create` reads it from).
fn print_key(db_file: &Path) -> Result<()> {
    let mut file = std::fs::File::open(db_file)?;
    let format = jackcess::crypt::rewriter::read_file_format(&mut file)?;
    let mut page = vec![0u8; format.page_size];
    jackcess::crypt::read_page(&mut file, &mut page, 0, format.page_size)?;
    let mut header = ByteBuf::wrap_le(page);
    apply_header_mask(&mut header, &format)?;
    let key = header.get_i32_at(format.offset_encoding_key)?;
    println!(
        "key={:08x} encoded={}",
        key as u32,
        if key == 0 { "false" } else { "true" }
    );
    Ok(())
}

/// The page transform values `java-src/Jet3CryptGen.java` prints for the same
/// synthetic page (see the unit tests in `src/crypt/encoding.rs`).
fn print_fixtures() {
    for page_number in [1i32, 1000] {
        let mut page: Vec<u8> = (0..2048usize)
            .map(|i| i.wrapping_mul(31).wrapping_add(5) as u8)
            .collect();
        jackcess::crypt::encoding::crypt_page(&mut page, DEFAULT_KEY, page_number);
        println!(
            "jet3 page={} key={:08x} size=2048 head={} tail={}",
            page_number,
            DEFAULT_KEY as u32,
            hex(&page[..16]),
            hex(&page[2032..])
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ---------------------------------------------------------------------------
// dump
// ---------------------------------------------------------------------------

fn dump_file(db_file: &Path, password: Option<&str>) -> Result<()> {
    let mut db = open_with_provider(db_file, password)?;
    let mut out = String::with_capacity(1 << 20);
    match dump(&mut db, &mut out) {
        Ok(()) => {
            print!("{}", out);
            Ok(())
        }
        Err(e) => Err(e),
    }
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

/// Java's `String.CASE_INSENSITIVE_ORDER`, spelled out the same way as
/// `jet3dump` and `Jet3DumpGen`: compare the upper cased names, then the exact
/// names.
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

    for idx_name in &index_names {
        {
            let table = db.tables.get(&page).unwrap();
            if let Some(idx) = table.indexes.iter().find(|i| &i.name == idx_name) {
                dump_index_definition(table, idx, out);
            }
        }
        let mut ids: Vec<(i32, i32)> = Vec::new();
        let mut error: Option<Error> = None;
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
// values (exactly `jet3dump`'s rendering, which `Jet3DumpGen` matches)
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
