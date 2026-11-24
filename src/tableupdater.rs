//! Port of `com.healthmarketscience.jackcess.impl.TableUpdater` together with
//! the table definition mutation methods of `TableImpl` it drives
//! (`mutateAddColumn`, `mutateAddIndexData`, `mutateAddIndex`,
//! `addUsageMaps`, `populateIndexData`).
//!
//! jackcess 5.0.4 only supports *adding* columns and indexes to an existing
//! table (there is no drop API), so that is all this module implements.
//!
//! A mutation works on the complete table definition buffer: the current
//! definition is loaded from its page(s), the new definition is inserted into
//! it, and the buffer is written back (splitting it over more pages when it no
//! longer fits on one).  `orig_tdef_len` / `added_tdef_len` mirror the Java
//! bookkeeping which validates that the result is exactly as long as the
//! definition said it would grow.

use crate::bytebuf::ByteBuf;
use crate::column::{self, Column, ColumnOffsets, ColumnSpec, ColumnState};
use crate::datatype::DataType;
use crate::error::{Error, Result};
use crate::format::{JetFormat, SortOrder};
use crate::index::{
    ForeignKeyReference, IndexData, IndexImpl, COLUMN_UNUSED, IGNORE_NULLS_INDEX_FLAG, MAX_COLUMNS,
    REQUIRED_INDEX_FLAG, UNIQUE_INDEX_FLAG,
};
use crate::page_types;
use crate::pagechannel::PageChannel;
use crate::table::{self, Table};
use crate::tablecreator::IndexSpec;
use crate::usagemap;

/// `RelationshipCreator.IGNORED_PRIMARY_INDEX_FLAGS`: index flags which can be
/// ignored when looking for a backing index data for a foreign key.
pub const IGNORED_PRIMARY_INDEX_FLAGS: u8 = IGNORE_NULLS_INDEX_FLAG | REQUIRED_INDEX_FLAG;
/// `RelationshipCreator.IGNORED_SECONDARY_INDEX_FLAGS`.
pub const IGNORED_SECONDARY_INDEX_FLAGS: u8 = IGNORED_PRIMARY_INDEX_FLAGS | UNIQUE_INDEX_FLAG;

/// `TableMutator.IndexDataState`, the state of an index data being created.
#[derive(Clone, Copy, Debug, Default)]
struct IndexDataState {
    index_data_number: i32,
    root_page_number: i32,
    umap_page_number: i32,
    umap_row_number: u8,
}

/// `TableUpdater`: the state kept while a table definition is mutated.
#[derive(Default)]
struct Updater {
    orig_tdef_len: usize,
    added_tdef_len: usize,
    next_pages: Vec<i32>,
    col_offsets: ColumnOffsets,
    col_state: Option<ColumnState>,
    idx_state: Option<IndexDataState>,
    /// `TableUpdater._fkReference`, carried by an internally added foreign key
    /// index
    fk_reference: Option<ForeignKeyReference>,
}

impl Updater {
    fn add_tdef_len(&mut self, add: usize) {
        self.added_tdef_len += add;
    }

    fn reset_tdef_info(&mut self) {
        self.added_tdef_len = 0;
        self.orig_tdef_len = 0;
        self.next_pages.clear();
    }

    /// `TableImpl.loadCompleteTableDefinitionBufferForUpdate`: loads the whole
    /// definition, makes room for the new data and stores the new length.
    fn load_for_update(
        &mut self,
        t: &Table,
        ch: &mut PageChannel,
        format: &JetFormat,
    ) -> Result<ByteBuf> {
        let mut table_buffer = ch.create_page_buffer();
        ch.read_page(&mut table_buffer, t.table_def_page_number)?;

        // loadCompleteTableDefinitionBuffer: gather the continuation pages
        self.next_pages.clear();
        let mut next_page = table_buffer.get_i32_at(format.offset_next_table_def_page)?;
        while next_page != 0 {
            self.next_pages.push(next_page);
            let mut next_buffer = ch.create_page_buffer();
            ch.read_page(&mut next_buffer, next_page)?;
            let following = next_buffer.get_i32_at(format.offset_next_table_def_page)?;
            table_buffer = expand_table_buffer(table_buffer, format)?;
            let tail = next_buffer.get_bytes_at(8, format.page_size - 8)?;
            table_buffer.put_bytes(&tail)?;
            table_buffer.flip();
            next_page = following;
        }

        // make sure the table buffer has enough room for the new info
        let orig_tdef_len = table_buffer.get_i32_at(8)? as usize;
        self.orig_tdef_len = orig_tdef_len;
        let new_tdef_len = orig_tdef_len + self.added_tdef_len;
        while new_tdef_len > table_buffer.capacity() {
            table_buffer = expand_table_buffer(table_buffer, format)?;
            table_buffer.flip();
        }
        table_buffer.set_limit(orig_tdef_len)?;
        table_buffer.put_i32_at(8, new_tdef_len as i32)?;
        Ok(table_buffer)
    }

    /// `TableUpdater.validateUpdatedTdef`.
    fn validate_updated_tdef(&self, table_buffer: &ByteBuf) -> Result<()> {
        if self.orig_tdef_len + self.added_tdef_len != table_buffer.limit() {
            return Err(Error::IllegalState(format!(
                "Failed updating table definition (unexpected length {} != {} + {})",
                table_buffer.limit(),
                self.orig_tdef_len,
                self.added_tdef_len
            )));
        }
        Ok(())
    }
}

/// `TableImpl.expandTableBuffer`.
fn expand_table_buffer(mut table_buffer: ByteBuf, format: &JetFormat) -> Result<ByteBuf> {
    let mut new_buffer = ByteBuf::allocate(table_buffer.capacity() + format.page_size - 8);
    new_buffer.set_order(table_buffer.order());
    let remaining = table_buffer.get_bytes(table_buffer.remaining())?;
    new_buffer.put_bytes(&remaining)?;
    Ok(new_buffer)
}

