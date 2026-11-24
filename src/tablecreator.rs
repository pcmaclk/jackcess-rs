//! Port of `TableCreator` / `TableBuilder` / `IndexBuilder` creation logic.

use crate::bytebuf::ByteBuf;
use crate::column::write_name;
use crate::column::{ColumnSpec, ColumnState};
use crate::database::{
    Database, SYSTEM_OBJECT_FLAGS, TABLE_SYSTEM_ACES, TABLE_SYSTEM_CATALOG, TYPE_TABLE,
};
use crate::datatype::DataType;
use crate::error::{Error, Result};
use crate::format::JetFormat;
use crate::index::{
    ASCENDING_COLUMN_FLAG, COLUMN_UNUSED, IGNORE_NULLS_INDEX_FLAG, MAGIC_INDEX_NUMBER, MAX_COLUMNS,
    REQUIRED_INDEX_FLAG, UNIQUE_INDEX_FLAG, UNKNOWN_INDEX_FLAG,
};
use crate::page_types;
use crate::table::{self, add_data_page_row, get_row_space_usage};
use crate::value::Value;

/// `IndexBuilder`
#[derive(Clone, Debug)]
pub struct IndexSpec {
    pub name: String,
    pub index_type: u8,
    pub flags: u8,
    pub columns: Vec<(String, bool)>,
    pub index_number: i32,
}

impl IndexSpec {
    pub fn new(name: impl Into<String>, columns: Vec<(String, bool)>) -> IndexSpec {
        IndexSpec {
            name: name.into(),
            index_type: 0,
            flags: UNKNOWN_INDEX_FLAG,
            columns,
            index_number: 0,
        }
    }

    pub fn primary_key(name: impl Into<String>, columns: Vec<(String, bool)>) -> IndexSpec {
        IndexSpec {
            name: name.into(),
            index_type: crate::index::IndexImpl::PRIMARY_KEY_INDEX_TYPE,
            flags: UNKNOWN_INDEX_FLAG | UNIQUE_INDEX_FLAG | REQUIRED_INDEX_FLAG,
            columns,
            index_number: 0,
        }
    }

    pub fn unique(mut self) -> IndexSpec {
        self.flags |= UNIQUE_INDEX_FLAG;
        self
    }

    pub fn required(mut self) -> IndexSpec {
        self.flags |= REQUIRED_INDEX_FLAG;
        self
    }

    pub fn ignore_nulls(mut self) -> IndexSpec {
        self.flags |= IGNORE_NULLS_INDEX_FLAG;
        self
    }
}

/// `TableBuilder`
#[derive(Clone, Debug)]
pub struct TableSpec {
    pub name: String,
    pub columns: Vec<ColumnSpec>,
    pub indexes: Vec<IndexSpec>,
    /// `TableBuilder.putProperty`: properties the creator stores in the
    /// table's property map after the table has been created
    pub properties: Vec<crate::property::Property>,
}

impl TableSpec {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnSpec>) -> TableSpec {
        TableSpec {
            name: name.into(),
            columns,
            indexes: Vec::new(),
            properties: Vec::new(),
        }
    }

    pub fn with_index(mut self, index: IndexSpec) -> TableSpec {
        self.indexes.push(index);
        self
    }

    /// `TableBuilder.putProperty(name, value)`.
    pub fn put_property(mut self, name: impl Into<String>, value: Value) -> Result<TableSpec> {
        let name = name.into();
        let prop = crate::property::create_property(&name, value)?;
        crate::property::put_builder_property(&mut self.properties, prop);
        Ok(self)
    }

    /// `TableBuilder.putProperty(name, type, value)`.
    pub fn put_property_typed(
        mut self,
        name: impl Into<String>,
        data_type: crate::datatype::DataType,
        value: Value,
    ) -> TableSpec {
        let prop = crate::property::Property::new(name.into(), data_type, value);
        crate::property::put_builder_property(&mut self.properties, prop);
        self
    }
}

/// `TableMutator.IndexDataState`
#[derive(Clone, Debug)]
pub struct IndexDataState {
    pub index_data_number: i32,
    pub root_page_number: i32,
    pub umap_page_number: i32,
    pub umap_row_number: u8,
    pub indexes: Vec<usize>,
}

struct Creator {
    columns: Vec<ColumnSpec>,
    indexes: Vec<IndexSpec>,
    index_states: Vec<IndexDataState>,
    tdef_page_number: i32,
    umap_page_number: i32,
    index_count: i32,
    logical_index_count: i32,
    column_states: Vec<ColumnState>,
}

impl Creator {
    fn reserve_page(ch: &mut crate::pagechannel::PageChannel) -> Result<i32> {
        ch.allocate_new_page()
    }

    fn long_value_columns(&self) -> Vec<usize> {
        (0..self.columns.len())
            .filter(|i| self.columns[*i].data_type.is_long_value())
            .collect()
    }
}

