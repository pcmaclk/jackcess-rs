//! Port of `com.healthmarketscience.jackcess.crypt.impl.JetCryptCodecHandler`
//! and its `BaseJetCryptCodecHandler` base class.
//!
//! Every page above page 0 is run through RC4 under the encoding key xored
//! with the page number.  Page 0 is never encoded and holds the key in the
//! clear at [`JetFormat::offset_encoding_key`], so the scheme keeps nothing
//! secret and stops only a reader which does not know about it.

use crate::bytebuf::ByteBuf;
use crate::crypt::codec::{
    apply_page_number, is_blank_key, read_header_page, CodecHandler, DummyHandler, KeyCache,
};
use crate::error::Result;
use crate::pagechannel::PageChannel;
use rc4::{KeyInit, Rc4, StreamCipher};

/// `JetCryptCodecHandler.ENCODING_KEY_LENGTH`.
pub const ENCODING_KEY_LENGTH: usize = 0x4;

/// Port of `com.healthmarketscience.jackcess.crypt.impl.JetCryptCodecHandler`.
pub struct JetCryptCodecHandler {
    encoding_key: Vec<u8>,
    param_cache: KeyCache,
    /// `BaseJetCryptCodecHandler.getMaxEncodedPage`: the last page the handler
    /// transforms.  The jet page encoding covers every page above page 0
    /// (`Integer.MAX_VALUE`); the old MSISAM encoding covers only pages 1
    /// through 14 (`MSISAMCryptCodecHandler.MSISAM_MAX_ENCRYPTED_PAGE`).
    max_encoded_page: i32,
}

impl JetCryptCodecHandler {
    /// `JetCryptCodecHandler.create`: reads the encoding key from page 0 and
    /// returns the real handler, or the dummy handler when the key is blank
    /// (the database is not encoded).
    pub fn create(channel: &mut PageChannel) -> Result<Box<dyn CodecHandler>> {
        let buffer = read_header_page(channel)?;
        let format = channel.format.clone();
        let encoding_key = buffer.get_bytes_at(format.offset_encoding_key, ENCODING_KEY_LENGTH)?;
        Ok(if is_blank_key(&encoding_key) {
            Box::new(DummyHandler)
        } else {
            Box::new(JetCryptCodecHandler::new(encoding_key))
        })
    }

    pub fn new(encoding_key: Vec<u8>) -> JetCryptCodecHandler {
        JetCryptCodecHandler::with_max_encoded_page(encoding_key, i32::MAX)
    }

    /// The `JetCryptCodecHandler` constructor with an explicit
    /// `getMaxEncodedPage` override: the old MSISAM encoding is this same jet
    /// page transform stopped at page 14.
    pub fn with_max_encoded_page(
        encoding_key: Vec<u8>,
        max_encoded_page: i32,
    ) -> JetCryptCodecHandler {
        JetCryptCodecHandler {
            encoding_key,
            param_cache: KeyCache::new(),
            max_encoded_page,
        }
    }

    pub fn encoding_key(&self) -> &[u8] {
        &self.encoding_key
    }

    /// `BaseJetCryptCodecHandler.maxEncodedPage` (the page range the handler
    /// transforms).
    pub fn max_encoded_page(&self) -> i32 {
        self.max_encoded_page
    }

    /// `BaseJetCryptCodecHandler.isEncryptedPage`: page 0 is never encoded and
    /// the handler covers pages up to `getMaxEncodedPage` (every page above 0
    /// for the jet encoding, only the first 14 for old MSISAM).
    fn is_encrypted_page(&self, page_number: i32) -> bool {
        page_number > 0 && page_number <= self.max_encoded_page
    }

    /// `BaseJetCryptCodecHandler.computeCipherParams`: the key for the page.
    fn page_key(&mut self, page_number: i32) -> Vec<u8> {
        let key = &self.encoding_key;
        self.param_cache
            .get(page_number, || apply_page_number(key, 0, page_number))
            .to_vec()
    }

    /// `BaseCryptCodecHandler.streamDecrypt` / `streamEncrypt` for RC4: RC4 is
    /// its own inverse, so one routine covers both directions.
    fn apply_keystream(&mut self, data: &mut [u8], page_number: i32) {
        let key = self.page_key(page_number);
        let mut cipher = Rc4::new_from_slice(&key).expect("4 byte keys are supported");
        cipher.apply_keystream(data);
    }
}

impl CodecHandler for JetCryptCodecHandler {
    fn decode_page_inline(&mut self, page: &mut ByteBuf, page_number: i32) -> Result<()> {
        if !self.is_encrypted_page(page_number) {
            // not encoded
            return Ok(());
        }
        let limit = page.limit();
        self.apply_keystream(&mut page.array_mut()[..limit], page_number);
        Ok(())
    }