/// `TableImpl.skipNames`.
fn skip_names(table_buffer: &mut ByteBuf, count: usize) -> Result<()> {
    for _ in 0..count {
        let len = table_buffer.get_i16()? as usize;
        table_buffer.advance(len as i64)?;
    }
    Ok(())
}

/// `TableUpdater.getNextColumnId`: Access never reuses a column id, so the
/// next free id is one past the highest one.
fn next_column_id(t: &Table) -> i16 {
    let mut max_id: i16 = -1;
    for col in &t.columns {
        if col.column_id > max_id {
            max_id = col.column_id;
        }
    }
    max_id + 1
}

fn to_lookup_name(name: &str) -> String {
    name.to_uppercase()
}

fn column_names(t: &Table) -> Vec<String> {
    t.columns.iter().map(|c| to_lookup_name(&c.name)).collect()
}

/// `TableUpdater.validateAddColumn` (plus `TableMutator.validateColumn`).
fn validate_add_column(t: &Table, column: &ColumnSpec, format: &JetFormat) -> Result<()> {
    if t.columns.len() + 1 > format.max_columns_per_table {
        return Err(Error::IllegalArgument(format!(
            "Cannot add column to table with {} columns",
            format.max_columns_per_table
        )));
    }
    if column.name.is_empty() || column.name.encode_utf16().count() > format.max_column_name_length
    {
        return Err(Error::IllegalArgument(format!(
            "Column name '{}' is not valid",
            column.name
        )));
    }
    if column.data_type.is_unsupported() {
        return Err(Error::IllegalArgument(format!(
            "Cannot create column with unsupported type {}",
            column.data_type.get_name()
        )));
    }
    // the parts of `TableUpdater.addColumn` which need subsystems the port does
    // not have yet are rejected rather than half-implemented
    if column.data_type == DataType::ComplexType {
        match &column.complex_desc {
            Some(_) => {}
            None => {
                return Err(Error::IllegalArgument(
                    "A complex column is declared with setMultiValue, setAttachment or, for a \
                     version history, setAppendOnly on a memo column"
                        .into(),
                ))
            }
        }
    }
    if column.append_only {
        if column.data_type != DataType::Memo {
            return Err(Error::IllegalArgument(format!(
                "Only memo columns can keep a version history (Column={})",
                column.name
            )));
        }
        // every version history column has the same name, so a table can only
        // hold one (`TableUpdater.validateAddColumn`)
        if column_names(t)
            .iter()
            .any(|n| *n == to_lookup_name(crate::complex::VERSION_HISTORY_COL_NAME))
        {
            return Err(Error::IllegalArgument(
                "Only one memo column of a table can keep a version history, \
                 because every version history column has the same name"
                    .into(),
            ));
        }
    }
    if column_names(t)
        .iter()
        .any(|n| *n == to_lookup_name(&column.name))
    {
        return Err(Error::IllegalArgument(format!(
            "duplicate column name: {}",
            column.name
        )));
    }
    if column.auto_number {
        if !column.data_type.may_be_auto_number() {
            return Err(Error::IllegalArgument(
                "Auto number column must be long integer or guid".into(),
            ));
        }
        // for most autonumber types, there can only be one of each type
        if column.data_type != DataType::ComplexType
            && t.auto_num_columns
                .iter()
                .any(|&ci| t.columns[ci].data_type == column.data_type)
        {
            return Err(Error::IllegalArgument(format!(
                "Can have at most one AutoNumber column of type {} per table",
                column.data_type.get_name()
            )));
        }
    }
    Ok(())
}

/// `TableUpdater.validateAddIndex` (plus `TableMutator.validateIndex`).
fn validate_add_index(t: &Table, index: &IndexSpec, format: &JetFormat) -> Result<()> {
    if t.logical_index_count as usize + 1 > format.max_indexes_per_table {
        return Err(Error::IllegalArgument(format!(
            "Cannot add index to table with {} indexes",
            format.max_indexes_per_table
        )));
    }
    if index.name.is_empty() || index.name.encode_utf16().count() > format.max_index_name_length {
        return Err(Error::IllegalArgument(format!(
            "Index name '{}' is not valid",
            index.name
        )));
    }
    if index.columns.is_empty() {
        return Err(Error::IllegalArgument("index has no columns".into()));
    }
    if index.columns.len() > MAX_COLUMNS {
        return Err(Error::IllegalArgument(format!(
            "index has too many columns, max {}",
            MAX_COLUMNS
        )));
    }
    let col_names = column_names(t);
    let mut idx_col_names: Vec<String> = Vec::new();
    for (name, _) in &index.columns {
        let lookup = to_lookup_name(name);
        if idx_col_names.contains(&lookup) {
            return Err(Error::IllegalArgument(format!(
                "duplicate column name {} in index",
                name
            )));
        }
        if !col_names.contains(&lookup) {
            return Err(Error::IllegalArgument(format!(
                "column named {} not found in table",
                name
            )));
        }
        idx_col_names.push(lookup);
    }
    if t.indexes
        .iter()
        .any(|i| i.name.to_uppercase() == to_lookup_name(&index.name))
    {
        return Err(Error::IllegalArgument(format!(
            "duplicate index name: {}",
            index.name
        )));
    }
    if index.index_type == IndexImpl::PRIMARY_KEY_INDEX_TYPE
        && t.indexes.iter().any(|i| i.is_primary_key())
    {
        return Err(Error::IllegalArgument(format!(
            "found second primary key index: {}",
            index.name
        )));
    }
    if index.index_type == IndexImpl::FOREIGN_KEY_INDEX_TYPE {
        return Err(Error::Unsupported(
            "Foreign key indexes are not supported".into(),
        ));
    }
    Ok(())
}

/// `TableMutator.getColumnNumber`: the number of the named column, matched
/// case insensitively.
fn column_numbers(columns: &[Column]) -> Vec<(String, i16)> {
    columns
        .iter()
        .map(|c| (c.name.clone(), c.column_number))
        .collect()
}

