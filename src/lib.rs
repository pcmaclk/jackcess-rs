//! Rust port of the Jackcess MS Access database library.
//!
//! This crate mirrors the structure and behaviour of the Java Jackcess
//! implementation (version 5.0.4) closely enough that artifacts produced by
//! the Rust writer are byte-for-byte identical to artifacts produced by the
//! Java library.

pub mod bitset;
pub mod builder;
pub mod bytebuf;
pub mod byteutil;
pub mod codec;
pub mod column;
pub mod columnfmt;
pub mod complex;
#[cfg(feature = "crypto")]
pub mod crypt;
pub mod cursor;
pub mod database;
pub mod datatype;
pub mod datenums;
pub mod deflate;
pub mod embedded;
pub mod error;
pub mod export;
pub mod expr;
pub mod fkenforcer;
pub mod format;
pub mod index;
pub mod indexcodes;
pub mod indexpagecache;
pub mod linked;
pub mod mem;
pub mod namemap;
pub mod ole;
pub mod page_types;
pub mod pagechannel;
pub mod property;
pub mod query;
pub mod relationship;
pub mod row;
pub mod table;
pub mod tablecreator;
pub mod tableupdater;
pub mod usagemap;
pub mod validator;
pub mod value;

pub use error::{Error, Result};
