//! `namemap` - dumps the name maps of a database in the canonical form which
//! `java-src/NameMapGen.java dump` prints, so the two can be compared byte for
//! byte.
//!
//! Usage:  namemap <file.mdb|file.accdb>

use jackcess::database::Database;
use jackcess::error::Error;
use jackcess::namemap::{Mismatch, Record};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: namemap <file.mdb>");
        std::process::exit(2);
    }
    let mut db = match Database::open_path(std::path::Path::new(&args[1])) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    // `get_table_names` keeps the catalog spelling and sorts like Java; the
    // name map dump uses the table's own name, like `Table.getName()`.
    let mut names: Vec<String> = match db.get_table_names() {
        Ok(names) => names
            .into_iter()
            .filter(|name| name.starts_with("NM"))
            .collect(),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    names.sort();

    let mut out = String::new();
    for name in names {
        emit(&mut out, &format!("== {name} =="));

        let blob = match db.get_table(&name) {
            Ok(table) => table.get_name_map_blob().map(|b| b.to_vec()),
            Err(e) => {
                emit(&mut out, "table=error");
                emit(&mut out, &format!("error={}", exc(&e)));
                continue;
            }
        };
        emit(
            &mut out,
            &format!(
                "blob={}",
                match &blob {
                    Some(b) => hex(b),
                    None => "-".to_string(),
                }
            ),
        );

        let map = match db.get_table(&name) {
            Ok(table) => table.get_name_map(),
            Err(e) => {
                emit(&mut out, "map=error");
                emit(&mut out, &format!("error={}", exc(&e)));
                continue;
            }
        };

        let map = match map {
            Ok(Some(map)) => map,
            Ok(None) => {
                emit(&mut out, "map=absent");
                continue;
            }
            Err(e) => {
                emit(&mut out, "map=error");
                emit(&mut out, &format!("error={}", exc(&e)));
                continue;
            }
        };

        emit(&mut out, "map=present");
        let records = map.records();
        emit(&mut out, &format!("records={}", records.len()));
        for (i, rec) in records.iter().enumerate() {
            emit(&mut out, &format!("r{i} {}", fmt_record(rec)));
        }
        emit(
            &mut out,
            &format!("tableRecords={}", map.get_table_records().len()),
        );
        emit(
            &mut out,
            &format!("columnRecords={}", map.get_column_records().len()),
        );
        emit(
            &mut out,
            &format!("references={}", map.get_references().len()),
        );
        for (i, rec) in map.get_references().iter().enumerate() {
            emit(&mut out, &format!("ref{i} {}", fmt_record(rec)));
        }
        emit(
            &mut out,
            &format!(
                "tableRecord={}",
                map.get_table_record()
                    .map(fmt_record)
                    .unwrap_or_else(|| "-".to_string())
            ),
        );
        emit(
            &mut out,
            &format!(
                "writerTag={}",
                map.get_writer_tag()
                    .map(|tag| tag.to_string())
                    .unwrap_or_else(|| "-".to_string())
            ),
        );

        let column_names: Vec<String> = match db.get_table(&name) {
            Ok(table) => table.columns.iter().map(|c| c.name.clone()).collect(),
            Err(_) => Vec::new(),
        };
        for column_name in &column_names {
            emit(
                &mut out,
                &format!(
                    "columnRecord[{}]={}",
                    escape(column_name),
                    fmt_or_dash(map.get_column_record(column_name))
                ),
            );
            emit(
                &mut out,
                &format!(
                    "columnRecordUpper[{}]={}",
                    escape(column_name),
                    fmt_or_dash(map.get_column_record(&column_name.to_uppercase()))
                ),
            );
        }

        let mut guids: Vec<String> = Vec::new();
        for rec in records {
            if let Some(guid) = rec.get_guid() {
                if !guids.contains(&guid) {
                    guids.push(guid);
                }
            }
        }
        guids.push("{00000000-0000-0000-0000-000000000001}".to_string());
        for guid in guids {
            emit(
                &mut out,
                &format!("record[{guid}]={}", fmt_or_dash(map.get_record(&guid))),
            );
            let lower = guid.to_lowercase();
            if lower != guid {
                emit(
                    &mut out,
                    &format!(
                        "recordLower[{guid}]={}",
                        fmt_or_dash(map.get_record(&lower))
                    ),
                );
            }
        }

        let mismatches = {
            let props = match db.get_table_properties_by_name(&name) {
                Ok(props) => props,
                Err(e) => {
                    emit(&mut out, "mismatches=error");
                    emit(&mut out, &format!("mismatchError={}", exc(&e)));
                    continue;
                }
            };
            match db.get_table(&name) {
                Ok(table) => match map.find_mismatches(table, &props) {
                    Ok(mismatches) => mismatches,
                    Err(e) => {
                        emit(&mut out, "mismatches=error");
                        emit(&mut out, &format!("mismatchError={}", exc(&e)));
                        continue;
                    }
                },
                Err(e) => {
                    emit(&mut out, "mismatches=error");
                    emit(&mut out, &format!("mismatchError={}", exc(&e)));
                    continue;
                }
            }
        };
        emit(&mut out, &format!("mismatches={}", mismatches.len()));
        for (i, mismatch) in mismatches.iter().enumerate() {
            emit(&mut out, &format!("m{i} {}", fmt_mismatch(mismatch)));
        }
    }

    print!("{out}");
}