/// `TableImpl.addUsageMaps`: allocates `num_maps` usage map rows, reusing a
/// known usage map page when it has enough free space.
///
/// @return the (page number, first row number) of the new rows
fn add_usage_maps(
    t: &Table,
    ch: &mut PageChannel,
    num_maps: usize,
    first_used_page: Option<i32>,
) -> Result<(i32, i32)> {
    let format = ch.format.clone();
    let umap_row_length = format.offset_usage_map_start + format.usage_map_table_byte_length;
    let total_umap_space_usage = table::get_row_space_usage(umap_row_length, &format) * num_maps;
    let mut umap_page_number = crate::pagechannel::INVALID_PAGE_NUMBER;
    let mut first_row_num: i32 = -1;
    let mut free_space: i32 = 0;

    // search currently known usage map pages for one with enough free space
    // (pages with free space will probably be newer pages, so sort in reverse)
    let mut known_pages: Vec<i32> = Vec::new();
    collect_usage_map_pages(t, &mut known_pages);
    known_pages.sort_unstable();
    known_pages.dedup();
    known_pages.reverse();

    let mut umap_buf = ch.create_page_buffer();
    for &page_num in &known_pages {
        ch.read_page(&mut umap_buf, page_num)?;
        free_space = umap_buf.get_i16_at(format.offset_free_space)? as i32;
        if free_space >= total_umap_space_usage as i32 {
            // found a page!
            umap_page_number = page_num;
            first_row_num = table::get_rows_on_data_page(&umap_buf, &format) as i32;
            break;
        }
    }

    if umap_page_number == crate::pagechannel::INVALID_PAGE_NUMBER {
        // didn't find any existing pages, need to create a new one
        umap_page_number = ch.allocate_new_page()?;
        free_space = format.data_page_initial_free_space as i32;
        first_row_num = 0;
        umap_buf = create_usage_map_def_page(ch, free_space, &format)?;
    }

    // write the actual usage map defs
    let mut row_start = table::find_row_end(&umap_buf, first_row_num as usize, &format)? as i32
        - umap_row_length as i32;
    let mut umap_row_num = first_row_num;
    for _ in 0..num_maps {
        umap_buf.put_i16_at(
            table::get_row_start_offset(umap_row_num as usize, &format),
            row_start as i16,
        )?;
        umap_buf.put_u8_at(row_start as usize, usagemap::MAP_TYPE_INLINE)?;

        let mut data_offset = row_start as usize + 1;
        if let Some(first_used_page) = first_used_page {
            // fill in the first used page of the usage map
            umap_buf.put_i32_at(data_offset, first_used_page)?;
            data_offset += 4;
            umap_buf.put_u8_at(data_offset, 1)?;
            data_offset += 1;
        }
        umap_buf.clear_range(data_offset, (row_start + umap_row_length as i32) as usize)?;

        row_start -= umap_row_length as i32;
        umap_row_num += 1;
    }

    // finish the page
    free_space -= total_umap_space_usage as i32;
    umap_buf.put_i16_at(format.offset_free_space, free_space as i16)?;
    umap_buf.put_i16_at(format.offset_num_rows_on_data_page, umap_row_num as i16)?;
    ch.write_page(&umap_buf, umap_page_number)?;

    Ok((umap_page_number, first_row_num))
}

/// `TableImpl.collectUsageMapPages`: every page holding a usage map of this
/// table (the table's own maps, its index data maps and its long value column
/// maps).
fn collect_usage_map_pages(t: &Table, pages: &mut Vec<i32>) {
    if let Some(p) = &t.owned_pages {
        pages.push(p.get_table_page_number());
    }
    if let Some(p) = &t.free_space_pages {
        pages.push(p.get_table_page_number());
    }
    for idx in &t.index_datas {
        if let Some(p) = &idx.owned_pages {
            pages.push(p.get_table_page_number());
        }
    }
    for (owned, free) in t.column_umaps.values() {
        pages.push(owned.get_table_page_number());
        pages.push(free.get_table_page_number());
    }
}

/// `TableImpl.createUsageMapDefPage`.
fn create_usage_map_def_page(
    ch: &mut PageChannel,
    free_space: i32,
    _format: &JetFormat,
) -> Result<ByteBuf> {
    let mut umap_buf = ch.create_page_buffer();
    umap_buf.put_u8(page_types::DATA)?;
    umap_buf.put_u8(0x01)?; // constant 1 on every page type
    umap_buf.put_i16(free_space as i16)?; // free space in page
    umap_buf.put_i32(0)?; // table definition
    umap_buf.put_i32(0)?; // write stamp, see newDataPage
    umap_buf.put_i16(0)?; // number of records on this page
    Ok(umap_buf)
}

/// `ColumnBuilder.addToTable` / `TableUpdater.addColumn` plus
/// `TableImpl.mutateAddColumn`.
///
/// `db_sort_order` is the database default sort order, which becomes the sort
/// order of a new textual column (`TableMutator.setColumnSortOrder`).
pub fn add_column(
    t: &mut Table,
    ch: &mut PageChannel,
    spec: ColumnSpec,
    db_sort_order: SortOrder,
) -> Result<Column> {
    add_column_with_complex(t, ch, spec, db_sort_order, None)
}