/// Computes the layout of a new table definition and writes it.
pub fn create_table(db: &mut Database, spec: &TableSpec) -> Result<i32> {
    create_table_internal(db, spec, 0, false)
}

/// `TableCreator.createTable(table, catalogFlags, isInternal)`.
///
/// The supporting tables of a complex column are created inside this method,
/// before the definition of the table which holds the column is written, so
/// the page allocation order matches the Java library.
pub(crate) fn create_table_internal(
    db: &mut Database,
    spec: &TableSpec,
    mut catalog_flags: i32,
    is_internal: bool,
) -> Result<i32> {
    let format = db.format.clone();
    validate_new_table_name(db, &spec.name, &format)?;

    let mut columns = spec.columns.clone();
    if columns.is_empty() {
        return Err(Error::IllegalArgument(
            "Cannot create table with no columns".into(),
        ));
    }
    // `TableCreator.addVersionHistoryColumn`: a memo column which keeps a
    // version history gets a complex column beside it, at the end of the list
    columns = add_version_history_column(columns)?;
    // `TableCreator.validate` -> `TableMutator.validateColumn` ->
    // `ColumnBuilder.validate`, which rejects a calculated column the format
    // does not support and fills in a missing result type
    for col in columns.iter_mut() {
        col.validate_calculated(&format)?;
    }
    let column_states: Vec<ColumnState> = columns
        .iter()
        .map(|col| ColumnState {
            extra_flags: col.extra_flags,
            ..ColumnState::default()
        })
        .collect();
    for (i, col) in columns.iter_mut().enumerate() {
        col.column_number = i as i16;
        col.column_id = i as i16;
        if col.data_type.is_textual() && col.sort_order.is_none() {
            // `TableMutator.setColumnSortOrder`: every textual column (TEXT and
            // MEMO) without an explicit order gets the database's sort order
            col.sort_order = Some(db.default_sort_order);
        }
        if let Some(desc) = &col.complex_desc {
            desc.validate(&col.name)?;
        }
    }

    let mut indexes = spec.indexes.clone();
    for (i, idx) in indexes.iter_mut().enumerate() {
        idx.index_number = i as i32;
    }

    // group logical indexes by backing index data
    let mut index_states: Vec<IndexDataState> = Vec::new();
    for (i, idx) in indexes.iter().enumerate() {
        let mut found = None;
        for state in &index_states {
            let first = &indexes[state.indexes[0]];
            if same_index_data(first, idx) {
                found = Some(state.index_data_number);
                break;
            }
        }
        match found {
            Some(n) => {
                index_states
                    .iter_mut()
                    .find(|s| s.index_data_number == n)
                    .unwrap()
                    .indexes
                    .push(i);
            }
            None => {
                let number = index_states.len() as i32;
                index_states.push(IndexDataState {
                    index_data_number: number,
                    root_page_number: -1,
                    umap_page_number: -1,
                    umap_row_number: 0,
                    indexes: vec![i],
                });
            }
        }
    }

    let mut creator = Creator {
        columns: columns.clone(),
        indexes: indexes.clone(),
        index_states,
        tdef_page_number: -1,
        umap_page_number: -1,
        index_count: 0,
        logical_index_count: indexes.len() as i32,
        column_states,
    };
    creator.index_count = creator.index_states.len() as i32;

    if is_internal {
        // this table is part of a larger operation which already holds an
        // exclusive write
        db.channel.start_write();
    } else {
        db.channel.start_exclusive_write()?;
    }
    let result = (|| -> Result<i32> {
        creator.tdef_page_number = Creator::reserve_page(&mut db.channel)?;
        creator.umap_page_number = Creator::reserve_page(&mut db.channel)?;

        // the supporting tables of a complex column carry the page number
        // reserved above, so they are built before the definition is written
        for i in 0..creator.columns.len() {
            if creator.columns[i].complex_desc.is_some() {
                let complex_id = create_complex_column(
                    db,
                    &creator.columns[i],
                    &spec.name,
                    creator.tdef_page_number,
                    &format,
                )?;
                creator.column_states[i].complex_id = complex_id;
                catalog_flags |= crate::complex::CATALOG_FLAGS_HAS_COMPLEX;
            }
        }

        // usage map definition page (also allocates the index root pages)
        create_usage_map_definition_buffer(&mut db.channel, &mut creator, &format)?;

        // write the table definition
        let total_size = table_def_size(&creator, &format);
        let mut buffer = ByteBuf::allocate(total_size.max(format.page_size));
        write_table_definition_header(&creator, &mut buffer, total_size)?;
        write_row_count_definitions(&mut buffer, creator.index_count as usize, &format)?;
        crate::column::write_definitions(
            &creator.columns,
            &mut crate::column::ColumnOffsets::default(),
            &creator.column_states,
            &mut buffer,
            &format,
        )?;
        write_index_data_definitions(&mut db.channel, &creator, &mut buffer, &format)?;
        write_logical_index_definitions(&creator, &mut buffer, &format)?;
        write_col_usage_map_definitions(&creator, &mut buffer, &format)?;
        buffer.put_u8(0xFF)?;
        buffer.put_u8(0xFF)?;
        let wrote = buffer.position();
        buffer.set_limit(wrote)?;
        table::write_table_definition_buffer(
            &mut db.channel,
            &buffer,
            creator.tdef_page_number,
            &mut Vec::new(),
        )?;

        // add the catalog entries
        add_new_table(
            db,
            &spec.name,
            creator.tdef_page_number,
            TYPE_TABLE,
            catalog_flags,
            None,
            None,
        )?;

        // add any table and column properties (`TableCreator.createTable`
        // applies them after the catalog row exists and saves the whole group)
        apply_creation_properties(db, spec, creator.tdef_page_number)?;

        Ok(creator.tdef_page_number)
    })();
    db.channel.finish_write()?;
    result
}

