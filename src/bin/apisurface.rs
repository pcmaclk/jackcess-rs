//! `apisurface` - the Rust counterpart of `java-src/ApiSurfaceGen.java`: dumps
//! the public read-side API surface of [`Database`], [`Table`] and [`Cursor`]
//! in the canonical text `tools/verify-apisurface.ps1` compares against the
//! Java reference.
//!
//! Besides the table/cursor surface the dump covers the column sort ids
//! (`SortOrder.getSortId`, including the Jet 3 `IllegalStateException`), the
//! resolved `Index.getReferencedIndex` of every index (with Java's
//! reciprocal-reference validation), the `MSysACEs` table of
//! `Database.getAccessControlEntries` and `Table.getNameMap` for every table
//! (system tables included).
//!
//! Usage: `apisurface <database>` (a `.mny` file is opened through the MSISAM
//! codec, like [`Database::open_path`]).
//!
//! The output is a sorted list of `key=value` lines.  Values are type tagged
//! (`str:`, `bool:`, `num:`, `null`, `throws:<SimpleName>:<message>`), and the
//! date getters are dumped as epoch milliseconds in UTC because Java's
//! `LocalDateTime` carries no zone.  A missing database object is `absent`:
//! Java's `getTable`/`getSystemTable` return `null`, while this port's
//! `get_table` reports the same case as a `Jackcess` error which this tool
//! canonicalizes to `absent` (the `Result`-based port never returns a null
//! `Table`).
//!
//! The comparisons the Java tool documents also apply here: the time zone is
//! fixed to `GMT+05:30` before `getZoneId` (this port has no zone database,
//! only fixed offsets), and a null database object (Java) is `absent` (a
//! missing name through the `Result`/`Option` APIs).  `get_table` on a system
//! table is the one documented semantic difference (Java returns null, the
//! port resolves it) and is deliberately not part of the dump.

use std::path::Path;