/// [`add_column`] with the id of an already created `MSysComplexColumns` row
/// (`TableUpdater.addColumn` creates the supporting tables before it mutates
/// the table definition).
pub fn add_column_with_complex(
    t: &mut Table,
    ch: &mut PageChannel,
    spec: ColumnSpec,
    db_sort_order: SortOrder,
    complex_id: Option<i32>,
) -> Result<Column> {
    let format = ch.format.clone();
    let mut column = spec;
    // `TableMutator.validateColumn` -> `ColumnBuilder.validate`: a calculated
    // column is checked against the format's calculated types and gets a
    // result type property when it has none
    column.validate_calculated(&format)?;
    validate_add_column(t, &column, &format)?;

    // assign column number and do some assorted column bookkeeping
    column.column_number = t.max_column_count;
    column.column_id = next_column_id(t);
    if column.data_type.is_textual() && column.sort_order.is_none() {
        column.sort_order = Some(db_sort_order);
    }
    let is_var_col = column.is_variable_length();
    let is_long_val = column.data_type.is_long_value();

    // calculate how much more space we need in the table def
    let mut updater = Updater::default();
    if is_long_val {
        updater.add_tdef_len(10);
    }
    updater.add_tdef_len(format.size_column_def_block);
    let name_byte_len = column::calculate_name_length(&column.name);
    updater.add_tdef_len(name_byte_len);
    if is_long_val {
        updater.col_state = Some(ColumnState::default());
    }
    if let Some(complex_id) = complex_id {
        let state = updater.col_state.get_or_insert_with(ColumnState::default);
        state.complex_id = complex_id;
    }

    ch.start_exclusive_write()?;
    let result = (|| -> Result<Column> {
        // load current table definition and add space for new info
        let mut table_buffer = updater.load_for_update(t, ch, &format)?;

        // update various bits of the table def
        table_buffer.set_position(format.offset_max_cols)?;
        table_buffer.put_i16(t.max_column_count + 1)?;
        table_buffer.put_i16(t.max_var_column_count + if is_var_col { 1 } else { 0 })?;
        table_buffer.put_i16(t.columns.len() as i16 + 1)?;

        // move to end of column def blocks
        table_buffer.set_position(
            format.size_tdef_header
                + (t.index_count as usize * format.size_index_definition)
                + (t.columns.len() * format.size_column_def_block),
        )?;

        // figure out the data offsets for the new column
        let mut var_offset: i16 = 0;
        for &ci in &t.var_columns {
            let col = &t.columns[ci];
            if col.is_variable_length() && col.var_len_table_index >= var_offset {
                var_offset = col.var_len_table_index + 1;
            }
        }
        let mut fixed_offset: i16 = 0;
        if !column.is_variable_length() && !column.store_in_null_mask() {
            for col in &t.columns {
                if !col.is_variable_length() && col.fixed_data_offset >= fixed_offset {
                    fixed_offset = col.fixed_data_offset + col.fixed_data_size() as i16;
                }
            }
        }
        updater
            .col_offsets
            .set(fixed_offset, var_offset, var_offset);

        // insert space for the column definition and write it
        let col_def_pos = table_buffer.position();
        table_buffer.insert_empty_data(format.size_column_def_block)?;
        {
            let state = updater.col_state;
            column::write_definition(
                &column,
                state.as_ref(),
                &mut updater.col_offsets,
                &mut table_buffer,
                &format,
            )?;
        }

        // skip existing column names and write new name
        skip_names(&mut table_buffer, t.columns.len())?;
        table_buffer.insert_empty_data(name_byte_len)?;
        column::write_name(&mut table_buffer, &column.name, format.charset)?;

        let mut umap_pos: Option<usize> = None;
        if is_long_val {
            // allocate usage maps for the long value col
            let (umap_page_number, first_row_num) = add_usage_maps(t, ch, 2, None)?;
            let state = updater.col_state.as_mut().unwrap();
            state.umap_page_number = umap_page_number;
            state.umap_owned_row_number = first_row_num as u8;
            state.umap_free_row_number = first_row_num as u8 + 1;

            // skip past index defs
            table_buffer
                .advance((t.index_count as usize * format.size_index_column_block) as i64)?;
            table_buffer
                .advance((t.logical_index_count as usize * format.size_index_info_block) as i64)?;
            skip_names(&mut table_buffer, t.logical_index_count as usize)?;

            // skip existing usage maps
            while table_buffer.remaining() >= 2 {
                if table_buffer.get_i16()? == COLUMN_UNUSED {
                    // found end of tdef, we want to insert before this
                    table_buffer.advance(-2)?;
                    break;
                }
                table_buffer.advance(8)?;
            }

            // write new column usage map info
            umap_pos = Some(table_buffer.position());
            table_buffer.insert_empty_data(10)?;
            column::write_col_usage_map_definition(
                &column,
                updater.col_state.as_ref().unwrap(),
                &mut table_buffer,
            )?;
        }

        // sanity check the updates
        updater.validate_updated_tdef(&table_buffer)?;

        // before writing the new table def, create the column
        let col_type_byte = table_buffer.get_u8_at(col_def_pos)?;
        let mut new_col = Column::read(
            &table_buffer,
            col_def_pos,
            column.name.clone(),
            t.columns.len(),
            &format,
        )?;
        let data_type = DataType::from_byte(col_type_byte).unwrap_or(DataType::UnsupportedVarLen);
        new_col.set_type(data_type, &table_buffer, col_def_pos, &format)?;
        new_col.column_index = t.columns.len();

        // write updated table def back to the database
        table::write_table_definition_buffer(
            ch,
            &table_buffer,
            t.table_def_page_number,
            &mut updater.next_pages,
        )?;

        // now, update the current table
        t.columns.push(new_col.clone());
        t.max_column_count += 1;
        if new_col.is_variable_length() {
            t.var_columns.push(t.columns.len() - 1);
            t.max_var_column_count += 1;
        }
        if new_col.auto_number {
            t.auto_num_columns.push(t.columns.len() - 1);
        }

        if let Some(umap_pos) = umap_pos {
            // read column usage map
            table_buffer.set_position(umap_pos)?;
            t.read_column_usage_maps(ch, &mut table_buffer, &format)?;
        }

        t.mod_count += 1;
        Ok(new_col)
    })();
    let finish = ch.finish_write();
    match result {
        Ok(col) => {
            finish?;
            Ok(col)
        }
        Err(e) => {
            let _ = finish;
            Err(e)
        }
    }
}