/// `TableCreator.addVersionHistoryColumn`: appends the complex column which
/// keeps the versions of the (single) memo column declared `setAppendOnly`.
fn add_version_history_column(mut columns: Vec<ColumnSpec>) -> Result<Vec<ColumnSpec>> {
    for col in &columns {
        if col.append_only && col.data_type != crate::datatype::DataType::Memo {
            return Err(Error::IllegalArgument(format!(
                "Only memo columns can keep a version history (Column={})",
                col.name
            )));
        }
    }
    let mut memo: Option<String> = None;
    for col in &columns {
        if !col.append_only {
            continue;
        }
        if memo.is_some() {
            return Err(Error::IllegalArgument(
                "Only one memo column of a table can keep a version history, \
                 because every version history column has the same name"
                    .into(),
            ));
        }
        memo = Some(col.name.clone());
    }
    if let Some(memo_name) = memo {
        columns.push(crate::complex::version_history_column_spec(&memo_name));
    }
    Ok(columns)
}

/// `ComplexColumnCreator.create`: creates the flat table of a complex column,
/// adds its `MSysComplexColumns` row and returns the id of that row.
pub(crate) fn create_complex_column(
    db: &mut Database,
    col: &ColumnSpec,
    parent_name: &str,
    parent_id: i32,
    format: &JetFormat,
) -> Result<i32> {
    let desc = col
        .complex_desc
        .as_ref()
        .ok_or_else(|| Error::IllegalState("complex column without a declaration".into()))?;
    let type_table_page = match desc.kind {
        crate::complex::ComplexDataType::MultiValue => {
            let value_type = desc
                .value_type
                .ok_or_else(|| Error::IllegalState("multi-value column without a type".into()))?;
            let type_name = crate::complex::multi_value_type_table_name(value_type)
                .ok_or_else(|| {
                    Error::IllegalArgument(format!(
                        "The database has no type table for a multi-value column of type {} (Column={})",
                        value_type.get_name(),
                        col.name
                    ))
                })?;
            db.table_page(type_name)?
        }
        crate::complex::ComplexDataType::Attachment => {
            db.table_page(crate::complex::ATTACHMENT_TYPE_TABLE)?
        }
        crate::complex::ComplexDataType::VersionHistory => {
            // a version history column is the one kind whose type table is not
            // shared: it is created per column
            let page = create_version_history_type_table(db, desc)?;
            // the flat table copies the columns of the type table, so it has to
            // be loaded (creating a table drops the cached definition)
            db.load_table_by_page(page)?;
            page
        }
        crate::complex::ComplexDataType::Unsupported => {
            return Err(Error::IllegalArgument(format!(
                "Unsupported complex column kind (Column={})",
                col.name
            )))
        }
    };

    let flat_table_name = next_flat_table_name(db, &col.name, format);
    let flat_spec = crate::complex::build_flat_table_spec(
        db,
        parent_name,
        col,
        desc,
        type_table_page,
        flat_table_name,
        format,
    )?;
    let flat_table_page = create_table_internal(
        db,
        &flat_spec,
        crate::complex::CATALOG_FLAGS_FLAT_TABLE,
        true,
    )?;

    let cc_page = db.table_page(crate::complex::COMPLEX_COLUMNS_TABLE)?;
    let values = {
        let table = db
            .tables
            .get(&cc_page)
            .ok_or_else(|| Error::IllegalState("MSysComplexColumns is not loaded".into()))?;
        let mut values = Vec::with_capacity(table.columns.len());
        for c in &table.columns {
            let v = if c.name.eq_ignore_ascii_case(crate::complex::COL_COMPLEX_ID) {
                Value::Null // AUTO_NUMBER
            } else if c.name.eq_ignore_ascii_case(crate::complex::COL_TABLE_ID) {
                Value::Long(parent_id)
            } else if c
                .name
                .eq_ignore_ascii_case(crate::complex::COL_COMPLEX_TYPE_OBJECT_ID)
            {
                Value::Long(type_table_page)
            } else if c
                .name
                .eq_ignore_ascii_case(crate::complex::COL_FLAT_TABLE_ID)
            {
                Value::Long(flat_table_page)
            } else if c.name.eq_ignore_ascii_case(crate::complex::COL_COLUMN_NAME) {
                Value::Text(col.name.clone())
            } else {
                Value::Null
            };
            values.push(v);
        }
        values
    };
    db.add_system_row(crate::complex::COMPLEX_COLUMNS_TABLE, values)?;
    let complex_id = db
        .tables
        .get(&cc_page)
        .map(|t| t.last_long_auto_number)
        .unwrap_or(0);
    Ok(complex_id)
}