fn emit(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

fn fmt_record(rec: &Record) -> String {
    format!(
        "type={} guid={} parent={} name={} date={}",
        rec.get_type().name(),
        rec.get_guid().unwrap_or_else(|| "-".to_string()),
        rec.get_parent_guid().unwrap_or_else(|| "-".to_string()),
        escape(rec.get_name()),
        rec.get_date()
            .map(|date| java_local_date_time_text(&date))
            .unwrap_or_else(|| "-".to_string())
    )
}

/// `LocalDateTime.toString()`: the seconds are omitted when they and the
/// fraction are zero; a non zero fraction is printed with 3, 6 or 9 digits
/// (the first group of nine which does not end in zero).
fn java_local_date_time_text(value: &jackcess::datenums::LocalDateTime) -> String {
    let mut out = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}",
        value.year, value.month, value.day, value.hour, value.minute
    );
    if value.second != 0 || value.nano != 0 {
        out.push_str(&format!(":{:02}", value.second));
        if value.nano != 0 {
            out.push('.');
            if value.nano.is_multiple_of(1_000_000) {
                out.push_str(&format!("{:03}", value.nano / 1_000_000));
            } else if value.nano.is_multiple_of(1_000) {
                out.push_str(&format!("{:06}", value.nano / 1_000));
            } else {
                out.push_str(&format!("{:09}", value.nano));
            }
        }
    }
    out
}

fn fmt_or_dash(rec: Option<&Record>) -> String {
    rec.map(fmt_record).unwrap_or_else(|| "-".to_string())
}

fn fmt_mismatch(mismatch: &Mismatch) -> String {
    format!(
        "type={} table={} query={} column={} record={}",
        mismatch.get_type().name(),
        mismatch
            .get_table()
            .map(escape)
            .unwrap_or_else(|| "-".to_string()),
        mismatch
            .get_query()
            .map(escape)
            .unwrap_or_else(|| "-".to_string()),
        mismatch
            .get_column()
            .map(escape)
            .unwrap_or_else(|| "-".to_string()),
        fmt_or_dash(mismatch.get_record())
    )
}

fn exc(e: &Error) -> String {
    match e {
        Error::Jackcess(message) => format!("IOException {}", escape(message)),
        other => format!("{} {}", variant_name(other), escape(&other.to_string())),
    }
}

fn variant_name(e: &Error) -> &'static str {
    match e {
        Error::Io(_) => "IOException",
        Error::Jackcess(_) => "IOException",
        Error::IllegalArgument(_) => "IllegalArgumentException",
        Error::IllegalState(_) => "IllegalStateException",
        Error::Unsupported(_) => "UnsupportedOperationException",
        _ => "RuntimeException",
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// The same escaping as `NameMapGen.esc`: printable ASCII stays, every other
/// UTF-16 code unit becomes `\uXXXX`.
fn escape(value: &str) -> String {
    let mut out = String::new();
    for unit in value.encode_utf16() {
        if (0x20..=0x7E).contains(&unit) && unit != '\\' as u16 {
            out.push(char::from_u32(unit as u32).unwrap());
        } else {
            out.push_str(&format!("\\u{unit:04X}"));
        }
    }
    out
}