use jackcess::cursor::{ColumnMatcher, Cursor, TableScanCursor};
use jackcess::database::{Database, TableMetaData, TableMetaDataType};
use jackcess::error::{Error, Result};
use jackcess::format::FileFormat;
use jackcess::table::{ColumnMetaData, Table};
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: apisurface <database>");
        std::process::exit(2);
    }
    let path = Path::new(&args[1]);
    match run(path) {
        Ok(lines) => {
            let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
            for line in lines {
                out.push_str(&line);
                out.push('\n');
            }
            print!("{out}");
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn run(path: &Path) -> Result<Vec<String>> {
    let mut db = Database::open_path(path)?;
    let mut out = Vec::new();
    dump_database(&mut db, &mut out)?;
    out.sort();
    Ok(out)
}

fn dump_database(db: &mut Database, out: &mut Vec<String>) -> Result<()> {
    put(
        out,
        "db.file_format",
        str_value(file_format_name(db.get_file_format()?)),
    );
    put(out, "db.charset", str_value(db.get_charset().name()));
    let sort_order = db.get_default_sort_order();
    put(
        out,
        "db.default_sort_order",
        format!(
            "num:{},num:{},num:{}",
            sort_order.value, sort_order.variant, sort_order.version
        ),
    );
    put(
        out,
        "db.default_code_page",
        num_value(db.get_default_code_page() as i64),
    );
    put(
        out,
        "db.column_order",
        str_value(db.get_column_order().name()),
    );
    put(
        out,
        "db.date_time_type",
        str_value(db.get_date_time_type().name()),
    );
    put(
        out,
        "db.enforce_foreign_keys",
        bool_value(db.is_enforce_foreign_keys()),
    );
    put(
        out,
        "db.evaluate_expressions",
        bool_value(db.is_evaluate_expressions()),
    );
    put(
        out,
        "db.allow_auto_number_insert",
        bool_value(db.is_allow_auto_number_insert()),
    );
    put(
        out,
        "db.reuse_free_pages",
        bool_value(db.is_reuse_free_pages()),
    );
    put(
        out,
        "db.write_broken_index",
        bool_value(db.is_write_broken_index()),
    );
    put(out, "db.read_only", bool_value(db.is_read_only()));
    put(out, "db.name", str_value(db.get_name()));
    put(
        out,
        "db.path",
        str_value(
            &db.get_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ),
    );
    let password = db.get_database_password()?;
    put(out, "db.password", str_option(password.as_deref()));

    // the default offset is comparable; the ZoneId is read after installing a
    // fixed offset zone on both sides (this port has no named zone database)
    put(
        out,
        "db.time_zone_offset_minutes_default",
        num_value(db.time_zone_offset_minutes() as i64),
    );
    db.set_time_zone_offset_minutes(330);
    put(out, "db.zone_id", str_value(&db.get_zone_id()));

    let table_names = db.get_table_names()?;
    let system_names = db.get_system_table_names()?;
    put(out, "db.table_names", str_value(&table_names.join(",")));
    put(
        out,
        "db.system_table_names",
        str_value(&system_names.join(",")),
    );
    put(out, "db.table_count", num_value(table_names.len() as i64));
    put(
        out,
        "db.system_table_count",
        num_value(system_names.len() as i64),
    );

    put(
        out,
        "db.system_table_missing",
        table_option_ref(db.get_system_table("__APISURFACE_MISSING__")?),
    );
    put(
        out,
        "db.table_missing",
        table_result_ref(db.get_table("__APISURFACE_MISSING__")),
    );
    put(
        out,
        "db.system_table_msysobjects",
        table_option_ref(db.get_system_table("MSysObjects")?),
    );
    put(
        out,
        "db.reclaim_free_space",
        num_value(db.reclaim_free_space()? as i64),
    );
    // `Database.getAccessControlEntries`: the ACE table name and row count
    match db.get_access_control_entries() {
        Ok(table) => {
            let value = format!("{}:{}", table.get_name(), table.get_row_count());
            put(out, "db.aces", str_value(&value));
        }
        Err(e) => put(out, "db.aces", exc(&e)),
    }

    dump_name_maps(db, out)?;

    dump_property_maps(
        db.get_database_properties()?,
        "db.database_properties.",
        out,
    );
    dump_property_maps(db.get_summary_properties()?, "db.summary_properties.", out);
    dump_property_maps(
        db.get_user_defined_properties()?,
        "db.user_defined_properties.",
        out,
    );

    dump_queries(db, out)?;
    dump_table_meta_data(db, out)?;
    dump_tables(db, out)?;
    Ok(())
}

fn dump_queries(db: &mut Database, out: &mut Vec<String>) -> Result<()> {
    let queries = db.get_queries()?;
    let names: Vec<String> = queries.iter().map(|q| q.name().to_string()).collect();
    put(out, "db.query_count", num_value(names.len() as i64));
    put(out, "db.query_names", str_value(&names.join(",")));
    for query in &queries {
        let prefix = format!("query.{}.", query.name());
        put(
            out,
            &(prefix.clone() + "type"),
            str_value(query_type_name(query.query_type())),
        );
        put(
            out,
            &(prefix.clone() + "hidden"),
            bool_value(query.is_hidden()),
        );
        put(
            out,
            &(prefix.clone() + "object_id"),
            num_value(query.object_id() as i64),
        );
        put(
            out,
            &(prefix.clone() + "object_flag"),
            num_value(query.object_flag() as i64),
        );
        match query.get_owner_access_type() {
            Ok(value) => put(
                out,
                &(prefix.clone() + "owner_access_type"),
                str_value(value),
            ),
            Err(e) => put(out, &(prefix.clone() + "owner_access_type"), exc(&e)),
        }
        match query.get_parameters() {
            Ok(parameters) => put(
                out,
                &(prefix.clone() + "parameters"),
                str_value(&parameters.join(",")),
            ),
            Err(e) => put(out, &(prefix.clone() + "parameters"), exc(&e)),
        }
        match query.get_properties() {
            Ok(map) => dump_properties(map, &(prefix + "properties."), out),
            Err(e) => put(out, &(prefix + "properties.read"), exc(&e)),
        }
    }
    Ok(())
}

fn dump_table_meta_data(db: &mut Database, out: &mut Vec<String>) -> Result<()> {
    let mut names = db.get_table_names()?;
    names.extend(db.get_system_table_names()?);
    names.sort();
    names.dedup();
    put(out, "meta.count", num_value(names.len() as i64));
    for name in names {
        match db.get_table_meta_data(&name)? {
            None => put(out, &format!("meta.{name}"), str_value("absent")),
            Some(md) => dump_one_table_meta_data(&md, out),
        }
    }
    Ok(())
}

/// `Table.getNameMap()` of every table, system tables included (Java loads the
/// `NameMap` property of any table).  A linked table's map lives in the linkee.
fn dump_name_maps(db: &mut Database, out: &mut Vec<String>) -> Result<()> {
    let mut names = db.get_table_names()?;
    names.extend(db.get_system_table_names()?);
    names.sort();
    names.dedup();
    for name in names {
        let linked = matches!(
            db.get_table_meta_data(&name)?.map(|md| md.table_type),
            Some(TableMetaDataType::Linked)
        );
        if linked {
            put(out, &format!("namemap.{name}"), str_value("linked"));
            continue;
        }
        let value = match db.get_table_name_map(&name) {
            Ok(None) => str_value("absent"),
            Ok(Some(name_map)) => {
                let table_record = name_map.get_table_record();
                str_value(&format!(
                    "present:{}:{}",
                    name_map.name_map().records().len(),
                    match table_record {
                        Some(record) => record.get_guid().unwrap_or_else(|| "null".to_string()),
                        None => "null".to_string(),
                    }
                ))
            }
            Err(e) => exc(&e),
        };
        put(out, &format!("namemap.{name}"), value);
    }
    Ok(())
}

fn dump_one_table_meta_data(md: &TableMetaData, out: &mut Vec<String>) {
    let prefix = format!("meta.{}.", md.name);
    put(
        out,
        &(prefix.clone() + "type"),
        str_value(md.table_type.name()),
    );
    put(
        out,
        &(prefix.clone() + "is_linked"),
        bool_value(md.is_linked()),
    );
    put(
        out,
        &(prefix.clone() + "is_system"),
        bool_value(md.is_system()),
    );
    put(
        out,
        &(prefix.clone() + "linked_table_name"),
        str_option(md.linked_table_name.as_deref()),
    );
    put(
        out,
        &(prefix.clone() + "linked_db_name"),
        str_option(md.linked_db_name.as_deref()),
    );
    put(
        out,
        &(prefix.clone() + "connection_name"),
        str_option(md.connection_name.as_deref()),
    );
}

fn dump_tables(db: &mut Database, out: &mut Vec<String>) -> Result<()> {
    let names = db.get_table_names()?;
    for name in names {
        let prefix = format!("table.{name}.");
        let meta = db.get_table_meta_data(&name)?;
        if matches!(
            meta.as_ref().map(|md| md.table_type),
            Some(TableMetaDataType::Linked)
        ) {
            // opening a type 6 link needs a LinkResolver (and a linkee file)
            put(out, &(prefix + "skipped"), str_value("linked"));
            continue;
        }
        let (page, definition_only) = match db.get_table(&name) {
            Ok(table) => (table.table_def_page_number, table.is_definition_only()),
            Err(e) => {
                put(out, &(prefix + "skipped"), str_value("absent"));
                let _ = e;
                continue;
            }
        };
        let db_allow_auto_number = db.is_allow_auto_number_insert();

        {
            let table = db.get_table(&name)?;
            dump_table_basic(table, &prefix, db_allow_auto_number, out);
        }

        // `Index.getReferencedIndex` of every index: the reference lives on the
        // index definition, the other table is resolved through the database
        {
            let index_names: Vec<String> = db
                .get_table(&name)?
                .get_indexes()
                .iter()
                .map(|index| index.name.clone())
                .collect();
            for index_name in index_names {
                let value = referenced_index(db, &name, &index_name);
                put(
                    out,
                    &format!("{}index.{}.referenced", prefix, index_name),
                    value,
                );
            }
        }

        // the version history link needs the database, so the column metadata
        // is taken from the database level API (`Column.isAppendOnly` /
        // `getVersionHistoryColumn` ask the database in Java too)
        {
            let columns = db.get_column_meta_data_all(&name)?;
            put(
                out,
                &(prefix.clone() + "column_meta"),
                str_value(&column_meta(&columns)),
            );
        }

        let maps = db.get_table_properties(page)?;
        let mut map_names: Vec<String> = maps
            .iter()
            // a map which holds no properties is not part of the stored
            // surface (the port pre-creates empty per-column maps while
            // loading a table)
            .filter(|map| !map.is_empty())
            .map(|map| format!("{}:{}", map.name(), map.iter().count()))
            .collect();
        map_names.sort();
        put(
            out,
            &(prefix.clone() + "property_maps"),
            str_value(&map_names.join(",")),
        );
        if let Some(map) = maps.default_map() {
            dump_properties(map, &(prefix.clone() + "properties."), out)
        }

        if definition_only {
            // `TableDefinitionImpl.createRowState` throws the port's documented
            // "no data access" exception, which is the exact exception text
            let message = {
                let table = db.get_table(&name)?;
                match table.check_data_access() {
                    Ok(()) => String::new(),
                    Err(e) => exc(&e),
                }
            };
            put(out, &(prefix.clone() + "cursor"), message);
        } else {
            dump_cursor(db, &name, &(prefix.clone() + "cursor."), out)?;
        }
    }
    Ok(())
}

fn dump_table_basic(
    table: &Table,
    prefix: &str,
    db_allow_auto_number: bool,
    out: &mut Vec<String>,
) {
    put(
        out,
        &(prefix.to_string() + "kind"),
        str_value(if table.is_definition_only() {
            "definition"
        } else {
            "table"
        }),
    );
    put(
        out,
        &(prefix.to_string() + "name"),
        str_value(table.get_name()),
    );
    put(
        out,
        &(prefix.to_string() + "system"),
        bool_value(table.is_system()),
    );
    put(
        out,
        &(prefix.to_string() + "hidden"),
        bool_value(table.is_hidden()),
    );
    put(
        out,
        &(prefix.to_string() + "column_count"),
        num_value(table.get_column_count() as i64),
    );
    put(
        out,
        &(prefix.to_string() + "max_column_count"),
        num_value(table.get_max_column_count() as i64),
    );
    let columns: Vec<String> = table
        .get_columns()
        .iter()
        .map(|column| column.name.clone())
        .collect();
    put(
        out,
        &(prefix.to_string() + "columns"),
        str_value(&columns.join(",")),
    );
    // `ColumnImpl.SortOrder.getSortId` (the Jet 3 `IllegalStateException`
    // included); a column without a textual sort order is Java's null
    for column in table.get_columns() {
        let value = match column.sort_order {
            None => str_value("-"),
            Some(sort_order) => match sort_order.get_sort_id() {
                Ok(sort_id) => num_value(sort_id as i64),
                Err(e) => exc(&e),
            },
        };
        put(
            out,
            &format!("{}column.{}.sort_id", prefix, column.name),
            value,
        );
    }
    put(
        out,
        &(prefix.to_string() + "row_count"),
        num_value(table.get_row_count() as i64),
    );
    put(
        out,
        &(prefix.to_string() + "created_ms"),
        date_millis(table.get_created_date()),
    );
    put(
        out,
        &(prefix.to_string() + "updated_ms"),
        date_millis(table.get_updated_date()),
    );
    put(
        out,
        &(prefix.to_string() + "logical_index_count"),
        num_value(table.get_logical_index_count() as i64),
    );
    put(
        out,
        &(prefix.to_string() + "index_data_count"),
        num_value(table.get_index_datas().len() as i64),
    );
    put(
        out,
        &(prefix.to_string() + "approx_owned_pages"),
        num_value(table.get_approximate_owned_page_count() as i64),
    );
    let allow_auto_number = table
        .allow_auto_number_insert
        .unwrap_or(db_allow_auto_number);
    put(
        out,
        &(prefix.to_string() + "auto_number_insert"),
        bool_value(allow_auto_number),
    );

    let mut index_names = Vec::new();
    for index in table.get_indexes() {
        let metadata = table.index_meta_data(index);
        let ip = format!("{}index.{}.", prefix, metadata.name);
        index_names.push(metadata.name.clone());
        put(
            out,
            &(ip.clone() + "primary"),
            bool_value(metadata.primary_key),
        );
        put(
            out,
            &(ip.clone() + "foreign"),
            bool_value(metadata.foreign_key),
        );
        put(out, &(ip.clone() + "unique"), bool_value(metadata.unique));
        put(
            out,
            &(ip.clone() + "required"),
            bool_value(metadata.required),
        );
        put(
            out,
            &(ip.clone() + "ignore_nulls"),
            bool_value(metadata.ignore_nulls),
        );
        put(
            out,
            &(ip.clone() + "column_count"),
            num_value(metadata.columns.len() as i64),
        );
        let cols: Vec<String> = metadata
            .columns
            .iter()
            .zip(metadata.ascending.iter())
            .map(|(name, ascending)| {
                format!("{}:{}", name, if *ascending { "asc" } else { "desc" })
            })
            .collect();
        put(out, &(ip + "columns"), str_value(&cols.join(",")));
    }
    put(
        out,
        &(prefix.to_string() + "index_names"),
        str_value(&index_names.join(",")),
    );
    match table.get_primary_key_index() {
        Ok(index) => put(
            out,
            &(prefix.to_string() + "primary_key"),
            str_value(&index.name),
        ),
        Err(e) => put(out, &(prefix.to_string() + "primary_key"), exc(&e)),
    }
    match table.get_index("__APISURFACE_MISSING__") {
        Ok(_) => put(
            out,
            &(prefix.to_string() + "missing_index"),
            str_value("found"),
        ),
        Err(e) => put(out, &(prefix.to_string() + "missing_index"), exc(&e)),
    }
    match table.get_foreign_key_index(table) {
        Ok(index) => put(
            out,
            &(prefix.to_string() + "foreign_key_missing"),
            str_value(&index.name),
        ),
        Err(e) => put(out, &(prefix.to_string() + "foreign_key_missing"), exc(&e)),
    }
}

/// `Index.getReferencedIndex`: the referenced `table.index`, `-` for an index
/// without a reference, or the Java exception (`IOException` for a missing
/// table/index or a broken reciprocal reference).
fn referenced_index(db: &mut Database, table_name: &str, index_name: &str) -> String {
    let reference_page = db
        .get_table(table_name)
        .ok()
        .and_then(|table| table.get_index(index_name).ok())
        .and_then(|index| index.reference.map(|r| r.other_table_page_number));
    match db.get_referenced_index(table_name, index_name) {
        Ok(None) => str_value("-"),
        Ok(Some(referenced)) => {
            let referenced_table = reference_page
                .and_then(|page| db.get_table_by_page(page).ok())
                .map(|table| table.name.clone())
                .unwrap_or_default();
            str_value(&format!("{}.{}", referenced_table, referenced.name))
        }
        Err(e) => exc(&e),
    }
}

fn column_meta(columns: &[ColumnMetaData]) -> String {
    let entries: Vec<String> = columns.iter().map(column_meta_entry).collect();
    entries.join(",")
}

fn column_meta_entry(column: &ColumnMetaData) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        column.name,
        column.data_type.get_name(),
        column.length,
        column.length_in_units,
        column.auto_number,
        column.hidden,
        column.calculated,
        column.append_only,
        column.variable_length,
        column.version_history_column.as_deref().unwrap_or("-")
    )
}