/// `TableUpdater.getIndexNames`: the lookup names of every index of the table.
pub fn get_index_names(t: &Table) -> std::collections::HashSet<String> {
    t.indexes.iter().map(|i| to_lookup_name(&i.name)).collect()
}

/// `TableUpdater.initIndexDataState` / `findIndexData` / `sameIndexData`.
///
/// `ignore_idx_flags` / `ignore_col_flags` are the flags which are masked out
/// before comparing an index definition with existing index datas (used by
/// relationship creation, where, for instance, a non-unique foreign key index
/// can share the backing data of an existing unique index).
fn find_same_index_data(
    t: &Table,
    index: &IndexSpec,
    ignore_idx_flags: u8,
    ignore_col_flags: u8,
) -> Option<i32> {
    for data in &t.index_datas {
        if (index.flags | ignore_idx_flags) != (data.index_flags | ignore_idx_flags) {
            continue;
        }
        if index.columns.len() != data.columns.len() {
            continue;
        }
        let same = index
            .columns
            .iter()
            .zip(data.columns.iter())
            .all(|(c1, c2)| {
                let expected_flags = if c1.1 {
                    crate::index::ASCENDING_COLUMN_FLAG
                } else {
                    0
                };
                c1.0 == c2.column.name
                    && (expected_flags | ignore_col_flags) == (c2.flags | ignore_col_flags)
            });
        if same {
            return Some(data.number);
        }
    }
    None
}

/// `TableImpl.populateIndexData`: adds every existing row of the table to the
/// new index data, then writes the table definition (`updateTableDefinition`).
fn populate_index_data(t: &mut Table, ch: &mut PageChannel, format: &JetFormat) -> Result<()> {
    let rows = t.read_all_rows_with_ids(ch)?;
    for (row_id, values) in rows {
        let values = match values {
            Some(values) => values,
            // a deleted row has no index entry
            None => continue,
        };
        let data = t
            .index_datas
            .last_mut()
            .ok_or_else(|| Error::IllegalState("index data missing".into()))?;
        if let Some(pending) = data.prepare_add_row(ch, &values, row_id, format)? {
            data.commit_add_row(ch, pending, false)?;
        }
    }
    t.update_table_definition_ch(ch)?;
    Ok(())
}

/// `TableImpl.mutateAddIndexData`.
fn mutate_add_index_data(
    t: &mut Table,
    ch: &mut PageChannel,
    format: &JetFormat,
    updater: &mut Updater,
    index: &IndexSpec,
) -> Result<()> {
    // calculate how much more space we need in the table def
    updater.add_tdef_len(format.size_index_definition + format.size_index_column_block);

    // load current table definition and add space for new info
    let mut table_buffer = updater.load_for_update(t, ch, format)?;

    // update various bits of the table def
    table_buffer.set_position(format.offset_num_indexes)?;
    table_buffer.put_i32(t.index_count + 1)?;

    // move to end of index data def blocks
    table_buffer.set_position(
        format.size_tdef_header + (t.index_count as usize * format.size_index_definition),
    )?;

    // write index row count definition (empty initially)
    table_buffer.insert_empty_data(format.size_index_definition)?;
    table_buffer.advance(format.size_index_definition as i64)?;

    // skip columns and column names
    table_buffer.advance((t.columns.len() * format.size_column_def_block) as i64)?;
    skip_names(&mut table_buffer, t.columns.len())?;

    // move to end of current index datas
    table_buffer.advance((t.index_count as usize * format.size_index_column_block) as i64)?;

    // allocate usage maps and root page
    let root_page_number = ch.allocate_new_page()?;
    let (umap_page_number, first_row_num) = add_usage_maps(t, ch, 1, Some(root_page_number))?;
    {
        let state = updater
            .idx_state
            .as_mut()
            .ok_or_else(|| Error::IllegalState("index data state missing".into()))?;
        state.root_page_number = root_page_number;
        state.umap_page_number = umap_page_number;
        state.umap_row_number = first_row_num as u8;
    }
    let state = updater.idx_state.unwrap();

    // write index data def
    let idx_data_def_pos = table_buffer.position();
    table_buffer.insert_empty_data(format.size_index_column_block)?;
    crate::tablecreator::write_index_data_definition(
        &mut table_buffer,
        &column_numbers(&t.columns),
        index,
        state.umap_row_number,
        state.umap_page_number,
        state.root_page_number,
        ch,
        t.table_def_page_number,
        format,
    )?;

    // sanity check the updates
    updater.validate_updated_tdef(&table_buffer)?;

    // before writing the new table def, create the index data
    let mut new_idx_data = IndexData::create(&table_buffer, state.index_data_number, format)?;
    new_idx_data.set_tdef_page_number(t.table_def_page_number);
    table_buffer.set_position(idx_data_def_pos)?;
    let columns = t.columns.clone();
    new_idx_data.read(ch, &mut table_buffer, &columns, format)?;

    // write updated table def back to the database
    table::write_table_definition_buffer(
        ch,
        &table_buffer,
        t.table_def_page_number,
        &mut updater.next_pages,
    )?;

    // now, update the current table
    t.index_count += 1;
    t.index_datas.push(new_idx_data);
    t.mod_count += 1;

    // don't forget to populate the new index
    populate_index_data(t, ch, format)
}

