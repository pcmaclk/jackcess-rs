//! Port of jackcess-encrypt's `BaseCryptCodecHandler` support methods and the
//! codec interfaces re-exported from [`crate::codec`].

pub use crate::codec::{
    CodecHandler, CodecProvider, DefaultCodecProvider, DummyHandler, UnsupportedHandler,
    DECODE_NOT_SUPPORTED, ENCODE_NOT_SUPPORTED,
};

use crate::bytebuf::ByteBuf;
use crate::error::Result;
use crate::format::JetFormat;
use crate::pagechannel::PageChannel;

// ---------------------------------------------------------------------------
// `BaseCryptCodecHandler` support methods
// ---------------------------------------------------------------------------

/// `BaseCryptCodecHandler.KEY_MARKER_BITS`: the bits ms access sets in every
/// page 0 key it writes.
const KEY_MARKER_BITS: u32 = 0xa0a0_0000;

/// `BaseCryptCodecHandler.applyPageNumber`: returns a copy of the given key
/// with the bytes of the given page number applied at the given offset using
/// XOR.
pub fn apply_page_number(key: &[u8], offset: usize, page_number: i32) -> Vec<u8> {
    let mut tmp = key.to_vec();
    let pn = (page_number as u32).to_le_bytes();
    // Java writes the page number over the key bytes (`putInt`), then XORs
    // the original key bytes back in, which leaves `key[i] ^ pageNumber[i]`.
    for i in 0..4 {
        tmp[offset + i] ^= pn[i];
    }
    tmp
}

/// `BaseCryptCodecHandler.applyHeaderMask`: removes the header mask from page
/// 0, or puts it back (the mask is XOR, so the two are one operation).
pub fn apply_header_mask(page: &mut ByteBuf, format: &JetFormat) -> Result<()> {
    let mask = format.header_mask;
    for (i, m) in mask.iter().enumerate() {
        let pos = format.offset_masked_header + i;
        let b = page.get_u8_at(pos)? ^ m;
        page.put_u8_at(pos, b)?;
    }
    Ok(())
}

/// `BaseCryptCodecHandler.readHeaderPage`: reads page 0 (header mask removed).
pub fn read_header_page(page_channel: &mut PageChannel) -> Result<ByteBuf> {
    let mut buffer = page_channel.create_page_buffer();
    page_channel.read_page(&mut buffer, 0)?;
    Ok(buffer)
}

/// `DatabaseImpl.getPasswordMask`: the extra password mask generated from the
/// database creation date, or `None` for a format which does not use one.
pub fn get_password_mask(buffer: &ByteBuf, format: &JetFormat) -> Result<Option<[u8; 4]>> {
    if format.offset_header_date < 0 {
        return Ok(None);
    }
    let date_val = buffer.get_f64_at(format.offset_header_date as usize)?;
    // Java `(int)dateVal`: truncate, saturating, NaN becomes 0
    Ok(Some((date_val as i32).to_le_bytes()))
}

/// `BaseCryptCodecHandler.readPasswordRegion`: the password field followed by
/// the same number of bytes again, with the creation date mask removed from
/// the password field (only the field, not the bytes which follow it).
pub fn read_password_region(buffer: &ByteBuf, format: &JetFormat) -> Result<Vec<u8>> {
    let mut region = buffer.get_bytes_at(format.offset_password, format.size_password * 2)?;
    if let Some(mask) = get_password_mask(buffer, format)? {
        for i in 0..format.size_password {
            region[i] ^= mask[i % mask.len()];
        }
    }
    Ok(region)
}

/// `BaseCryptCodecHandler.isBlankKey`.
pub fn is_blank_key(key: &[u8]) -> bool {
    key.iter().all(|&b| b == 0)
}

/// `BaseCryptCodecHandler.createEncodingKey`: a page 0 key of the shape ms
/// access writes (a random value with the `0xa0a00000` marker bits set).
///
/// Java uses `SecureRandom`.  This port has no cryptographic random source in
/// its dependency set and the key is stored in the file anyway (the scheme
/// keeps nothing secret), so the value comes from the standard library's
/// randomly seeded hasher plus the clock and the process id.
pub fn create_encoding_key() -> i32 {
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    (h.finish() as u32 | KEY_MARKER_BITS) as i32
}

/// Port of `KeyCache`: keeps the last few computed per-page cipher parameters.
/// The cache is an LRU of five entries, like the Java `KeyMap`.
pub struct KeyCache {
    entries: std::collections::VecDeque<(i32, Vec<u8>)>,
}

impl KeyCache {
    const MAX_KEY_CACHE_SIZE: usize = 5;

    pub fn new() -> KeyCache {
        KeyCache {
            entries: std::collections::VecDeque::new(),
        }
    }

    /// `KeyCache.get`: computes the entry for the page on a miss and keeps it.
    pub fn get<F>(&mut self, page_number: i32, compute: F) -> &[u8]
    where
        F: FnOnce() -> Vec<u8>,
    {
        if let Some(pos) = self.entries.iter().position(|(p, _)| *p == page_number) {
            // access order: the used entry moves to the back
            if let Some(entry) = self.entries.remove(pos) {
                self.entries.push_back(entry);
            }
        } else {
            let key = compute();
            self.entries.push_back((page_number, key));
            if self.entries.len() > KeyCache::MAX_KEY_CACHE_SIZE {
                self.entries.pop_front();
            }
        }
        let (_, key) = self.entries.back().unwrap();
        key
    }
}

impl Default for KeyCache {
    fn default() -> Self {
        KeyCache::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BaseCryptCodecHandler.applyPageNumber` on a known key, checked
    /// against the Java expression (`putInt` then byte-wise XOR).
    #[test]
    fn page_number_is_xored_over_the_key_bytes() {
        let key = [0xABu8, 0xCD, 0x34, 0x12];
        assert_eq!(apply_page_number(&key, 0, 0), key.to_vec());
        assert_eq!(apply_page_number(&key, 0, 1), vec![0xAA, 0xCD, 0x34, 0x12]);
        assert_eq!(
            apply_page_number(&key, 0, 0x1234),
            vec![0xAB ^ 0x34, 0xCD ^ 0x12, 0x34, 0x12]
        );
        // a page number above 0x7f exercises the sign bits of the int
        assert_eq!(
            apply_page_number(&key, 0, -1),
            vec![0xAB ^ 0xFF, 0xCD ^ 0xFF, 0x34 ^ 0xFF, 0x12 ^ 0xFF]
        );
    }

    /// The key cache returns the value computed for the page and keeps
    /// returning it for that page.
    #[test]
    fn key_cache_hits_and_evicts() {
        let mut cache = KeyCache::new();
        for p in 0..7 {
            let key = cache.get(p, || vec![p as u8; 4]).to_vec();
            assert_eq!(key, vec![p as u8; 4]);
        }
        // the oldest two entries were evicted, the rest still match
        let mut calls = 0;
        let key = cache
            .get(5, || {
                calls += 1;
                vec![0xFF; 4]
            })
            .to_vec();
        assert_eq!(calls, 0, "page 5 is still cached");
        assert_eq!(key, vec![5; 4]);
        let key = cache
            .get(0, || {
                calls += 1;
                vec![0xEE; 4]
            })
            .to_vec();
        assert_eq!(calls, 1, "page 0 was evicted");
        assert_eq!(key, vec![0xEE; 4]);
    }
}