/// `ComplexColumnCreator.createVersionHistoryTypeTable`: creates the per
/// column type table of a version history column, which holds one version of
/// the memo column's value and the time it was written.
fn create_version_history_type_table(
    db: &mut Database,
    desc: &crate::complex::ComplexColumnDesc,
) -> Result<i32> {
    let memo_name = desc.memo_column_name.clone().ok_or_else(|| {
        Error::IllegalState("version history column without a memo column".into())
    })?;
    let name = next_version_history_type_table_name(db);
    let spec = TableSpec::new(
        name,
        vec![
            ColumnSpec::new(memo_name, DataType::Memo),
            ColumnSpec::new(crate::complex::MODIFIED_COL_NAME, DataType::ShortDateTime),
        ],
    );
    create_table_internal(db, &spec, crate::complex::CATALOG_FLAGS_TYPE_TABLE, true)
}

/// The name of the type table of a version history column.  Access names it
/// `MSysComplexTypeVH_<random guid>`; the port uses the name the verification
/// harness read back from the Java file, falling back to a deterministic
/// counter.
fn next_version_history_type_table_name(db: &mut Database) -> String {
    if !db.complex_names.is_empty() {
        return db.complex_names.remove(0);
    }
    db.complex_name_counter += 1;
    format!(
        "{}{:032X}",
        crate::complex::VERSION_HISTORY_TYPE_PREFIX,
        db.complex_name_counter
    )
}

/// The name of the flat table of a complex column.  Access names it
/// `f_<random guid>_<column>`; the port uses the name the verification harness
/// read back from the Java file (so the two files can be compared byte for
/// byte), falling back to a deterministic counter.
fn next_flat_table_name(db: &mut Database, col_name: &str, format: &JetFormat) -> String {
    if !db.complex_names.is_empty() {
        return db.complex_names.remove(0);
    }
    db.complex_name_counter += 1;
    crate::complex::truncate(
        &format!(
            "{}{:032X}_{}",
            crate::complex::FLAT_TABLE_PREFIX,
            db.complex_name_counter,
            col_name
        ),
        format.max_table_name_length,
    )
}

/// `TableCreator.createTable`: puts the table builder's properties into the new
/// table's default map and every column builder's properties into the column's
/// map, then saves the whole group once.
fn apply_creation_properties(
    db: &mut Database,
    spec: &TableSpec,
    tdef_page_number: i32,
) -> Result<()> {
    let has_column_props = spec.columns.iter().any(|col| !col.properties.is_empty());
    // `TableCreator.hasVersionHistory`: a table whose memo column keeps a
    // history carries the AppendOnly property too
    let has_version_history = spec.columns.iter().any(|col| col.append_only);
    if spec.properties.is_empty() && !has_column_props && !has_version_history {
        return Ok(());
    }
    let mut maps = db.get_table_properties(tdef_page_number)?;
    // the column maps come first: loading a user table creates the property map
    // of every column (its validator initialization reads the map), in column
    // order, and the table's own ("default") map is created afterwards by
    // `newTable.getProperties()`.  The write order of the blocks follows the
    // order in which the maps were created, so the definition of the new table
    // has to be applied in the same order to stay byte identical.
    for col in &spec.columns {
        if col.properties.is_empty() {
            continue;
        }
        crate::property::put_builder_properties(maps.get_mut(&col.name), &col.properties);
    }
    if !spec.properties.is_empty() {
        crate::property::put_builder_properties(maps.get_default(), &spec.properties);
    }
    if has_version_history {
        // `newTable.getProperties().put(APPEND_ONLY_PROP, BOOLEAN, TRUE)`: put
        // after the builder's properties, so it lands at the end of the map
        maps.get_default().put(crate::property::Property::new(
            crate::property::APPEND_ONLY_PROP,
            DataType::Boolean,
            Value::Bool(true),
        ));
    }
    maps.save(db)
}

