//! A `java.nio.ByteBuffer` work-alike.
//!
//! Jackcess uses `ByteBuffer` pervasively (page buffers, table definition
//! buffers, row buffers), with a mix of relative and absolute access and an
//! explicitly settable byte order.  This module provides the same semantics so
//! that the Java code translates directly.

use crate::error::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ByteOrder {
    BigEndian,
    LittleEndian,
}

#[derive(Clone, Debug)]
pub struct ByteBuf {
    /// backing storage; `buf.len()` is the capacity
    buf: Vec<u8>,
    /// read/write position
    pos: usize,
    /// limit (exclusive)
    limit: usize,
    order: ByteOrder,
    /// mark for `reset()`
    mark: Option<usize>,
}

impl ByteBuf {
    /// Equivalent to `ByteBuffer.allocate(size)` followed by setting the byte
    /// order (jackcess always uses little endian for these).
    pub fn allocate(size: usize) -> ByteBuf {
        ByteBuf::allocate_order(size, ByteOrder::LittleEndian)
    }

    pub fn allocate_order(size: usize, order: ByteOrder) -> ByteBuf {
        ByteBuf {
            buf: vec![0u8; size],
            pos: 0,
            limit: size,
            order,
            mark: None,
        }
    }

    /// Equivalent to `ByteBuffer.wrap(bytes).order(...)`; limit == len.
    pub fn wrap(bytes: Vec<u8>, order: ByteOrder) -> ByteBuf {
        let len = bytes.len();
        ByteBuf {
            buf: bytes,
            pos: 0,
            limit: len,
            order,
            mark: None,
        }
    }

    pub fn wrap_le(bytes: Vec<u8>) -> ByteBuf {
        ByteBuf::wrap(bytes, ByteOrder::LittleEndian)
    }

    pub fn order(&self) -> ByteOrder {
        self.order
    }

    pub fn set_order(&mut self, order: ByteOrder) {
        self.order = order;
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn set_limit(&mut self, limit: usize) -> Result<()> {
        if limit > self.buf.len() {
            // `Buffer.limit(int)`: "newLimit > capacity: (N > M)"
            return Err(Error::IllegalArgument(format!(
                "newLimit > capacity: ({} > {})",
                limit,
                self.buf.len()
            )));
        }
        self.limit = limit;
        if self.pos > limit {
            self.pos = limit;
        }
        Ok(())
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn set_position(&mut self, pos: usize) -> Result<()> {
        if pos > self.limit {
            // `Buffer.position(int)`: "newPosition > limit: (N > M)"
            return Err(Error::IllegalArgument(format!(
                "newPosition > limit: ({} > {})",
                pos, self.limit
            )));
        }
        self.pos = pos;
        Ok(())
    }

    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.pos)
    }

    pub fn has_remaining(&self) -> bool {
        self.remaining() > 0
    }

    pub fn mark(&mut self) {
        self.mark = Some(self.pos);
    }

    pub fn reset(&mut self) -> Result<()> {
        match self.mark {
            Some(m) if m <= self.limit => {
                self.pos = m;
                Ok(())
            }
            _ => Err(Error::IllegalState("invalid mark".into())),
        }
    }

    /// Clear: position 0, limit capacity.
    pub fn clear(&mut self) {
        self.pos = 0;
        self.limit = self.buf.len();
        self.mark = None;
    }

    /// Flip: limit = position, position = 0.
    pub fn flip(&mut self) {
        self.limit = self.pos;
        self.pos = 0;
        self.mark = None;
    }

    /// Rewind: position 0.
    pub fn rewind(&mut self) {
        self.pos = 0;
        self.mark = None;
    }

    /// Advance the position (used where Java code does `buffer.position(p)`).
    ///
    /// A negative target position reports the `Buffer.position(int)` wording
    /// (`newPosition < 0: (N < 0)`), like a position past the limit
    /// (`newPosition > limit: (N > M)`); both are a
    /// `java.lang.IllegalArgumentException` in Java.
    pub fn advance(&mut self, n: i64) -> Result<()> {
        let new_pos = self.pos as i64 + n;
        if new_pos < 0 {
            return Err(Error::IllegalArgument(format!(
                "newPosition < 0: ({} < 0)",
                new_pos
            )));
        }
        if new_pos > self.limit as i64 {
            return Err(Error::IllegalArgument(format!(
                "newPosition > limit: ({} > {})",
                new_pos, self.limit
            )));
        }
        self.pos = new_pos as usize;
        Ok(())
    }

