//! Port of `com.healthmarketscience.jackcess.impl.UsageMap`.
//!
//! A usage map is a bitmap of page numbers held in a row of a table definition
//! page (inline) or in separate "usage map" pages referenced from such a row.

use crate::bitset::BitSet;
use crate::bytebuf::ByteBuf;
use crate::error::{Error, Result};
use crate::format::JetFormat;
use crate::page_types;
use crate::pagechannel::PageChannel;

pub const MAP_TYPE_INLINE: u8 = 0x0;
pub const MAP_TYPE_REFERENCE: u8 = 0x1;

/// the size of the usage map pointer in a table definition
pub const POINTER_SIZE: usize = 4;

const INVALID_BIT_INDEX: i32 = -1;

#[derive(Clone, Debug)]
pub struct UsageMap {
    pub format: JetFormat,
    /// page number of the page holding this map's declaration row
    pub table_page_num: i32,
    /// offset of the declaration row within that page
    pub row_start: usize,
    /// offset within `table_buffer` at which the map data starts
    pub start_offset: usize,
    pub start_page: i32,
    pub end_page: i32,
    pub page_numbers: BitSet,
    pub map_type: u8,
    pub is_global: bool,
    /// buffer of the declaration page, limited to the row end
    pub table_buffer: ByteBuf,
    pub mod_count: i32,
    pub allocating_page: bool,
    pub pending_page: Option<i32>,
}

impl UsageMap {
    pub fn new(format: JetFormat) -> UsageMap {
        UsageMap {
            table_buffer: ByteBuf::allocate(format.page_size),
            format,
            table_page_num: -1,
            row_start: 0,
            start_offset: 0,
            start_page: 0,
            end_page: 0,
            page_numbers: BitSet::new(),
            map_type: MAP_TYPE_INLINE,
            is_global: false,
            mod_count: 0,
            allocating_page: false,
            pending_page: None,
        }
    }

    fn find_row_start(buf: &ByteBuf, row_num: usize, format: &JetFormat) -> Result<usize> {
        let off = format.offset_row_start + format.size_row_location * row_num;
        Ok((buf.get_u16_at(off)? & 0x1FFF) as usize)
    }

    fn find_row_end(buf: &ByteBuf, row_num: usize, format: &JetFormat) -> Result<usize> {
        if row_num == 0 {
            Ok(format.page_size)
        } else {
            let off = format.offset_row_start + format.size_row_location * (row_num - 1);
            Ok((buf.get_u16_at(off)? & 0x1FFF) as usize)
        }
    }

    /// `UsageMap.read(database, tableBuffer)` - consumes the 4 byte pointer
    /// from the table definition buffer and loads the referenced map.
    pub fn read_ptr(ch: &mut PageChannel, buf: &mut ByteBuf) -> Result<UsageMap> {
        if buf.remaining() < POINTER_SIZE {
            return Err(Error::Jackcess(format!(
                "Table definition ends inside a usage map pointer, {} bytes left",
                buf.remaining()
            )));
        }
        let row_num = buf.get_u8()? as usize;
        let page_num = crate::byteutil::get3_byte_int(buf)?;
        UsageMap::read(ch, page_num, row_num, false)
    }

    /// `UsageMap.read(database, pageNum, rowNum, isGlobal)`
    pub fn read(
        ch: &mut PageChannel,
        page_num: i32,
        row_num: usize,
        is_global: bool,
    ) -> Result<UsageMap> {
        let format = ch.format.clone();
        if page_num <= 0 {
            return Err(Error::Jackcess(format!(
                "Invalid usage map page number {}",
                page_num
            )));
        }
        if format.offset_row_start + format.size_row_location * row_num + format.size_row_location
            > format.page_size
        {
            return Err(Error::Jackcess(format!(
                "Invalid usage map row number {} on page {}",
                row_num, page_num
            )));
        }
        let mut table_buffer = ch.create_page_buffer();
        ch.read_page(&mut table_buffer, page_num)?;

        let row_start = Self::find_row_start(&table_buffer, row_num, &format)?;
        let row_end = Self::find_row_end(&table_buffer, row_num, &format)?;
        if row_start >= row_end
            || row_end > format.page_size
            || (row_end - row_start) < format.offset_usage_map_start
        {
            return Err(Error::Jackcess(format!(
                "Invalid usage map row {} on page {}, {} to {} is not a usage map within the page",
                row_num, page_num, row_start, row_end
            )));
        }
        table_buffer.set_limit(row_end)?;

        let map_type = table_buffer.get_u8_at(row_start)?;
        let mut rtn = UsageMap {
            table_buffer,
            format,
            table_page_num: page_num,
            row_start,
            start_offset: row_start,
            start_page: 0,
            end_page: 0,
            page_numbers: BitSet::new(),
            map_type,
            is_global,
            mod_count: 0,
            allocating_page: false,
            pending_page: None,
        };
        rtn.start_offset = rtn.row_start + rtn.format.offset_usage_map_start;
        rtn.init_handler(ch)?;
        Ok(rtn)
    }