fn same_index_data(a: &IndexSpec, b: &IndexSpec) -> bool {
    if a.flags != b.flags || a.columns.len() != b.columns.len() {
        return false;
    }
    a.columns
        .iter()
        .zip(b.columns.iter())
        .all(|(c1, c2)| c1.0 == c2.0 && c1.1 == c2.1)
}

fn validate_new_table_name(db: &Database, name: &str, format: &JetFormat) -> Result<()> {
    if db.table_lookup.contains_key(&name.to_uppercase()) {
        return Err(Error::IllegalArgument(format!(
            "Cannot create table with name of existing table '{}'",
            name
        )));
    }
    if name.is_empty() || name.encode_utf16().count() > format.max_table_name_length {
        return Err(Error::IllegalArgument(format!(
            "Table name '{}' is not valid",
            name
        )));
    }
    Ok(())
}

fn calculate_name_length(name: &str) -> usize {
    2 * name.encode_utf16().count() + 2
}

fn table_def_size(creator: &Creator, format: &JetFormat) -> usize {
    let idx_data_len = creator.index_count as usize
        * (format.size_index_definition + format.size_index_column_block)
        + creator.logical_index_count as usize * format.size_index_info_block;
    let col_umap_len = creator.long_value_columns().len() * 10;
    let mut total = format.size_tdef_header
        + format.size_column_def_block * creator.columns.len()
        + idx_data_len
        + col_umap_len
        + format.size_tdef_trailer;
    for col in &creator.columns {
        total += calculate_name_length(&col.name);
    }
    for idx in &creator.indexes {
        total += calculate_name_length(&idx.name);
    }
    total
}

fn write_table_definition_header(
    creator: &Creator,
    buffer: &mut ByteBuf,
    total_size: usize,
) -> Result<()> {
    table::write_table_page_header(buffer)?;
    buffer.put_i32(total_size as i32)?;
    buffer.put_i32(table::MAGIC_TABLE_NUMBER)?;
    buffer.put_i32(0)?;
    buffer.put_i32(0)?;
    buffer.put_u8(1)?;
    for _ in 0..15 {
        buffer.put_u8(0)?;
    }
    buffer.put_u8(table::TYPE_USER)?;
    buffer.put_i16(creator.columns.len() as i16)?;
    buffer.put_i16(crate::column::count_variable_length(&creator.columns))?;
    buffer.put_i16(creator.columns.len() as i16)?;
    buffer.put_i32(creator.logical_index_count)?;
    buffer.put_i32(creator.index_count)?;
    buffer.put_u8(0)?;
    crate::byteutil::put3_byte_int(buffer, creator.umap_page_number)?;
    buffer.put_u8(1)?;
    crate::byteutil::put3_byte_int(buffer, creator.umap_page_number)?;
    Ok(())
}

fn write_row_count_definitions(
    buffer: &mut ByteBuf,
    idx_count: usize,
    format: &JetFormat,
) -> Result<()> {
    buffer.advance((idx_count * format.size_index_definition) as i64)
}

fn write_index_data_definitions(
    ch: &mut crate::pagechannel::PageChannel,
    creator: &Creator,
    buffer: &mut ByteBuf,
    format: &JetFormat,
) -> Result<()> {
    let columns: Vec<(String, i16)> = creator
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.column_number))
        .collect();
    for state in &creator.index_states {
        let idx = &creator.indexes[state.indexes[0]];
        write_index_data_definition(
            buffer,
            &columns,
            idx,
            state.umap_row_number,
            state.umap_page_number,
            state.root_page_number,
            ch,
            creator.tdef_page_number,
            format,
        )?;
    }
    Ok(())
}

/// `IndexData.writeDefinition`: writes one index data definition block and its
/// empty root page.  Shared by the create path (`IndexData.writeDefinitions`)
/// and the update path (`TableImpl.mutateAddIndexData`).
///
/// `columns` holds the (name, column number) pairs of the table's columns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_index_data_definition(
    buffer: &mut ByteBuf,
    columns: &[(String, i16)],
    idx: &IndexSpec,
    umap_row_number: u8,
    umap_page_number: i32,
    root_page_number: i32,
    ch: &mut crate::pagechannel::PageChannel,
    tdef_page_number: i32,
    format: &JetFormat,
) -> Result<()> {
    buffer.put_i32(MAGIC_INDEX_NUMBER)?;
    for i in 0..MAX_COLUMNS {
        let mut column_number = COLUMN_UNUSED;
        let mut flags = 0u8;
        if i < idx.columns.len() {
            flags = if idx.columns[i].1 {
                ASCENDING_COLUMN_FLAG
            } else {
                0
            };
            column_number = columns
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&idx.columns[i].0))
                .map(|(_, number)| *number)
                .unwrap_or(COLUMN_UNUSED);
            if column_number == COLUMN_UNUSED {
                return Err(Error::IllegalArgument(format!(
                    "Column with name {} not found",
                    idx.columns[i].0
                )));
            }
        }
        buffer.put_i16(column_number)?;
        buffer.put_u8(flags)?;
    }
    buffer.put_u8(umap_row_number)?;
    crate::byteutil::put3_byte_int(buffer, umap_page_number)?;

    // write the empty root index page
    let root_page = crate::index::create_root_page_buffer(ch, tdef_page_number, format)?;
    ch.write_page(&root_page, root_page_number)?;

    buffer.put_i32(root_page_number)?;
    buffer.put_i32(0)?;
    buffer.put_u8(idx.flags)?;
    buffer.put_u8(0)?;
    buffer.advance(4)?;
    Ok(())
}

