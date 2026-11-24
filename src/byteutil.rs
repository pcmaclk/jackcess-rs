//! Port of the numeric helpers of `com.healthmarketscience.jackcess.impl.ByteUtil`.

use crate::bytebuf::{ByteBuf, ByteOrder};
use crate::error::Result;

/// Read a 3 byte int using the buffer's byte order (jackcess page buffers are
/// little endian).
pub fn get3_byte_int(buffer: &mut ByteBuf) -> Result<i32> {
    let order = buffer.order();
    let pos = buffer.position();
    let v = get3_byte_int_at(buffer, pos, order)?;
    buffer.set_position(pos + 3)?;
    Ok(v)
}

pub fn get3_byte_int_at(buffer: &ByteBuf, offset: usize, order: ByteOrder) -> Result<i32> {
    match order {
        ByteOrder::LittleEndian => {
            let a = buffer.get_u8_at(offset)? as i32;
            let b = buffer.get_u8_at(offset + 1)? as i32;
            let c = buffer.get_u8_at(offset + 2)? as i32;
            Ok(a | (b << 8) | (c << 16))
        }
        ByteOrder::BigEndian => {
            let a = buffer.get_u8_at(offset)? as i32;
            let b = buffer.get_u8_at(offset + 1)? as i32;
            let c = buffer.get_u8_at(offset + 2)? as i32;
            Ok((a << 16) | (b << 8) | c)
        }
    }
}

pub fn put3_byte_int(buffer: &mut ByteBuf, val: i32) -> Result<()> {
    let order = buffer.order();
    let pos = buffer.position();
    put3_byte_int_at(buffer, val, pos, order)?;
    buffer.set_position(pos + 3)?;
    Ok(())
}

pub fn put3_byte_int_at(
    buffer: &mut ByteBuf,
    val: i32,
    offset: usize,
    order: ByteOrder,
) -> Result<()> {
    match order {
        ByteOrder::LittleEndian => {
            buffer.put_u8_at(offset, (val & 0xFF) as u8)?;
            buffer.put_u8_at(offset + 1, ((val >> 8) & 0xFF) as u8)?;
            buffer.put_u8_at(offset + 2, ((val >> 16) & 0xFF) as u8)?;
        }
        ByteOrder::BigEndian => {
            buffer.put_u8_at(offset + 2, (val & 0xFF) as u8)?;
            buffer.put_u8_at(offset + 1, ((val >> 8) & 0xFF) as u8)?;
            buffer.put_u8_at(offset, ((val >> 16) & 0xFF) as u8)?;
        }
    }
    Ok(())
}

/// `ByteUtil.getUnsignedVarInt(buffer, numBytes)`
pub fn get_unsigned_var_int(buffer: &mut ByteBuf, num_bytes: usize) -> Result<i32> {
    let pos = buffer.position();
    let v = match num_bytes {
        1 => buffer.get_u8_at(pos)? as i32,
        2 => buffer.get_u16_at(pos)? as i32,
        3 => get3_byte_int_at(buffer, pos, buffer.order())?,
        4 => buffer.get_i32_at(pos)?,
        _ => {
            return Err(crate::Error::IllegalArgument(format!(
                "Invalid num bytes {}",
                num_bytes
            )))
        }
    };
    buffer.set_position(pos + num_bytes)?;
    Ok(v)
}

pub fn copy_of(src: &[u8], len: usize) -> Vec<u8> {
    src[..len.min(src.len())].to_vec()
}