    fn init_handler(&mut self, ch: &mut PageChannel) -> Result<()> {
        match self.map_type {
            MAP_TYPE_INLINE => {
                let max_inline_pages = (self.get_row_end() - self.get_inline_data_start()) * 8;
                let start_page = self.table_buffer.get_i32_at(self.row_start + 1)?;
                self.set_page_range(start_page, start_page + max_inline_pages as i32);
                self.table_buffer
                    .set_position(self.get_inline_data_start())?;
                let bytes = self
                    .table_buffer
                    .get_bytes(self.get_row_end() - self.get_inline_data_start())?;
                self.process_map_bytes(&bytes, 0)?;
            }
            MAP_TYPE_REFERENCE => self.reference_init(ch)?,
            _ => {
                return Err(Error::Jackcess(format!(
                    "Unrecognized map type: {}",
                    self.map_type
                )))
            }
        }
        Ok(())
    }

    fn reference_init(&mut self, ch: &mut PageChannel) -> Result<()> {
        let format = self.format.clone();
        self.start_offset = format.offset_usage_map_page_data;
        let max_pages_per_page =
            ((format.page_size - format.offset_usage_map_page_data) * 8) as i32;
        let num_usage_pages = (self.get_row_end() - self.row_start - 1) / 4;
        self.set_page_range(0, num_usage_pages as i32 * max_pages_per_page);
        for i in 0..num_usage_pages {
            let off = self.calculate_map_page_pointer_offset(i);
            let map_page_num = self.table_buffer.get_i32_at(off)?;
            if map_page_num > 0 {
                let mut buf = ch.create_page_buffer();
                ch.read_page(&mut buf, map_page_num)?;
                let page_type = buf.get_u8_at(0)?;
                if page_type != page_types::USAGE_MAP {
                    return Err(Error::Jackcess(format!(
                        "Looking for usage map at page {}, but page type is {}",
                        map_page_num, page_type
                    )));
                }
                let bytes = buf.get_bytes_at(
                    format.offset_usage_map_page_data,
                    format.page_size - format.offset_usage_map_page_data,
                )?;
                self.process_map_bytes(&bytes, max_pages_per_page * i as i32)?;
            }
        }
        Ok(())
    }

