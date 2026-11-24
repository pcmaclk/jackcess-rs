//! Port of `com.healthmarketscience.jackcess.impl.IndexPageCache`.
//!
//! The page cache owns the in-memory representation of the index B-tree and
//! writes modified index pages back to the database.  It mirrors the Java
//! implementation closely, including the deferred writes, the root nesting,
//! page splitting and the child-tail bookkeeping.

use std::collections::HashMap;

use crate::bytebuf::ByteBuf;
use crate::error::{Error, Result};
use crate::format::JetFormat;
use crate::index::{
    calc_max_page_entry_size, missing_index_to_insertion_point, read_data_page, write_data_page,
    DataPage, Entry, INVALID_INDEX_PAGE_NUMBER,
};
use crate::pagechannel::PageChannel;
use crate::usagemap::UsageMap;

/// the type of an entry update
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateType {
    Add,
    Remove,
    Replace,
}

/// max number of pages to cache (unless a write operation is in progress)
pub const MAX_CACHE_SIZE: usize = 25;

/// A cached index page: `DataPageMain` + `DataPageExtra` + the `CacheDataPage`
/// view of them.
#[derive(Clone, Debug)]
struct CachePage {
    data: DataPage,
    /// page number of this page's parent (`None` if not yet resolved)
    parent_page_number: Option<i32>,
    /// whether this page is the tail child of its parent
    tail: bool,
    modified: bool,
}

/// Manages the index pages of an [`crate::index::IndexData`].
#[derive(Clone, Debug)]
pub struct IndexPageCache {
    pub root_page_number: i32,
    pub owned_pages: Option<UsageMap>,
    pub format: JetFormat,
    pub initialized: bool,
    /// page number of the owning table definition (index page header field)
    pub tdef_page_number: i32,
    /// the currently loaded pages, page number -> page
    pages: HashMap<i32, CachePage>,
    /// the loaded page numbers in access order, least recently used first.
    /// Java's `_dataPages` is a `LinkedHashMap` with access order, whose
    /// `removeEldestEntry` purges the eldest pages once the cache grows past
    /// `MAX_CACHE_SIZE`; the eviction order decides which pages are re-read
    /// from the file later, and a re-read leaves the page image in the reused
    /// index page buffer (whose stale tail ends up in the written page), so
    /// the order has to be reproduced exactly for byte identical output.
    page_order: Vec<i32>,
    /// the currently modified page numbers, in the order they were modified
    modified: Vec<i32>,
    /// temp buffer used to read/write the index pages (re-used like the Java
    /// TempBufferHolder, whose stale trailing bytes end up in the written page)
    page_buffer: Option<ByteBuf>,
    max_page_entry_size: usize,
}

impl IndexPageCache {
    pub fn new(
        root_page_number: i32,
        owned_pages: Option<UsageMap>,
        format: JetFormat,
    ) -> IndexPageCache {
        let max_page_entry_size = calc_max_page_entry_size(&format);
        IndexPageCache {
            root_page_number,
            owned_pages,
            format,
            initialized: false,
            tdef_page_number: 0,
            pages: HashMap::new(),
            page_order: Vec::new(),
            modified: Vec::new(),
            page_buffer: None,
            max_page_entry_size,
        }
    }

    /// `setRootPageNumber` - loads the root page and marks it as parentless.
    pub fn initialize(&mut self, ch: &mut PageChannel) -> Result<()> {
        let root_pn = self.root_page_number;
        if root_pn <= INVALID_INDEX_PAGE_NUMBER {
            return Err(Error::Jackcess(format!(
                "invalid index root page number {}",
                root_pn
            )));
        }
        self.ensure_loaded(ch, root_pn)?;
        let page = self.page_mut(root_pn)?;
        if page.parent_page_number.is_none() {
            // root page has no parent
            page.parent_page_number = Some(INVALID_INDEX_PAGE_NUMBER);
            page.tail = false;
        }
        self.initialized = true;
        Ok(())
    }

    // ------------------------------------------------------------------
    // page access
    // ------------------------------------------------------------------

    fn take_page_buffer(&mut self) -> ByteBuf {
        match self.page_buffer.take() {
            Some(mut b) => {
                b.clear();
                b
            }
            None => ByteBuf::allocate(self.format.page_size),
        }
    }

    fn put_page_buffer(&mut self, buffer: ByteBuf) {
        self.page_buffer = Some(buffer);
    }

    fn page(&self, page_number: i32) -> Result<&CachePage> {
        self.pages
            .get(&page_number)
            .ok_or_else(|| Error::IllegalState(format!("index page {} is not loaded", page_number)))
    }

    fn page_mut(&mut self, page_number: i32) -> Result<&mut CachePage> {
        self.pages
            .get_mut(&page_number)
            .ok_or_else(|| Error::IllegalState(format!("index page {} is not loaded", page_number)))
    }

    /// `LinkedHashMap.get` on Java's access ordered `_dataPages`: the page
    /// becomes the most recently used one.
    fn touch_page(&mut self, page_number: i32) {
        if let Some(pos) = self.page_order.iter().position(|&pn| pn == page_number) {
            self.page_order.remove(pos);
        }
        self.page_order.push(page_number);
    }

    /// `purgeOldPages`: drops the least recently used pages (never the root
    /// page) until the cache is back within `MAX_CACHE_SIZE`.
    fn purge_old_pages(&mut self) {
        let root_page_number = self.root_page_number;
        let mut i = 0usize;
        while i < self.page_order.len() {
            let page_number = self.page_order[i];
            // note, we never purge the root page
            if page_number != root_page_number {
                self.page_order.remove(i);
                self.pages.remove(&page_number);
                if self.pages.len() <= MAX_CACHE_SIZE {
                    break;
                }
            } else {
                i += 1;
            }
        }
    }