fn dump_cursor(db: &mut Database, table: &str, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let mut cursor = TableScanCursor::new(db, table)?;
    put(
        out,
        &(prefix.to_string() + "table"),
        str_value(cursor.table_name()),
    );
    put(
        out,
        &(prefix.to_string() + "matcher"),
        str_value(column_matcher_name(cursor.get_column_matcher())),
    );
    put(
        out,
        &(prefix.to_string() + "before_first.is_before_first"),
        bool_value(cursor.is_before_first()),
    );
    put(
        out,
        &(prefix.to_string() + "before_first.is_after_last"),
        bool_value(cursor.is_after_last()),
    );

    let moved = cursor.move_next_rows(db, 1)?;
    put(
        out,
        &(prefix.to_string() + "moved"),
        num_value(moved as i64),
    );
    if moved > 0 {
        put(
            out,
            &(prefix.to_string() + "row.is_before_first"),
            bool_value(cursor.is_before_first()),
        );
        put(
            out,
            &(prefix.to_string() + "row.is_after_last"),
            bool_value(cursor.is_after_last()),
        );
        put(
            out,
            &(prefix.to_string() + "row.is_current_row_deleted"),
            bool_value(cursor.is_current_row_deleted(db)?),
        );
        let savepoint = cursor.get_savepoint();
        let mut other = TableScanCursor::new(db, table)?;
        other.move_next_rows(db, 1)?;
        let other_savepoint = other.get_savepoint();
        put(
            out,
            &(prefix.to_string() + "savepoint.same_id_across_cursors"),
            bool_value(savepoint.get_cursor_id() == other_savepoint.get_cursor_id()),
        );
        put(
            out,
            &(prefix.to_string() + "savepoint.same_position_across_cursors"),
            bool_value(savepoint.get_current_row_id() == other_savepoint.get_current_row_id()),
        );
    }

    cursor.after_last();
    put(
        out,
        &(prefix.to_string() + "after_last.is_after_last"),
        bool_value(cursor.is_after_last()),
    );
    cursor.reset();
    put(
        out,
        &(prefix.to_string() + "reset.is_before_first"),
        bool_value(cursor.is_before_first()),
    );
    Ok(())
}