    fn process_map_bytes(&mut self, bytes: &[u8], buffer_start_page: i32) -> Result<()> {
        for (byte_count, &b) in bytes.iter().enumerate() {
            if b != 0 {
                for i in 0..8 {
                    if (b & (1 << i)) != 0 {
                        let page_number_offset = (byte_count as i32 * 8 + i) + buffer_start_page;
                        let page_number = self.bit_index_to_page_number(page_number_offset, -1);
                        if !self.is_page_within_range(page_number) {
                            return Err(Error::Jackcess(format!(
                                "found page number {} in usage map outside of expected range {} to {}",
                                page_number, self.start_page, self.end_page
                            )));
                        }
                        self.page_numbers.set(page_number_offset as usize);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn get_row_start(&self) -> usize {
        self.row_start
    }

    pub fn get_row_end(&self) -> usize {
        self.table_buffer.limit()
    }

    pub fn get_inline_data_start(&self) -> usize {
        self.row_start + self.format.offset_usage_map_start
    }

    pub fn get_table_page_number(&self) -> i32 {
        self.table_page_num
    }

    pub fn set_page_range(&mut self, start: i32, end: i32) {
        self.start_page = start;
        self.end_page = end;
    }

    pub fn is_page_within_range(&self, page: i32) -> bool {
        page >= self.start_page && page < self.end_page
    }

    pub fn bit_index_to_page_number(&self, bit_index: i32, invalid: i32) -> i32 {
        if bit_index >= 0 {
            self.start_page + bit_index
        } else {
            invalid
        }
    }

    pub fn page_number_to_bit_index(&self, page: i32) -> i32 {
        if page >= 0 {
            page - self.start_page
        } else {
            INVALID_BIT_INDEX
        }
    }

    pub fn get_first_page_number(&self) -> i32 {
        let b = self.page_numbers.next_set_bit(0);
        self.bit_index_to_page_number(b as i32, page_types::LAST_PAGE_NUMBER)
    }

    pub fn get_next_page_number(&self, cur_page: i32) -> i32 {
        let idx = (self.page_number_to_bit_index(cur_page) + 1).max(0) as usize;
        let b = self.page_numbers.next_set_bit(idx);
        self.bit_index_to_page_number(b as i32, page_types::LAST_PAGE_NUMBER)
    }

    pub fn get_last_page_number(&self) -> i32 {
        let b = self.page_numbers.prev_set_bit(self.page_numbers.length());
        self.bit_index_to_page_number(b as i32, page_types::FIRST_PAGE_NUMBER)
    }

    pub fn get_prev_page_number(&self, cur_page: i32) -> i32 {
        let idx = self.page_number_to_bit_index(cur_page).max(0) as usize;
        let b = self.page_numbers.prev_set_bit(idx);
        self.bit_index_to_page_number(b as i32, page_types::FIRST_PAGE_NUMBER)
    }

    fn clear_table_and_pages(&mut self) -> Result<()> {
        self.page_numbers.clear();
        self.start_page = 0;
        self.end_page = 0;
        self.mod_count += 1;
        let table_start = self.row_start + 1;
        let table_end = self.get_row_end();
        self.table_buffer.clear_range(table_start, table_end)?;
        Ok(())
    }

    fn write_table(&mut self, ch: &mut PageChannel) -> Result<()> {
        ch.write_page_at(&self.table_buffer, self.table_page_num, self.row_start)
    }

    pub fn contains_page_number(&self, page: i32) -> bool {
        if self.is_global {
            // Java throws UnsupportedOperationException for the global map
            return false;
        }
        self.is_page_within_range(page)
            && self
                .page_numbers
                .get(self.page_number_to_bit_index(page).max(0) as usize)
    }

    pub fn get_next_page_number_from(&self, from_page: i32) -> i32 {
        let mut from_index = self.page_number_to_bit_index(from_page);
        if from_index < 0 {
            from_index = 0;
        }
        let b = self.page_numbers.next_set_bit(from_index as usize);
        self.bit_index_to_page_number(b as i32, -1)
    }

    pub fn holds_page_number(&self, page: i32) -> bool {
        let index = self.page_number_to_bit_index(page);
        index >= 0 && self.page_numbers.get(index as usize)
    }

    pub fn page_count(&self) -> usize {
        self.page_numbers.cardinality()
    }

    pub fn add_page_number(&mut self, ch: &mut PageChannel, page: i32) -> Result<()> {
        self.mod_count += 1;
        self.add_or_remove(ch, page, true, false)
    }

    pub fn remove_page_number(&mut self, ch: &mut PageChannel, page: i32) -> Result<()> {
        self.mod_count += 1;
        self.add_or_remove(ch, page, false, true)
    }

    /// `removePageNumber(page, force)`
    pub fn remove_page_number_forced(
        &mut self,
        ch: &mut PageChannel,
        page: i32,
        force: bool,
    ) -> Result<()> {
        self.mod_count += 1;
        self.add_or_remove(ch, page, false, force)
    }

    fn add_or_remove(
        &mut self,
        ch: &mut PageChannel,
        page: i32,
        add: bool,
        force: bool,
    ) -> Result<()> {
        let mut new_map_page: Option<i32> = None;
        match self.map_type {
            MAP_TYPE_INLINE => {
                self.inline_add_or_remove(ch, page, add, force)?;
            }
            MAP_TYPE_REFERENCE => {
                new_map_page = self.reference_add_or_remove(ch, page, add, force)?;
            }
            _ => {
                return Err(Error::Jackcess(format!(
                    "Unrecognized map type: {}",
                    self.map_type
                )))
            }
        }
        if self.is_global {
            if let Some(p) = new_map_page {
                self.add_or_remove(ch, p, false, true)?;
            }
        }
        Ok(())
    }

    fn inline_add_or_remove(
        &mut self,
        ch: &mut PageChannel,
        page: i32,
        add: bool,
        force: bool,
    ) -> Result<()> {
        if self.is_page_within_range(page) {
            let rel = self.page_number_to_bit_index(page);
            self.update_map_bytes(page, rel, add, force)?;
            self.write_table(ch)?;
            Ok(())
        } else {
            self.inline_add_or_remove_outside_range(ch, page, add, force)
        }
    }

    fn inline_add_or_remove_outside_range(
        &mut self,
        ch: &mut PageChannel,
        page: i32,
        add: bool,
        force: bool,
    ) -> Result<()> {
        if self.is_global {
            if !add {
                self.promote_global_inline_to_reference(ch, page)?;
            }
            return Ok(());
        }
        if add {
            let mut first = self.get_first_page_number();
            let mut last = self.get_last_page_number();
            if first <= -1 {
                first = page;
                last = page;
            } else if page > last {
                last = page;
            } else {
                first = page;
            }
            first = (first / 8) * 8;
            let max_inline_pages = (self.get_row_end() - self.get_inline_data_start()) * 8;
            if (last - first + 1) < max_inline_pages as i32 {
                self.move_to_new_start_page(ch, first, page)
            } else {
                self.promote_inline_to_reference(ch, page)
            }
        } else if !force {
            Err(Error::Jackcess(format!(
                "Page number {} already removed from usage map, expected range {} to {}",
                page, self.start_page, self.end_page
            )))
        } else {
            Ok(())
        }
    }

    fn move_to_new_start_page(
        &mut self,
        ch: &mut PageChannel,
        new_start_page: i32,
        new_page_number: i32,
    ) -> Result<()> {
        let old_start_page = self.start_page;
        let old_page_numbers = self.page_numbers.clone();
        self.clear_table_and_pages()?;
        self.table_buffer
            .put_i32_at(self.row_start + 1, new_start_page)?;
        self.write_table(ch)?;
        let max_inline_pages = (self.get_row_end() - self.get_inline_data_start()) * 8;
        self.set_page_range(new_start_page, new_start_page + max_inline_pages as i32);
        self.re_add_pages(ch, old_start_page, &old_page_numbers, new_page_number)
    }

    fn promote_inline_to_reference(&mut self, ch: &mut PageChannel, page: i32) -> Result<()> {
        let old_start = self.start_page;
        let old_pages = self.page_numbers.clone();
        self.clear_table_and_pages()?;
        self.table_buffer
            .put_u8_at(self.row_start, MAP_TYPE_REFERENCE)?;
        self.map_type = MAP_TYPE_REFERENCE;
        self.write_table(ch)?;
        self.reference_init(ch)?;
        self.re_add_pages(ch, old_start, &old_pages, page)
    }

    fn promote_global_inline_to_reference(
        &mut self,
        ch: &mut PageChannel,
        frontier: i32,
    ) -> Result<()> {
        self.clear_table_and_pages()?;
        self.table_buffer
            .put_u8_at(self.row_start, MAP_TYPE_REFERENCE)?;
        self.map_type = MAP_TYPE_REFERENCE;
        self.write_table(ch)?;
        self.reference_init(ch)?;
        for p in 0..=frontier {
            self.add_or_remove(ch, p, false, true)?;
        }
        Ok(())
    }

    fn re_add_pages(
        &mut self,
        ch: &mut PageChannel,
        old_start: i32,
        old_pages: &BitSet,
        new_page_number: i32,
    ) -> Result<()> {
        let mut pages: Vec<i32> = old_pages.iter().map(|i| old_start + i as i32).collect();
        pages.sort_unstable();
        for p in pages {
            self.add_page_number(ch, p)?;
        }
        if new_page_number > -1 {
            self.add_page_number(ch, new_page_number)?;
        }
        Ok(())
    }

    fn calculate_map_page_pointer_offset(&self, page_index: usize) -> usize {
        self.row_start + self.format.offset_reference_map_page_numbers + page_index * 4
    }

    fn reference_add_or_remove(
        &mut self,
        ch: &mut PageChannel,
        page: i32,
        add: bool,
        force: bool,
    ) -> Result<Option<i32>> {
        let max_pages_per_page =
            ((self.format.page_size - self.format.offset_usage_map_page_data) * 8) as i32;

        if !self.is_page_within_range(page) {
            if force {
                return Ok(None);
            }
            return Err(Error::Jackcess(format!(
                "Page number {} is out of supported range",
                page
            )));
        }
        let page_index = (page / max_pages_per_page) as usize;
        let off = self.calculate_map_page_pointer_offset(page_index);
        let map_page_num = self.table_buffer.get_i32_at(off)?;
        let mut new_page = None;
        let mut map_page_buffer = if map_page_num > 0 {
            let mut b = ch.create_page_buffer();
            ch.read_page(&mut b, map_page_num)?;
            b
        } else {
            let (buf, p) = self.allocate_new_usage_map_page(ch, page_index)?;
            new_page = Some(p);
            buf
        };
        self.update_map_map_page(
            page,
            page - max_pages_per_page * page_index as i32,
            add,
            force,
            &mut map_page_buffer,
        )?;
        let written_page = match new_page {
            Some(p) => p,
            None => map_page_num,
        };
        ch.write_page(&map_page_buffer, written_page)?;
        Ok(new_page)
    }

    fn allocate_new_usage_map_page(
        &mut self,
        ch: &mut PageChannel,
        page_index: usize,
    ) -> Result<(ByteBuf, i32)> {
        let format = self.format.clone();
        let page = if self.is_global {
            ch.allocate_new_page_internal()?
        } else {
            ch.allocate_new_page()?
        };
        let mut buf = ch.create_page_buffer();
        buf.put_u8(page_types::USAGE_MAP)?;
        buf.put_u8(0x01)?;
        buf.put_u16(0)?;
        if self.is_global {
            let data_start = format.offset_usage_map_page_data;
            // `ByteUtil.fillRange(buffer, dataStart, PAGE_SIZE - dataStart)`:
            // jackcess passes the *length* where `fillRange` expects the end
            // index, so the last `dataStart` bytes of the page stay zero (a
            // Java quirk which shows once the global map is a reference page)
            buf.put_range(data_start, format.page_size - data_start, 0xff)?;
            let per_page = (format.page_size - data_start) * 8;
            self.page_numbers
                .set_range(page_index * per_page, (page_index + 1) * per_page);
        }
        self.table_buffer
            .put_i32_at(self.calculate_map_page_pointer_offset(page_index), page)?;
        self.write_table(ch)?;
        Ok((buf, page))
    }

    fn update_map_bytes(
        &mut self,
        absolute_page: i32,
        relative_page: i32,
        add: bool,
        force: bool,
    ) -> Result<()> {
        let offset = self.start_offset + (relative_page / 8) as usize;
        let bitmask = 1u8 << (relative_page % 8);
        let mut b = self.table_buffer.get_u8_at(offset)?;
        let page_number_offset = self.page_number_to_bit_index(absolute_page);
        let is_on = self.page_numbers.get(page_number_offset.max(0) as usize);
        if is_on == add && !force {
            return Err(Error::Jackcess(format!(
                "Page number {} already {} usage map, expected range {} to {}",
                absolute_page,
                if add { "added to" } else { "removed from" },
                self.start_page,
                self.end_page
            )));
        }
        if add {
            b |= bitmask;
            self.page_numbers.set(page_number_offset.max(0) as usize);
        } else {
            b &= !bitmask;
            self.page_numbers
                .clear_bit(page_number_offset.max(0) as usize);
        }
        self.table_buffer.put_u8_at(offset, b)?;
        Ok(())
    }

    fn update_map_map_page(
        &mut self,
        absolute_page: i32,
        relative_page: i32,
        add: bool,
        force: bool,
        buf: &mut ByteBuf,
    ) -> Result<()> {
        let offset = self.start_offset + (relative_page / 8) as usize;
        let bitmask = 1u8 << (relative_page % 8);
        let mut b = buf.get_u8_at(offset)?;
        let page_number_offset = self.page_number_to_bit_index(absolute_page);
        let is_on = self.page_numbers.get(page_number_offset.max(0) as usize);
        if is_on == add && !force {
            return Err(Error::Jackcess(format!(
                "Page number {} already {} usage map, expected range {} to {}",
                absolute_page,
                if add { "added to" } else { "removed from" },
                self.start_page,
                self.end_page
            )));
        }
        if add {
            b |= bitmask;
            self.page_numbers.set(page_number_offset.max(0) as usize);
        } else {
            b &= !bitmask;
            self.page_numbers
                .clear_bit(page_number_offset.max(0) as usize);
        }
        buf.put_u8_at(offset, b)?;
        Ok(())
    }

    pub fn iter_pages(&self) -> impl Iterator<Item = i32> + '_ {
        let start = self.start_page;
        self.page_numbers.iter().map(move |b| start + b as i32)
    }
}
