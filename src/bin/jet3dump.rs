//! `jet3dump` - canonical dump of a Jet 3 (Access 97) database, used to
//! compare the port's reading with the Java reference (`Jet3DumpGen`).
//!
//! The output is pure ASCII (text is escaped) and uses `\n` line endings, so
//! `java-src/Jet3DumpGen.java` has to produce the exact same text.  See that
//! file for what the dump covers and why the table definition header fields
//! are read one way in each implementation (Java reads the raw header bytes at
//! its format offsets, this dumps the port's parsed values, so a wrong offset
//! cannot hide).
//!
//! Usage: `jet3dump <file.mdb>` (stdout is the dump, stderr the diagnostics).

use jackcess::cursor::Cursor;
use jackcess::database::Database;
use jackcess::datenums::LocalDateTime;
use jackcess::format::FileFormat;
use jackcess::property::PropertyMap;
use jackcess::table::Table;
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: jet3dump <file.mdb>");
        std::process::exit(2);
    }
    let mut db = match Database::open_path(std::path::Path::new(&args[1])) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    let mut out = String::with_capacity(1 << 20);
    match dump(&mut db, &mut out) {
        Ok(()) => print!("{}", out),
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}

fn dump(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    let jet3 = db.format.name == "VERSION_3";
    out.push_str("version 1\n");
    out.push_str(&format!("format {}\n", db.format.name));
    out.push_str(&format!("charset {}\n", db.charset.name()));
    if jet3 {
        dump_database_info(db, out)?;
    }

    let user_tables = sort_names(db.get_table_names()?);
    let system_tables = sort_names(db.get_system_table_names()?);
    out.push_str(&format!("usertables {} ", user_tables.len()));
    push_joined_names(out, &user_tables);
    out.push_str(&format!("systemtables {} ", system_tables.len()));
    push_joined_names(out, &system_tables);

    if jet3 {
        dump_database_properties(db, out)?;
        dump_table_meta_data(db, &user_tables, &system_tables, out)?;
        dump_relationships(db, out)?;
        dump_access_control_entries(db, out)?;
    }

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

// ---------------------------------------------------------------------------
// database level depth (Jet 3 only)
// ---------------------------------------------------------------------------

fn file_format_name(ff: FileFormat) -> &'static str {
    match ff {
        FileFormat::V1997 => "V1997",
        FileFormat::GenericJet4 => "GENERIC_JET4",
        FileFormat::V2000 => "V2000",
        FileFormat::V2003 => "V2003",
        FileFormat::V2007 => "V2007",
        FileFormat::V2010 => "V2010",
        FileFormat::V2016 => "V2016",
        FileFormat::V2019 => "V2019",
        FileFormat::Msisam => "MSISAM",
    }
}

fn dump_database_info(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    let ff = file_format_name(db.get_file_format()?);
    out.push_str(&format!("fileformat {}\n", ff));
    out.push_str(&format!("readonly {}\n", db.is_read_only()));
    let sort = db.get_default_sort_order();
    out.push_str(&format!(
        "defaultsort {}/{}/{}\n",
        sort.value, sort.variant, sort.version
    ));
    out.push_str(&format!("defaultcodepage {}\n", db.get_default_code_page()));
    out.push_str(&format!("columnorder {}\n", db.get_column_order().name()));
    out.push_str(&format!("enforcefk {}\n", db.is_enforce_foreign_keys()));
    out.push_str(&format!("reusefreepages {}\n", db.is_reuse_free_pages()));
    out.push_str(&format!(
        "allowautonumberinsert {}\n",
        db.is_allow_auto_number_insert()
    ));
    out.push_str(&format!(
        "evaluateexpressions {}\n",
        db.is_evaluate_expressions()
    ));
    out.push_str(&format!("tzoffset {}\n", db.time_zone_offset_minutes()));
    match db.get_database_password()? {
        Some(pwd) => out.push_str(&format!("password {}\n", esc(&pwd))),
        None => out.push_str("password null\n"),
    }
    Ok(())
}