fn dump_property_maps(maps: jackcess::property::PropertyMaps, prefix: &str, out: &mut Vec<String>) {
    if let Some(map) = maps.default_map() {
        dump_properties(map, prefix, out);
    }
}

fn dump_properties(map: &jackcess::property::PropertyMap, prefix: &str, out: &mut Vec<String>) {
    for property in map.iter() {
        put(
            out,
            &format!("{}{}", prefix, property.name),
            property_value(&property.value),
        );
    }
}

fn property_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => bool_value(value.as_i64() == Some(1)),
        Value::Byte(_) | Value::Int(_) | Value::Long(_) | Value::BigInt(_) => {
            num_value(value.as_i64().unwrap_or(0))
        }
        Value::Float(v) => format!("num:{v:?}"),
        Value::Double(v) => format!("num:{v:?}"),
        Value::Decimal { unscaled, scale } => {
            format!("num:{}", decimal_to_string(*unscaled, *scale))
        }
        Value::Bytes(bytes) => format!("bytes:{}", hex(bytes)),
        Value::Text(text) => str_value(text),
        Value::DateTime(date) => num_value(date.to_epoch_millis()),
        Value::DateBits(bits) => str_value(&format!("datebits:{bits}")),
        Value::Uuid(bytes) => format!("bytes:{}", hex(bytes)),
        Value::Keep => str_value("KEEP_VALUE"),
    }
}

