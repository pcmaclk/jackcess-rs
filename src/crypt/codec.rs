//! Port of `com.healthmarketscience.jackcess.impl.CodecHandler`,
//! `CodecProvider` and `DefaultCodecProvider`, plus the common support methods
//! of jackcess-encrypt's `BaseCryptCodecHandler`.

use crate::bytebuf::ByteBuf;
use crate::error::{Error, Result};
use crate::format::{CodecType, JetFormat};
use crate::pagechannel::PageChannel;

/// Port of `com.healthmarketscience.jackcess.impl.CodecHandler`.
///
/// A handler can encode and decode one kind of database page encoding.  The
/// methods mirror the Java interface exactly, with the two "inline" variants
/// spelled as separate methods because Rust cannot pass the same buffer as
/// both `&` and `&mut` (Java passes the same `ByteBuffer` twice when
/// `canDecodeInline()` is `true`).
pub trait CodecHandler {
    /// `true` if this handler can encode partial pages.  When `false`,
    /// [`PageChannel`] never calls [`CodecHandler::encode_page`] with a
    /// non-zero `page_offset`: it merges the modified part into the current
    /// page contents first, exactly like the Java `PageChannel.writePage`.
    fn can_encode_partial_page(&self) -> bool {
        true
    }

    /// `true` if this handler can decode a page in place.  When `false`,
    /// [`PageChannel`] reads the page into a separate buffer and calls
    /// [`CodecHandler::decode_page`].
    fn can_decode_inline(&self) -> bool {
        true
    }

    /// `CodecHandler.decodePage` for the in-place case.
    fn decode_page_inline(&mut self, page: &mut ByteBuf, page_number: i32) -> Result<()>;

    /// `CodecHandler.decodePage(inPage, outPage, pageNumber)` for the
    /// out-of-place case.  The default copies the input and decodes in place,
    /// which is what a handler which cannot decode inline has to do anyway.
    fn decode_page(
        &mut self,
        in_page: &ByteBuf,
        out_page: &mut ByteBuf,
        page_number: i32,
    ) -> Result<()> {
        *out_page = in_page.dup();
        self.decode_page_inline(out_page, page_number)
    }

    /// `CodecHandler.encodePage`: returns the encoded page buffer.  The
    /// returned buffer is used immediately and discarded.
    fn encode_page(
        &mut self,
        page: &ByteBuf,
        page_number: i32,
        page_offset: usize,
    ) -> Result<ByteBuf>;
}

/// Port of `com.healthmarketscience.jackcess.impl.CodecProvider`.
pub trait CodecProvider {
    /// Returns a new handler for the database of the given channel.  The
    /// handler may read the header page (page 0) while it is being created,
    /// which is how the Jet codec finds its encoding key.
    fn create_handler(
        &self,
        channel: &mut PageChannel,
        charset: crate::format::Charset,
    ) -> Result<Box<dyn CodecHandler>>;
}

/// `DefaultCodecProvider.DUMMY_HANDLER`: does nothing.
pub struct DummyHandler;

impl CodecHandler for DummyHandler {
    fn decode_page_inline(&mut self, _page: &mut ByteBuf, _page_number: i32) -> Result<()> {
        Ok(())
    }

    fn encode_page(
        &mut self,
        page: &ByteBuf,
        _page_number: i32,
        _page_offset: usize,
    ) -> Result<ByteBuf> {
        Ok(page.dup())
    }
}

/// `DefaultCodecProvider.UNSUPPORTED_HANDLER`: always throws.
pub struct UnsupportedHandler;

pub const DECODE_NOT_SUPPORTED: &str = "Decoding not supported.  Please choose \
     a CodecProvider which supports reading the current database encoding.";
pub const ENCODE_NOT_SUPPORTED: &str = "Encoding not supported.  Please choose \
     a CodecProvider which supports writing the current database encoding.";

impl CodecHandler for UnsupportedHandler {
    fn decode_page_inline(&mut self, _page: &mut ByteBuf, _page_number: i32) -> Result<()> {
        Err(Error::Jackcess(DECODE_NOT_SUPPORTED.into()))
    }

    fn encode_page(
        &mut self,
        _page: &ByteBuf,
        _page_number: i32,
        _page_offset: usize,
    ) -> Result<ByteBuf> {
        Err(Error::Jackcess(ENCODE_NOT_SUPPORTED.into()))
    }
}

/// Port of `com.healthmarketscience.jackcess.impl.DefaultCodecProvider`: no
/// encoding support.  Databases with no page encoding open fine; an encoded
/// database is rejected the way the Java default provider rejects it.
pub struct DefaultCodecProvider;

impl DefaultCodecProvider {
    pub const INSTANCE: DefaultCodecProvider = DefaultCodecProvider;
}

impl CodecProvider for DefaultCodecProvider {
    fn create_handler(
        &self,
        channel: &mut PageChannel,
        _charset: crate::format::Charset,
    ) -> Result<Box<dyn CodecHandler>> {
        let format = channel.format.clone();
        match format.codec_type {
            CodecType::None => Ok(Box::new(DummyHandler)),
            CodecType::Jet | CodecType::Office => {
                // check for an encode key.  if 0, not encoded
                let mut bb = channel.create_page_buffer();
                channel.read_root_page(&mut bb)?;
                let codec_key = bb.get_i32_at(format.offset_encoding_key)?;
                if codec_key == 0 {
                    Ok(Box::new(DummyHandler))
                } else {
                    Ok(Box::new(UnsupportedHandler))
                }
            }
            // always encoded, we don't handle it
            CodecType::Msisam => Ok(Box::new(UnsupportedHandler)),
        }
    }
}

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