    fn encode_page(
        &mut self,
        page: &ByteBuf,
        page_number: i32,
        _page_offset: usize,
    ) -> Result<ByteBuf> {
        if !self.is_encrypted_page(page_number) {
            // not encoded
            return Ok(page.dup());
        }
        // note, the stream always starts at offset 0 so that the cipher is
        // applied to the correct part of the stream, exactly like
        // `BaseCryptCodecHandler.streamEncrypt`
        let limit = page.limit();
        let mut out = page.dup();
        self.apply_keystream(&mut out.array_mut()[..limit], page_number);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `getMaxEncodedPage` bounds the transform: the old MSISAM encoding is
    /// this handler with the bound at page 14, so page 15 and above stay clear.
    #[test]
    fn max_encoded_page_bounds_the_transform() {
        let page = ByteBuf::wrap_le(vec![7u8; 16]);
        let mut handler = JetCryptCodecHandler::with_max_encoded_page(vec![1, 2, 3, 4], 4);
        assert_eq!(handler.max_encoded_page(), 4);

        let mut decoded = page.dup();
        handler.decode_page_inline(&mut decoded, 4).unwrap();
        assert_ne!(decoded.bytes(), page.bytes(), "page 4 is transformed");

        let mut decoded = page.dup();
        handler.decode_page_inline(&mut decoded, 5).unwrap();
        assert_eq!(decoded.bytes(), page.bytes(), "page 5 is clear");
        let encoded = handler.encode_page(&page, 5, 0).unwrap();
        assert_eq!(encoded.bytes(), page.bytes());

        // the default constructor keeps the jet behaviour (every page above 0)
        let handler = JetCryptCodecHandler::new(vec![1, 2, 3, 4]);
        assert_eq!(handler.max_encoded_page(), i32::MAX);
    }

    /// The page transform pinned against a fixture produced by the Java
    /// `JetEncodingHandler.cryptPage` (`java-src/CryptFixture.java`): a page
    /// whose byte `i` is `i * 31 + 5`, transformed under key `0x1234abcd` on
    /// page 1.
    #[test]
    fn page_transform_matches_the_java_fixture() {
        let mut plain = vec![0u8; 4096];
        for (i, b) in plain.iter_mut().enumerate() {
            *b = (i.wrapping_mul(31).wrapping_add(5)) as u8;
        }
        let mut handler = JetCryptCodecHandler::new(0x1234_abcdu32.to_le_bytes().to_vec());
        let mut page = ByteBuf::wrap_le(plain.clone());
        handler.decode_page_inline(&mut page, 1).unwrap();
        assert_eq!(
            &page.bytes()[..16],
            &[
                0xe8, 0xe8, 0x54, 0x24, 0xa0, 0x86, 0x90, 0x0f, 0x74, 0xc7, 0xd7, 0x6e, 0x51, 0x39,
                0x22, 0x6a
            ]
        );
        assert_eq!(
            &page.bytes()[4080..],
            &[
                0x00, 0x06, 0x10, 0xd8, 0xc5, 0x17, 0xe3, 0x9d, 0x4d, 0xd9, 0x5c, 0x54, 0x68, 0xd1,
                0xcd, 0xf8
            ]
        );
        // the transform is its own inverse
        handler.decode_page_inline(&mut page, 1).unwrap();
        assert_eq!(page.bytes(), &plain[..]);
    }

    /// The same transform through `encode_page` (which the write path uses)
    /// produces the same bytes.
    #[test]
    fn encode_page_matches_the_decode_transform() {
        let mut plain = vec![0u8; 4096];
        for (i, b) in plain.iter_mut().enumerate() {
            *b = (i.wrapping_mul(31).wrapping_add(5)) as u8;
        }
        let mut handler = JetCryptCodecHandler::new(0x1234_abcdu32.to_le_bytes().to_vec());
        let input = ByteBuf::wrap_le(plain.clone());
        let encoded = handler.encode_page(&input, 1, 0).unwrap();
        let mut decoded = ByteBuf::wrap_le(plain.clone());
        handler.decode_page_inline(&mut decoded, 1).unwrap();
        assert_eq!(encoded.bytes(), decoded.bytes());
        // a partial page encode starts at offset 0 of the stream, so the bytes
        // at a given position are the same as in the full page encode
        let partial = handler.encode_page(&input, 1, 1000).unwrap();
        assert_eq!(partial.bytes(), encoded.bytes());
    }

    /// Page 0 is never transformed, and `encode_page` leaves it alone.
    #[test]
    fn page_zero_is_never_encoded() {
        let mut handler = JetCryptCodecHandler::new(vec![1, 2, 3, 4]);
        let page = ByteBuf::wrap_le(vec![9u8; 8]);
        let encoded = handler.encode_page(&page, 0, 0).unwrap();
        assert_eq!(encoded.bytes(), page.bytes());
        let mut decoded = page.dup();
        handler.decode_page_inline(&mut decoded, 0).unwrap();
        assert_eq!(decoded.bytes(), page.bytes());
    }

    /// `apply_page_number` (which the codec uses) equals `putInt(key ^ page)`
    /// for a range of page numbers, which is how `JetEncodingHandler` builds
    /// the same key.
    #[test]
    fn codec_key_equals_the_encoding_handler_key() {
        for key in [0x1234_abcdu32, 0xa0a0_0001, 0xffff_ffff] {
            for page in [1i32, 2, 7, 4095, 0x7fff_ffff] {
                let expected = (key ^ (page as u32)).to_le_bytes();
                assert_eq!(
                    apply_page_number(&key.to_le_bytes(), 0, page),
                    expected.to_vec()
                );
            }
        }
    }
}