fn dump_database_properties(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    let maps = db.get_database_properties()?;
    dump_prop_map(out, "dbprops", "dbprop", None, maps.default_map());
    let maps = db.get_summary_properties()?;
    dump_prop_map(out, "summaryprops", "summaryprop", None, maps.default_map());
    let maps = db.get_user_defined_properties()?;
    dump_prop_map(out, "userdefprops", "userdefprop", None, maps.default_map());
    Ok(())
}

/// A whole property map: the size line, then one line per property in the
/// map's iteration (blob) order.
fn dump_prop_map(
    out: &mut String,
    head: &str,
    item: &str,
    owner: Option<&str>,
    map: Option<&PropertyMap>,
) {
    let size = map.map(|m| m.len()).unwrap_or(0);
    match owner {
        Some(o) => out.push_str(&format!("{} {} {}\n", head, o, size)),
        None => out.push_str(&format!("{} {}\n", head, size)),
    }
    let map = match map {
        Some(m) => m,
        None => return,
    };
    for p in map.iter() {
        let base = match owner {
            Some(o) => format!(
                "{} {} {} {}",
                item,
                o,
                esc(p.name()),
                p.data_type().get_name()
            ),
            None => format!("{} {} {}", item, esc(p.name()), p.data_type().get_name()),
        };
        out.push_str(&format!(
            "{} ddl {} skip {} {}\n",
            base,
            map.is_ddl(p.name()),
            map.is_skip_handler(p.name()),
            val(p.value())
        ));
    }
}

fn nullable_string(value: Option<&str>) -> String {
    match value {
        Some(s) => esc(s),
        None => "null".to_string(),
    }
}

fn dump_table_meta_data(
    db: &mut Database,
    user_tables: &[String],
    system_tables: &[String],
    out: &mut String,
) -> jackcess::Result<()> {
    for (i, name) in user_tables.iter().chain(system_tables.iter()).enumerate() {
        let is_system = i >= user_tables.len();
        let meta = db.get_table_meta_data(name)?;
        match meta {
            None => out.push_str(&format!("tmeta {} null\n", esc(name))),
            Some(m) => out.push_str(&format!(
                "tmeta {} type {} linked {} system {} linkedTable {} linkedDb {} connect {}\n",
                esc(name),
                m.table_type.name(),
                m.is_linked(),
                is_system,
                nullable_string(m.linked_table_name.as_deref()),
                nullable_string(m.linked_db_name.as_deref()),
                nullable_string(m.connection_name.as_deref()),
            )),
        }
    }
    Ok(())
}

fn dump_relationships(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    let rels = db.relationships_all()?;
    dump_relationship_list(out, "rel", "relcount", &rels);
    let rels = db.system_relationships()?;
    dump_relationship_list(out, "sysrel", "sysrelcount", &rels);
    Ok(())
}

fn dump_relationship_list(
    out: &mut String,
    prefix: &str,
    count_prefix: &str,
    rels: &[jackcess::relationship::Relationship],
) {
    out.push_str(&format!("{} {}\n", count_prefix, rels.len()));
    for (i, r) in rels.iter().enumerate() {
        out.push_str(&format!("{} {} ", prefix, i));
        out.push_str(&esc(&r.name));
        out.push_str(&format!(
            " from {} to {} fromCols ",
            esc(&r.primary_table),
            esc(&r.secondary_table)
        ));
        append_col_names(out, &r.primary_columns);
        out.push_str(" toCols ");
        append_col_names(out, &r.secondary_columns);
        out.push_str(&format!(
            " oneToOne {} refInteg {} cascadeUpdates {} cascadeDeletes {} cascadeNull {} leftOuter {} rightOuter {} join {}\n",
            r.is_one_to_one(),
            r.has_referential_integrity(),
            r.cascade_updates(),
            r.cascade_deletes(),
            r.cascade_null_on_delete(),
            r.is_left_outer_join(),
            r.is_right_outer_join(),
            match r.join_type() {
                jackcess::relationship::JoinType::Inner => "INNER",
                jackcess::relationship::JoinType::LeftOuter => "LEFT_OUTER",
                jackcess::relationship::JoinType::RightOuter => "RIGHT_OUTER",
            },
        ));
    }
}