    /// Reads the given index page from the file.
    fn ensure_loaded(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<()> {
        if self.pages.contains_key(&page_number) {
            self.touch_page(page_number);
            return Ok(());
        }
        let mut buffer = self.take_page_buffer();
        if let Err(e) = ch.read_page(&mut buffer, page_number) {
            self.put_page_buffer(buffer);
            return Err(e);
        }
        let format = self.format.clone();
        let mut data = match read_data_page(&mut buffer, page_number, &format) {
            Ok(d) => d,
            Err(e) => {
                self.put_page_buffer(buffer);
                return Err(e);
            }
        };
        // the table definition page number is stored in every index page
        // header, so pick it up when creating a cache for a newly created
        // table (Java knows it through the owning table).
        if self.tdef_page_number == 0 {
            if let Ok(tdef) = buffer.get_i32_at(4) {
                self.tdef_page_number = tdef;
            }
        }
        self.put_page_buffer(buffer);

        if data.child_tail_page_number != INVALID_INDEX_PAGE_NUMBER {
            data.child_tail_entry =
                self.build_child_tail_entry(ch, page_number, data.child_tail_page_number)?;
        }

        self.pages.insert(
            page_number,
            CachePage {
                data,
                parent_page_number: None,
                tail: false,
                modified: false,
            },
        );
        self.page_order.push(page_number);
        // `LinkedHashMap.removeEldestEntry` runs on every new entry
        if self.pages.len() > MAX_CACHE_SIZE && !ch.is_writing() {
            self.purge_old_pages();
        }
        Ok(())
    }

    /// Builds the node entry which stands for the last entry of the given
    /// child tail page (the cached `EntryListView._childTailEntry`).
    ///
    /// Java's `EntryListView` constructor uses `main.getChildTailPage()`, i.e.
    /// `getChildPage(childTailPageNumber, true)`, which also initializes the
    /// child page's parent info (and touches the page cache).
    fn build_child_tail_entry(
        &mut self,
        ch: &mut PageChannel,
        parent_page_number: i32,
        child_tail_page_number: i32,
    ) -> Result<Option<Entry>> {
        if child_tail_page_number == INVALID_INDEX_PAGE_NUMBER {
            return Ok(None);
        }
        let child_tail = self
            .get_child_page(ch, parent_page_number, child_tail_page_number, true)?
            .ok_or_else(|| {
                Error::Jackcess(format!(
                    "invalid child tail page {} of index page {}",
                    child_tail_page_number, parent_page_number
                ))
            })?;
        let last = self.page(child_tail)?.data.view_last().cloned();
        match last {
            Some(entry) => Ok(Some(entry.as_node_entry(child_tail_page_number))),
            None => Err(Error::Jackcess(format!(
                "child tail page {} has no entries",
                child_tail_page_number
            ))),
        }
    }

    /// `DataPageExtra.setEntryView` - (re)computes the page's cached tail entry.
    fn set_entry_view(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<()> {
        let child_tail = self.page(page_number)?.data.child_tail_page_number;
        let entry = self.build_child_tail_entry(ch, page_number, child_tail)?;
        self.page_mut(page_number)?.data.child_tail_entry = entry;
        Ok(())
    }

    /// Returns the page number if it is a valid, loaded index page.
    pub fn get_page(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<Option<i32>> {
        if page_number <= INVALID_INDEX_PAGE_NUMBER {
            return Ok(None);
        }
        // `getDataPage`: a cached page is just touched (the LinkedHashMap
        // access order), a missing page is read and cached
        self.ensure_loaded(ch, page_number)?;
        Ok(Some(page_number))
    }

    /// Returns a child page for the given page number, updating its parent info
    /// if necessary.
    fn get_child_page(
        &mut self,
        ch: &mut PageChannel,
        parent_page_number: i32,
        child_page_number: i32,
        is_tail: bool,
    ) -> Result<Option<i32>> {
        match self.get_page(ch, child_page_number)? {
            Some(pn) => {
                let page = self.page_mut(pn)?;
                if page.parent_page_number.is_none() {
                    page.parent_page_number = Some(parent_page_number);
                    page.tail = is_tail;
                }
                Ok(Some(pn))
            }
            None => Ok(None),
        }
    }

    fn get_child_tail_page(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<i32> {
        let child_tail = self.page(page_number)?.data.child_tail_page_number;
        match self.get_child_page(ch, page_number, child_tail, true)? {
            Some(pn) => Ok(pn),
            None => Err(Error::IllegalState(format!(
                "missing child tail page {} of index page {}",
                child_tail, page_number
            ))),
        }
    }

    fn get_parent_page(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<i32> {
        if self.page(page_number)?.parent_page_number.is_none() {
            self.resolve_parent(ch, page_number)?;
        }
        let parent = self
            .page(page_number)?
            .parent_page_number
            .ok_or_else(|| Error::IllegalState("Parent was not resolved".into()))?;
        // `DataPageMain.getParentPage` returns `getDataPage(_parentPageNumber)`,
        // so the parent page is loaded (and touched) as well
        self.get_page(ch, parent)?;
        Ok(parent)
    }

    /// `DataPageMain.isTail` - resolves the parent page (which searches the
    /// tree, loading pages) before answering.
    fn is_tail(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<bool> {
        if self.page(page_number)?.parent_page_number.is_none() {
            self.resolve_parent(ch, page_number)?;
        }
        Ok(self.page(page_number)?.tail)
    }

    fn resolve_parent(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<()> {
        let last = self.page(page_number)?.data.view_last().cloned();
        let last = last.ok_or_else(|| {
            Error::IllegalState(format!("index page {} has no entries", page_number))
        })?;
        // the act of searching for the last entry should resolve any parent
        // pages along the path
        self.find_cache_data_page(ch, &last)?;
        if self.page(page_number)?.parent_page_number.is_none() {
            return Err(Error::IllegalState("Parent was not resolved".into()));
        }
        Ok(())
    }

    fn get_level(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<i32> {
        let (leaf, level) = {
            let page = self.page(page_number)?;
            (page.data.leaf, page.data.level)
        };
        if !leaf && level == 0 {
            // a node page is never at level 0, take it from a child instead
            let first_child = self
                .page(page_number)?
                .data
                .entries
                .first()
                .and_then(|e| e.get_sub_page_number())
                .ok_or_else(|| {
                    Error::IllegalState(format!("node page {} has no child pages", page_number))
                })?;
            // `getChildPage(entry)`: loads the child page and initializes its
            // parent info
            let is_tail = self
                .page(page_number)?
                .data
                .is_child_tail_page_number(first_child);
            let child = self
                .get_child_page(ch, page_number, first_child, is_tail)?
                .ok_or_else(|| {
                    Error::IllegalState(format!(
                        "missing child page {} of index page {}",
                        first_child, page_number
                    ))
                })?;
            let child_level = self.get_level(ch, child)?;
            self.page_mut(page_number)?.data.level = child_level + 1;
        }
        Ok(self.page(page_number)?.data.level)
    }

    // ------------------------------------------------------------------
    // entry modification
    // ------------------------------------------------------------------

    /// Marks the given index page as modified and saves it for writing.
    fn set_modified(&mut self, page_number: i32) -> Result<()> {
        let needs = !self.page(page_number)?.modified;
        if needs {
            self.page_mut(page_number)?.modified = true;
            self.modified.push(page_number);
        }
        Ok(())
    }

    /// `validateEntryForPage`
    fn validate_entry_for_page(&self, page_number: i32, entry: &Entry) -> Result<()> {
        let leaf = self.page(page_number)?.data.leaf;
        if leaf != entry.is_leaf_entry() {
            return Err(Error::IllegalState(format!(
                "Trying to update page with wrong entry type; pageLeaf {}, entryLeaf {}",
                leaf,
                entry.is_leaf_entry()
            )));
        }
        Ok(())
    }

    /// `addEntry`
    pub fn add_entry(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        page_number: i32,
        entry_idx: usize,
        new_entry: Entry,
    ) -> Result<()> {
        self.update_entry(
            ch,
            owned_pages,
            page_number,
            entry_idx,
            Some(new_entry),
            UpdateType::Add,
        )?;
        Ok(())
    }

    /// `removeEntry`
    pub fn remove_entry(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        page_number: i32,
        entry_idx: usize,
    ) -> Result<Option<Entry>> {
        self.update_entry(
            ch,
            owned_pages,
            page_number,
            entry_idx,
            None,
            UpdateType::Remove,
        )
    }

    /// `updateEntry` - updates the entries on the given page.
    fn update_entry(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        page_number: i32,
        entry_idx: usize,
        new_entry: Option<Entry>,
        up_type: UpdateType,
    ) -> Result<Option<Entry>> {
        if let Some(entry) = &new_entry {
            self.validate_entry_for_page(page_number, entry)?;
        }

        // note, it's slightly ucky, but we need to load the parent page before we
        // start mucking with our entries because our parent may use our entries.
        let is_root = page_number == self.root_page_number;
        let parent_page_number = if !is_root {
            Some(self.get_parent_page(ch, page_number)?)
        } else {
            None
        };

        let old_last_entry = self.page(page_number)?.data.view_last().cloned();

        let mut entry_size_diff = 0i32;

        let old_entry: Option<Entry> = match up_type {
            UpdateType::Add => {
                let entry = new_entry.as_ref().unwrap().clone();
                entry_size_diff += entry.size();
                self.page_mut(page_number)?
                    .data
                    .view_add(entry_idx, entry)?;
                None
            }
            UpdateType::Replace => {
                let entry = new_entry.unwrap();
                let old = self
                    .page_mut(page_number)?
                    .data
                    .view_set(entry_idx, entry)?;
                entry_size_diff += self
                    .page(page_number)?
                    .data
                    .view_get(entry_idx)
                    .unwrap()
                    .size()
                    - old.size();
                Some(old)
            }
            UpdateType::Remove => {
                let old = self.page_mut(page_number)?.data.view_remove(entry_idx)?;
                entry_size_diff -= old.size();
                Some(old)
            }
        };

        let new_last_entry = self.page(page_number)?.data.view_last().cloned();
        let update_last = !same_entry_identity(&old_last_entry, &new_last_entry);
        let has_child_tail = self.page(page_number)?.data.has_child_tail();

        // child tail entry updates do not modify the page
        if !update_last || !has_child_tail {
            let page = self.page_mut(page_number)?;
            page.data.total_entry_size += entry_size_diff;
            // for now, just clear the prefix, we'll fix it later
            page.data.entry_prefix.clear();
            self.set_modified(page_number)?;
        }

        let is_view_empty = self.page(page_number)?.data.is_view_empty();
        if is_view_empty {
            // this page is dead
            self.remove_data_page(
                ch,
                owned_pages,
                parent_page_number,
                page_number,
                old_last_entry,
            )?;
            return Ok(old_entry);
        }

        // determine if we need to update our parent page
        if !update_last || is_root {
            // no parent
            return Ok(old_entry);
        }

        // the update to the last entry needs to be propagated to our parent
        self.replace_parent_entry(
            ch,
            owned_pages,
            parent_page_number.unwrap(),
            page_number,
            old_last_entry,
        )?;
        Ok(old_entry)
    }

    /// `removeDataPage` - removes an index page which has become empty.
    fn remove_data_page(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        parent_page_number: Option<i32>,
        page_number: i32,
        old_last_entry: Option<Entry>,
    ) -> Result<()> {
        if self.page(page_number)?.data.has_child_tail() {
            return Err(Error::IllegalState("Still has child tail?".into()));
        }

        let total = self.page(page_number)?.data.total_entry_size;
        if total != 0 {
            return Err(Error::IllegalState(format!(
                "Empty page but size is not 0? {}",
                total
            )));
        }

        if page_number == self.root_page_number {
            // clear out this page (we don't actually remove it)
            let page = self.page_mut(page_number)?;
            page.data.entry_prefix.clear();
            // when the root page becomes empty, it becomes a leaf page again
            page.data.leaf = true;
            page.data.level = 0;
            return Ok(());
        }

        // remove this page from its parent page
        let parent = parent_page_number.ok_or_else(|| {
            Error::IllegalState(format!("index page {} has no parent", page_number))
        })?;
        self.update_parent_entry(
            ch,
            owned_pages,
            parent,
            page_number,
            old_last_entry,
            None,
            UpdateType::Remove,
        )?;

        // remove this page from any next/prev pages
        self.remove_from_peers(ch, page_number)
    }

    /// `removeFromPeers`
    fn remove_from_peers(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<()> {
        let (prev_page_number, next_page_number) = {
            let page = self.page(page_number)?;
            (page.data.prev_page_number, page.data.next_page_number)
        };

        if let Some(prev) = self.get_page(ch, prev_page_number)? {
            self.set_modified(prev)?;
            self.page_mut(prev)?.data.next_page_number = next_page_number;
        }

        if let Some(next) = self.get_page(ch, next_page_number)? {
            self.set_modified(next)?;
            self.page_mut(next)?.data.prev_page_number = prev_page_number;
        }
        Ok(())
    }

    /// `addParentEntry`
    fn add_parent_entry(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        parent_page_number: i32,
        child_page_number: i32,
    ) -> Result<()> {
        let new_entry = self
            .page(child_page_number)?
            .data
            .view_last()
            .cloned()
            .ok_or_else(|| {
                Error::IllegalState(format!("index page {} has no entries", child_page_number))
            })?;
        self.update_parent_entry(
            ch,
            owned_pages,
            parent_page_number,
            child_page_number,
            None,
            Some(new_entry),
            UpdateType::Add,
        )
    }

    /// `replaceParentEntry`
    fn replace_parent_entry(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        parent_page_number: i32,
        child_page_number: i32,
        old_entry: Option<Entry>,
    ) -> Result<()> {
        let new_entry = self
            .page(child_page_number)?
            .data
            .view_last()
            .cloned()
            .ok_or_else(|| {
                Error::IllegalState(format!("index page {} has no entries", child_page_number))
            })?;
        self.update_parent_entry(
            ch,
            owned_pages,
            parent_page_number,
            child_page_number,
            old_entry,
            Some(new_entry),
            UpdateType::Replace,
        )
    }

    /// `updateParentEntry`
    #[allow(clippy::too_many_arguments)]
    fn update_parent_entry(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        parent_page_number: i32,
        child_page_number: i32,
        old_entry: Option<Entry>,
        new_entry: Option<Entry>,
        up_type: UpdateType,
    ) -> Result<()> {
        let child_is_tail = self.is_tail(ch, child_page_number)?;

        if child_is_tail && up_type != UpdateType::Remove {
            // for add or replace, update the child tail info before updating the
            // parent entries
            self.update_parent_tail(parent_page_number, child_page_number, up_type)?;
        }

        let old_entry = old_entry.map(|e| e.as_node_entry(child_page_number));
        let new_entry = new_entry.map(|e| e.as_node_entry(child_page_number));

        let expect_found = up_type != UpdateType::Add;
        let mut idx = match up_type {
            UpdateType::Add => {
                let key = new_entry.as_ref().unwrap();
                self.page(parent_page_number)?.data.view_find(key)
            }
            _ => {
                let key = old_entry.as_ref().ok_or_else(|| {
                    Error::IllegalState("missing old entry for parent update".into())
                })?;
                self.page(parent_page_number)?.data.view_find(key)
            }
        };

        if idx < 0 {
            if expect_found {
                return Err(Error::IllegalState(
                    "Could not find child entry in parent".into(),
                ));
            }
            idx = missing_index_to_insertion_point(idx);
        } else if !expect_found {
            return Err(Error::IllegalState(
                "Unexpectedly found child entry in parent".into(),
            ));
        }

        let entry = if up_type == UpdateType::Remove {
            None
        } else {
            new_entry
        };
        self.update_entry(
            ch,
            owned_pages,
            parent_page_number,
            idx as usize,
            entry,
            up_type,
        )?;

        if child_is_tail && up_type == UpdateType::Remove {
            // for remove, update the child tail info after updating the parent
            // entries
            self.update_parent_tail(parent_page_number, child_page_number, up_type)?;
        }
        Ok(())
    }

    /// `updateParentTail`
    fn update_parent_tail(
        &mut self,
        parent_page_number: i32,
        child_page_number: i32,
        up_type: UpdateType,
    ) -> Result<()> {
        let new_child_tail_page_number = if up_type == UpdateType::Remove {
            INVALID_INDEX_PAGE_NUMBER
        } else {
            child_page_number
        };
        if self.page(parent_page_number)?.data.child_tail_page_number != new_child_tail_page_number
        {
            self.set_modified(parent_page_number)?;
            self.page_mut(parent_page_number)?
                .data
                .child_tail_page_number = new_child_tail_page_number;
        }
        Ok(())
    }

    /// `findCacheDataPage` - finds the index page on which the given entry does
    /// or should reside.
    pub fn find_cache_data_page(&mut self, ch: &mut PageChannel, entry: &Entry) -> Result<i32> {
        let mut cur_page = self.root_page_number;
        loop {
            if self.page(cur_page)?.data.leaf {
                // nowhere to go from here
                return Ok(cur_page);
            }

            let mut idx = self.page(cur_page)?.data.view_find(entry);
            if idx < 0 {
                idx = missing_index_to_insertion_point(idx);
                let size = self.page(cur_page)?.data.view_len();
                if idx as usize == size {
                    // just move to last child page
                    idx -= 1;
                }
            }
            if idx < 0 {
                return Err(Error::IllegalState(format!(
                    "index page {} has no child entries",
                    cur_page
                )));
            }

            let node_entry = self
                .page(cur_page)?
                .data
                .view_get(idx as usize)
                .cloned()
                .ok_or_else(|| {
                    Error::IllegalState(format!("missing entry {} on index page {}", idx, cur_page))
                })?;
            let child_page_number = node_entry
                .get_sub_page_number()
                .ok_or_else(|| Error::IllegalState("leaf entry found on a node page".into()))?;
            let is_tail = self
                .page(cur_page)?
                .data
                .is_child_tail_page_number(child_page_number);
            let child = self
                .get_child_page(ch, cur_page, child_page_number, is_tail)?
                .ok_or_else(|| {
                    Error::IllegalState(format!(
                        "missing child page {} of index page {}",
                        child_page_number, cur_page
                    ))
                })?;
            cur_page = child;
        }
    }

    /// The result of `DataPage.findEntry` for the given page.
    pub fn find_entry(
        &mut self,
        ch: &mut PageChannel,
        page_number: i32,
        entry: &Entry,
    ) -> Result<i32> {
        self.ensure_loaded(ch, page_number)?;
        Ok(self.page(page_number)?.data.find_entry(entry))
    }

    /// `IndexData.getNextPosition` (returns the entry at the next position).
    pub fn get_next_position(
        &mut self,
        ch: &mut PageChannel,
        page_number: i32,
        idx: usize,
        between: bool,
    ) -> Result<Option<Entry>> {
        let next_idx = if between { idx } else { idx + 1 };
        let entries_len = self.page(page_number)?.data.entries.len();
        if next_idx < entries_len {
            return Ok(Some(self.page(page_number)?.data.entries[next_idx].clone()));
        }

        let mut next_page_number = self.page(page_number)?.data.next_page_number;
        while next_page_number != INVALID_INDEX_PAGE_NUMBER {
            let page = match self.get_page(ch, next_page_number)? {
                Some(pn) => pn,
                None => break,
            };
            if !self.page(page)?.data.entries.is_empty() {
                return Ok(Some(self.page(page)?.data.entries[0].clone()));
            }
            next_page_number = self.page(page)?.data.next_page_number;
        }
        Ok(None)
    }

    /// `IndexData.getNextPosition`, also returning the page and index the
    /// entry was found at, so that the caller can keep walking.  This is the
    /// movement `IndexData.EntryCursor.getNextEntry` performs (and the reason
    /// `get_next_position` exists separately: the write paths only need the
    /// entry itself).
    pub fn get_next_entry_position(
        &mut self,
        ch: &mut PageChannel,
        page_number: i32,
        idx: usize,
        between: bool,
    ) -> Result<Option<(i32, usize, Entry)>> {
        let next_idx = if between { idx } else { idx + 1 };
        let entries_len = self.page(page_number)?.data.entries.len();
        if next_idx < entries_len {
            return Ok(Some((
                page_number,
                next_idx,
                self.page(page_number)?.data.entries[next_idx].clone(),
            )));
        }

        let mut next_page_number = self.page(page_number)?.data.next_page_number;
        while next_page_number != INVALID_INDEX_PAGE_NUMBER {
            let page = match self.get_page(ch, next_page_number)? {
                Some(pn) => pn,
                None => break,
            };
            if !self.page(page)?.data.entries.is_empty() {
                return Ok(Some((page, 0, self.page(page)?.data.entries[0].clone())));
            }
            next_page_number = self.page(page)?.data.next_page_number;
        }
        Ok(None)
    }

    /// `IndexData.getPreviousPosition` (returns the entry at the previous
    /// position).
    pub fn get_previous_position(
        &mut self,
        ch: &mut PageChannel,
        page_number: i32,
        idx: usize,
    ) -> Result<Option<Entry>> {
        let prev_idx = idx as i64 - 1;
        if prev_idx >= 0 {
            let i = prev_idx as usize;
            return Ok(Some(self.page(page_number)?.data.entries[i].clone()));
        }

        let mut prev_page_number = self.page(page_number)?.data.prev_page_number;
        while prev_page_number != INVALID_INDEX_PAGE_NUMBER {
            let page = match self.get_page(ch, prev_page_number)? {
                Some(pn) => pn,
                None => break,
            };
            if !self.page(page)?.data.entries.is_empty() {
                let entries = &self.page(page)?.data.entries;
                return Ok(Some(entries[entries.len() - 1].clone()));
            }
            prev_page_number = self.page(page)?.data.prev_page_number;
        }
        Ok(None)
    }

    /// `IndexData.getPreviousPosition` with the position retained: the entry
    /// before the entry at `idx` on the given page (`Position.getPrevIndex` plus
    /// the previous page walk), returning `(page, index, entry)`.
    ///
    /// This is the backward counterpart of
    /// [`IndexPageCache::get_next_entry_position`] and is what the index cursor
    /// uses to walk an index in reverse.
    pub fn get_previous_entry_position(
        &mut self,
        ch: &mut PageChannel,
        page_number: i32,
        idx: usize,
    ) -> Result<Option<(i32, usize, Entry)>> {
        if idx > 0 {
            let i = idx - 1;
            return Ok(Some((
                page_number,
                i,
                self.page(page_number)?.data.entries[i].clone(),
            )));
        }

        let mut prev_page_number = self.page(page_number)?.data.prev_page_number;
        while prev_page_number != INVALID_INDEX_PAGE_NUMBER {
            let page = match self.get_page(ch, prev_page_number)? {
                Some(pn) => pn,
                None => break,
            };
            let entries_len = self.page(page)?.data.entries.len();
            if entries_len > 0 {
                let i = entries_len - 1;
                return Ok(Some((page, i, self.page(page)?.data.entries[i].clone())));
            }
            prev_page_number = self.page(page)?.data.prev_page_number;
        }
        Ok(None)
    }

    /// The first leaf entry of the index (the left-most entry), with its
    /// position.  `None` for an empty index.
    ///
    /// This is the position a forward `EntryCursor` starts from, found by
    /// descending the left-most child at every level.
    pub fn find_first_entry_position(
        &mut self,
        ch: &mut PageChannel,
    ) -> Result<Option<(i32, usize, Entry)>> {
        let mut page_number = self.root_page_number;
        loop {
            let (leaf, first) = {
                let page = self.page(page_number)?;
                (page.data.leaf, page.data.view_get(0).cloned())
            };
            if leaf {
                let entries_len = self.page(page_number)?.data.entries.len();
                if entries_len == 0 {
                    return Ok(None);
                }
                return Ok(Some((
                    page_number,
                    0,
                    self.page(page_number)?.data.entries[0].clone(),
                )));
            }
            let first_entry = first.ok_or_else(|| {
                Error::IllegalState(format!("index page {} has no child entries", page_number))
            })?;
            let child_page_number = first_entry
                .get_sub_page_number()
                .ok_or_else(|| Error::IllegalState("leaf entry found on a node page".into()))?;
            let is_tail = self
                .page(page_number)?
                .data
                .is_child_tail_page_number(child_page_number);
            page_number = self
                .get_child_page(ch, page_number, child_page_number, is_tail)?
                .ok_or_else(|| {
                    Error::IllegalState(format!(
                        "missing child page {} of index page {}",
                        child_page_number, page_number
                    ))
                })?;
        }
    }

    /// The last leaf entry of the index (the right-most entry), with its
    /// position.  `None` for an empty index.
    ///
    /// This is the position a reverse `EntryCursor` starts from, found by
    /// descending the right-most child at every level.
    pub fn find_last_entry_position(
        &mut self,
        ch: &mut PageChannel,
    ) -> Result<Option<(i32, usize, Entry)>> {
        let mut page_number = self.root_page_number;
        loop {
            let (leaf, last) = {
                let page = self.page(page_number)?;
                (page.data.leaf, page.data.view_last().cloned())
            };
            if leaf {
                let entries_len = self.page(page_number)?.data.entries.len();
                if entries_len == 0 {
                    return Ok(None);
                }
                let i = entries_len - 1;
                return Ok(Some((
                    page_number,
                    i,
                    self.page(page_number)?.data.entries[i].clone(),
                )));
            }
            let last_entry = last.ok_or_else(|| {
                Error::IllegalState(format!("index page {} has no child entries", page_number))
            })?;
            let child_page_number = last_entry
                .get_sub_page_number()
                .ok_or_else(|| Error::IllegalState("leaf entry found on a node page".into()))?;
            let is_tail = self
                .page(page_number)?
                .data
                .is_child_tail_page_number(child_page_number);
            page_number = self
                .get_child_page(ch, page_number, child_page_number, is_tail)?
                .ok_or_else(|| {
                    Error::IllegalState(format!(
                        "missing child page {} of index page {}",
                        child_page_number, page_number
                    ))
                })?;
        }
    }

    /// Helpers used by the (optional) row deletion path.
    pub fn page_entries_len(&mut self, page_number: i32) -> Result<usize> {
        Ok(self.page(page_number)?.data.entries.len())
    }

    pub fn page_entry(&mut self, page_number: i32, idx: usize) -> Result<Entry> {
        Ok(self.page(page_number)?.data.entries[idx].clone())
    }

    pub fn page_next(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<Option<i32>> {
        let next = self.page(page_number)?.data.next_page_number;
        self.get_page(ch, next)
    }

    // ------------------------------------------------------------------
    // writing
    // ------------------------------------------------------------------

    /// `IndexPageCache.write` - writes any outstanding changes for this index
    /// to the file.
    pub fn write(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
    ) -> Result<()> {
        // first discard any empty pages
        self.handle_empty_pages(ch, owned_pages)?;
        // next, handle any necessary page splitting
        self.prepare_pages_for_writing(ch, owned_pages)?;
        // finally, write all the modified pages (which are not being deleted)
        self.write_data_pages(ch)?;
        // after we write everything, we can purge our cache if necessary
        if self.pages.len() > MAX_CACHE_SIZE {
            self.purge_old_pages();
        }
        Ok(())
    }

    /// Handles any modified pages which are empty as the first pass.
    fn handle_empty_pages(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
    ) -> Result<()> {
        let mut i = 0usize;
        while i < self.modified.len() {
            let page_number = self.modified[i];
            let empty = self.page(page_number)?.data.is_view_empty();
            if empty {
                if page_number != self.root_page_number {
                    self.delete_data_page(ch, owned_pages, page_number)?;
                } else {
                    self.write_data_page(ch, page_number)?;
                }
                self.modified.remove(i);
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    /// Prepares any non-empty modified pages for writing: updates entry
    /// prefixes, promotes/demotes tail pages, and splits pages as needed.
    fn prepare_pages_for_writing(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
    ) -> Result<()> {
        let max_page_entry_size = self.max_page_entry_size as i32;
        loop {
            let mut split_pages = false;
            let mut i = 0usize;
            while i < self.modified.len() {
                let page_number = self.modified[i];

                if !self.page(page_number)?.data.leaf {
                    // see if we need to update any child tail status
                    let size = self.page(page_number)?.data.view_len();
                    let has_child_tail = self.page(page_number)?.data.has_child_tail();
                    if has_child_tail {
                        if size == 1 {
                            self.demote_tail(ch, page_number)?;
                        }
                    } else if size > 1 {
                        // only a leaf page can become a tail page
                        let last_child_entry = self
                            .page(page_number)?
                            .data
                            .view_last()
                            .cloned()
                            .ok_or_else(|| {
                                Error::IllegalState("node page has no entries".into())
                            })?;
                        let last_child_page_number =
                            last_child_entry.get_sub_page_number().ok_or_else(|| {
                                Error::IllegalState("leaf entry found on a node page".into())
                            })?;
                        let last_child =
                            self.get_page(ch, last_child_page_number)?.ok_or_else(|| {
                                Error::IllegalState(format!(
                                    "missing child page {}",
                                    last_child_page_number
                                ))
                            })?;
                        if self.page(last_child)?.data.leaf {
                            self.promote_tail(ch, page_number, last_child)?;
                        }
                    }
                }

                // look for pages with more entries than can fit on a page
                if self.page(page_number)?.data.total_entry_size > max_page_entry_size {
                    // make sure the prefix is up-to-date
                    self.update_entry_prefix(page_number)?;

                    // now, see if the page will fit when compressed
                    if self.page(page_number)?.data.compressed_entry_size() > max_page_entry_size {
                        // need to split this page
                        split_pages = true;
                        self.split_data_page(ch, owned_pages, page_number)?;
                    }
                }
                i += 1;
            }
            if !split_pages {
                break;
            }
        }
        Ok(())
    }

    /// `DataPageExtra.updateEntryPrefix`
    fn update_entry_prefix(&mut self, page_number: i32) -> Result<()> {
        if self.page(page_number)?.data.entry_prefix.is_empty() {
            let first = self
                .page(page_number)?
                .data
                .entries
                .first()
                .cloned()
                .ok_or_else(|| Error::IllegalState("cannot find prefix of empty page".into()))?;
            let last = self
                .page(page_number)?
                .data
                .entries
                .last()
                .cloned()
                .ok_or_else(|| Error::IllegalState("cannot find prefix of empty page".into()))?;
            let prefix = find_common_prefix(&first, &last);
            self.page_mut(page_number)?.data.entry_prefix = prefix;
        }
        Ok(())
    }

    /// Writes any non-empty modified pages as the last pass.
    fn write_data_pages(&mut self, ch: &mut PageChannel) -> Result<()> {
        if self.modified.is_empty() {
            return Ok(());
        }
        let pages = self.modified.clone();
        for page_number in pages {
            if self.page(page_number)?.data.is_view_empty() {
                return Err(Error::IllegalState(format!(
                    "Unexpected empty page {}",
                    page_number
                )));
            }
            self.write_data_page(ch, page_number)?;
        }
        self.modified.clear();
        Ok(())
    }

    /// Writes the given index page to the file.
    fn write_data_page(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<()> {
        let max_page_entry_size = self.max_page_entry_size as i32;
        if self.page(page_number)?.data.compressed_entry_size() > max_page_entry_size {
            return Err(Error::IllegalState("data page is too large".into()));
        }

        let mut buffer = self.take_page_buffer();
        {
            let page = self.page(page_number)?;
            if let Err(e) =
                write_data_page(&mut buffer, &page.data, self.tdef_page_number, &self.format)
            {
                self.put_page_buffer(buffer);
                return Err(e);
            }
        }
        if let Err(e) = ch.write_page(&buffer, page_number) {
            self.put_page_buffer(buffer);
            return Err(e);
        }
        self.page_mut(page_number)?.modified = false;
        self.put_page_buffer(buffer);
        Ok(())
    }

    /// `deleteDataPage` - deletes the given index page from the file.
    fn delete_data_page(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        page_number: i32,
    ) -> Result<()> {
        // the index gives up the page before the database offers it for re-use,
        // otherwise the index still claims a page which something else can take
        remove_owned_page(owned_pages, ch, page_number)?;

        // free this database page
        ch.deallocate_page(page_number)?;

        // discard from our cache
        self.pages.remove(&page_number);
        if let Some(pos) = self.page_order.iter().position(|&pn| pn == page_number) {
            self.page_order.remove(pos);
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // page splitting
    // ------------------------------------------------------------------

    /// `splitDataPage` - splits an index page which has too many entries on it.
    fn split_data_page(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        orig_page_number: i32,
    ) -> Result<()> {
        self.set_modified(orig_page_number)?;

        let num_entries = self.page(orig_page_number)?.data.entries.len();
        if num_entries < 2 {
            return Err(Error::IllegalState(format!(
                "Cannot split page with less than 2 entries {}",
                orig_page_number
            )));
        }

        let mut orig = orig_page_number;
        if orig == self.root_page_number {
            // we can't split the root page directly, so we need to put another
            // page between the root page and its sub-pages, and then split that
            // page.
            orig = self.nest_root_data_page(ch, owned_pages, orig)?;
        }

        // note, it's slightly ucky, but we need to load the parent page before we
        // start mucking with our entries because our parent may use our entries.
        let parent_page_number = self.get_parent_page(ch, orig)?;

        let leaf = self.page(orig)?.data.leaf;
        let level = self.get_level(ch, orig)?;
        let new_page_number =
            self.allocate_new_cache_data_page(ch, owned_pages, parent_page_number, leaf, level)?;

        // naively move half the entries from one page to the new page
        let half = num_entries.div_ceil(2);
        let moved: Vec<Entry> = {
            let page = self.page_mut(orig)?;
            page.data.entries.drain(0..half).collect()
        };
        let moved_size: i32 = moved.iter().map(|e| e.size()).sum();
        {
            let page = self.page_mut(new_page_number)?;
            page.data.total_entry_size += moved_size;
            page.data.entries = moved;
        }
        self.set_entry_view(ch, new_page_number)?;

        // remove the moved entries from the old page
        let new_total = self.page(new_page_number)?.data.total_entry_size;
        {
            let page = self.page_mut(orig)?;
            page.data.entry_prefix.clear();
            page.data.total_entry_size -= new_total;
        }

        // insert this new page between the old page and any previous page
        self.add_to_peers_before(ch, new_page_number, orig)?;

        if !self.page(new_page_number)?.data.leaf {
            // reparent the children pages of the new page
            self.reparent_children(new_page_number)?;

            // if the children of this page are also node pages, then the
            // next/prev links should not cross parent boundaries
            let last_child = self
                .page(new_page_number)?
                .data
                .view_last()
                .and_then(|e| e.get_sub_page_number())
                .ok_or_else(|| Error::IllegalState("node page has no child entries".into()))?;
            let last_child_page = self
                .get_page(ch, last_child)?
                .ok_or_else(|| Error::IllegalState(format!("missing child page {}", last_child)))?;
            if !self.page(last_child_page)?.data.leaf {
                self.separate_from_next_peer(ch, last_child_page)?;
            }
        }

        // lastly, we need to add the new page to the parent page's entries
        self.add_parent_entry(ch, owned_pages, parent_page_number, new_page_number)
    }

    /// `nestRootDataPage` - copies the current root page into a new page and
    /// nests this page under the root page.
    fn nest_root_data_page(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        root_page_number: i32,
    ) -> Result<i32> {
        if root_page_number != self.root_page_number {
            return Err(Error::IllegalArgument(
                "should be called with root, duh".into(),
            ));
        }

        let (leaf, level) = {
            let page = self.page(root_page_number)?;
            (page.data.leaf, self.get_level(ch, root_page_number)?)
        };
        let new_page_number =
            self.allocate_new_cache_data_page(ch, owned_pages, root_page_number, leaf, level)?;

        // move entries to the new page
        let (child_tail, entries, prefix, total) = {
            let page = self.page(root_page_number)?;
            (
                page.data.child_tail_page_number,
                page.data.entries.clone(),
                page.data.entry_prefix.clone(),
                page.data.total_entry_size,
            )
        };
        {
            let page = self.page_mut(new_page_number)?;
            page.data.child_tail_page_number = child_tail;
            page.data.entries = entries;
            page.data.entry_prefix = prefix;
            page.data.total_entry_size = total;
        }
        self.set_entry_view(ch, new_page_number)?;

        if !self.page(new_page_number)?.data.leaf {
            // we need to re-parent all the child pages
            self.reparent_children(new_page_number)?;
        }

        let new_level = self.get_level(ch, new_page_number)?;
        // clear the root page, which now sits one level above the new page
        // holding its entries
        {
            let page = self.page_mut(root_page_number)?;
            page.data.leaf = false;
            page.data.level = new_level + 1;
            page.data.child_tail_page_number = INVALID_INDEX_PAGE_NUMBER;
            page.data.entries.clear();
            page.data.entry_prefix.clear();
            page.data.total_entry_size = 0;
        }
        self.set_entry_view(ch, root_page_number)?;

        // add the new page as the first child of the root page
        self.add_parent_entry(ch, owned_pages, root_page_number, new_page_number)?;

        Ok(new_page_number)
    }

    /// Allocates a new index page with the given parent page and type.
    fn allocate_new_cache_data_page(
        &mut self,
        ch: &mut PageChannel,
        owned_pages: &mut Option<UsageMap>,
        parent_page_number: i32,
        is_leaf: bool,
        level: i32,
    ) -> Result<i32> {
        let page_number = ch.allocate_new_page()?;
        let data = DataPage::new(page_number, is_leaf, level);
        let page = CachePage {
            data,
            parent_page_number: Some(parent_page_number),
            tail: false,
            modified: false,
        };
        self.pages.insert(page_number, page);
        self.page_order.push(page_number);
        // `_dataPages.put` runs `removeEldestEntry` as well
        if self.pages.len() > MAX_CACHE_SIZE && !ch.is_writing() {
            self.purge_old_pages();
        }

        // update owned pages cache
        add_owned_page(owned_pages, ch, page_number)?;

        // needs to be written out
        self.set_modified(page_number)?;
        Ok(page_number)
    }

    /// `addToPeersBefore` - inserts the new page as a peer between the given
    /// original page and any previous peer page.
    fn add_to_peers_before(
        &mut self,
        ch: &mut PageChannel,
        new_page_number: i32,
        orig_page_number: i32,
    ) -> Result<()> {
        let (orig_prev, _orig_next) = {
            let page = self.page(orig_page_number)?;
            (page.data.prev_page_number, page.data.next_page_number)
        };

        {
            let page = self.page_mut(new_page_number)?;
            page.data.next_page_number = orig_page_number;
            page.data.prev_page_number = orig_prev;
        }
        self.page_mut(orig_page_number)?.data.prev_page_number = new_page_number;

        if let Some(prev) = self.get_page(ch, orig_prev)? {
            self.set_modified(prev)?;
            self.page_mut(prev)?.data.next_page_number = new_page_number;
        }
        Ok(())
    }

    /// `separateFromNextPeer` - separates the given index page from any next
    /// peer page.
    fn separate_from_next_peer(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<()> {
        self.set_modified(page_number)?;

        let next_page_number = self.page(page_number)?.data.next_page_number;
        let next = self.get_page(ch, next_page_number)?.ok_or_else(|| {
            Error::IllegalState(format!(
                "missing next peer page {} of index page {}",
                next_page_number, page_number
            ))
        })?;
        self.set_modified(next)?;

        self.page_mut(next)?.data.prev_page_number = INVALID_INDEX_PAGE_NUMBER;
        self.page_mut(page_number)?.data.next_page_number = INVALID_INDEX_PAGE_NUMBER;
        Ok(())
    }

    /// Sets the parent info for the children of the given page to the given
    /// page.
    fn reparent_children(&mut self, page_number: i32) -> Result<()> {
        // note, the "parent" page number is not actually persisted, so we do not
        // need to mark any updated pages as modified.  for the same reason, we
        // don't need to load the pages if not already loaded
        let child_page_numbers: Vec<i32> = {
            let page = self.page(page_number)?;
            let len = page.data.view_len();
            let mut out = Vec::with_capacity(len);
            for i in 0..len {
                if let Some(sub) = page.data.view_get(i).and_then(|e| e.get_sub_page_number()) {
                    out.push(sub);
                }
            }
            out
        };
        let child_tail = self.page(page_number)?.data.child_tail_page_number;

        for child in child_page_numbers {
            if self.pages.contains_key(&child) {
                // Java reads `_dataPages.get(childPageNumber)` directly, which
                // is a plain (access ordered) map lookup
                self.touch_page(child);
                let is_tail = child == child_tail;
                let page = self.page_mut(child)?;
                page.parent_page_number = Some(page_number);
                page.tail = is_tail;
            }
        }
        Ok(())
    }

    /// `demoteTail` - makes the tail entry of the given page a normal entry on
    /// that page.
    fn demote_tail(&mut self, ch: &mut PageChannel, page_number: i32) -> Result<()> {
        // there's only one entry on the page, and it's the tail.  make it a
        // normal entry
        self.set_modified(page_number)?;

        let tail_page_number = self.get_child_tail_page(ch, page_number)?;

        // move the tail entry to the last normal entry
        self.update_parent_tail(page_number, tail_page_number, UpdateType::Remove)?;
        let tail_entry = self.page_mut(page_number)?.data.view_demote_tail()?;
        {
            let page = self.page_mut(page_number)?;
            page.data.total_entry_size += tail_entry.size();
            page.data.entry_prefix.clear();
        }

        let tail = self.page_mut(tail_page_number)?;
        tail.parent_page_number = Some(page_number);
        tail.tail = false;
        Ok(())
    }

    /// `promoteTail` - makes the last normal entry of the given page the tail
    /// entry on that page.
    fn promote_tail(
        &mut self,
        ch: &mut PageChannel,
        page_number: i32,
        last_page_number: i32,
    ) -> Result<()> {
        // there's no tail currently on this page, make last entry a tail
        self.set_modified(page_number)?;

        // make sure the child page is loaded (this also resolves its parent
        // info, like the java getChildPage call in promoteTail)
        self.get_page(ch, last_page_number)?.ok_or_else(|| {
            Error::IllegalState(format!("missing child page {}", last_page_number))
        })?;

        // move the "last" normal entry to the tail entry
        self.update_parent_tail(page_number, last_page_number, UpdateType::Add)?;
        let last_entry = self.page_mut(page_number)?.data.view_promote_tail()?;
        {
            let page = self.page_mut(page_number)?;
            page.data.total_entry_size -= last_entry.size();
            page.data.entry_prefix.clear();
        }

        let last = self.page_mut(last_page_number)?;
        last.parent_page_number = Some(page_number);
        last.tail = true;
        Ok(())
    }
}

/// `IndexPageCache.findCommonPrefix`
fn find_common_prefix(e1: &Entry, e2: &Entry) -> Vec<u8> {
    let empty = Vec::new();
    let b1 = e1.get_entry_bytes().unwrap_or(&empty);
    let b2 = e2.get_entry_bytes().unwrap_or(&empty);

    let max_len = b1.len().min(b2.len());
    let mut len = 0;
    while len < max_len && b1[len] == b2[len] {
        len += 1;
    }
    if len == 0 {
        return Vec::new();
    }
    b1[..len].to_vec()
}

/// Compares whether two optional entries are the same object (Java compares
/// entries by reference when deciding whether the last entry changed).
fn same_entry_identity(a: &Option<Entry>, b: &Option<Entry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.id() == y.id(),
        _ => false,
    }
}

fn add_owned_page(
    owned_pages: &mut Option<UsageMap>,
    ch: &mut PageChannel,
    page_number: i32,
) -> Result<()> {
    match owned_pages {
        Some(map) => map.add_page_number(ch, page_number),
        None => Err(Error::IllegalState(
            "index has no owned pages usage map".into(),
        )),
    }
}

fn remove_owned_page(
    owned_pages: &mut Option<UsageMap>,
    ch: &mut PageChannel,
    page_number: i32,
) -> Result<()> {
    match owned_pages {
        Some(map) => map.remove_page_number(ch, page_number),
        None => Err(Error::IllegalState(
            "index has no owned pages usage map".into(),
        )),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytebuf::ByteBuf;
    use crate::index::{DataPage, Entry};

    #[test]
    fn common_prefix() {
        let a = Entry::leaf(vec![1, 2, 3, 4], crate::index::RowId::new(1, 0));
        let b = Entry::leaf(vec![1, 2, 9, 9, 9], crate::index::RowId::new(1, 1));
        assert_eq!(find_common_prefix(&a, &b), vec![1, 2]);
        let c = Entry::leaf(vec![7], crate::index::RowId::new(1, 2));
        assert_eq!(find_common_prefix(&a, &c), Vec::<u8>::new());
        // identical entries
        assert_eq!(find_common_prefix(&a, &a), vec![1, 2, 3, 4]);
    }

    #[test]
    fn cache_page_creation() {
        let format = JetFormat::version_4();
        let cache = IndexPageCache::new(5, None, format);
        assert_eq!(cache.root_page_number, 5);
        assert!(!cache.initialized);
        assert_eq!(cache.max_page_entry_size, 3616);
        let _ = ByteBuf::allocate(10);
        let _ = DataPage::new_root();
    }

    fn fake_cache_page(page_number: i32) -> CachePage {
        CachePage {
            data: DataPage::new(page_number, true, 0),
            parent_page_number: None,
            tail: false,
            modified: false,
        }
    }

    /// The index page cache reproduces Java's `_dataPages`, an access ordered
    /// `LinkedHashMap` whose `removeEldestEntry` purges the least recently used
    /// pages (never the root page) once it holds more than `MAX_CACHE_SIZE`
    /// pages.  The eviction order decides which pages are re-read later, and a
    /// re-read leaves its image in the reused page buffer, so it has to be
    /// exact.
    #[test]
    fn cache_purges_least_recently_used_pages() {
        let format = JetFormat::version_4();
        let mut cache = IndexPageCache::new(5, None, format);
        for page_number in 1..=30 {
            cache
                .pages
                .insert(page_number, fake_cache_page(page_number));
            cache.page_order.push(page_number);
        }

        cache.purge_old_pages();
        assert_eq!(cache.pages.len(), MAX_CACHE_SIZE);
        assert_eq!(cache.page_order.len(), MAX_CACHE_SIZE);
        // the five eldest pages are dropped, but the root page (5) is kept
        for page_number in [1, 2, 3, 4, 6] {
            assert!(
                !cache.pages.contains_key(&page_number),
                "page {} should have been purged",
                page_number
            );
        }
        assert!(
            cache.pages.contains_key(&5),
            "the root page is never purged"
        );
        for page_number in 7..=30 {
            assert!(cache.pages.contains_key(&page_number));
        }

        // touching a page makes it the most recently used one, so it survives
        // the next purge while the pages around it are dropped
        cache.touch_page(7);
        assert_eq!(*cache.page_order.last().unwrap(), 7);
        for page_number in 1..=40 {
            if let std::collections::hash_map::Entry::Vacant(e) = cache.pages.entry(page_number) {
                e.insert(fake_cache_page(page_number));
                cache.page_order.push(page_number);
            }
        }
        cache.purge_old_pages();
        assert_eq!(cache.pages.len(), MAX_CACHE_SIZE);
        assert!(cache.pages.contains_key(&7));
        assert!(cache.pages.contains_key(&5));
    }

    /// A page buffer taken from the cache keeps its content (only the
    /// position/limit are reset), like Java's `TempBufferHolder`: the stale
    /// trailing bytes of the previous page end up in the next written page.
    #[test]
    fn page_buffer_keeps_stale_tail_bytes() {
        let format = JetFormat::version_4();
        let page_size = format.page_size;
        let mut cache = IndexPageCache::new(5, None, format);
        let mut buffer = ByteBuf::allocate(page_size);
        buffer.set_position(100).unwrap();
        buffer.put_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]).unwrap();
        cache.put_page_buffer(buffer);

        let taken = cache.take_page_buffer();
        assert_eq!(taken.limit(), page_size);
        assert_eq!(
            taken.get_bytes_at(100, 4).unwrap(),
            vec![0xaa, 0xbb, 0xcc, 0xdd]
        );
        assert_eq!(taken.get_u8_at(4095).unwrap(), 0);
    }

    /// Reads the live index pages of the given roots from a generated file.
    ///
    /// The page numbers are collected by walking each index tree from its root
    /// (following the node entries and the child tail links), so freed pages
    /// which still carry stale index page bytes are not picked up.
    fn read_index_tree(
        path: &std::path::Path,
        format: &JetFormat,
        roots: &[i32],
    ) -> std::collections::HashMap<i32, DataPage> {
        let bytes = std::fs::read(path).unwrap();
        let page_size = format.page_size;
        let mut pages = std::collections::HashMap::new();
        let mut pending: Vec<i32> = roots.to_vec();
        while let Some(page_number) = pending.pop() {
            if pages.contains_key(&page_number) {
                continue;
            }
            let offset = page_number as usize * page_size;
            let page_type = bytes[offset];
            assert!(
                page_type == crate::page_types::LEAF
                    || page_type == crate::page_types::INTERMEDIATE,
                "page {} of an index tree is not an index page (type {})",
                page_number,
                page_type
            );
            let mut buffer = ByteBuf::allocate(page_size);
            buffer
                .put_bytes(&bytes[offset..offset + page_size])
                .unwrap();
            let data = crate::index::read_data_page(&mut buffer, page_number, format).unwrap();
            for entry in &data.entries {
                if let Some(child) = entry.get_sub_page_number() {
                    pending.push(child);
                }
            }
            if data.child_tail_page_number != INVALID_INDEX_PAGE_NUMBER {
                pending.push(data.child_tail_page_number);
            }
            pages.insert(page_number, data);
        }
        pages
    }

    /// The last entry of a page's entry view (a page with a child tail takes it
    /// from the child tail page, recursively).
    fn last_entry_key(
        pages: &std::collections::HashMap<i32, DataPage>,
        page_number: i32,
    ) -> Vec<u8> {
        let page = &pages[&page_number];
        if page.child_tail_page_number != INVALID_INDEX_PAGE_NUMBER {
            return last_entry_key(pages, page.child_tail_page_number);
        }
        page.entries
            .last()
            .expect("leaf page without entries")
            .get_entry_bytes()
            .unwrap()
            .to_vec()
    }

    /// Walks the index tree of one root and checks every structural invariant
    /// the split/merge code must maintain, returning the leaf pages in index
    /// order along with their entry counts.
    fn check_index_tree(
        pages: &std::collections::HashMap<i32, DataPage>,
        root: i32,
    ) -> (Vec<i32>, usize) {
        let _root_page = &pages[&root];
        let mut leaves: Vec<i32> = Vec::new();
        let mut leaf_entries = 0usize;
        let mut stack = vec![root];
        let mut seen = std::collections::HashSet::new();
        while let Some(page_number) = stack.pop() {
            assert!(
                seen.insert(page_number),
                "page {} visited twice",
                page_number
            );
            let page = &pages[&page_number];
            if page.leaf {
                assert_eq!(page.level, 0, "leaf page {} has a level", page_number);
                leaf_entries += page.entries.len();
                leaves.push(page_number);
                continue;
            }
            assert!(page.level >= 1, "node page {} has level 0", page_number);
            assert!(
                !page.entries.is_empty(),
                "node page {} has no entries",
                page_number
            );
            // a node page's entries are ordered and every entry holds the last
            // key of its child page
            for pair in page.entries.windows(2) {
                let a = pair[0].get_entry_bytes().unwrap();
                let b = pair[1].get_entry_bytes().unwrap();
                // duplicate keys are legal in a non unique index, the entries
                // of identical keys are then ordered by their row ids
                assert!(a <= b, "node page {} entries are not ordered", page_number);
            }
            let is_tail_child = |child: i32| page.child_tail_page_number == child;
            // the children are visited in reverse so the leaves come out in
            // index order when popped from the stack
            let mut children: Vec<i32> = page
                .entries
                .iter()
                .map(|entry| entry.get_sub_page_number().unwrap())
                .collect();
            // the child tail page is the last child of the page's entry view
            if page.child_tail_page_number != INVALID_INDEX_PAGE_NUMBER {
                assert!(is_tail_child(page.child_tail_page_number));
                children.push(page.child_tail_page_number);
            }
            for child in children.iter().rev() {
                let child_page = &pages[child];
                assert_eq!(
                    child_page.level,
                    page.level - 1,
                    "child {} of page {} has the wrong level",
                    child,
                    page_number
                );
                stack.push(*child);
            }
            for (idx, entry) in page.entries.iter().enumerate() {
                let child = entry.get_sub_page_number().unwrap();
                assert_eq!(
                    entry.get_entry_bytes().unwrap(),
                    last_entry_key(pages, child).as_slice(),
                    "node entry {} of page {} is not its child's last key",
                    idx,
                    page_number
                );
            }
            if page.child_tail_page_number != INVALID_INDEX_PAGE_NUMBER {
                assert_eq!(page.child_tail_page_number, *children.last().unwrap());
            }
        }
        // the children were pushed in reverse order, so the pop order above is
        // the in-order traversal and the leaves were collected in index order
        assert!(!leaves.is_empty());
        // a single page carries the highest level of this tree (the root)
        let max_level = seen.iter().map(|pn| pages[pn].level).max().unwrap();
        assert_eq!(pages[&root].level, max_level);
        assert_eq!(
            seen.iter()
                .filter(|pn| pages[pn].level == max_level)
                .count(),
            1,
            "root {} is not the only page at its level",
            root
        );
        assert_eq!(
            pages[&leaves[0]].prev_page_number, INVALID_INDEX_PAGE_NUMBER,
            "the first leaf of root {} has a previous page",
            root
        );
        for pair in leaves.windows(2) {
            assert_eq!(
                pages[&pair[0]].next_page_number, pair[1],
                "leaf {} does not link to leaf {}",
                pair[0], pair[1]
            );
            assert_eq!(pages[&pair[1]].prev_page_number, pair[0]);
        }
        assert_eq!(
            pages[leaves.last().unwrap()].next_page_number,
            INVALID_INDEX_PAGE_NUMBER
        );
        // the leaves are ordered by their entry keys
        let mut previous: Option<Vec<u8>> = None;
        for (leaf_idx, leaf) in leaves.iter().enumerate() {
            for entry in &pages[leaf].entries {
                let key = entry.get_entry_bytes().unwrap();
                if let Some(prev) = previous.as_ref() {
                    assert!(
                        *prev <= key.to_vec(),
                        "entries out of order on leaf {}",
                        leaf
                    );
                }
                previous = Some(key.to_vec());
                let _ = leaf_idx;
            }
        }
        (leaves, leaf_entries)
    }

    /// Inserts enough rows (one at a time, so every insert writes the index
    /// pages) into a table with long text keys that each index needs more than
    /// `MAX_CACHE_SIZE` pages, then deletes every third row.  The generated
    /// file is parsed again: the trees must stay balanced and ordered, the
    /// node entries must stay in sync with their children (page splitting,
    /// tail promotion/demotion and the delete/merge path), the leaf counts must
    /// match the row count, and the written free space field must match the
    /// page's entry sizes.
    #[test]
    fn split_and_delete_index_trees_stay_consistent() {
        use crate::column::ColumnSpec;
        use crate::database::Database;
        use crate::datatype::DataType;
        use crate::format::FileFormat;
        use crate::tablecreator::{IndexSpec, TableSpec};
        use crate::value::Value;

        let out_path = test_tmp_dir().join("jackcess_idx_tree.mdb");
        let _ = std::fs::remove_file(&out_path);
        let format = JetFormat::version_4();
        let page_size = format.page_size;

        let spec = TableSpec::new(
            "Big",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("K", DataType::Text).with_length(255),
                ColumnSpec::new("N", DataType::Long),
                ColumnSpec::new("S", DataType::Text).with_length(30),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ))
        .with_index(IndexSpec::new("ByK", vec![("K".to_string(), true)]).unique())
        .with_index(IndexSpec::new(
            "ByKN",
            vec![("K".to_string(), true), ("N".to_string(), false)],
        ))
        .with_index(IndexSpec::new("ByN", vec![("N".to_string(), true)]));

        // 127 character keys built from a deterministic noise sequence: the
        // index key encoding cannot compress them, so every leaf page holds few
        // entries and a single index needs more than MAX_CACHE_SIZE pages
        const ROWS: i32 = 900;
        fn noisy_key(seed: i32, len: usize) -> String {
            let mut state = (seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let alphabet = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&";
            let mut key = String::with_capacity(len);
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                key.push(alphabet[(state % alphabet.len() as u64) as usize] as char);
            }
            key
        }
        let mut db = Database::create(FileFormat::V2003, &out_path).unwrap();
        db.create_table(&spec).unwrap();
        for i in 0..ROWS {
            let row = vec![
                Value::Null,
                Value::Text(noisy_key(i, 127)),
                Value::Long(i % 53),
                Value::Text(format!("s{}", i)),
            ];
            db.add_rows("Big", &[row]).unwrap();
        }
        db.flush().unwrap();
        drop(db);

        let roots: Vec<i32> = {
            let mut db = Database::open_path(&out_path).unwrap();
            let table = db.get_table("Big").unwrap();
            table
                .index_datas
                .iter()
                .map(|index| index.root_page_number)
                .collect()
        };
        assert_eq!(roots.len(), 4, "every index has its own root page");
        let bytes = std::fs::read(&out_path).unwrap();
        let pages = read_index_tree(&out_path, &format, &roots);
        let mut depths = Vec::new();
        let mut page_counts = Vec::new();
        for root in &roots {
            let (leaves, entries) = check_index_tree(&pages, *root);
            assert_eq!(
                entries, ROWS as usize,
                "every row has one entry in every index"
            );
            depths.push(pages[root].level);
            page_counts.push(leaves.len());
        }
        // long keys: the text indexes need more than MAX_CACHE_SIZE leaf pages,
        // so the page cache is purged (and pages are re-read) during a write
        assert!(
            page_counts.iter().any(|count| *count > MAX_CACHE_SIZE),
            "no index has more than {} leaf pages: {:?}",
            MAX_CACHE_SIZE,
            page_counts
        );
        assert!(
            depths.iter().any(|depth| *depth >= 1),
            "the index trees are not deeper than one level: {:?}",
            depths
        );
        // the free space field of every index page matches its entry sizes
        for (page_number, page) in &pages {
            let offset = *page_number as usize * page_size;
            let free = i16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]) as i32;
            let used = (format.offset_index_entry_mask + format.size_index_entry_mask) as i32
                + page.compressed_entry_size();
            assert_eq!(
                free,
                page_size as i32 - used,
                "page {} free space does not match its entries",
                page_number
            );
        }

        // delete every third row: the entries are removed from the leaves (and
        // the leaf chain/keys stay consistent)
        let mut db = Database::open_path(&out_path).unwrap();
        let rows = db.read_rows_with_ids("Big").unwrap();
        for (idx, (row_id, _)) in rows.iter().enumerate() {
            if idx % 3 == 0 {
                db.delete_row("Big", *row_id).unwrap();
            }
        }
        db.flush().unwrap();
        drop(db);

        let live_rows = ROWS - (ROWS + 2) / 3;
        let pages = read_index_tree(&out_path, &format, &roots);
        for root in &roots {
            let (_leaves, entries) = check_index_tree(&pages, *root);
            assert_eq!(
                entries, live_rows as usize,
                "the deleted rows are gone from every index"
            );
        }

        // now delete almost every row: whole leaf pages empty out, are removed
        // from the tree and freed (`handleEmptyPages`/`deleteDataPage`), and the
        // tree collapses
        let mut db = Database::open_path(&out_path).unwrap();
        let id_col = {
            let table = db.get_table("Big").unwrap();
            table.column_index("ID").unwrap_or(0)
        };
        let rows = db.read_rows_with_ids("Big").unwrap();
        let mut kept = 0usize;
        for (row_id, values) in rows {
            let id = match values.as_ref().and_then(|values| values.get(id_col)) {
                Some(Value::Long(id)) => *id,
                _ => -1,
            };
            if id % 100 == 0 {
                kept += 1;
            } else {
                db.delete_row("Big", row_id).unwrap();
            }
        }
        db.flush().unwrap();
        drop(db);
        assert!(kept > 0 && kept < 20, "expected a handful of rows");

        let pages = read_index_tree(&out_path, &format, &roots);
        let mut live_page_counts = Vec::new();
        for root in &roots {
            let (leaves, entries) = check_index_tree(&pages, *root);
            assert_eq!(entries, kept, "only the kept rows are left in every index");
            live_page_counts.push(leaves.len());
        }
        // the emptied leaf pages are no longer part of any tree
        assert!(
            live_page_counts
                .iter()
                .zip(page_counts.iter())
                .any(|(after, before)| after < before),
            "no index leaf page was freed by the deletes: {:?} -> {:?}",
            page_counts,
            live_page_counts
        );
    }

    /// Compares the index pages of a 600 row database (which forces index page
    /// splits) with the pages written by jackcess for the same data.  The
    /// reference file is produced by
    ///
    /// ```text
    /// java -cp "jackcess-5.0.4.jar;java-classes" \
    ///   com.healthmarketscience.jackcess.impl.IdxOracle many out/many.mdb
    /// ```
    ///
    /// which writes `out/many.mdb.pages`; the test is skipped when that file
    /// is absent.
    #[test]
    fn many_rows_match_java_index_pages() {
        use crate::column::ColumnSpec;
        use crate::database::Database;
        use crate::datatype::DataType;
        use crate::datenums::LocalDateTime;
        use crate::format::FileFormat;
        use crate::tablecreator::{IndexSpec, TableSpec};
        use crate::value::Value;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let reference_path = root.join("out").join("j_many.mdb.pages");
        let Ok(reference) = std::fs::read_to_string(&reference_path) else {
            eprintln!(
                "skipping: {} not present (run the IdxOracle 'many' command)",
                reference_path.display()
            );
            return;
        };

        let out_path = test_tmp_dir().join("jackcess_idx_many.mdb");
        let _ = std::fs::remove_file(&out_path);

        let spec = TableSpec::new(
            "Idx",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("L", DataType::Long),
                ColumnSpec::new("D", DataType::Double),
                ColumnSpec::new("DT", DataType::ShortDateTime),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "pidx",
            vec![("ID".to_string(), true)],
        ))
        .with_index(IndexSpec::new("idxL", vec![("L".to_string(), true)]))
        .with_index(IndexSpec::new(
            "idxLD",
            vec![("L".to_string(), true), ("D".to_string(), false)],
        ));

        let mut db = Database::create(FileFormat::V2003, &out_path).unwrap();
        db.create_table(&spec).unwrap();
        let mut rows = Vec::with_capacity(600);
        for i in 0..600i32 {
            let day = LocalDateTime::of_date(2000, 1, 1).to_epoch_day() + (i % 401) as i64;
            rows.push(vec![
                Value::Null,
                Value::Long(((i * 37) % 500) - 250),
                Value::Double(i as f64 * 0.25),
                Value::DateTime(LocalDateTime::from_epoch_day(day)),
            ]);
        }
        db.add_rows("Idx", &rows).unwrap();
        db.flush().unwrap();
        drop(db);

        let bytes = std::fs::read(&out_path).unwrap();
        let page_size = 4096usize;
        let mut actual = String::new();
        for p in 0..(bytes.len() / page_size) {
            let off = p * page_size;
            let ty = bytes[off];
            if ty == crate::page_types::INTERMEDIATE || ty == crate::page_types::LEAF {
                let mut hex = String::with_capacity(page_size * 2);
                for b in &bytes[off..off + page_size] {
                    hex.push_str(&format!("{:02x}", b));
                }
                actual.push_str(&format!("PAGE {} {}\n", p, hex));
            }
        }
        assert_eq!(actual, reference, "index pages differ from jackcess");
    }
}