    /// Direct access to the backing array (the `array()` of Java).
    pub fn array(&self) -> &[u8] {
        &self.buf
    }

    pub fn array_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    /// The whole backing storage, regardless of limit.
    pub fn backing(&self) -> &[u8] {
        &self.buf
    }

    /// Duplicate: independent position/limit/order, same contents.
    pub fn dup(&self) -> ByteBuf {
        ByteBuf {
            buf: self.buf.clone(),
            pos: self.pos,
            limit: self.limit,
            order: self.order,
            mark: self.mark,
        }
    }

    fn check_get(&self, n: usize) -> Result<()> {
        if self.pos + n > self.limit {
            return Err(Error::BufferUnderflow(format!(
                "need {} bytes at {} (limit {})",
                n, self.pos, self.limit
            )));
        }
        Ok(())
    }

    fn check_put(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.limit {
            return Err(Error::BufferOverflow(format!(
                "need {} bytes at {} (limit {})",
                n, self.pos, self.limit
            )));
        }
        Ok(())
    }

    fn check_abs_get(&self, offset: usize, n: usize) -> Result<()> {
        if offset + n > self.limit {
            return Err(Error::BufferUnderflow(format!(
                "need {} bytes at offset {} (limit {})",
                n, offset, self.limit
            )));
        }
        Ok(())
    }

    fn check_abs_put(&self, offset: usize, n: usize) -> Result<()> {
        if offset + n > self.limit {
            return Err(Error::BufferOverflow(format!(
                "need {} bytes at offset {} (limit {})",
                n, offset, self.limit
            )));
        }
        Ok(())
    }