fn append_col_names(out: &mut String, cols: &[String]) {
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&esc(c));
    }
}

/// The MSysACEs contents through the ACL table, in row id order.
fn dump_access_control_entries(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    let mut rows: Vec<(jackcess::table::RowId, Vec<Value>)> = db
        .read_rows_with_ids(jackcess::database::TABLE_SYSTEM_ACES)?
        .into_iter()
        .filter_map(|(id, values)| values.map(|v| (id, v)))
        .collect();
    rows.sort_by_key(|(id, _)| (id.0, id.1));
    out.push_str(&format!("aces {}\n", rows.len()));
    for (_, values) in rows {
        out.push_str("ace");
        for v in &values {
            out.push(' ');
            out.push_str(&val(v));
        }
        out.push('\n');
    }
    Ok(())
}

fn dump_table(db: &mut Database, name: &str, out: &mut String) -> jackcess::Result<()> {
    let jet3 = db.format.name == "VERSION_3";
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

    {
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
    }

    if jet3 {
        // `Table.getCreatedDate()` reads the catalog row of the table on every
        // call.  The port caches it on the loaded table, but skips the system
        // catalog table itself (reading it would recurse); the catalog
        // metadata API reads the same row, so use it as the documented
        // equivalent for that table.
        let (meta_created, meta_updated) = match db.get_table_meta_data(name)? {
            Some(meta) => (meta.created_date, meta.updated_date),
            None => (None, None),
        };
        let maps = db.get_table_properties(page)?;
        {
            let table = db.tables.get(&page).unwrap();
            dump_table_depth(table, &maps, meta_created, meta_updated, out);
        }
    }

    let (index_names, pages): (Vec<String>, Vec<i32>) = {
        let table = db.tables.get(&page).unwrap();
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
        let positions: Vec<Option<usize>> = {
            let table = db.tables.get(&page).unwrap();
            let cols: Vec<String> = table
                .indexes
                .iter()
                .find(|i| &i.name == idx_name)
                .and_then(|idx| table.index_data_for(idx))
                .map(|d| d.columns.iter().map(|c| c.column.name.clone()).collect())
                .unwrap_or_default();
            cols.iter()
                .map(|name| table.columns.iter().position(|c| &c.name == name))
                .collect()
        };
        let ref_line = if jet3 {
            build_index_reference(db, page, idx_name)?
        } else {
            String::new()
        };
        {
            let table = db.tables.get(&page).unwrap();
            if let Some(idx) = table.indexes.iter().find(|i| &i.name == idx_name) {
                dump_index_definition(table, idx, &ref_line, out);
            }
        }
        let mut ids: Vec<(i32, i32)> = Vec::new();
        let mut key_lines = String::new();
        let mut error: Option<jackcess::Error> = None;
        match jackcess::cursor::IndexCursor::new(db, name, idx_name) {
            Ok(mut cursor) => loop {
                match cursor.get_next_row(db) {
                    Ok(Some(values)) => {
                        let id = cursor.get_current_row_id();
                        ids.push(id);
                        key_lines.push_str(&format!(
                            "indexentry {} {} {}/{}",
                            esc(idx_name),
                            ids.len() - 1,
                            id.0,
                            id.1
                        ));
                        for pos in &positions {
                            let value = pos.and_then(|p| values.get(p)).unwrap_or(&Value::Null);
                            key_lines.push(' ');
                            key_lines.push_str(&val(value));
                        }
                        key_lines.push('\n');
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
        out.push_str(&key_lines);
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

fn dump_table_depth(
    table: &Table,
    maps: &jackcess::property::PropertyMaps,
    meta_created: Option<LocalDateTime>,
    meta_updated: Option<LocalDateTime>,
    out: &mut String,
) {
    let name = esc(&table.name);
    match table.get_created_date().or(meta_created) {
        Some(d) => out.push_str(&format!("tablecreated {} dt:{}\n", name, iso(&d))),
        None => out.push_str(&format!("tablecreated {} null\n", name)),
    }
    match table.get_updated_date().or(meta_updated) {
        Some(d) => out.push_str(&format!("tableupdated {} dt:{}\n", name, iso(&d))),
        None => out.push_str(&format!("tableupdated {} null\n", name)),
    }
    dump_prop_map(
        out,
        "tprops",
        "tprop",
        Some(&format!("{} {}", name, esc(""))),
        maps.default_map(),
    );
    for col in &table.columns {
        dump_prop_map(
            out,
            "cprops",
            "cprop",
            Some(&format!("{} {}", name, esc(&col.name))),
            maps.get(&col.name),
        );
    }
    for idx in &table.indexes {
        dump_prop_map(
            out,
            "iprops",
            "iprop",
            Some(&format!("{} {}", name, esc(&idx.name))),
            maps.get_index(&idx.name),
        );
    }
    dump_table_name_map(table, maps, out);
}

fn dump_table_name_map(table: &Table, maps: &jackcess::property::PropertyMaps, out: &mut String) {
    let name = esc(&table.name);
    let name_map = if table.is_system() {
        table.get_name_map_from(maps).ok().flatten()
    } else {
        table.get_name_map().ok().flatten()
    };
    let name_map = match name_map {
        Some(nm) => nm,
        None => {
            out.push_str(&format!("namemap {} present false\n", name));
            return;
        }
    };
    let table_record = name_map.get_table_record();
    out.push_str(&format!(
        "namemap {} present true records {} tableRecords {} columnRecords {} references {} table {}\n",
        name,
        name_map.name_map().records().len(),
        name_map.name_map().get_table_records().len(),
        name_map.name_map().get_column_records().len(),
        name_map.get_references().len(),
        match table_record {
            Some(r) => nullable_string(r.get_guid().as_deref()),
            None => "null".to_string(),
        }
    ));
    for (i, r) in name_map.name_map().iter().enumerate() {
        out.push_str(&format!(
            "namemaprec {} {} {} guid {} parent {} recname {} date {}\n",
            name,
            i,
            r.get_type().name(),
            nullable_string(r.get_guid().as_deref()),
            nullable_string(r.get_parent_guid().as_deref()),
            esc(r.get_name()),
            match r.get_date() {
                Some(d) => format!("dt:{}", iso(&d)),
                None => "null".to_string(),
            }
        ));
    }
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

/// The foreign key reference stored in the index definition (Jet 3 only),
/// resolved the way `IndexImpl.getReferencedIndex` resolves it.
fn build_index_reference(db: &mut Database, page: i32, idx_name: &str) -> jackcess::Result<String> {
    let (reference, _index_number) = {
        let table = db.tables.get(&page).unwrap();
        match table.indexes.iter().find(|i| i.name == idx_name) {
            Some(idx) => (idx.reference, idx.index_number),
            None => return Ok(String::new()),
        }
    };
    let name = esc(idx_name);
    let reference = match reference {
        None => return Ok(format!("indexref {} none\n", name)),
        Some(r) => r,
    };
    let mut line = format!(
        "indexref {} page {} index {} tableType {} primaryTable {} cascadeUpdates {} cascadeDeletes {} cascadeNull {}",
        name,
        reference.other_table_page_number,
        reference.other_index_number,
        reference.table_type,
        reference.is_primary_table(),
        reference.cascade_updates,
        reference.cascade_deletes,
        reference.cascade_null,
    );
    let resolved = db
        .table_by_page(reference.other_table_page_number)
        .ok()
        .and_then(|t| {
            t.indexes
                .iter()
                .find(|i| i.index_number == reference.other_index_number)
                .map(|i| (i.name.clone(), i.index_data_number))
        });
    match resolved {
        Some((ref_name, ref_data)) => {
            line.push_str(&format!(
                " refIndex {} refData {}",
                esc(&ref_name),
                ref_data
            ));
        }
        None => line.push_str(" refIndex error"),
    }
    line.push('\n');
    Ok(line)
}

fn dump_index_definition(
    table: &Table,
    idx: &jackcess::index::IndexImpl,
    ref_line: &str,
    out: &mut String,
) {
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
    out.push_str(ref_line);
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