fn decimal_to_string(unscaled: i128, scale: i32) -> String {
    // `java.math.BigDecimal.toString`: a plain decimal for a non-negative
    // scale (keeping trailing zeros), scientific notation for a negative one
    let negative = unscaled < 0;
    let digits = unscaled.unsigned_abs().to_string();
    let body = if scale == 0 {
        digits
    } else if scale < 0 {
        format!("{}E+{}", digits, -(scale as i64))
    } else {
        let scale = scale as usize;
        if digits.len() > scale {
            let point = digits.len() - scale;
            format!("{}.{}", &digits[..point], &digits[point..])
        } else {
            format!("0.{}{}", "0".repeat(scale - digits.len()), digits)
        }
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

fn file_format_name(format: FileFormat) -> &'static str {
    match format {
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

fn query_type_name(query_type: jackcess::query::QueryType) -> &'static str {
    use jackcess::query::QueryType;
    match query_type {
        QueryType::Select => "SELECT",
        QueryType::MakeTable => "MAKE_TABLE",
        QueryType::Append => "APPEND",
        QueryType::Update => "UPDATE",
        QueryType::Delete => "DELETE",
        QueryType::CrossTab => "CROSS_TAB",
        QueryType::DataDefinition => "DATA_DEFINITION",
        QueryType::Passthrough => "PASSTHROUGH",
        QueryType::Union => "UNION",
        QueryType::Unknown => "UNKNOWN",
    }
}

fn column_matcher_name(matcher: ColumnMatcher) -> &'static str {
    match matcher {
        ColumnMatcher::Simple => "SimpleColumnMatcher",
        ColumnMatcher::CaseInsensitive => "CaseInsensitiveColumnMatcher",
    }
}

fn date_millis(date: Option<jackcess::datenums::LocalDateTime>) -> String {
    match date {
        Some(date) => num_value(date.to_epoch_millis()),
        None => "null".to_string(),
    }
}

fn table_option_ref(table: Option<&mut Table>) -> String {
    match table {
        Some(table) => str_value(&format!("table:{}", table.get_name())),
        None => "absent".to_string(),
    }
}

fn table_result_ref(result: Result<&mut Table>) -> String {
    match result {
        Ok(table) => str_value(&format!("table:{}", table.get_name())),
        Err(Error::Jackcess(message))
            if message.starts_with("Table ") && message.ends_with(" does not exist") =>
        {
            "absent".to_string()
        }
        Err(e) => exc(&e),
    }
}

fn exc(error: &Error) -> String {
    let (class, message) = match error {
        Error::Io(e) => ("IOException", e.to_string()),
        Error::Jackcess(message) => ("IOException", message.clone()),
        Error::IllegalArgument(message) => ("IllegalArgumentException", message.clone()),
        Error::IllegalState(message) => ("IllegalStateException", message.clone()),
        Error::InvalidCredentials(message) => ("InvalidCredentialsException", message.clone()),
        Error::Unsupported(message) => ("UnsupportedOperationException", message.clone()),
        Error::ConstraintViolation(message) => ("ConstraintViolationException", message.clone()),
        Error::InvalidValue(message) => ("InvalidValueException", message.clone()),
        Error::BatchUpdate { message, .. } => ("BatchUpdateException", message.clone()),
        Error::BufferUnderflow(message) => ("BufferUnderflowException", message.clone()),
        Error::BufferOverflow(message) => ("BufferOverflowException", message.clone()),
        // `InvalidMarkException.getMessage()` is null, which the Java half
        // renders as an empty message
        Error::InvalidMark => ("InvalidMarkException", String::new()),
        Error::NumberFormat(message) => ("NumberFormatException", message.clone()),
    };
    format!("throws:{}:{}", class, escape(&message))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

fn str_value(value: &str) -> String {
    format!("str:{}", escape(value))
}

fn str_option(value: Option<&str>) -> String {
    match value {
        Some(value) => str_value(value),
        None => "null".to_string(),
    }
}

fn bool_value(value: bool) -> String {
    format!("bool:{value}")
}

fn num_value(value: i64) -> String {
    format!("num:{value}")
}

fn put(out: &mut Vec<String>, key: &str, value: String) {
    out.push(format!("{key}={value}"));
}
