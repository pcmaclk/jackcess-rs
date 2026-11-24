//! The page-codec traits (`CodecHandler`, `CodecProvider`) and the plain
//! default provider.
//!
//! These live outside `crypt` so a build with `--no-default-features` (no
//! `crypto`) still opens unencoded databases: `PageChannel` always has a
//! handler, and `DefaultCodecProvider` accepts unencoded pages and rejects
//! encoded ones exactly like Jackcess's own default provider.

use crate::bytebuf::ByteBuf;
use crate::error::{Error, Result};
use crate::format::CodecType;
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
    /// out-of-place case.  The default copies the input and decodes in place.
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
