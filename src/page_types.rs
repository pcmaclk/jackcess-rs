//! Port of `PageTypes`.

pub const INVALID: u8 = 0x00;
pub const DATA: u8 = 0x01;
pub const TABLE_DEF: u8 = 0x02;
pub const INTERMEDIATE: u8 = 0x03;
pub const LEAF: u8 = 0x04;
pub const USAGE_MAP: u8 = 0x05;

/// `RowIdImpl` page number bounds used by cursor traversals.
pub const FIRST_PAGE_NUMBER: i32 = -1;
pub const LAST_PAGE_NUMBER: i32 = -2;
pub const INVALID_PAGE_NUMBER: i32 = -1;