/// `TableImpl.mutateAddIndex`.
fn mutate_add_index(
    t: &mut Table,
    ch: &mut PageChannel,
    format: &JetFormat,
    updater: &mut Updater,
    index: &IndexSpec,
) -> Result<()> {
    // calculate how much more space we need in the table def
    updater.add_tdef_len(format.size_index_info_block);
    let name_byte_len = column::calculate_name_length(&index.name);
    updater.add_tdef_len(name_byte_len);

    // load current table definition and add space for new info
    let mut table_buffer = updater.load_for_update(t, ch, format)?;

    // update various bits of the table def
    table_buffer.set_position(format.offset_num_index_slots)?;
    table_buffer.put_i32(t.logical_index_count + 1)?;

    // move to end of index data def blocks
    table_buffer.set_position(
        format.size_tdef_header + (t.index_count as usize * format.size_index_definition),
    )?;

    // skip columns and column names
    table_buffer.advance((t.columns.len() * format.size_column_def_block) as i64)?;
    skip_names(&mut table_buffer, t.columns.len())?;

    // move to end of current index datas
    table_buffer.advance((t.index_count as usize * format.size_index_column_block) as i64)?;
    // move to end of current indexes
    table_buffer.advance((t.logical_index_count as usize * format.size_index_info_block) as i64)?;

    let idx_def_pos = table_buffer.position();
    table_buffer.insert_empty_data(format.size_index_info_block)?;
    let index_data_number = updater
        .idx_state
        .ok_or_else(|| Error::IllegalState("index data state missing".into()))?
        .index_data_number;
    IndexImpl::write(
        &mut table_buffer,
        index.index_number,
        index_data_number,
        index.index_type,
        updater.fk_reference.as_ref(),
    )?;

    // skip existing index names and write new name
    skip_names(&mut table_buffer, t.logical_index_count as usize)?;
    table_buffer.insert_empty_data(name_byte_len)?;
    column::write_name(&mut table_buffer, &index.name, format.charset)?;

    // sanity check the updates
    updater.validate_updated_tdef(&table_buffer)?;

    // before writing the new table def, create the index
    table_buffer.set_position(idx_def_pos)?;
    let mut new_idx = IndexImpl::read(&mut table_buffer, format)?;
    new_idx.name = index.name.clone();

    // write updated table def back to the database
    table::write_table_definition_buffer(
        ch,
        &table_buffer,
        t.table_def_page_number,
        &mut updater.next_pages,
    )?;

    // now, update the current table
    t.logical_index_count += 1;
    if let Some(data) = t
        .index_datas
        .iter_mut()
        .find(|d| d.number == index_data_number)
    {
        data.logical_index_numbers.push(new_idx.index_number);
        data.primary_key |= new_idx.is_primary_key();
    }
    t.indexes.push(new_idx);
    t.mod_count += 1;
    Ok(())
}

/// `IndexBuilder.addToTable` / `TableUpdater.addIndex` plus
/// `TableImpl.mutateAddIndexData` / `TableImpl.mutateAddIndex`.
pub fn add_index(t: &mut Table, ch: &mut PageChannel, spec: IndexSpec) -> Result<IndexImpl> {
    let format = ch.format.clone();
    validate_add_index(t, &spec, &format)?;
    add_index_impl(t, ch, spec, None, 0, 0, false)
}