fn write_logical_index_definitions(
    creator: &Creator,
    buffer: &mut ByteBuf,
    format: &JetFormat,
) -> Result<()> {
    for idx in &creator.indexes {
        let state = creator.index_data_state_by_index(idx.index_number as usize);
        crate::index::IndexImpl::write(
            buffer,
            idx.index_number,
            state.index_data_number,
            idx.index_type,
            None,
        )?;
    }
    for idx in &creator.indexes {
        write_name(buffer, &idx.name, format.charset)?;
    }
    Ok(())
}

impl Creator {
    fn index_data_state_by_index(&self, index_idx: usize) -> &IndexDataState {
        self.index_states
            .iter()
            .find(|s| s.indexes.contains(&index_idx))
            .unwrap()
    }
}

fn write_col_usage_map_definitions(
    creator: &Creator,
    buffer: &mut ByteBuf,
    _format: &JetFormat,
) -> Result<()> {
    for ci in creator.long_value_columns() {
        let col = &creator.columns[ci];
        let state = &creator.column_states[ci];
        crate::column::write_col_usage_map_definition(col, state, buffer)?;
    }
    Ok(())
}

fn create_usage_map_definition_buffer(
    ch: &mut crate::pagechannel::PageChannel,
    creator: &mut Creator,
    format: &JetFormat,
) -> Result<()> {
    let lval_cols = creator.long_value_columns();
    let index_umap_end = 2 + creator.index_count as usize;
    let umap_num = index_umap_end + lval_cols.len() * 2;

    let umap_row_length = format.offset_usage_map_start + format.usage_map_table_byte_length;
    let umap_space_usage = get_row_space_usage(umap_row_length, format);
    let mut umap_page_number = crate::pagechannel::INVALID_PAGE_NUMBER;
    let mut umap_buf: Option<ByteBuf> = None;
    let mut free_space = 0i32;
    let mut row_start = 0usize;
    let mut umap_row_num = 0usize;

    for i in 0..umap_num {
        if umap_buf.is_none() {
            if umap_page_number == crate::pagechannel::INVALID_PAGE_NUMBER {
                umap_page_number = creator.umap_page_number;
            } else {
                umap_page_number = Creator::reserve_page(ch)?;
            }
            free_space = format.data_page_initial_free_space as i32;
            let mut buf = ch.create_page_buffer();
            buf.put_u8(page_types::DATA)?;
            buf.put_u8(0x01)?;
            buf.put_i16(free_space as i16)?;
            buf.put_i32(0)?;
            buf.put_i32(0)?;
            buf.put_i16(0)?;
            row_start = table::find_row_end(&buf, 0, format)? - umap_row_length;
            umap_row_num = 0;
            umap_buf = Some(buf);
        }

        let buf = umap_buf.as_mut().unwrap();
        buf.put_i16_at(
            format.offset_row_start + format.size_row_location * umap_row_num,
            row_start as i16,
        )?;

        if i == 0 {
            buf.put_u8_at(row_start, crate::usagemap::MAP_TYPE_REFERENCE)?;
        } else if i == 1 {
            buf.put_u8_at(row_start, crate::usagemap::MAP_TYPE_INLINE)?;
        } else if i < index_umap_end {
            let index_idx = i - 2;
            let root_page_number = ch.allocate_new_page()?;
            let state = creator
                .index_states
                .iter_mut()
                .find(|s| s.index_data_number == index_idx as i32)
                .unwrap();
            state.root_page_number = root_page_number;
            state.umap_page_number = umap_page_number;
            state.umap_row_number = umap_row_num as u8;
            let buf = umap_buf.as_mut().unwrap();
            buf.put_u8_at(row_start, crate::usagemap::MAP_TYPE_INLINE)?;
            buf.put_i32_at(row_start + 1, root_page_number)?;
            buf.put_u8_at(row_start + 5, 1)?;
        } else {
            let lval_col_idx = (i - index_umap_end) / 2;
            let umap_type = (i - index_umap_end) % 2;
            let ci = lval_cols[lval_col_idx];
            let state = &mut creator.column_states[ci];
            let buf = umap_buf.as_mut().unwrap();
            buf.put_u8_at(row_start, crate::usagemap::MAP_TYPE_INLINE)?;
            if umap_type == 0 {
                state.umap_owned_row_number = umap_row_num as u8;
                state.umap_page_number = umap_page_number;
            } else {
                state.umap_free_row_number = umap_row_num as u8;
            }
        }

        row_start -= umap_row_length;
        free_space -= umap_space_usage as i32;
        umap_row_num += 1;

        if free_space <= umap_space_usage as i32 || i == umap_num - 1 {
            let mut buf = umap_buf.take().unwrap();
            buf.put_i16_at(format.offset_free_space, free_space as i16)?;
            buf.put_i16_at(format.offset_num_rows_on_data_page, umap_row_num as i16)?;
            ch.write_page(&buf, umap_page_number)?;
        }
    }
    Ok(())
}

