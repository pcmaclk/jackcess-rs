//! `mdbdump` - dumps database structure/rows for debugging.

use jackcess::database::Database;
use jackcess::value::Value;

fn fmt_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("bool({})", b),
        Value::Byte(x) => format!("byte({})", x),
        Value::Int(x) => format!("int({})", x),
        Value::Long(x) => format!("long({})", x),
        Value::BigInt(x) => format!("bigint({})", x),
        Value::Float(x) => format!("float({:?})", x),
        Value::Double(x) => format!("double({:?})", x),
        Value::Decimal { unscaled, scale } => format!("decimal({},{})", unscaled, scale),
        Value::Text(s) => format!("text({:?})", s),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{:02x}", x)).collect();
            format!("bytes({})", hex)
        }
        Value::DateTime(d) => format!(
            "date({:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09})",
            d.year, d.month, d.day, d.hour, d.minute, d.second, d.nano
        ),
        Value::DateBits(bits) => format!("datebits({:016x})", bits.to_bits()),
        Value::Uuid(u) => format!("uuid({:02x?})", u),
        Value::Keep => "keep".to_string(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: mdbdump <file.mdb> [table]");
        std::process::exit(2);
    }
    let mut db = match Database::open_path(std::path::Path::new(&args[1])) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    if args.len() > 2 && args[2] == "--relationships" {
        print_relationships(&mut db);
        return;
    }
    if args.len() > 2 && args[2] == "--dbprops" {
        print_props(&mut db, "db");
        return;
    }
    if args.len() > 3 && args[2] == "--props" {
        print_props(&mut db, &args[3].clone());
        return;
    }
    let names = db.table_names();
    println!("tables: {}", names.join(", "));
    let reencode_mode = args.iter().any(|a| a == "--reencode");
    let selected: Vec<String> = if args.len() > 2 && !args[2].starts_with("--") {
        vec![args[2].clone()]
    } else {
        names.clone()
    };
    for name in selected {
        let (tdef_page, header, columns, indexes) = {
            let table = match db.find_table(&name) {
                Ok(Some(t)) => t,
                _ => {
                    println!("missing table {}", name);
                    continue;
                }
            };
            (
                table.table_def_page_number,
                format!(
                    "== {} (tdef page {}) rows {} cols {}",
                    table.name,
                    table.table_def_page_number,
                    table.row_count,
                    table.columns.len()
                ),
                table
                    .columns
                    .iter()
                    .map(|c| {
                        format!(
                            "   col#{} {} type={:?} var={} len={} fixedOff={} varIdx={} umap={}",
                            c.column_number,
                            c.name,
                            c.data_type.get_name(),
                            c.is_variable_length(),
                            c.get_length(),
                            c.fixed_data_offset,
                            c.var_len_table_index,
                            c.umap_page_number
                        )
                    })
                    .collect::<Vec<_>>(),
                table
                    .indexes
                    .iter()
                    .map(|i| {
                        let fk = match &i.reference {
                            Some(r) => format!(
                                " fk(tableType={} otherIdx={} otherPage={} upd={} del={} null={})",
                                r.table_type,
                                r.other_index_number,
                                r.other_table_page_number,
                                r.cascade_updates,
                                r.cascade_deletes,
                                r.cascade_null
                            ),
                            None => String::new(),
                        };
                        format!(
                            "   index {} name={} type={} data={}{}",
                            i.index_number, i.name, i.index_type, i.index_data_number, fk
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        };
        println!("{}", header);
        for line in columns {
            println!("{}", line);
        }
        for line in indexes {
            println!("{}", line);
        }
        if reencode_mode {
            let m = reencode(&mut db, tdef_page);
            println!("   reencode mismatches: {}", m);
            continue;
        }
        match db.read_table_rows(tdef_page) {
            Ok(rows) => {
                for (i, row) in rows.iter().enumerate() {
                    match row {
                        None => println!("   row{}: <deleted>", i),
                        Some(values) => {
                            let parts: Vec<String> = values.iter().map(fmt_value).collect();
                            println!("   row{}: {}", i, parts.join(" | "));
                        }
                    }
                }
            }
            Err(e) => println!("   rows error: {}", e),
        }
    }
}

/// Reads every row of the table and writes it again, reporting the rows whose
/// bytes differ.  A relocated row is compared at its final location (the header
/// row only holds the overflow pointer), the buffer is seeded with the stored
/// row bytes (a null fixed length value is not written, so the bytes a recycled
/// page buffer kept behind it are part of the row), and a long value which
/// lives on other pages keeps its on-page definition instead of taking new
/// pages.  Calculated columns are unwrapped by the read and wrapped again by
/// the write, so their values are checked too.
fn reencode(db: &mut Database, tdef_page: i32) -> usize {
    let format = db.format.clone();
    let _charset = db.charset;
    let tz = db.tz;
    let page_size = format.page_size;
    let mut page_nums: Vec<i32> = {
        let t = db.tables.get(&tdef_page).unwrap();
        t.owned_pages
            .as_ref()
            .map(|p| p.iter_pages().collect())
            .unwrap_or_default()
    };
    page_nums.sort_unstable();
    db.channel.start_write();
    let mut mismatches = 0;
    let mut shared_buf = None;
    for page_num in page_nums {
        let mut page = db.channel.create_page_buffer();
        if db.channel.read_page(&mut page, page_num).is_err() {
            continue;
        }
        if page.get_u8_at(0).unwrap_or(0) != 1 {
            continue;
        }
        let rows = page.get_u16_at(12).unwrap_or(0) as usize;
        for row_num in 0..rows {
            // locate the row data, following overflow pointers (a relocated row
            // keeps only a pointer in its header row)
            let row_id = (page_num, row_num as i32);
            let loc = {
                let t = db.tables.get(&tdef_page).unwrap();
                match t.position_at_row_data(&mut db.channel, row_id) {
                    Ok(loc) => loc,
                    Err(_) => continue,
                }
            };
            if loc.deleted {
                continue;
            }
            let raw = loc.final_page.backing()[loc.row_start..loc.row_end].to_vec();
            // a long value which lives on other pages is not written again (that
            // would allocate new pages and point the row at them); its on-page
            // definition is re-used.  An inline long value is re-encoded like
            // any other value.
            let raw_values: std::collections::HashMap<usize, Vec<u8>> = {
                let row_buffer = match loc.final_page.narrow(loc.row_start, loc.row_end) {
                    Ok(buffer) => buffer,
                    Err(_) => continue,
                };
                let row_start = row_buffer.position();
                let column_count = row_buffer.get_u16_at(row_start).unwrap_or(0) as usize;
                let mask_size = column_count.div_ceil(8);
                let null_mask = match jackcess::table::NullMask::read(
                    &row_buffer,
                    row_buffer.limit() - mask_size,
                    column_count,
                ) {
                    Ok(mask) => mask,
                    Err(_) => continue,
                };
                let mut raw_values = std::collections::HashMap::new();
                let t = db.tables.get(&tdef_page).unwrap();
                for (ci, col) in t.columns.iter().enumerate() {
                    if !col.data_type.is_long_value() {
                        continue;
                    }
                    if let Ok(Some(def)) =
                        t.get_raw_row_value(&db.channel, &row_buffer, col, &null_mask, true)
                    {
                        let inline = def.len() >= 4
                            && (u32::from_le_bytes([def[0], def[1], def[2], def[3]]) >> 24)
                                == ((jackcess::column::LONG_VALUE_TYPE_THIS_PAGE as u32) << 24);
                        if !inline {
                            raw_values.insert(ci, def);
                        }
                    }
                }
                raw_values
            };
            let values = {
                let t = db.tables.get(&tdef_page).unwrap();
                match t.read_row_at(&mut db.channel, row_id) {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "   reencode page {} row {}: read error {}",
                            page_num, row_num, e
                        );
                        mismatches += 1;
                        continue;
                    }
                }
            };
            let mut buf = shared_buf
                .take()
                .unwrap_or_else(|| jackcess::bytebuf::ByteBuf::allocate(page_size));
            let _ = buf.set_limit(page_size);
            let _ = buf.set_position(0);
            // a recycled page buffer keeps the bytes of the previous contents in
            // the parts the row does not write (a null fixed length value is not
            // written at all), so seed the buffer with the stored row bytes and
            // keep the row's size
            let _ = buf.put_bytes(&raw);
            let _ = buf.set_position(0);
            let encoded = {
                let t = db.tables.get_mut(&tdef_page).unwrap();
                match t.create_row_with_raw(
                    &mut db.channel,
                    &values,
                    &mut buf,
                    raw.len(),
                    &raw_values,
                    &tz,
                ) {
                    Ok(()) => buf.remaining_bytes().to_vec(),
                    Err(e) => {
                        println!(
                            "   reencode page {} row {}: write error {}",
                            page_num, row_num, e
                        );
                        mismatches += 1;
                        continue;
                    }
                }
            };
            shared_buf = Some(buf);
            if encoded != raw {
                mismatches += 1;
                let first = encoded
                    .iter()
                    .zip(raw.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or_else(|| encoded.len().min(raw.len()));
                println!(
                    "   reencode page {} row {}: {} vs {} bytes, first diff at {} (id {:?})",
                    page_num,
                    row_num,
                    raw.len(),
                    encoded.len(),
                    first,
                    values.first()
                );
                let lo = first.saturating_sub(8);
                let hi_raw = (first + 16).min(raw.len());
                let hi_enc = (first + 16).min(encoded.len());
                println!(
                    "     raw: {}",
                    raw[lo..hi_raw]
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                println!(
                    "     enc: {}",
                    encoded[lo..hi_enc]
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
    }
    let _ = db.channel.finish_write();
    mismatches
}

fn print_relationships(db: &mut Database) {
    let names = db.table_names();
    match db.relationships_all() {
        Ok(rels) => {
            println!("relationships: {}", rels.len());
            for rel in &rels {
                println!(
                    "   {} {}[{}] -> {}[{}] flags={:#x} integrity={} oneToOne={} upd={} del={} null={}",
                    rel.name,
                    rel.primary_table,
                    rel.primary_columns.join(","),
                    rel.secondary_table,
                    rel.secondary_columns.join(","),
                    rel.flags,
                    rel.has_referential_integrity(),
                    rel.is_one_to_one(),
                    rel.cascade_updates(),
                    rel.cascade_deletes(),
                    rel.cascade_null_on_delete()
                );
            }
        }
        Err(e) => println!("relationships error: {}", e),
    }
    for name in names {
        if name.starts_with("MSYS") {
            continue;
        }
        match db.relationships(&name) {
            Ok(rels) => {
                for rel in &rels {
                    println!(
                        "   [{}] {} -> {}",
                        name, rel.primary_table, rel.secondary_table
                    );
                }
            }
            Err(e) => println!("   [{}] relationships error: {}", name, e),
        }
    }
}

fn print_props(db: &mut Database, which: &str) {
    let maps = if which == "db" {
        db.get_database_properties()
    } else {
        let page: i32 = which.parse().unwrap_or(0);
        db.get_table_properties(page)
    };
    let maps = match maps {
        Ok(m) => m,
        Err(e) => {
            println!("props error: {}", e);
            return;
        }
    };
    for map in maps.iter() {
        // a loaded table has an (empty) property map for every column; only
        // the maps which hold properties are interesting in a dump
        if map.is_empty() {
            continue;
        }
        if !map.name().is_empty() {
            println!("map {}", map.name());
        }
        let mut lines: Vec<String> = map
            .iter()
            .map(|p| format!("{}={}", p.name(), fmt_prop_value(p.value())))
            .collect();
        lines.sort();
        for l in lines {
            println!("{}", l);
        }
    }
}

fn fmt_prop_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("bool({})", b),
        Value::Byte(x) => format!("num({})", x),
        Value::Int(x) => format!("num({})", x),
        Value::Long(x) => format!("num({})", x),
        Value::BigInt(x) => format!("num({})", x),
        Value::Float(x) => format!("num({})", x),
        Value::Double(x) => format!("num({})", x),
        Value::Decimal { unscaled, scale } => {
            format!("num({})", *unscaled as f64 / 10f64.powi(*scale))
        }
        Value::Text(s) => format!("text({})", s),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{:02x}", x)).collect();
            format!("bytes({})", hex)
        }
        Value::DateTime(d) => format!(
            "text({:04}-{:02}-{:02}T{:02}:{:02}:{:02})",
            d.year, d.month, d.day, d.hour, d.minute, d.second
        ),
        Value::DateBits(bits) => format!("num({})", bits),
        Value::Uuid(u) => {
            let hex: String = u.iter().map(|x| format!("{:02x}", x)).collect();
            format!("bytes({})", hex)
        }
        Value::Keep => "keep".to_string(),
    }
}