/// `TableUpdater.addIndex(index, isInternal = true, ignoreIdxFlags,
/// ignoreColFlags)`: the path used while creating the supporting indexes of a
/// relationship.  It skips the validation of the public API (which also
/// refuses foreign key indexes) and expects the caller to already hold the
/// exclusive write, but otherwise behaves exactly like [`add_index`].
pub fn add_index_internal(
    t: &mut Table,
    ch: &mut PageChannel,
    spec: IndexSpec,
    fk_reference: Option<ForeignKeyReference>,
    ignore_idx_flags: u8,
    ignore_col_flags: u8,
) -> Result<IndexImpl> {
    add_index_impl(
        t,
        ch,
        spec,
        fk_reference,
        ignore_idx_flags,
        ignore_col_flags,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn add_index_impl(
    t: &mut Table,
    ch: &mut PageChannel,
    spec: IndexSpec,
    fk_reference: Option<ForeignKeyReference>,
    ignore_idx_flags: u8,
    ignore_col_flags: u8,
    is_internal: bool,
) -> Result<IndexImpl> {
    let format = ch.format.clone();
    let mut index = spec;

    // assign index number and do some assorted index bookkeeping
    index.index_number = t.logical_index_count;

    // initialize backing index state: search for an existing index which
    // matches the given index (in terms of the backing data)
    let index_data_number = find_same_index_data(t, &index, ignore_idx_flags, ignore_col_flags)
        .unwrap_or(t.index_count);
    let mut updater = Updater {
        idx_state: Some(IndexDataState {
            index_data_number,
            root_page_number: 0,
            umap_page_number: 0,
            umap_row_number: 0,
        }),
        fk_reference,
        ..Updater::default()
    };

    if is_internal {
        // this index is part of a larger operation which already holds the
        // exclusive write
        ch.start_write();
    } else {
        ch.start_exclusive_write()?;
    }
    let result = (|| -> Result<IndexImpl> {
        if index_data_number == t.index_count {
            // we need a new backing index data
            mutate_add_index_data(t, ch, &format, &mut updater, &index)?;
            // we need to modify the table def again when adding the Index, so
            // reset the tdef info
            updater.reset_tdef_info();
        }

        mutate_add_index(t, ch, &format, &mut updater, &index)?;

        t.indexes
            .iter()
            .find(|i| i.index_number == index.index_number)
            .cloned()
            .ok_or_else(|| Error::IllegalState("added index missing".into()))
    })();
    let finish = ch.finish_write();
    match result {
        Ok(idx) => {
            finish?;
            Ok(idx)
        }
        Err(e) => {
            let _ = finish;
            Err(e)
        }
    }
}

/// Test scratch directory inside the workspace (the sandbox does not allow
/// writes outside it for spawned test processes).
#[cfg(test)]
fn test_tmp_dir() -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ---------------------------------------------------------------------------
// tests: the byte level effects of adding columns and indexes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::format::FileFormat;
    use crate::tablecreator::{IndexSpec, TableSpec};
    use crate::value::Value;

    fn create_test_db(name: &str) -> (std::path::PathBuf, Database) {
        let path = test_tmp_dir().join(format!(
            "jackcess_addupdate_{}_{}.mdb",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        (path, db)
    }

    /// `T(ID autonumber, Name text(50), Value double)` with a primary key.
    fn make_table(db: &mut Database) {
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(50),
                ColumnSpec::new("Value", DataType::Double),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).expect("create table");
    }

    fn add_rows(db: &mut Database, n: usize) {
        let rows: Vec<Vec<Value>> = (0..n)
            .map(|i| {
                vec![
                    Value::Null,
                    Value::Text(format!("name{}", i)),
                    Value::Double(i as f64 + 0.5),
                ]
            })
            .collect();
        db.add_rows("T", &rows).expect("add rows");
    }

    fn table_page(db: &Database) -> i32 {
        db.table_lookup.get("T").expect("table lookup").0
    }

    fn read_page(db: &mut Database, page_number: i32) -> ByteBuf {
        let mut page = db.channel.create_page_buffer();
        db.channel
            .read_page(&mut page, page_number)
            .expect("read page");
        page
    }

    fn tdef_len(db: &mut Database, tdef_page: i32) -> usize {
        read_page(db, tdef_page).get_i32_at(8).expect("tdef length") as usize
    }

    #[test]
    fn add_column_rewrites_the_definition_and_its_header() {
        let (path, mut db) = create_test_db("addcol");
        make_table(&mut db);
        add_rows(&mut db, 5);
        let tdef_page = table_page(&db);
        let format = db.format.clone();
        let orig_len = tdef_len(&mut db, tdef_page);

        let col = db
            .add_column("T", ColumnSpec::new("Extra", DataType::Long))
            .expect("add column");

        // the definition grows by the column definition block and the name
        let name_len = column::calculate_name_length("Extra");
        let new_len = orig_len + format.size_column_def_block + name_len;
        assert_eq!(tdef_len(&mut db, tdef_page), new_len);

        let page = read_page(&mut db, tdef_page);
        // tdef length, max columns, variable columns, column count
        assert_eq!(page.get_i32_at(8).expect("tdef length") as usize, new_len);
        assert_eq!(
            page.get_i16_at(format.offset_max_cols).expect("max cols"),
            4
        );
        assert_eq!(
            page.get_i16_at(format.offset_num_var_cols)
                .expect("var cols"),
            1
        );
        assert_eq!(page.get_i16_at(format.offset_num_cols).expect("cols"), 4);
        // the page free space is recomputed from the new definition length
        assert_eq!(
            page.get_i16_at(format.offset_free_space)
                .expect("free space"),
            (format.page_size as i32 - new_len as i32 - 8) as i16
        );

        // the new definition block sits right after the existing ones
        let col_def_pos = format.size_tdef_header
            + format.size_index_definition
            + (3 * format.size_column_def_block);
        assert_eq!(
            page.get_u8_at(col_def_pos).expect("type"),
            DataType::Long.value()
        );
        assert_eq!(
            page.get_i16_at(col_def_pos + format.offset_column_number)
                .expect("column number"),
            3
        );
        assert_eq!(
            page.get_i16_at(col_def_pos + format.offset_column_id)
                .expect("column id"),
            3
        );
        assert_eq!(
            page.get_i16_at(col_def_pos + format.offset_column_variable_table_index)
                .expect("var index"),
            1
        );
        assert_eq!(
            page.get_i16_at(col_def_pos + format.offset_column_fixed_data_offset)
                .expect("fixed offset"),
            12
        );
        assert_eq!(
            page.get_u8_at(col_def_pos + format.offset_column_flags)
                .expect("flags"),
            0x03 // updatable | fixed length
        );
        assert_eq!(
            page.get_i16_at(col_def_pos + format.offset_column_length)
                .expect("length"),
            4
        );

        // the returned column is the one now held by the table
        assert_eq!(col.name, "Extra");
        assert_eq!(col.column_number, 3);
        assert_eq!(col.column_id, 3);
        assert_eq!(col.column_index, 3);
        assert_eq!(col.fixed_data_offset, 12);
        let table = db.tables.get(&tdef_page).expect("table loaded");
        assert_eq!(table.columns.len(), 4);
        assert_eq!(table.max_column_count, 4);
        assert_eq!(table.columns[3].name, "Extra");

        // the rows written before the column was added read it as null, and a
        // new row can store a value in it
        let rows = db.read_rows_with_ids("T").expect("rows");
        for (_, values) in &rows {
            assert_eq!(values.as_ref().expect("row")[3], Value::Null);
        }
        db.add_rows(
            "T",
            &[vec![
                Value::Null,
                Value::Text("extra".to_string()),
                Value::Double(9.5),
                Value::Long(42),
            ]],
        )
        .expect("add row");
        let rows = db.read_rows_with_ids("T").expect("rows");
        let last = rows.last().unwrap().1.as_ref().expect("row");
        assert_eq!(last[3], Value::Long(42));
        // ... and the old rows still read as null
        assert_eq!(rows[0].1.as_ref().expect("row")[3], Value::Null);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn add_index_creates_data_root_page_and_populates_it() {
        let (path, mut db) = create_test_db("addindex");
        // a table without any index, so the first add creates its backing data
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(30),
                ColumnSpec::new("Num", DataType::Long),
            ],
        );
        db.create_table(&spec).expect("create table");
        let rows: Vec<Vec<Value>> = (0..5)
            .map(|i| {
                vec![
                    Value::Null,
                    Value::Text(format!("name{}", i)),
                    Value::Long(i),
                ]
            })
            .collect();
        db.add_rows("T", &rows).expect("add rows");
        let tdef_page = table_page(&db);
        let format = db.format.clone();
        let orig_len = tdef_len(&mut db, tdef_page);

        let idx = db
            .add_index(
                "T",
                IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique(),
            )
            .expect("add index");

        // the definition grows by the new index data block and the logical
        // index block plus its name
        let name_len = column::calculate_name_length("ByName");
        let after_data = orig_len + format.size_index_definition + format.size_index_column_block;
        let new_len = after_data + format.size_index_info_block + name_len;
        assert_eq!(tdef_len(&mut db, tdef_page), new_len);

        let page = read_page(&mut db, tdef_page);
        assert_eq!(
            page.get_i32_at(format.offset_num_indexes).expect("indexes"),
            1
        );
        assert_eq!(
            page.get_i32_at(format.offset_num_index_slots)
                .expect("slots"),
            1
        );

        let (data_number, root, unique_count, unique_offset) = {
            let table = db.tables.get(&tdef_page).expect("table loaded");
            assert_eq!(table.index_datas.len(), 1);
            assert_eq!(table.indexes.len(), 1);
            assert_eq!(table.index_count, 1);
            assert_eq!(table.logical_index_count, 1);
            assert_eq!(table.indexes[0].name, "ByName");
            assert_eq!(table.indexes[0].index_number, 0);
            assert_eq!(table.indexes[0].index_data_number, 0);
            assert_eq!(table.columns.len(), 3); // no columns changed
            (
                table.index_datas[0].number,
                table.index_datas[0].root_page_number,
                table.index_datas[0].unique_entry_count,
                table.index_datas[0].unique_entry_count_offset,
            )
        };
        assert_eq!(idx.name, "ByName");
        assert_eq!(data_number, 0);
        // the existing rows were added to the new index
        assert_eq!(unique_count, 5);
        assert_eq!(
            page.get_i32_at(unique_offset).expect("unique entry count"),
            5
        );

        // the empty root page was written and is owned by the index data
        let root_page = read_page(&mut db, root);
        assert_eq!(root_page.get_u8_at(0).expect("page type"), page_types::LEAF);
        assert_eq!(root_page.get_i32_at(4).expect("tdef page"), tdef_page);
        // populating the index moved entries onto the root page, which shrank
        // its free space below the empty-page value
        let empty_free = (format.page_size
            - format.offset_index_entry_mask
            - format.size_index_entry_mask) as i16;
        let free = root_page
            .get_i16_at(format.offset_free_space)
            .expect("free space");
        assert!(free > 0, "root page free space {}", free);
        assert!(
            free < empty_free,
            "root page still empty ({} >= {})",
            free,
            empty_free
        );
        let owns_root = {
            let table = db.tables.get(&tdef_page).expect("table loaded");
            table.index_datas[0]
                .owned_pages
                .as_ref()
                .expect("owned pages")
                .contains_page_number(root)
        };
        assert!(owns_root, "index data does not own its root page");

        // adding an index over the same column with the same flags reuses the
        // backing index data: only the logical index is appended
        let prev_len = tdef_len(&mut db, tdef_page);
        let copy_name_len = column::calculate_name_length("ByNameCopy");
        db.add_index(
            "T",
            IndexSpec::new("ByNameCopy", vec![("Name".to_string(), true)]).unique(),
        )
        .expect("add second index");
        assert_eq!(
            tdef_len(&mut db, tdef_page),
            prev_len + format.size_index_info_block + copy_name_len
        );
        {
            let table = db.tables.get(&tdef_page).expect("table loaded");
            assert_eq!(table.index_count, 1);
            assert_eq!(table.logical_index_count, 2);
            assert_eq!(table.indexes.len(), 2);
            assert_eq!(table.indexes[1].index_data_number, 0);
            // no new entries: the rows did not change
            assert_eq!(table.index_datas[0].unique_entry_count, 5);
        }

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn add_column_grows_the_definition_onto_more_pages() {
        let (path, mut db) = create_test_db("addmany");
        make_table(&mut db);
        add_rows(&mut db, 3);
        let tdef_page = table_page(&db);
        let format = db.format.clone();
        let page_size = format.page_size;
        let mut expected_len = tdef_len(&mut db, tdef_page);

        // add enough LONG columns to push the definition past one page
        let mut added = 0;
        while expected_len <= page_size + 200 {
            let name = format!("C{:03}", added);
            db.add_column("T", ColumnSpec::new(name.clone(), DataType::Long))
                .unwrap_or_else(|e| panic!("add column {}: {}", name, e));
            expected_len += format.size_column_def_block + column::calculate_name_length(&name);
            added += 1;
        }
        assert!(added > 50, "not enough columns added: {}", added);
        assert_eq!(tdef_len(&mut db, tdef_page), expected_len);
        assert!(expected_len > page_size);

        // the definition really spilled onto a continuation page, which the
        // table definition points at
        let page = read_page(&mut db, tdef_page);
        let next_page = page
            .get_i32_at(format.offset_next_table_def_page)
            .expect("next tdef page");
        assert_ne!(next_page, 0);
        let next = read_page(&mut db, next_page);
        assert_eq!(next.get_u8_at(0).expect("page type"), page_types::TABLE_DEF);

        // the in-memory table is consistent and the rows still read back
        {
            let table = db.tables.get(&tdef_page).expect("table loaded");
            assert_eq!(table.columns.len(), 3 + added);
            assert_eq!(table.max_column_count as usize, 3 + added);
            assert_eq!(
                table.columns.last().unwrap().name,
                format!("C{:03}", added - 1)
            );
        }
        let rows = db.read_rows_with_ids("T").expect("rows");
        assert_eq!(rows.len(), 3);
        for (_, values) in &rows {
            let values = values.as_ref().expect("row");
            assert_eq!(values.len(), 3 + added);
            assert_eq!(values[3 + added - 1], Value::Null);
        }

        // the database re-reads the multi-page definition from disk
        db.flush().expect("flush");
        drop(db);
        let mut db = Database::open_path(&path).expect("reopen database");
        {
            let table = db.get_table("T").expect("reopen table");
            assert_eq!(table.columns.len(), 3 + added);
            assert_eq!(table.max_column_count as usize, 3 + added);
        }
        let rows = db.read_rows_with_ids("T").expect("rows after reopen");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].1.as_ref().expect("row")[1],
            Value::Text("name0".to_string())
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