/// `DatabaseImpl.addNewTable`: adds a newly created table (or linked table) to
/// the internal lookup, the system catalog and the access control entries.
///
/// A linked table has no table definition page, so its `tdef_page_number` is
/// the synthetic object id of its catalog row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_new_table(
    db: &mut Database,
    name: &str,
    tdef_page_number: i32,
    obj_type: i16,
    flags: i32,
    linked_db_name: Option<&str>,
    linked_table_name: Option<&str>,
) -> Result<()> {
    let parent_id = db.table_parent_id;
    add_to_system_catalog(
        db,
        name,
        tdef_page_number,
        obj_type,
        flags,
        linked_db_name,
        linked_table_name,
        parent_id,
    )?;
    add_to_access_control_entries(db, tdef_page_number, parent_id)?;
    // `addTable`: the in-memory lookup gets the object right away
    db.table_lookup
        .insert(name.to_uppercase(), (tdef_page_number, flags));
    if obj_type == TYPE_TABLE {
        db.tables.remove(&tdef_page_number);
    }
    Ok(())
}

/// `DatabaseImpl.addToSystemCatalogFlags`: ORs the given flags into the
/// `MSysObjects` row of an existing object (the table which gets a complex
/// column needs `CATALOG_FLAGS_HAS_COMPLEX`).
pub(crate) fn add_to_system_catalog_flags(
    db: &mut Database,
    object_id: i32,
    flags: i32,
) -> Result<()> {
    let rows = db.read_rows_with_ids(crate::database::TABLE_SYSTEM_CATALOG)?;
    let (idx_id, idx_flags) = {
        let t = db
            .tables
            .get(&crate::database::PAGE_SYSTEM_CATALOG)
            .ok_or_else(|| Error::IllegalState("system catalog is not loaded".into()))?;
        (
            t.column_index("Id")
                .ok_or_else(|| Error::IllegalState("catalog has no Id column".into()))?,
            t.column_index("Flags")
                .ok_or_else(|| Error::IllegalState("catalog has no Flags column".into()))?,
        )
    };
    for (row_id, values) in rows {
        let mut values = match values {
            Some(v) => v,
            None => continue,
        };
        let id = values.get(idx_id).and_then(|v| v.as_i64());
        if id != Some(object_id as i64) {
            continue;
        }
        let old = values.get(idx_flags).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        values[idx_flags] = Value::Long(old | flags);
        let tz = db.tz;
        let table = db
            .tables
            .get_mut(&crate::database::PAGE_SYSTEM_CATALOG)
            .unwrap();
        table.update_row(&mut db.channel, row_id, &values, &tz)?;
        // keep the in-memory lookup flags in sync
        for entry in db.table_lookup.values_mut() {
            if entry.0 == object_id {
                entry.1 = old | flags;
            }
        }
        return Ok(());
    }
    Err(Error::IllegalState(format!(
        "The system catalog has no object with id {}",
        object_id
    )))
}

/// `DatabaseImpl.addToSystemCatalog`: adds a row to `MSysObjects`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_to_system_catalog(
    db: &mut Database,
    name: &str,
    object_id: i32,
    obj_type: i16,
    flags: i32,
    linked_db_name: Option<&str>,
    linked_table_name: Option<&str>,
    parent_id: i32,
) -> Result<()> {
    let owner = db.get_new_object_owner()?;
    let now = db.now_local_datetime();
    let col_names: Vec<String> = {
        let t = db
            .tables
            .get(&crate::database::PAGE_SYSTEM_CATALOG)
            .unwrap();
        t.columns.iter().map(|c| c.name.clone()).collect()
    };
    let mut values: Vec<Value> = Vec::with_capacity(col_names.len());
    for col_name in &col_names {
        let v = match col_name.as_str() {
            "Id" => Value::Long(object_id),
            "Name" => Value::Text(name.to_string()),
            "Type" => Value::Int(obj_type),
            "DateCreate" | "DateUpdate" => Value::DateTime(now),
            "ParentId" => Value::Long(parent_id),
            "Flags" => Value::Long(flags),
            "Owner" => Value::Bytes(owner.clone()),
            "Database" => linked_db_name
                .map(|s| Value::Text(s.to_string()))
                .unwrap_or(Value::Null),
            "ForeignName" => linked_table_name
                .map(|s| Value::Text(s.to_string()))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        };
        values.push(v);
    }
    db.add_system_row(TABLE_SYSTEM_CATALOG, values)
}