    pub fn get_u8(&mut self) -> Result<u8> {
        self.check_get(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn get_i8(&mut self) -> Result<i8> {
        Ok(self.get_u8()? as i8)
    }

    pub fn get_u16(&mut self) -> Result<u16> {
        let v = self.get_u16_at(self.pos)?;
        self.pos += 2;
        Ok(v)
    }

    pub fn get_i16(&mut self) -> Result<i16> {
        Ok(self.get_u16()? as i16)
    }

    pub fn get_u32(&mut self) -> Result<u32> {
        let v = self.get_u32_at(self.pos)?;
        self.pos += 4;
        Ok(v)
    }

    pub fn get_i32(&mut self) -> Result<i32> {
        Ok(self.get_u32()? as i32)
    }

    pub fn get_u64(&mut self) -> Result<u64> {
        let v = self.get_u64_at(self.pos)?;
        self.pos += 8;
        Ok(v)
    }

    pub fn get_i64(&mut self) -> Result<i64> {
        Ok(self.get_u64()? as i64)
    }

    pub fn get_f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.get_u32()?))
    }

    pub fn get_f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.get_u64()?))
    }

    pub fn get_u16_at(&self, offset: usize) -> Result<u16> {
        self.check_abs_get(offset, 2)?;
        let b = &self.buf[offset..offset + 2];
        Ok(match self.order {
            ByteOrder::LittleEndian => u16::from_le_bytes([b[0], b[1]]),
            ByteOrder::BigEndian => u16::from_be_bytes([b[0], b[1]]),
        })
    }

    pub fn get_i16_at(&self, offset: usize) -> Result<i16> {
        Ok(self.get_u16_at(offset)? as i16)
    }

    pub fn get_u32_at(&self, offset: usize) -> Result<u32> {
        self.check_abs_get(offset, 4)?;
        let b = &self.buf[offset..offset + 4];
        Ok(match self.order {
            ByteOrder::LittleEndian => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            ByteOrder::BigEndian => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        })
    }

    pub fn get_i32_at(&self, offset: usize) -> Result<i32> {
        Ok(self.get_u32_at(offset)? as i32)
    }

    pub fn get_u64_at(&self, offset: usize) -> Result<u64> {
        self.check_abs_get(offset, 8)?;
        let b = &self.buf[offset..offset + 8];
        let arr = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        Ok(match self.order {
            ByteOrder::LittleEndian => u64::from_le_bytes(arr),
            ByteOrder::BigEndian => u64::from_be_bytes(arr),
        })
    }

    pub fn get_i64_at(&self, offset: usize) -> Result<i64> {
        Ok(self.get_u64_at(offset)? as i64)
    }

    pub fn get_u8_at(&self, offset: usize) -> Result<u8> {
        self.check_abs_get(offset, 1)?;
        Ok(self.buf[offset])
    }

    pub fn get_i8_at(&self, offset: usize) -> Result<i8> {
        Ok(self.get_u8_at(offset)? as i8)
    }

    pub fn get_f64_at(&self, offset: usize) -> Result<f64> {
        Ok(f64::from_bits(self.get_u64_at(offset)?))
    }

    pub fn get_f32_at(&self, offset: usize) -> Result<f32> {
        Ok(f32::from_bits(self.get_u32_at(offset)?))
    }

    pub fn get_bytes(&mut self, len: usize) -> Result<Vec<u8>> {
        self.check_get(len)?;
        let v = self.buf[self.pos..self.pos + len].to_vec();
        self.pos += len;
        Ok(v)
    }

    pub fn get_bytes_at(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.check_abs_get(offset, len)?;
        Ok(self.buf[offset..offset + len].to_vec())
    }

    /// Read `len` bytes into `out`.
    pub fn read_into(&mut self, out: &mut [u8]) -> Result<()> {
        let len = out.len();
        self.check_get(len)?;
        out.copy_from_slice(&self.buf[self.pos..self.pos + len]);
        self.pos += len;
        Ok(())
    }

    pub fn put_u8(&mut self, v: u8) -> Result<()> {
        self.check_put(1)?;
        self.buf[self.pos] = v;
        self.pos += 1;
        Ok(())
    }

    pub fn put_i8(&mut self, v: i8) -> Result<()> {
        self.put_u8(v as u8)
    }

    pub fn put_u16(&mut self, v: u16) -> Result<()> {
        self.put_u16_at(self.pos, v)?;
        self.pos += 2;
        Ok(())
    }

    pub fn put_i16(&mut self, v: i16) -> Result<()> {
        self.put_u16(v as u16)
    }

    pub fn put_u32(&mut self, v: u32) -> Result<()> {
        self.put_u32_at(self.pos, v)?;
        self.pos += 4;
        Ok(())
    }

    pub fn put_i32(&mut self, v: i32) -> Result<()> {
        self.put_u32(v as u32)
    }

    pub fn put_u64(&mut self, v: u64) -> Result<()> {
        self.put_u64_at(self.pos, v)?;
        self.pos += 8;
        Ok(())
    }

    pub fn put_i64(&mut self, v: i64) -> Result<()> {
        self.put_u64(v as u64)
    }

    pub fn put_f32(&mut self, v: f32) -> Result<()> {
        self.put_u32(v.to_bits())
    }

    pub fn put_f64(&mut self, v: f64) -> Result<()> {
        self.put_u64(v.to_bits())
    }

    pub fn put_bytes(&mut self, v: &[u8]) -> Result<()> {
        self.check_put(v.len())?;
        self.buf[self.pos..self.pos + v.len()].copy_from_slice(v);
        self.pos += v.len();
        Ok(())
    }

    pub fn put_u8_at(&mut self, offset: usize, v: u8) -> Result<()> {
        self.check_abs_put(offset, 1)?;
        self.buf[offset] = v;
        Ok(())
    }

    pub fn put_i8_at(&mut self, offset: usize, v: i8) -> Result<()> {
        self.put_u8_at(offset, v as u8)
    }

    pub fn put_u16_at(&mut self, offset: usize, v: u16) -> Result<()> {
        self.check_abs_put(offset, 2)?;
        let b = match self.order {
            ByteOrder::LittleEndian => v.to_le_bytes(),
            ByteOrder::BigEndian => v.to_be_bytes(),
        };
        self.buf[offset..offset + 2].copy_from_slice(&b);
        Ok(())
    }

    pub fn put_i16_at(&mut self, offset: usize, v: i16) -> Result<()> {
        self.put_u16_at(offset, v as u16)
    }

    pub fn put_u32_at(&mut self, offset: usize, v: u32) -> Result<()> {
        self.check_abs_put(offset, 4)?;
        let b = match self.order {
            ByteOrder::LittleEndian => v.to_le_bytes(),
            ByteOrder::BigEndian => v.to_be_bytes(),
        };
        self.buf[offset..offset + 4].copy_from_slice(&b);
        Ok(())
    }

    pub fn put_i32_at(&mut self, offset: usize, v: i32) -> Result<()> {
        self.put_u32_at(offset, v as u32)
    }

    pub fn put_u64_at(&mut self, offset: usize, v: u64) -> Result<()> {
        self.check_abs_put(offset, 8)?;
        let b = match self.order {
            ByteOrder::LittleEndian => v.to_le_bytes(),
            ByteOrder::BigEndian => v.to_be_bytes(),
        };
        self.buf[offset..offset + 8].copy_from_slice(&b);
        Ok(())
    }

    pub fn put_i64_at(&mut self, offset: usize, v: i64) -> Result<()> {
        self.put_u64_at(offset, v as u64)
    }

    pub fn put_f64_at(&mut self, offset: usize, v: f64) -> Result<()> {
        self.put_u64_at(offset, v.to_bits())
    }

    pub fn put_bytes_at(&mut self, offset: usize, v: &[u8]) -> Result<()> {
        self.check_abs_put(offset, v.len())?;
        self.buf[offset..offset + v.len()].copy_from_slice(v);
        Ok(())
    }

    /// Sets all bytes in the given byte range to the given byte value.
    pub fn put_range(&mut self, start: usize, end: usize, b: u8) -> Result<()> {
        if end > self.limit {
            return Err(Error::BufferOverflow(format!(
                "range {}..{} beyond limit {}",
                start, end, self.limit
            )));
        }
        // an empty range stays a no-op: `fill` panics on an inverted one
        if start < end {
            self.buf[start..end].fill(b);
        }
        Ok(())
    }

    pub fn clear_range(&mut self, start: usize, end: usize) -> Result<()> {
        self.put_range(start, end, 0x00)
    }

    pub fn fill_range(&mut self, start: usize, end: usize) -> Result<()> {
        self.put_range(start, end, 0xff)
    }

    /// `ByteUtil.insertEmptyData`: insert `len` zero bytes at the current
    /// position, moving the rest of the data forward, and grow the limit.
    pub fn insert_empty_data(&mut self, len: usize) -> Result<()> {
        let pos = self.pos;
        let limit = self.limit;
        if limit + len > self.buf.len() {
            return Err(Error::BufferOverflow(format!(
                "insert {} at {} exceeds capacity {}",
                len,
                pos,
                self.buf.len()
            )));
        }
        self.buf.copy_within(pos..limit, pos + len);
        for b in &mut self.buf[pos..pos + len] {
            *b = 0;
        }
        self.limit = limit + len;
        Ok(())
    }

    /// Equivalent to `PageChannel.narrowBuffer(buffer, position, limit)`.
    pub fn narrow(&self, position: usize, limit: usize) -> Result<ByteBuf> {
        if position > limit || limit > self.limit {
            return Err(Error::IllegalArgument(format!(
                "invalid narrow {}..{} (limit {})",
                position, limit, self.limit
            )));
        }
        let mut out = self.dup();
        out.pos = position;
        out.limit = limit;
        out.mark = Some(position);
        Ok(out)
    }

    /// Direct slice of the backing store (as `array()` bytes do).
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.limit]
    }

    pub fn remaining_bytes(&self) -> &[u8] {
        &self.buf[self.pos..self.limit]
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The position/limit failures are the `java.nio.Buffer` ones: the same
    /// `IllegalArgumentException` and the exact Java message
    /// (`Buffer.position(int)` / `Buffer.limit(int)`).
    #[test]
    fn position_errors_use_java_wording() {
        let mut buffer = ByteBuf::allocate(16);
        let err = buffer.set_position(17).expect_err("past the limit");
        assert!(
            matches!(&err, Error::IllegalArgument(message)
                if message == "newPosition > limit: (17 > 16)"),
            "{err:?}"
        );

        let err = buffer.set_limit(17).expect_err("past the capacity");
        assert!(
            matches!(&err, Error::IllegalArgument(message)
                if message == "newLimit > capacity: (17 > 16)"),
            "{err:?}"
        );

        buffer.set_position(4).unwrap();
        buffer.set_limit(8).unwrap();
        // `advance` is Java's `position(current + delta)`
        let err = buffer.advance(5).expect_err("past the limit");
        assert!(
            matches!(&err, Error::IllegalArgument(message)
                if message == "newPosition > limit: (9 > 8)"),
            "{err:?}"
        );
        let err = buffer.advance(-9).expect_err("negative");
        assert!(
            matches!(&err, Error::IllegalArgument(message)
                if message == "newPosition < 0: (-5 < 0)"),
            "{err:?}"
        );
        // a legal move still works
        buffer.advance(-4).unwrap();
        assert_eq!(buffer.position(), 0);
        buffer.advance(8).unwrap();
        assert_eq!(buffer.position(), 8);
    }
}