/// `DatabaseImpl.addToAccessControlEntries`
pub(crate) fn add_to_access_control_entries(
    db: &mut Database,
    object_id: i32,
    parent_id: i32,
) -> Result<()> {
    let mut sids = collect_new_object_sids(db, parent_id)?;
    if sids.is_empty() {
        sids.push(vec![0xA6, 0x33]);
    }
    let col_names: Vec<String> = {
        let t = db
            .tables
            .get(&crate::database::PAGE_SYSTEM_CATALOG)
            .unwrap();
        let _ = t;
        let ac = db
            .find_table(TABLE_SYSTEM_ACES)?
            .ok_or_else(|| Error::Jackcess("missing MSysACEs".into()))?;
        ac.columns.iter().map(|c| c.name.clone()).collect()
    };
    let mut rows = Vec::new();
    for sid in sids {
        let mut values = Vec::with_capacity(col_names.len());
        for name in &col_names {
            let v = match name.as_str() {
                "ACM" => Value::Long(1048575),
                "FInheritable" => Value::Bool(false),
                "ObjectId" => Value::Long(object_id),
                "SID" => Value::Bytes(sid.clone()),
                _ => Value::Null,
            };
            values.push(v);
        }
        rows.push(values);
    }
    add_system_rows(db, TABLE_SYSTEM_ACES, rows)
}

fn collect_new_object_sids(db: &mut Database, parent_id: i32) -> Result<Vec<Vec<u8>>> {
    let page = match db.table_lookup.get(&TABLE_SYSTEM_ACES.to_uppercase()) {
        Some((p, _)) => *p,
        None => return Ok(Vec::new()),
    };
    let rows = db.read_table_rows(page)?;
    let t = db.tables.get(&page).unwrap();
    let idx_obj = t.column_index("ObjectId");
    let idx_sid = t.column_index("SID");
    let mut sids = Vec::new();
    for row in rows.into_iter().flatten() {
        let obj = idx_obj.and_then(|i| row.get(i)).and_then(|v| v.as_i64());
        if obj == Some(parent_id as i64) {
            if let Some(sid) = idx_sid.and_then(|i| row.get(i)).and_then(|v| v.as_bytes()) {
                sids.push(sid.to_vec());
            }
        }
    }
    Ok(sids)
}

pub(crate) fn add_system_rows(
    db: &mut Database,
    table_name: &str,
    rows: Vec<Vec<Value>>,
) -> Result<()> {
    let page = match db.table_lookup.get(&table_name.to_uppercase()) {
        Some((p, _)) => *p,
        None => {
            return Err(Error::Jackcess(format!(
                "missing system table {}",
                table_name
            )))
        }
    };
    if !db.tables.contains_key(&page) {
        let t = crate::table::Table::read(&mut db.channel, page, table_name, 0, &db.format)?;
        db.tables.insert(page, t);
    }
    let tz = db.tz;
    {
        let t = db.tables.get_mut(&page).unwrap();
        t.add_rows(&mut db.channel, &rows, &tz)?;
    }
    Ok(())
}

/// Writes a full tdef buffer to its pages (`writeTableDefinitionBuffer`).
pub fn write_definition_buffer(
    ch: &mut crate::pagechannel::PageChannel,
    buffer: &ByteBuf,
    tdef_page_number: i32,
) -> Result<()> {
    let format = ch.format.clone();
    let total_size = buffer.limit();
    if total_size <= format.page_size {
        let mut page = buffer.dup();
        page.clear();
        page.put_bytes(&buffer.backing()[..total_size])?;
        page.put_i16_at(
            format.offset_free_space,
            (format.page_size as i32 - total_size as i32 - 8).max(0) as i16,
        )?;
        ch.write_page(&page, tdef_page_number)?;
        Ok(())
    } else {
        crate::table::write_table_definition_buffer(ch, buffer, tdef_page_number, &mut Vec::new())
    }
}

/// Helper used by the writer for indexes: number of the index data state for a
/// logical index number.
pub fn index_data_number_for(creator_states: &[IndexDataState], index_number: i32) -> i32 {
    creator_states
        .iter()
        .find(|s| s.indexes.contains(&(index_number as usize)))
        .map(|s| s.index_data_number)
        .unwrap_or(-1)
}

/// Ensures the `add_data_page_row` import is used (kept for parity with Java).
#[allow(dead_code)]
fn _unused(_: &JetFormat) {
    let _ = add_data_page_row;
    let _ = SYSTEM_OBJECT_FLAGS;
}
