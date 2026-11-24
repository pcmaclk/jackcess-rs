//! Port of Jackcess' query model and SQL rendering
//! (`com.healthmarketscience.jackcess.query.Query` and the
//! `com.healthmarketscience.jackcess.impl.query.*` classes).
//!
//! A [`Query`] is built from the rows of the `MSysQueries` system table by
//! [`create_query`], mirroring `QueryImpl.create(int, String, List<Row>, int)`.
//! [`Query::to_sql_string`] then renders the same text as
//! `QueryImpl.toSQLString()`, including its quirks: the parameter list is
//! joined with `", "`, names are bracketed only when they hold a non word
//! character, `PARAMETERS` and `WITH OWNERACCESS OPTION` placement, the
//! reverse order of multiple `ON` conditions, the `TOP null` and `PIVOT null`
//! fallbacks, etc.
//!
//! The Rust API uses `snake_case` names; the Java method each one mirrors is
//! given on its documentation.  Where Java throws a runtime exception, the
//! Rust method returns [`Error`]:
//!
//! * `IllegalStateException` (including a `NullPointerException` raised while
//!   composing the SQL) becomes [`Error::IllegalState`],
//! * `UnsupportedOperationException` of an unknown query type becomes
//!   [`Error::Unsupported`].
//!
//! The exception *messages* are not part of the contract (Java formats the
//! offending row lists itself), but the error kind and the SQL text are.
//!
//! `NEWLINE` mirrors `System.lineSeparator()` like the Java code does, so the
//! rendering is byte-identical on the platform which runs it.  The reference
//! corpus under `resources/query_ref` is generated on Windows (where Jackcess
//! is normally used) and stores the actual `\r\n` sequences, escaped.
//!
//! # Database integration (later step)
//!
//! `Database::get_queries()` (`DatabaseImpl.getQueries`) only has to feed this
//! module:
//!
//! 1. Read the system catalog (`MSysObjects`) with the columns `Name`, `Id`,
//!    `Type` and `Flags`, and collect every row whose `Name` is not null and
//!    whose `Type` is 5 (`DatabaseImpl.TYPE_QUERY`).  `Flags` is the
//!    `object_flag` of [`create_query`] and `Id` the `object_id`; a catalog
//!    row with a null `Flags` is skipped (the flags carry the query type).
//! 2. Read every row of `MSysQueries` and map it one to one onto [`QueryRow`]:
//!    `Attribute` -> [`QueryRow::attribute`], `Expression` ->
//!    [`QueryRow::expression`], `Flag` -> [`QueryRow::flag`], `LvExtra` ->
//!    [`QueryRow::lv_extra`], `Name1`/`Name2` -> [`QueryRow::name1`] /
//!    [`QueryRow::name2`], `ObjectId` -> [`QueryRow::object_id`] and `Order` ->
//!    [`QueryRow::order`].  Group the rows by `ObjectId`, ignoring (with a
//!    warning) rows whose id has no catalog row.
//! 3. For each catalog row call `create_query(flags, name, &rows, id)`.
//!
//! That is all: the model, the row sorting (`QueryImpl.sortByOrder`) and every
//! SQL quirk live here.
//!
//! # Query properties and name maps
//!
//! `Query.getProperties()` and `Query.getNameMap()` read the `LvProp` property
//! group of the query's catalog (`MSysObjects`) row, exactly like
//! `QueryImpl.getPropertyMaps()` (`DatabaseImpl.getPropertiesForObject`) and
//! `QueryImpl.getNameMap()` do:
//!
//! * [`Query::get_properties`] is the object level ("default") property map of
//!   that group, and [`Query::get_name_map`] is the [`crate::namemap::
//!   QueryNameMap`] stored in its [`crate::property::QUERY_NAME_MAP_PROP`]
//!   (`"DOL"`) property, or `None` when the query has no name map.
//! * The group is loaded from the database, because a [`Query`] built by
//!   [`create_query`] alone (Java's `QueryImpl.create(String, List, int)`) has
//!   no database.  Use [`Database::get_queries_with_properties`] (the db-bound
//!   equivalent of `Database.getQueries()` for callers which need the
//!   properties) or [`Database::load_query_properties`] for a query built
//!   another way.  Java reads the group lazily, the first time
//!   `getProperties()` is called; the port reads it in those methods, and
//!   stores a decoding error so that [`Query::get_properties`] reports it at
//!   the same call (Java's `getProperties()` throws `IOException`).
//! * [`Query::set_property_maps`] installs a group directly (the port's
//!   `PropertyMaps`), and [`Database::save_query_properties`] stores it back
//!   into the catalog row (Java's `PropertyMaps.save()` plus the
//!   `QueryImpl.propertiesUpdated` notification).
//! * The text dump which `java-src/QueryPropsGen.java` and
//!   `src/bin/queryprops.rs` compare is written by
//!   [`dump_query_properties`].

use std::fmt::Write as _;

use crate::database::{Database, PAGE_SYSTEM_CATALOG, TABLE_SYSTEM_CATALOG};
use crate::error::{Error, Result};
use crate::namemap::QueryNameMap;
use crate::property::{PropertyMap, PropertyMaps, QUERY_NAME_MAP_PROP};
use crate::value::Value;

// ---------------------------------------------------------------------------
// QueryFormat constants
// ---------------------------------------------------------------------------

/// Object flags which select the concrete query class (`QueryFormat`).
pub const SELECT_QUERY_OBJECT_FLAG: i32 = 0;
pub const MAKE_TABLE_QUERY_OBJECT_FLAG: i32 = 80;
pub const APPEND_QUERY_OBJECT_FLAG: i32 = 64;
pub const UPDATE_QUERY_OBJECT_FLAG: i32 = 48;
pub const DELETE_QUERY_OBJECT_FLAG: i32 = 32;
pub const CROSS_TAB_QUERY_OBJECT_FLAG: i32 = 16;
pub const DATA_DEF_QUERY_OBJECT_FLAG: i32 = 96;
pub const PASSTHROUGH_QUERY_OBJECT_FLAG: i32 = 112;
pub const UNION_QUERY_OBJECT_FLAG: i32 = 128;

/// Mask which removes the superfluous flags from the object flag.
pub const OBJECT_FLAG_MASK: i32 = 0xF0;

/// `DatabaseImpl.HIDDEN_OBJECT_FLAG`; the bit is tested on the raw object flag
/// by `QueryImpl.isHidden()`.
pub const HIDDEN_OBJECT_FLAG: i32 = 0x08;

/// Column names of `MSysQueries` read into [`QueryRow`].
pub const COL_ATTRIBUTE: &str = "Attribute";
pub const COL_EXPRESSION: &str = "Expression";
pub const COL_FLAG: &str = "Flag";
pub const COL_EXTRA: &str = "LvExtra";
pub const COL_NAME1: &str = "Name1";
pub const COL_NAME2: &str = "Name2";
pub const COL_OBJECTID: &str = "ObjectId";
pub const COL_ORDER: &str = "Order";

/// Values of the `Attribute` column.
pub const START_ATTRIBUTE: u8 = 0;
pub const TYPE_ATTRIBUTE: u8 = 1;
pub const PARAMETER_ATTRIBUTE: u8 = 2;
pub const FLAG_ATTRIBUTE: u8 = 3;
pub const REMOTEDB_ATTRIBUTE: u8 = 4;
pub const TABLE_ATTRIBUTE: u8 = 5;
pub const COLUMN_ATTRIBUTE: u8 = 6;
pub const JOIN_ATTRIBUTE: u8 = 7;
pub const WHERE_ATTRIBUTE: u8 = 8;
pub const GROUPBY_ATTRIBUTE: u8 = 9;
pub const HAVING_ATTRIBUTE: u8 = 10;
pub const ORDERBY_ATTRIBUTE: u8 = 11;
pub const END_ATTRIBUTE: u8 = 255;

/// Flag of the flag row which makes a union `UNION` instead of `UNION ALL`.
pub const UNION_FLAG: i32 = 0x02;

/// `TEXT_FLAG` (`DataType.TEXT`); a text parameter may carry a size.
pub const TEXT_FLAG: i16 = 0x0A;

/// `Name1` of an order-by row which sorts descending.
pub const DESCENDING_FLAG: &str = "D";

/// Flags of the flag row of a select query.
pub const SELECT_STAR_SELECT_TYPE: i32 = 0x01;
pub const DISTINCT_SELECT_TYPE: i32 = 0x02;
pub const OWNER_ACCESS_SELECT_TYPE: i32 = 0x04;
pub const DISTINCT_ROW_SELECT_TYPE: i32 = 0x08;
pub const TOP_SELECT_TYPE: i32 = 0x10;
pub const PERCENT_SELECT_TYPE: i32 = 0x20;

/// Flag of an append column row which holds a target value, not a column.
pub const APPEND_VALUE_FLAG: i32 = 0x8000;

/// Flags of a crosstab column (or group-by) row.
pub const CROSSTAB_PIVOT_FLAG: i32 = 0x01;
pub const CROSSTAB_NORMAL_FLAG: i32 = 0x02;

/// `Name2` of the two table rows which hold the text of a union query.
pub const UNION_PART1: &str = "X7YZ_____1";
pub const UNION_PART2: &str = "X7YZ_____2";

/// Returned by `getSelectType()` / `getOwnerAccessType()` when there is no
/// keyword to write.
pub const DEFAULT_TYPE: &str = "";

pub const IDENTIFIER_SEP_CHAR: char = '.';

/// `QueryFormat.NEWLINE` (`System.lineSeparator()`).
pub const NEWLINE: &str = if cfg!(windows) { "\r\n" } else { "\n" };

// ---------------------------------------------------------------------------
// Query.Type
// ---------------------------------------------------------------------------

/// `com.healthmarketscience.jackcess.query.Query.Type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QueryType {
    Select,
    MakeTable,
    Append,
    Update,
    Delete,
    CrossTab,
    DataDefinition,
    Passthrough,
    Union,
    Unknown,
}

impl QueryType {
    /// `Type.getObjectFlag()`.
    pub fn object_flag(self) -> i32 {
        match self {
            QueryType::Select => SELECT_QUERY_OBJECT_FLAG,
            QueryType::MakeTable => MAKE_TABLE_QUERY_OBJECT_FLAG,
            QueryType::Append => APPEND_QUERY_OBJECT_FLAG,
            QueryType::Update => UPDATE_QUERY_OBJECT_FLAG,
            QueryType::Delete => DELETE_QUERY_OBJECT_FLAG,
            QueryType::CrossTab => CROSS_TAB_QUERY_OBJECT_FLAG,
            QueryType::DataDefinition => DATA_DEF_QUERY_OBJECT_FLAG,
            QueryType::Passthrough => PASSTHROUGH_QUERY_OBJECT_FLAG,
            QueryType::Union => UNION_QUERY_OBJECT_FLAG,
            QueryType::Unknown => -1,
        }
    }

    /// `Type.getValue()`.
    pub fn value(self) -> i16 {
        match self {
            QueryType::Select => 1,
            QueryType::MakeTable => 2,
            QueryType::Append => 3,
            QueryType::Update => 4,
            QueryType::Delete => 5,
            QueryType::CrossTab => 6,
            QueryType::DataDefinition => 7,
            QueryType::Passthrough => 8,
            QueryType::Union => 9,
            QueryType::Unknown => -1,
        }
    }

    /// `Type.name()`.
    pub fn name(self) -> &'static str {
        match self {
            QueryType::Select => "SELECT",
            QueryType::MakeTable => "MAKE_TABLE",
            QueryType::Append => "APPEND",
            QueryType::Update => "UPDATE",
            QueryType::Delete => "DELETE",
            QueryType::CrossTab => "CROSS_TAB",
            QueryType::DataDefinition => "DATA_DEFINITION",
            QueryType::Passthrough => "PASSTHROUGH",
            QueryType::Union => "UNION",
            QueryType::Unknown => "UNKNOWN",
        }
    }

    /// The `TYPE_MAP` lookup used by `QueryImpl.createQuery`; `Unknown` itself
    /// is not in the map.
    fn from_value(value: i16) -> Option<QueryType> {
        Some(match value {
            1 => QueryType::Select,
            2 => QueryType::MakeTable,
            3 => QueryType::Append,
            4 => QueryType::Update,
            5 => QueryType::Delete,
            6 => QueryType::CrossTab,
            7 => QueryType::DataDefinition,
            8 => QueryType::Passthrough,
            9 => QueryType::Union,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Input model
// ---------------------------------------------------------------------------

/// One row of the `MSysQueries` system table, as read by
/// `QueryImpl.Row(com.healthmarketscience.jackcess.Row)`.
///
/// The fields are the columns named by [`COL_ATTRIBUTE`] etc.; `None` means
/// SQL `NULL`, which the Java code distinguishes from an empty string in
/// several places (for instance a `NULL` column expression renders as
/// `null` while an empty one renders as nothing).  `order` holds the raw
/// bytes of the `Order` column.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryRow {
    /// `Attribute` (a `Byte` in Java).
    pub attribute: Option<u8>,
    /// `Expression`.
    pub expression: Option<String>,
    /// `Flag` (a `Short` in Java).
    pub flag: Option<i16>,
    /// `LvExtra` (`Integer`).
    pub lv_extra: Option<i32>,
    /// `Name1`.
    pub name1: Option<String>,
    /// `Name2`.
    pub name2: Option<String>,
    /// `ObjectId` (`Integer`).
    pub object_id: Option<i32>,
    /// `Order` (a `byte[]` in Java).
    pub order: Option<Vec<u8>>,
}

/// A query definition, mirroring `com.healthmarketscience.jackcess.query.Query`.
#[derive(Clone, Debug)]
pub struct Query {
    name: String,
    rows: Vec<QueryRow>,
    object_id: i32,
    object_flag: i32,
    query_type: QueryType,
    /// `QueryImpl._propertyMaps`: the property group of this query's catalog
    /// row, with the object level ("default") map created like
    /// `QueryImpl.getProperties()` does through `PropertyMaps.getDefault()`.
    property_maps: PropertyMaps,
    /// The error which decoding the stored property blob reported, or `None`.
    /// Java reads the blob lazily, so a corrupt blob fails `getProperties()`
    /// rather than `Database.getQueries()`; the port keeps the message here so
    /// that [`Query::get_properties`] fails at the same call.
    property_maps_error: Option<String>,
}

/// `QueryImpl.create(int objectFlag, String name, List<Row> rows, int
/// objectId)`.
///
/// Builds a query from the rows of `MSysQueries` alone (no database needed).
/// Like the Java method this only fails for programming errors: an object flag
/// which selects no known query class, or a type row which contradicts it,
/// yields a query of type [`QueryType::Unknown`] whose
/// [`Query::to_sql_string`] returns [`Error::Unsupported`] (the Java code logs
/// a warning and returns `UnknownQueryImpl`).
pub fn create_query(
    object_flag: i32,
    name: &str,
    rows: &[QueryRow],
    object_id: i32,
) -> Result<Query> {
    let rows = sort_by_order(rows);

    // remove other object flags before testing for query type
    let mut obj_type_flag = object_flag & OBJECT_FLAG_MASK;

    if obj_type_flag == 0 {
        // sometimes the query rows tell a different story
        let row_type_flag = rows
            .iter()
            .find(|row| row.attribute == Some(TYPE_ATTRIBUTE))
            .and_then(|row| row.flag)
            .unwrap_or(0);
        if let Some(row_type) = QueryType::from_value(row_type_flag) {
            if row_type.object_flag() != obj_type_flag {
                obj_type_flag = row_type.object_flag();
            }
        }
    }

    let mut query_type = match obj_type_flag {
        SELECT_QUERY_OBJECT_FLAG => QueryType::Select,
        MAKE_TABLE_QUERY_OBJECT_FLAG => QueryType::MakeTable,
        APPEND_QUERY_OBJECT_FLAG => QueryType::Append,
        UPDATE_QUERY_OBJECT_FLAG => QueryType::Update,
        DELETE_QUERY_OBJECT_FLAG => QueryType::Delete,
        CROSS_TAB_QUERY_OBJECT_FLAG => QueryType::CrossTab,
        DATA_DEF_QUERY_OBJECT_FLAG => QueryType::DataDefinition,
        PASSTHROUGH_QUERY_OBJECT_FLAG => QueryType::Passthrough,
        UNION_QUERY_OBJECT_FLAG => QueryType::Union,
        _ => QueryType::Unknown,
    };

    // `QueryImpl`'s constructor rejects rows whose type row names another type,
    // and `createQuery` turns that `IllegalStateException` into UnknownQueryImpl
    if query_type != QueryType::Unknown {
        let found_type = rows
            .iter()
            .find(|row| row.attribute == Some(TYPE_ATTRIBUTE))
            .and_then(|row| row.flag)
            .unwrap_or(query_type.value());
        if found_type != query_type.value() {
            query_type = QueryType::Unknown;
        }
    }

    let mut property_maps = PropertyMaps::new(object_id);
    // `QueryImpl.getProperties()` is `getPropertyMaps().getDefault()`, and
    // `PropertyMaps.getDefault()` creates the (empty) object level map when the
    // query stores no properties, so the group always holds one map
    property_maps.get_default();

    Ok(Query {
        name: name.to_string(),
        rows,
        object_id,
        object_flag,
        query_type,
        property_maps,
        property_maps_error: None,
    })
}

/// `QueryImpl.sortByOrder`: sorts the rows by attribute and then by the raw
/// `Order` bytes, unless a row has no order value (in which case the rows are
/// left untouched).  The sort is stable.
fn sort_by_order(rows: &[QueryRow]) -> Vec<QueryRow> {
    if rows.iter().any(|row| row.order.is_none()) {
        return rows.to_vec();
    }
    let mut sorted = rows.to_vec();
    sorted.sort_by(|left, right| {
        let attribute = |row: &QueryRow| row.attribute.map(|a| a as i8 as i32).unwrap_or(-1);
        attribute(left).cmp(&attribute(right)).then_with(|| {
            compare_unsigned(
                left.order.as_deref().unwrap_or(&[]),
                right.order.as_deref().unwrap_or(&[]),
            )
        })
    });
    sorted
}

/// `Arrays.compareUnsigned(byte[], byte[])`.
fn compare_unsigned(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    left.cmp(right)
}

impl Query {
    /// `Query.getName()`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `Query.getType()`.
    pub fn query_type(&self) -> QueryType {
        self.query_type
    }

    /// `Query.getObjectId()`.
    pub fn object_id(&self) -> i32 {
        self.object_id
    }

    /// `Query.getObjectFlag()`.
    pub fn object_flag(&self) -> i32 {
        self.object_flag
    }

    /// `Query.isHidden()`.
    pub fn is_hidden(&self) -> bool {
        (self.object_flag & HIDDEN_OBJECT_FLAG) != 0
    }

    /// `QueryImpl.getRows()`, the (sorted) `MSysQueries` rows.
    pub fn rows(&self) -> &[QueryRow] {
        &self.rows
    }

    // -- properties and name map --------------------------------------------

    /// `Query.getProperties()`: the object level ("default") property map of
    /// this query, which is empty when the query stores no properties.
    ///
    /// The property group is read from the query's catalog row by
    /// [`Database::get_queries_with_properties`] /
    /// [`Database::load_query_properties`] (or installed by
    /// [`Query::set_property_maps`]); a query built by [`create_query`] alone
    /// has no database to read from and therefore has an empty map.  Java's
    /// `QueryImpl.create(String, List, int)` has no database either, and its
    /// `getProperties()` throws a `NullPointerException`.
    ///
    /// A stored property blob which cannot be decoded is reported here, like
    /// the `IOException` of the Java `getProperties()`.
    pub fn get_properties(&self) -> Result<&PropertyMap> {
        self.check_property_maps_error()?;
        Ok(self
            .property_maps
            .default_map()
            .expect("a query always holds the object level property map"))
    }

    /// [`Query::get_properties`] for a caller which updates the map (Java's
    /// `getProperties()` returns the live map, whose `put` only takes effect
    /// after a `save`).  Save the group with
    /// [`Database::save_query_properties`].
    pub fn get_properties_mut(&mut self) -> Result<&mut PropertyMap> {
        self.check_property_maps_error()?;
        Ok(self.property_maps.get_default())
    }

    /// `QueryImpl.getPropertyMaps()`: the whole property group of this query
    /// (the object level map plus any column and index maps).
    pub fn get_property_maps(&self) -> Result<&PropertyMaps> {
        self.check_property_maps_error()?;
        Ok(&self.property_maps)
    }

    /// [`Query::get_property_maps`] for a caller which updates the group.
    pub fn get_property_maps_mut(&mut self) -> Result<&mut PropertyMaps> {
        self.check_property_maps_error()?;
        Ok(&mut self.property_maps)
    }

    /// Installs a property group, clearing the decoding error of a stored
    /// group which could not be read.  The object level map is created when
    /// the group does not hold one, like `PropertyMaps.getDefault()`.
    pub fn set_property_maps(&mut self, mut maps: PropertyMaps) {
        maps.get_default();
        self.property_maps = maps;
        self.property_maps_error = None;
    }

    /// The message of the error which decoding the stored property blob
    /// reported, if the blob could not be read.
    pub fn property_maps_error(&self) -> Option<&str> {
        self.property_maps_error.as_deref()
    }

    /// `Query.getNameMap()`: the name map stored in this query's
    /// [`QUERY_NAME_MAP_PROP`] property, or `None` when the query has none.
    ///
    /// The value is parsed on every call, which a read-only caller cannot
    /// distinguish from Java's cached instance (Java reparses it after
    /// `propertiesUpdated` as well).  A malformed value reports the
    /// `NameMapImpl` `IOException`, exactly when Java reports it.
    ///
    /// Java casts the property value to `byte[]`, so a non binary value throws
    /// a `ClassCastException`; the port reports [`Error::IllegalState`].
    pub fn get_name_map(&self) -> Result<Option<QueryNameMap>> {
        match self.get_properties()?.get_value(QUERY_NAME_MAP_PROP) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bytes(bytes)) => Ok(Some(QueryNameMap::read(bytes)?)),
            Some(_) => Err(Error::IllegalState(format!(
                "The {} property of query {} is not a binary value",
                QUERY_NAME_MAP_PROP, self.name
            ))),
        }
    }

    /// `QueryImpl.propertiesUpdated`: the name map property may have been
    /// replaced, so a stored decoding error no longer applies (the port parses
    /// the name map on demand, so it has no cache to drop).
    pub(crate) fn properties_updated(&mut self) {
        self.property_maps_error = None;
    }

    fn check_property_maps_error(&self) -> Result<()> {
        match &self.property_maps_error {
            Some(message) => Err(Error::Jackcess(message.clone())),
            None => Ok(()),
        }
    }

    /// `DatabaseImpl.getPropertiesForObject(objectId, this)`: reads (and
    /// decodes) the property group of this query's catalog row.  A blob which
    /// cannot be decoded is kept as an error state, so that `get_properties`
    /// reports it like Java's lazy `getProperties()`.
    pub(crate) fn load_property_maps(&mut self, db: &mut Database) -> Result<()> {
        match db.get_object_properties(self.object_id) {
            Ok(maps) => {
                self.set_property_maps(maps);
                Ok(())
            }
            Err(error) => {
                let message = error_message(&error);
                let mut maps = PropertyMaps::new(self.object_id);
                maps.get_default();
                self.property_maps = maps;
                self.property_maps_error = Some(message);
                Err(error)
            }
        }
    }

    /// `Query.getOwnerAccessType()`.
    pub fn get_owner_access_type(&self) -> Result<&'static str> {
        Ok(if self.has_flag(OWNER_ACCESS_SELECT_TYPE)? {
            "WITH OWNERACCESS OPTION"
        } else {
            DEFAULT_TYPE
        })
    }

    /// `Query.getParameters()`.
    pub fn get_parameters(&self) -> Result<Vec<String>> {
        let mut result = Vec::new();
        for row in self.rows_by_attribute(PARAMETER_ATTRIBUTE) {
            let type_name = param_type_name(row.flag).ok_or_else(|| {
                Error::IllegalState(format!(
                    "Unknown param type {} (Query: {})",
                    option_short(row.flag),
                    self.name
                ))
            })?;
            let mut builder = String::new();
            to_parameter_name(&mut builder, row.name1.as_deref());
            builder.push(' ');
            builder.push_str(type_name);
            if row.flag == Some(TEXT_FLAG) && row.lv_extra.unwrap_or(0) > 0 {
                builder.push('(');
                builder.push_str(&row.lv_extra.unwrap_or(0).to_string());
                builder.push(')');
            }
            result.push(builder);
        }
        Ok(result)
    }

    /// `Query.getSelectType()`, e.g. `"DISTINCTROW TOP 10"` or
    /// [`DEFAULT_TYPE`].
    pub fn get_select_type(&self) -> Result<String> {
        let mut builder = String::new();
        // TOP goes with either distinct type, as in "SELECT DISTINCTROW TOP 10"
        if self.has_flag(DISTINCT_SELECT_TYPE)? {
            builder.push_str("DISTINCT");
        } else if self.has_flag(DISTINCT_ROW_SELECT_TYPE)? {
            builder.push_str("DISTINCTROW");
        }

        if self.has_flag(TOP_SELECT_TYPE)? {
            if !builder.is_empty() {
                builder.push(' ');
            }
            builder.push_str("TOP ");
            if let Some(name) = self.flag_row()?.and_then(|row| row.name1.as_deref()) {
                builder.push_str(name);
            } else {
                // `StringBuilder.append((String)null)` writes "null"
                builder.push_str("null");
            }
            if self.has_flag(PERCENT_SELECT_TYPE)? {
                builder.push_str(" PERCENT");
            }
        }

        Ok(if builder.is_empty() {
            DEFAULT_TYPE.to_string()
        } else {
            builder
        })
    }

    /// `Query.toSQLString()`.
    pub fn to_sql_string(&self) -> Result<String> {
        let mut builder = String::new();
        if self.supports_standard_clauses() {
            self.append_parameter_string(&mut builder)?;
        }

        self.append_body(&mut builder)?;

        if self.supports_standard_clauses() {
            let access_type = self.get_owner_access_type()?;
            if access_type != DEFAULT_TYPE {
                builder.push_str(NEWLINE);
                builder.push_str(access_type);
            }
            builder.push(';');
        }
        Ok(builder)
    }

    // -- row access ---------------------------------------------------------

    fn rows_by_attribute(&self, attribute: u8) -> Vec<&QueryRow> {
        self.rows
            .iter()
            .filter(|row| row.attribute == Some(attribute))
            .collect()
    }

    /// `getUniqueRow`: zero rows is the empty row (`None` here), more than one
    /// throws.
    fn unique_row<'a>(&self, rows: Vec<&'a QueryRow>) -> Result<Option<&'a QueryRow>> {
        match rows.len() {
            1 => Ok(Some(rows[0])),
            0 => Ok(None),
            count => Err(Error::IllegalState(format!(
                "Unexpected number of rows for {} rows (Query: {})",
                count, self.name
            ))),
        }
    }

    fn row_by_attribute(&self, attribute: u8) -> Result<Option<&QueryRow>> {
        self.unique_row(self.rows_by_attribute(attribute))
    }

    fn type_row(&self) -> Result<Option<&QueryRow>> {
        self.row_by_attribute(TYPE_ATTRIBUTE)
    }

    fn flag_row(&self) -> Result<Option<&QueryRow>> {
        self.row_by_attribute(FLAG_ATTRIBUTE)
    }

    fn table_rows(&self) -> Vec<&QueryRow> {
        self.rows_by_attribute(TABLE_ATTRIBUTE)
    }

    fn join_rows(&self) -> Vec<&QueryRow> {
        self.rows_by_attribute(JOIN_ATTRIBUTE)
    }

    fn order_by_rows(&self) -> Vec<&QueryRow> {
        self.rows_by_attribute(ORDERBY_ATTRIBUTE)
    }

    /// All `Attribute = 6` rows (`super.getColumnRows()`).
    fn all_column_rows(&self) -> Vec<&QueryRow> {
        self.rows_by_attribute(COLUMN_ATTRIBUTE)
    }

    /// `getColumnRows()` with the subclass filters of crosstab and append.
    fn column_rows(&self) -> Vec<&QueryRow> {
        let rows = self.all_column_rows();
        match self.query_type {
            QueryType::CrossTab => rows
                .into_iter()
                .filter(|row| row_has_flag(row, CROSSTAB_NORMAL_FLAG))
                .collect(),
            QueryType::Append => rows
                .into_iter()
                .filter(|row| !row_has_flag(row, APPEND_VALUE_FLAG))
                .collect(),
            _ => rows,
        }
    }

    fn group_by_rows(&self) -> Vec<&QueryRow> {
        let rows = self.rows_by_attribute(GROUPBY_ATTRIBUTE);
        match self.query_type {
            QueryType::CrossTab => rows
                .into_iter()
                .filter(|row| row_has_flag(row, CROSSTAB_NORMAL_FLAG))
                .collect(),
            _ => rows,
        }
    }

    fn has_flag(&self, flag_mask: i32) -> Result<bool> {
        let flag = self.flag_row()?.and_then(|row| row.flag).unwrap_or(0) as i32;
        Ok((flag & flag_mask) != 0)
    }

    fn type_row_name1(&self) -> Result<Option<&str>> {
        Ok(self.type_row()?.and_then(|row| row.name1.as_deref()))
    }

    fn type_row_name2(&self) -> Result<Option<&str>> {
        Ok(self.type_row()?.and_then(|row| row.name2.as_deref()))
    }

    fn type_row_expression(&self) -> Result<Option<&str>> {
        Ok(self.type_row()?.and_then(|row| row.expression.as_deref()))
    }

    /// `QueryImpl.getTypeRowText()`: the type row text without the trailing
    /// NUL characters which sometimes end the stored value.
    fn type_row_text(&self) -> Result<Option<String>> {
        let Some(text) = self.type_row_expression()? else {
            return Ok(None);
        };
        let end = text.trim_end_matches('\0').len();
        Ok(Some(text[..end].to_string()))
    }

    fn where_expression(&self) -> Result<Option<&str>> {
        Ok(self
            .row_by_attribute(WHERE_ATTRIBUTE)?
            .and_then(|row| row.expression.as_deref()))
    }

    fn having_expression(&self) -> Result<Option<&str>> {
        Ok(self
            .row_by_attribute(HAVING_ATTRIBUTE)?
            .and_then(|row| row.expression.as_deref()))
    }

    // The `from_remote_db_*` names follow the Java `QueryImpl` remote-db
    // fields; they are accessors on the query, not constructors.
    #[expect(
        clippy::wrong_self_convention,
        reason = "name mirrors the Java remote-db attribute, not a constructor"
    )]
    fn from_remote_db_path(&self) -> Result<Option<&str>> {
        Ok(self
            .row_by_attribute(REMOTEDB_ATTRIBUTE)?
            .and_then(|row| row.name1.as_deref()))
    }

    #[expect(
        clippy::wrong_self_convention,
        reason = "name mirrors the Java remote-db attribute, not a constructor"
    )]
    fn from_remote_db_type(&self) -> Result<Option<&str>> {
        Ok(self
            .row_by_attribute(REMOTEDB_ATTRIBUTE)?
            .and_then(|row| row.expression.as_deref()))
    }

    fn supports_standard_clauses(&self) -> bool {
        !matches!(
            self.query_type,
            QueryType::Passthrough | QueryType::DataDefinition
        )
    }

    // -- clause builders ----------------------------------------------------

    /// `QueryImpl.toSQLParameterString`.
    fn append_parameter_string(&self, builder: &mut String) -> Result<()> {
        let params = self.get_parameters()?;
        if !params.is_empty() {
            builder.push_str("PARAMETERS ");
            // the Java code appends the `AppendableList`, whose separator is ", "
            builder.push_str(&params.join(", "));
            builder.push(';');
            builder.push_str(NEWLINE);
        }
        Ok(())
    }

    /// `QueryImpl.getOrderings`.
    fn get_orderings(&self) -> Vec<String> {
        self.order_by_rows()
            .into_iter()
            .map(|row| {
                let mut builder = String::new();
                builder.push_str(row.expression.as_deref().unwrap_or("null"));
                if row
                    .name1
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(DESCENDING_FLAG))
                {
                    builder.push_str(" DESC");
                }
                builder
            })
            .collect()
    }

    /// `BaseSelectQueryImpl.getGroupings`.
    fn get_groupings(&self) -> Vec<String> {
        self.group_by_rows()
            .into_iter()
            .map(|row| row.expression.as_deref().unwrap_or("null").to_string())
            .collect()
    }

    /// `BaseSelectQueryImpl.getSelectColumns` (with the delete and crosstab
    /// overrides).
    fn get_select_columns(&self) -> Result<Vec<String>> {
        let mut result: Vec<String> = self
            .column_rows()
            .into_iter()
            .map(|row| {
                let mut builder = String::new();
                builder.push_str(row.expression.as_deref().unwrap_or("null"));
                to_alias(&mut builder, row.name1.as_deref());
                builder
            })
            .collect();
        if self.has_flag(SELECT_STAR_SELECT_TYPE)? {
            result.push("*".to_string());
        }
        // DeleteQueryImpl writes "*" for a delete query which stores no columns
        if self.query_type == QueryType::Delete && result.is_empty() {
            result.push("*".to_string());
        }
        Ok(result)
    }

    /// `QueryImpl.appendSelectType`.
    fn append_select_type(&self, builder: &mut String) -> Result<()> {
        // Ms access stores the select type of a crosstab query but does not
        // write it in the sql text
        if self.query_type == QueryType::CrossTab {
            return Ok(());
        }
        let select_type = self.get_select_type()?;
        if select_type != DEFAULT_TYPE {
            builder.push_str(&select_type);
            builder.push(' ');
        }
        Ok(())
    }

    fn append_body(&self, builder: &mut String) -> Result<()> {
        match self.query_type {
            QueryType::Select | QueryType::MakeTable => self.to_sql_select_string(builder, true),
            QueryType::Append => self.append_append(builder),
            QueryType::Update => self.append_update(builder),
            QueryType::Delete => {
                builder.push_str("DELETE ");
                self.append_select_type(builder)?;
                self.to_sql_select_string(builder, false)
            }
            QueryType::CrossTab => self.append_crosstab(builder),
            QueryType::Union => self.append_union(builder),
            QueryType::Passthrough | QueryType::DataDefinition => {
                if let Some(text) = self.type_row_text()? {
                    builder.push_str(&text);
                }
                Ok(())
            }
            QueryType::Unknown => Err(Error::Unsupported(format!(
                "unknown query type (Query: {})",
                self.name
            ))),
        }
    }

    /// `BaseSelectQueryImpl.toSQLSelectString`.
    fn to_sql_select_string(&self, builder: &mut String, use_select_prefix: bool) -> Result<()> {
        if use_select_prefix {
            builder.push_str("SELECT ");
            self.append_select_type(builder)?;
        }

        builder.push_str(&self.get_select_columns()?.join(", "));
        self.to_select_into(builder)?;

        let from_tables = self.get_from_tables()?;
        if !from_tables.is_empty() {
            builder.push_str(NEWLINE);
            builder.push_str("FROM ");
            builder.push_str(&from_tables.join(", "));
            to_remote_db(
                builder,
                self.from_remote_db_path()?,
                self.from_remote_db_type()?,
            );
        }

        if let Some(where_expr) = self.where_expression()? {
            builder.push_str(NEWLINE);
            builder.push_str("WHERE ");
            builder.push_str(where_expr);
        }

        let groupings = self.get_groupings();
        if !groupings.is_empty() {
            builder.push_str(NEWLINE);
            builder.push_str("GROUP BY ");
            builder.push_str(&groupings.join(", "));
        }

        if let Some(having_expr) = self.having_expression()? {
            builder.push_str(NEWLINE);
            builder.push_str("HAVING ");
            builder.push_str(having_expr);
        }

        let orderings = self.get_orderings();
        if !orderings.is_empty() {
            builder.push_str(NEWLINE);
            builder.push_str("ORDER BY ");
            builder.push_str(&orderings.join(", "));
        }
        Ok(())
    }

    /// `MakeTableQueryImpl.toSelectInto` (a no-op for every other type).
    fn to_select_into(&self, builder: &mut String) -> Result<()> {
        if self.query_type != QueryType::MakeTable {
            return Ok(());
        }
        builder.push_str(" INTO ");
        let target_table = self
            .type_row_name1()?
            .ok_or_else(|| Error::IllegalState("NullPointerException: make table target".into()))?;
        to_optional_quoted_expr(builder, target_table, true);
        to_remote_db(builder, self.type_row_name2()?, self.type_row_expression()?);
        Ok(())
    }

    /// `AppendQueryImpl.toSQLString`.
    fn append_append(&self, builder: &mut String) -> Result<()> {
        builder.push_str("INSERT INTO ");
        let target_table = self
            .type_row_name1()?
            .ok_or_else(|| Error::IllegalState("NullPointerException: append target".into()))?;
        to_optional_quoted_expr(builder, target_table, true);

        let columns = self.get_target_columns();
        if !columns.is_empty() {
            builder.push_str(" (");
            builder.push_str(&columns.join(", "));
            builder.push(')');
        }
        to_remote_db(builder, self.type_row_name2()?, self.type_row_expression()?);
        builder.push_str(NEWLINE);

        let values = self.get_values();
        if !values.is_empty() {
            builder.push_str("VALUES (");
            builder.push_str(&values.join(", "));
            builder.push(')');
        } else {
            self.to_sql_select_string(builder, true)?;
        }
        Ok(())
    }

    /// `AppendQueryImpl.getTargetColumns`: every column row which names a
    /// target column, with the target column name as the reported text.
    fn get_target_columns(&self) -> Vec<String> {
        self.all_column_rows()
            .into_iter()
            .filter_map(|row| row.name2.as_deref())
            .map(|name| {
                let mut builder = String::new();
                to_optional_quoted_expr(&mut builder, name, true);
                builder
            })
            .collect()
    }

    /// `AppendQueryImpl.getValues`.
    fn get_values(&self) -> Vec<String> {
        self.all_column_rows()
            .into_iter()
            .filter(|row| row_has_flag(row, APPEND_VALUE_FLAG))
            .map(|row| row.expression.as_deref().unwrap_or("null").to_string())
            .collect()
    }

    /// `UpdateQueryImpl.toSQLString`.
    fn append_update(&self, builder: &mut String) -> Result<()> {
        builder.push_str("UPDATE ");
        self.append_select_type(builder)?;
        builder.push_str(&self.get_from_tables()?.join(", "));
        to_remote_db(
            builder,
            self.from_remote_db_path()?,
            self.from_remote_db_type()?,
        );

        builder.push_str(NEWLINE);
        builder.push_str("SET ");
        let mut new_values = Vec::new();
        for row in self.column_rows() {
            let name2 = row
                .name2
                .as_deref()
                .ok_or_else(|| Error::IllegalState("NullPointerException: update column".into()))?;
            let mut value = String::new();
            to_optional_quoted_expr(&mut value, name2, true);
            value.push_str(" = ");
            value.push_str(row.expression.as_deref().unwrap_or("null"));
            new_values.push(value);
        }
        builder.push_str(&new_values.join(", "));

        if let Some(where_expr) = self.where_expression()? {
            builder.push_str(NEWLINE);
            builder.push_str("WHERE ");
            builder.push_str(where_expr);
        }
        Ok(())
    }

    /// `CrossTabQueryImpl.getTransformRow`.
    fn get_transform_row(&self) -> Result<Option<&QueryRow>> {
        self.unique_row(
            self.all_column_rows()
                .into_iter()
                .filter(|row| !row_has_flag(row, CROSSTAB_PIVOT_FLAG | CROSSTAB_NORMAL_FLAG))
                .collect(),
        )
    }

    /// `CrossTabQueryImpl.getTransformExpression`.
    fn get_transform_expression(&self) -> Result<Option<String>> {
        let Some(row) = self.get_transform_row()? else {
            return Ok(None);
        };
        let Some(expression) = row.expression.as_deref() else {
            return Ok(None);
        };
        let mut builder = String::from(expression);
        to_alias(&mut builder, row.name1.as_deref());
        Ok(Some(builder))
    }

    /// `CrossTabQueryImpl.getPivotRow`.
    fn get_pivot_row(&self) -> Result<Option<&QueryRow>> {
        self.unique_row(
            self.all_column_rows()
                .into_iter()
                .filter(|row| row_has_flag(row, CROSSTAB_PIVOT_FLAG))
                .collect(),
        )
    }

    /// `CrossTabQueryImpl.getPivotExpression`.
    fn get_pivot_expression(&self) -> Result<Option<&str>> {
        Ok(self
            .get_pivot_row()?
            .and_then(|row| row.expression.as_deref()))
    }

    /// `CrossTabQueryImpl.toSQLString`.
    fn append_crosstab(&self, builder: &mut String) -> Result<()> {
        if let Some(transform_expr) = self.get_transform_expression()? {
            builder.push_str("TRANSFORM ");
            builder.push_str(&transform_expr);
            builder.push_str(NEWLINE);
        }

        self.to_sql_select_string(builder, true)?;

        builder.push_str(NEWLINE);
        builder.push_str("PIVOT ");
        builder.push_str(self.get_pivot_expression()?.unwrap_or("null"));
        Ok(())
    }

    /// `UnionQueryImpl.getUnionType`.
    fn get_union_type(&self) -> Result<&'static str> {
        Ok(if self.has_flag(UNION_FLAG)? {
            DEFAULT_TYPE
        } else {
            "ALL"
        })
    }

    /// `UnionQueryImpl.getUnionString`.
    fn get_union_string(&self, id: &str) -> Result<String> {
        for row in self.table_rows() {
            if row.name2.as_deref() == Some(id) {
                let expression = row.expression.as_deref().ok_or_else(|| {
                    Error::IllegalState(format!(
                        "NullPointerException: union text (Query: {})",
                        self.name
                    ))
                })?;
                return Ok(clean_union_string(expression));
            }
        }
        Err(Error::IllegalState(format!(
            "Could not find union query with id {}",
            id
        )))
    }

    /// `UnionQueryImpl.toSQLString`.
    fn append_union(&self, builder: &mut String) -> Result<()> {
        builder.push_str(&self.get_union_string(UNION_PART1)?);
        builder.push_str(NEWLINE);
        builder.push_str("UNION ");
        let union_type = self.get_union_type()?;
        if union_type != DEFAULT_TYPE {
            builder.push_str(union_type);
            builder.push(' ');
        }
        builder.push_str(&self.get_union_string(UNION_PART2)?);

        let orderings = self.get_orderings();
        if !orderings.is_empty() {
            builder.push_str(NEWLINE);
            builder.push_str("ORDER BY ");
            builder.push_str(&orderings.join(", "));
        }
        Ok(())
    }

    /// `QueryImpl.getFromTables`: the `FROM` sources, with the join rows folded
    /// into (possibly nested) join expressions.
    fn get_from_tables(&self) -> Result<Vec<String>> {
        // grab the list of query tables
        let mut table_exprs: Vec<TableEntry> = Vec::new();
        let mut next_id = 0usize;
        for table in self.table_rows() {
            let mut builder = String::new();

            if table.name1.is_none() {
                // a table row with an expression and no name is a derived
                // table, which is written in parentheses
                if let Some(expression) = &table.expression {
                    builder.push('(');
                    builder.push_str(expression);
                    builder.push(')');
                }
            } else {
                if let Some(expression) = &table.expression {
                    to_quoted_expr(&mut builder, expression);
                    builder.push(IDENTIFIER_SEP_CHAR);
                }
                // the name is known to be present here
                if let Some(name) = table.name1.as_deref() {
                    to_optional_quoted_expr(&mut builder, name, true);
                }
            }
            to_alias(&mut builder, table.name2.as_deref());

            let key = table.name2.clone().or_else(|| table.name1.clone());
            table_exprs.push(TableEntry {
                id: next_id,
                source: TableSource::Simple { key, expr: builder },
            });
            next_id += 1;
        }

        // combine the tables with any query joins.  Ms access's algorithm
        // removes the sources it consumes from the list, so identities are
        // tracked by `id` rather than by position.
        for join_row in self.join_rows() {
            let from_table = join_row.name1.as_deref();
            let to_table = join_row.name2.as_deref();

            let mut from_match: Option<(usize, TableSource)> = None;
            let mut to_match: Option<(usize, TableSource)> = None;

            let mut index = 0usize;
            while index < table_exprs.len() && (from_match.is_none() || to_match.is_none()) {
                let mut removed = false;
                if from_match.is_none() && table_exprs[index].source.contains_table(from_table)? {
                    let source = table_exprs[index].source.clone();
                    from_match = Some((table_exprs[index].id, source));

                    // special case adding expr to existing join
                    if to_match.is_none() && table_exprs[index].source.contains_table(to_table)? {
                        let source = table_exprs[index].source.clone();
                        to_match = Some((table_exprs[index].id, source));
                        break;
                    }
                    table_exprs.remove(index);
                    removed = true;
                } else if to_match.is_none()
                    && table_exprs[index].source.contains_table(to_table)?
                {
                    let source = table_exprs[index].source.clone();
                    to_match = Some((table_exprs[index].id, source));
                    table_exprs.remove(index);
                    removed = true;
                }
                if !removed {
                    index += 1;
                }
            }

            let join_type = join_row.flag.ok_or_else(|| {
                Error::IllegalState("NullPointerException: join type".to_string())
            })?;

            if let (Some((from_id, _)), Some((to_id, _))) = (&from_match, &to_match) {
                if from_id == to_id {
                    let position = table_exprs.iter().position(|entry| entry.id == *from_id);
                    if let Some(position) = position {
                        if table_exprs[position].source.add_condition(
                            from_table,
                            to_table,
                            join_type,
                            join_row.expression.clone(),
                        )? {
                            // easy-peasy, we just added the join expression to
                            // an existing join
                            continue;
                        }
                    }
                    return Err(Error::IllegalState(format!(
                        "Inconsistent join types for {} and {} (Query: {})",
                        from_table.unwrap_or("null"),
                        to_table.unwrap_or("null"),
                        self.name
                    )));
                }
            }

            let from_source = match from_match {
                Some((_, source)) => source,
                None => TableSource::simple(from_table, &self.name)?,
            };
            let to_source = match to_match {
                Some((_, source)) => source,
                None => TableSource::simple(to_table, &self.name)?,
            };

            table_exprs.push(TableEntry {
                id: next_id,
                source: TableSource::Join {
                    from: Box::new(from_source),
                    to: Box::new(to_source),
                    join_type,
                    // combine all the join expressions with "AND"
                    on: vec![join_row.expression.clone()],
                },
            });
            next_id += 1;
        }

        let mut result = Vec::new();
        for entry in &table_exprs {
            result.push(entry.source.render(true, &self.name)?);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Database integration (query properties and name maps)
// ---------------------------------------------------------------------------

impl Database {
    /// The raw `LvProp` values of every `MSysObjects` row, keyed by the object
    /// id (`DatabaseImpl.readProperties`'s storage step: the `LvProp` bytes of
    /// the object's catalog row, `None` when the row holds no value).
    ///
    /// This is the query counterpart of the raw data
    /// [`Database::get_object_properties`] decodes; it exists so that a
    /// verification tool can compare the stored bytes as well as the decoded
    /// properties.
    pub fn get_object_property_blobs(
        &mut self,
    ) -> Result<std::collections::HashMap<i32, Option<Vec<u8>>>> {
        // `DatabaseImpl.CAT_COL_ID` / `CAT_COL_PROPS`; the catalog column names
        // are not part of the database module's public API
        let (idx_id, idx_props) = match self.tables.get(&PAGE_SYSTEM_CATALOG) {
            Some(table) => (table.column_index("Id"), table.column_index("LvProp")),
            None => return Ok(std::collections::HashMap::new()),
        };
        let (Some(idx_id), Some(idx_props)) = (idx_id, idx_props) else {
            // a catalog without those columns stores no properties at all
            return Ok(std::collections::HashMap::new());
        };
        let mut blobs = std::collections::HashMap::new();
        for (_, values) in self.read_rows_with_ids(TABLE_SYSTEM_CATALOG)? {
            let Some(values) = values else {
                continue;
            };
            let Some(object_id) = values.get(idx_id).and_then(|value| value.as_i64()) else {
                continue;
            };
            let blob = match values.get(idx_props) {
                Some(Value::Bytes(bytes)) => Some(bytes.clone()),
                _ => None,
            };
            blobs.insert(object_id as i32, blob);
        }
        Ok(blobs)
    }

    /// Loads the properties of a query built by [`create_query`] (or by
    /// [`Database::get_queries`]) from the `LvProp` value of its catalog row,
    /// the port of `QueryImpl.getPropertyMaps()` ->
    /// `DatabaseImpl.getPropertiesForObject(objectId, this)`.
    ///
    /// Java reads the group lazily, when `getProperties()` is first called; the
    /// port reads it here, and keeps a decoding error on the query so that
    /// [`Query::get_properties`] reports it like the Java `IOException`.  The
    /// error is therefore returned here as well, but a caller which wants
    /// Java's lazy behaviour can ignore it and let `get_properties` report it.
    pub fn load_query_properties(&mut self, query: &mut Query) -> Result<()> {
        query.load_property_maps(self)
    }

    /// [`Database::get_queries`] with the properties of every query loaded
    /// from its catalog row, i.e. the db-bound `QueryImpl.create` which
    /// `Database.getQueries()` uses in Java.
    ///
    /// A property blob which cannot be decoded does not fail this method (Java
    /// reads it lazily, so `getQueries()` succeeds and `getProperties()` fails);
    /// the error is kept on the query and reported by [`Query::get_properties`].
    pub fn get_queries_with_properties(&mut self) -> Result<Vec<Query>> {
        let mut queries = self.get_queries()?;
        for query in &mut queries {
            let _ = query.load_property_maps(self);
        }
        Ok(queries)
    }

    /// Stores the property group of a query back into its catalog row, the
    /// port of `PropertyMaps.save()` (which calls `QueryImpl
    /// .propertiesUpdated`) for a query.
    ///
    /// A query built by [`create_query`] alone has no catalog row to write to
    /// and reports the same `IllegalStateException` as the Java
    /// `PropertyMaps.save` ("PropertyMaps cannot be saved without a row id").
    pub fn save_query_properties(&mut self, query: &mut Query) -> Result<()> {
        {
            let maps = query.get_property_maps_mut()?;
            self.save_properties(maps)?;
        }
        // `PropertyMaps.save` -> `Owner.propertiesUpdated`: the name map
        // property may have been replaced
        query.properties_updated();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Canonical property/name map dump (verification support)
// ---------------------------------------------------------------------------

/// The canonical text dump of one query's property maps and name map, as
/// written by `java-src/QueryPropsGen.java` and `src/bin/queryprops.rs` (the
/// two must be byte-identical; `tools/verify-queryprops.ps1` compares them).
///
/// `storage` is the raw `LvProp` value of the query's catalog row, which the
/// dump records so that a reader can decode the same bytes again.  Every
/// record is one line; string fields are escaped (`\\`, `\t`, `\r`, `\n`,
/// `\0`) and `<null>` marks a missing value:
///
/// ```text
/// QUERY|<name>|objectId=<id>
/// STORAGE|<hex>|<null>
/// PROPS|<name>|maps=<n>|props=<n>
/// MAP|<name>|<group>|<block type>|<count>
/// PROP|<name>|<group>|<property>|<DataType>|ddl=<0|1>|skip=<0|1>|<kind>|<value>
/// NM|<name>|records=<n>|writerTag=<n|->
/// NMREC|<name>|<index>|<type>|<guid>|<parentGuid>|<record name>|<date>
/// PROPSERROR|<name>|<tag>|<message>
/// NMERROR|<name>|<tag>|<message>
/// ```
///
/// The maps are sorted by group name and block type, and the properties of
/// each map by property name, so the dump does not depend on the encoding
/// order of a property block.  `PROPS`/`MAP`/`PROP` are replaced by
/// `PROPSERROR` when the stored blob cannot be decoded (Java's
/// `getProperties()` throws), in which case the name map is not read either
/// (`getNameMap()` would throw the same exception).  `NM`/`NMREC` are left out
/// when the query has no name map, and `NMERROR` reports a name map value which
/// cannot be decoded.
pub fn dump_query_properties(query: &Query, storage: Option<&[u8]>, out: &mut String) {
    let name = dump_escape(query.name());
    let _ = writeln!(out, "QUERY|{}|objectId={}", name, query.object_id());
    let _ = writeln!(
        out,
        "STORAGE|{}",
        match storage {
            Some(bytes) => dump_hex(bytes),
            None => "<null>".to_string(),
        }
    );

    let maps = match query.get_property_maps() {
        Ok(maps) => maps,
        Err(error) => {
            let _ = writeln!(
                out,
                "PROPSERROR|{}|{}|{}",
                name,
                dump_error_tag(&error),
                dump_escape(&dump_error_message(&error))
            );
            return;
        }
    };

    let property_count: usize = maps.iter().map(|map| map.len()).sum();
    let _ = writeln!(
        out,
        "PROPS|{}|maps={}|props={}",
        name,
        maps.len(),
        property_count
    );

    let mut sorted_maps: Vec<&PropertyMap> = maps.iter().collect();
    sorted_maps.sort_by(|left, right| {
        left.name()
            .cmp(right.name())
            .then(left.map_type().cmp(&right.map_type()))
    });
    for map in sorted_maps {
        let _ = writeln!(
            out,
            "MAP|{}|{}|{}|{}",
            name,
            dump_escape(map.name()),
            map.map_type(),
            map.len()
        );
        let mut props: Vec<&crate::property::Property> = map.iter().collect();
        props.sort_by(|left, right| left.name().cmp(right.name()));
        for prop in props {
            let _ = writeln!(
                out,
                "PROP|{}|{}|{}|{}|ddl={}|skip={}|{}|{}",
                name,
                dump_escape(map.name()),
                dump_escape(prop.name()),
                prop.data_type().get_name(),
                if map.is_ddl(prop.name()) { 1 } else { 0 },
                if map.is_skip_handler(prop.name()) {
                    1
                } else {
                    0
                },
                prop_value_kind(&prop.value),
                dump_escape(&prop_value_text(&prop.value))
            );
        }
    }

    match query.get_name_map() {
        Ok(Some(name_map)) => {
            let records = name_map.records();
            let _ = writeln!(
                out,
                "NM|{}|records={}|writerTag={}",
                name,
                records.len(),
                match name_map.get_writer_tag() {
                    Some(tag) => tag.to_string(),
                    None => "-".to_string(),
                }
            );
            for (index, record) in records.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "NMREC|{}|{}|{}|{}|{}|{}|{}",
                    name,
                    index,
                    record.get_type().name(),
                    dump_escape_option(record.get_guid().as_deref()),
                    dump_escape_option(record.get_parent_guid().as_deref()),
                    dump_escape(record.get_name()),
                    match record.get_date() {
                        Some(date) => dump_format_date(date),
                        None => "-".to_string(),
                    }
                );
            }
        }
        Ok(None) => {}
        Err(error) => {
            let _ = writeln!(
                out,
                "NMERROR|{}|{}|{}",
                name,
                dump_error_tag(&error),
                dump_escape(&dump_error_message(&error))
            );
        }
    }
}

/// The value class name of a property value, as the dump reports it.
fn prop_value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Bool(_) => "BOOL",
        Value::Byte(_) => "BYTE",
        Value::Int(_) => "SHORT",
        Value::Long(_) => "INT",
        Value::BigInt(_) => "BIG_INT",
        Value::Decimal { .. } => "DECIMAL",
        Value::Float(_) => "FLOAT",
        Value::Double(_) => "DOUBLE",
        Value::Text(_) => "TEXT",
        Value::Bytes(_) => "BYTES",
        Value::DateTime(_) => "DATETIME",
        Value::DateBits(_) => "DATEBITS",
        Value::Uuid(_) => "UUID",
        Value::Keep => "KEEP",
    }
}

/// The canonical text of a property value, independent of the platform (floats
/// are reported by their IEEE bits, so the dump does not depend on the
/// `Double.toString` formatting of the two languages).
fn prop_value_text(value: &Value) -> String {
    match value {
        Value::Null => "<null>".to_string(),
        // Java's Boolean.toString and Byte/Short/Integer/Long.toString
        Value::Bool(value) => value.to_string(),
        Value::Byte(value) => (*value as i8).to_string(),
        Value::Int(value) => value.to_string(),
        Value::Long(value) => value.to_string(),
        Value::BigInt(value) => value.to_string(),
        // java.math.BigDecimal's unscaled value and scale
        Value::Decimal { unscaled, scale } => format!("{unscaled}/{scale}"),
        Value::Float(value) => format!("{:08x}", value.to_bits()),
        Value::Double(value) => format!("{:016x}", value.to_bits()),
        Value::Text(text) => text.clone(),
        Value::Bytes(bytes) => dump_hex(bytes),
        Value::DateTime(date) => dump_format_date(*date),
        Value::DateBits(bits) => format!("{:016x}", bits.to_bits()),
        Value::Uuid(bytes) => dump_hex(bytes),
        Value::Keep => "<KEEP_VALUE>".to_string(),
    }
}

fn dump_error_tag(error: &Error) -> &'static str {
    match error {
        Error::Io(_) | Error::Jackcess(_) => "IOException",
        Error::IllegalArgument(_) => "IllegalArgumentException",
        Error::IllegalState(_) => "IllegalStateException",
        Error::InvalidCredentials(_) => "InvalidCredentialsException",
        Error::Unsupported(_) => "UnsupportedOperationException",
        Error::ConstraintViolation(_) => "ConstraintViolationException",
        Error::InvalidValue(_) => "InvalidValueException",
        Error::BatchUpdate { .. } => "BatchUpdateException",
        Error::BufferUnderflow(_) | Error::BufferOverflow(_) => "RuntimeException",
        // java.nio's `InvalidMarkException` (the corrupt null mask path); the
        // simple class name, like the other tags here
        Error::InvalidMark => "InvalidMarkException",
        Error::NumberFormat(_) => "NumberFormatException",
    }
}

/// The Java `Throwable.getMessage()` of the error.
fn dump_error_message(error: &Error) -> String {
    error_message(error)
}

/// The Java `Throwable.getMessage()` of an error, without the `Display` prefix
/// of the enum variants which have one.
fn error_message(error: &Error) -> String {
    match error {
        Error::Io(error) => error.to_string(),
        Error::Jackcess(message)
        | Error::IllegalArgument(message)
        | Error::IllegalState(message)
        | Error::InvalidCredentials(message)
        | Error::Unsupported(message)
        | Error::ConstraintViolation(message)
        | Error::InvalidValue(message)
        | Error::BufferUnderflow(message)
        | Error::BufferOverflow(message) => message.clone(),
        // `InvalidMarkException.getMessage()` is null
        Error::InvalidMark => "null".to_string(),
        Error::NumberFormat(message) => message.clone(),
        Error::BatchUpdate { message, .. } => message.clone(),
    }
}

fn dump_escape(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\t' => result.push_str("\\t"),
            '\r' => result.push_str("\\r"),
            '\n' => result.push_str("\\n"),
            '\0' => result.push_str("\\0"),
            other => result.push(other),
        }
    }
    result
}

fn dump_escape_option(text: Option<&str>) -> String {
    match text {
        Some(text) => dump_escape(text),
        None => "<null>".to_string(),
    }
}

fn dump_hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

/// `yyyy-MM-ddTHH:mm:ss.nnnnnnnnn`, the format `QueryPropsGen` uses (the nine
/// fractional digits are `LocalDateTime.getNano`).
fn dump_format_date(date: crate::datenums::LocalDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}",
        date.year, date.month, date.day, date.hour, date.minute, date.second, date.nano
    )
}

/// A `TableSource` plus a stable identity, used while folding join rows.
struct TableEntry {
    id: usize,
    source: TableSource,
}

// ---------------------------------------------------------------------------
// Table sources (QueryImpl.TableSource / SimpleTable / Join)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum TableSource {
    Simple {
        /// `Name2` of the table row, or `Name1` when there is no alias.
        key: Option<String>,
        expr: String,
    },
    Join {
        from: Box<TableSource>,
        to: Box<TableSource>,
        join_type: i16,
        /// The `ON` expressions; Java reverses the order when one is added to
        /// an existing join.
        on: Vec<Option<String>>,
    },
}

impl TableSource {
    /// `new SimpleTable(tableName)`.
    fn simple(table_name: Option<&str>, query_name: &str) -> Result<TableSource> {
        let Some(table_name) = table_name else {
            return Err(Error::IllegalState(format!(
                "NullPointerException: join table name (Query: {})",
                query_name
            )));
        };
        let mut expr = String::new();
        to_optional_quoted_expr(&mut expr, table_name, true);
        Ok(TableSource::Simple {
            key: Some(table_name.to_string()),
            expr,
        })
    }

    /// `TableSource.containsTable`.
    fn contains_table(&self, table: Option<&str>) -> Result<bool> {
        match self {
            TableSource::Simple { key, .. } => match key {
                Some(key) => Ok(match table {
                    Some(table) => key.to_lowercase() == table.to_lowercase(),
                    None => false,
                }),
                None => Err(Error::IllegalState(
                    "NullPointerException: table name".to_string(),
                )),
            },
            TableSource::Join { from, to, .. } => {
                Ok(from.contains_table(table)? || to.contains_table(table)?)
            }
        }
    }

    /// `TableSource.addCondition`.  Ms access puts the condition on the
    /// innermost join which holds both tables and has the same join type.
    fn add_condition(
        &mut self,
        table1: Option<&str>,
        table2: Option<&str>,
        join_type: i16,
        on: Option<String>,
    ) -> Result<bool> {
        let TableSource::Join {
            from,
            to,
            join_type: current_type,
            on: conditions,
        } = self
        else {
            return Ok(false);
        };
        if from.contains_table(table1)?
            && from.contains_table(table2)?
            && from.add_condition(table1, table2, join_type, on.clone())?
        {
            return Ok(true);
        }
        if to.contains_table(table1)?
            && to.contains_table(table2)?
            && to.add_condition(table1, table2, join_type, on.clone())?
        {
            return Ok(true);
        }
        if *current_type == join_type {
            // note, AND conditions are added in _reverse_ order
            conditions.insert(0, on);
            return Ok(true);
        }
        Ok(false)
    }

    /// `TableSource.toString(StringBuilder, boolean)`.
    fn render(&self, is_top_level: bool, query_name: &str) -> Result<String> {
        match self {
            TableSource::Simple { expr, .. } => Ok(expr.clone()),
            TableSource::Join {
                from,
                to,
                join_type,
                on,
            } => {
                let join_type_string = join_type_text(*join_type).ok_or_else(|| {
                    Error::IllegalState(format!(
                        "Unknown join type {} (Query: {})",
                        join_type, query_name
                    ))
                })?;

                let mut builder = String::new();
                if !is_top_level {
                    builder.push('(');
                }
                builder.push_str(&from.render(false, query_name)?);
                builder.push_str(join_type_string);
                builder.push_str(&to.render(false, query_name)?);
                builder.push_str(" ON ");

                let multiple_on_expr = on.len() > 1;
                if multiple_on_expr {
                    builder.push('(');
                }
                for (index, condition) in on.iter().enumerate() {
                    if index > 0 {
                        builder.push_str(") AND (");
                    }
                    let condition = condition.as_deref().ok_or_else(|| {
                        Error::IllegalState(format!(
                            "NullPointerException: join expression (Query: {})",
                            query_name
                        ))
                    })?;
                    builder.push_str(condition);
                }
                if multiple_on_expr {
                    builder.push(')');
                }

                if !is_top_level {
                    builder.push(')');
                }
                Ok(builder)
            }
        }
    }
}

/// `QueryFormat.JOIN_TYPE_MAP`.
fn join_type_text(join_type: i16) -> Option<&'static str> {
    match join_type {
        1 => Some(" INNER JOIN "),
        2 => Some(" LEFT JOIN "),
        3 => Some(" RIGHT JOIN "),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// `QueryFormat.PARAM_TYPE_MAP`.
fn param_type_name(flag: Option<i16>) -> Option<&'static str> {
    Some(match flag? {
        0 => "Value",
        0x01 => "Bit",
        0x0A => "Text",
        0x02 => "Byte",
        0x03 => "Short",
        0x04 => "Long",
        0x05 => "Currency",
        0x06 => "IEEESingle",
        0x07 => "IEEEDouble",
        0x08 => "DateTime",
        0x09 => "Binary",
        0x0B => "LongBinary",
        0x0F => "Guid",
        _ => return None,
    })
}

fn option_short(value: Option<i16>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "null".to_string(),
    }
}

fn row_has_flag(row: &QueryRow, flag_mask: i32) -> bool {
    (row.flag.unwrap_or(0) as i32 & flag_mask) != 0
}

/// `QueryImpl.toQuotedExpr`.
fn to_quoted_expr(builder: &mut String, expr: &str) {
    if is_quoted(expr) {
        builder.push_str(expr);
    } else {
        builder.push('[');
        builder.push_str(expr);
        builder.push(']');
    }
}

fn is_quoted(expr: &str) -> bool {
    let bytes = expr.as_bytes();
    (bytes.len() >= 2) && (bytes[0] == b'[') && (bytes[bytes.len() - 1] == b']')
}

/// `QueryFormat.QUOTABLE_CHAR_PAT` (`\W`, i.e. any character outside
/// `[A-Za-z0-9_]`).
fn has_quotable_char(expr: &str) -> bool {
    expr.chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '_'))
}

/// `QueryImpl.toOptionalQuotedExpr`.
fn to_optional_quoted_expr(builder: &mut String, full_expr: &str, is_identifier: bool) {
    if is_identifier {
        let exprs = java_split_identifier(full_expr);
        for (index, expr) in exprs.iter().enumerate() {
            if has_quotable_char(expr) {
                to_quoted_expr(builder, expr);
            } else {
                builder.push_str(expr);
            }
            if index < (exprs.len() - 1) {
                builder.push(IDENTIFIER_SEP_CHAR);
            }
        }
    } else if has_quotable_char(full_expr) {
        to_quoted_expr(builder, full_expr);
    } else {
        builder.push_str(full_expr);
    }
}

/// `Pattern.compile("\\.").split(fullExpr)`, i.e. `String.split` with limit 0:
/// a string without the separator is returned whole, trailing empty parts are
/// dropped.
fn java_split_identifier(full_expr: &str) -> Vec<&str> {
    if !full_expr.contains(IDENTIFIER_SEP_CHAR) {
        return vec![full_expr];
    }
    let mut parts: Vec<&str> = full_expr.split(IDENTIFIER_SEP_CHAR).collect();
    while let Some(last) = parts.last() {
        if last.is_empty() {
            parts.pop();
        } else {
            break;
        }
    }
    parts
}

/// `QueryImpl.toRemoteDb`.
fn to_remote_db(builder: &mut String, remote_db_path: Option<&str>, remote_db_type: Option<&str>) {
    if remote_db_path.is_some() || remote_db_type.is_some() {
        // note, always include path string, even if empty
        builder.push_str(" IN '");
        if let Some(path) = remote_db_path {
            builder.push_str(path);
        }
        builder.push('\'');
        if let Some(db_type) = remote_db_type {
            builder.push_str(" [");
            builder.push_str(db_type);
            builder.push(']');
        }
    }
}

/// `QueryImpl.toAlias`.
fn to_alias(builder: &mut String, alias: Option<&str>) {
    if let Some(alias) = alias {
        builder.push_str(" AS ");
        to_optional_quoted_expr(builder, alias, false);
    }
}

/// `QueryImpl.toParameterName`.
fn to_parameter_name(builder: &mut String, name: Option<&str>) {
    let Some(name) = name else {
        // `StringBuilder.append((String)null)` writes "null"
        builder.push_str("null");
        return;
    };

    let mut parts: Vec<&str> = Vec::new();
    let mut seps: Vec<char> = Vec::new();
    let mut has_bang = false;
    let mut start = 0usize;
    let mut in_brackets = false;
    for (index, character) in name.char_indices() {
        if character == '[' {
            in_brackets = true;
        } else if character == ']' {
            in_brackets = false;
        } else if !in_brackets && ((character == '!') || (character == IDENTIFIER_SEP_CHAR)) {
            has_bang |= character == '!';
            parts.push(&name[start..index]);
            seps.push(character);
            start = index + character.len_utf8();
        }
    }
    parts.push(&name[start..]);

    // a control reference always has a "!", and ms access names do not start
    // or end with a space
    let mut is_reference = has_bang;
    for part in &parts {
        if part.is_empty() || part.starts_with(' ') || part.ends_with(' ') {
            is_reference = false;
        }
    }
    if !is_reference {
        to_optional_quoted_expr(builder, name, false);
        return;
    }

    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            builder.push(seps[index - 1]);
        }
        to_optional_quoted_expr(builder, part, false);
    }
}

/// `UnionQueryImpl.cleanUnionString`: `trim()` then collapse runs of CR/LF
/// into `NEWLINE`.
fn clean_union_string(text: &str) -> String {
    // Java's String.trim() removes every character <= ' ' from both ends
    let trimmed = text.trim_matches(|c: char| c <= ' ');
    let mut result = String::with_capacity(trimmed.len());
    let mut in_newline_run = false;
    for character in trimmed.chars() {
        if (character == '\r') || (character == '\n') {
            if !in_newline_run {
                result.push_str(NEWLINE);
                in_newline_run = true;
            }
        } else {
            in_newline_run = false;
            result.push(character);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use super::*;

    use crate::datatype::DataType;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// `resources/query_ref`; resolved from the crate root so the tests work
    /// from any working directory.
    fn query_ref_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/query_ref")
    }

    fn unescape(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(character) = chars.next() {
            if character != '\\' {
                result.push(character);
                continue;
            }
            match chars.next() {
                Some('\\') => result.push('\\'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('n') => result.push('\n'),
                Some('0') => result.push('\0'),
                Some('e') => {}
                other => panic!("bad escape in {text:?}: {other:?}"),
            }
        }
        result
    }

    /// A TSV string field: empty is `null`, `\e` is the empty string, and any
    /// other value is unescaped.
    fn parse_string_field(field: &str) -> Option<String> {
        if field.is_empty() {
            None
        } else if field == "\\e" {
            Some(String::new())
        } else {
            Some(unescape(field))
        }
    }

    fn parse_int_field<T: std::str::FromStr>(field: &str) -> Option<T> {
        if field.is_empty() {
            None
        } else {
            field.parse().ok()
        }
    }

    fn parse_hex(field: &str) -> Vec<u8> {
        assert_eq!(field.len() % 2, 0, "bad hex {field:?}");
        (0..field.len() / 2)
            .map(|index| {
                u8::from_str_radix(&field[index * 2..index * 2 + 2], 16)
                    .unwrap_or_else(|e| panic!("bad hex {field:?}: {e}"))
            })
            .collect()
    }

    fn parse_rows(text: &str) -> Vec<QueryRow> {
        let mut rows = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 8, "bad row line {line:?}");
            rows.push(QueryRow {
                attribute: parse_int_field::<i32>(fields[0]).map(|value| value as u8),
                expression: parse_string_field(fields[1]),
                flag: parse_int_field::<i32>(fields[2]).map(|value| value as i16),
                lv_extra: parse_int_field::<i32>(fields[3]),
                name1: parse_string_field(fields[4]),
                name2: parse_string_field(fields[5]),
                object_id: parse_int_field::<i32>(fields[6]),
                order: if fields[7].is_empty() {
                    None
                } else {
                    Some(parse_hex(fields[7]))
                },
            });
        }
        rows
    }

    /// One case from `cases.tsv` plus its `expected.tsv` line.
    struct Case {
        id: String,
        object_flag: i32,
        object_id: i32,
        name: String,
        rows: Vec<QueryRow>,
        /// id, type, objectFlag, hidden, objectId, name, ownerAccess, params,
        /// sql (the raw escaped fields).
        expected: Vec<String>,
    }

    fn error_tag(error: &Error) -> &'static str {
        match error {
            Error::Unsupported(_) => "UnsupportedOperationException",
            Error::IllegalState(_) => "IllegalStateException",
            Error::IllegalArgument(_) => "IllegalArgumentException",
            Error::Jackcess(_) => "IOException",
            _ => "Exception",
        }
    }

    /// A field of the expected output: either text or `ERROR:<SimpleClassName>`.
    #[derive(PartialEq, Eq, Debug)]
    enum Expected {
        Text(String),
        Error(String),
    }

    /// The corpus is generated on Windows, where `System.lineSeparator()` is
    /// CRLF like `NEWLINE`; a run elsewhere renders LF.
    fn platform_text(text: String) -> String {
        if cfg!(windows) {
            text
        } else {
            text.replace("\r\n", "\n")
        }
    }

    fn expected_text(field: &str) -> Expected {
        match field.strip_prefix("ERROR:") {
            Some(tag) => Expected::Error(tag.to_string()),
            None => Expected::Text(platform_text(unescape(field))),
        }
    }

    fn expected_params(field: &str) -> Expected {
        match field.strip_prefix("ERROR:") {
            Some(tag) => Expected::Error(tag.to_string()),
            None => Expected::Text(platform_text(unescape(field))),
        }
    }

    fn actual_text(result: Result<String>) -> Expected {
        match result {
            Ok(text) => Expected::Text(text),
            Err(error) => Expected::Error(error_tag(&error).to_string()),
        }
    }

    fn actual_params(result: Result<Vec<String>>) -> Expected {
        match result {
            Ok(params) => Expected::Text(params.join("|")),
            Err(error) => Expected::Error(error_tag(&error).to_string()),
        }
    }

    fn compare(field: &str, expected: Expected, actual: Expected, problems: &mut Vec<String>) {
        if expected != actual {
            problems.push(format!("{field}: expected {expected:?}, got {actual:?}"));
        }
    }

    fn load_cases() -> Vec<Case> {
        let dir = query_ref_dir();
        let cases_text = std::fs::read_to_string(dir.join("cases.tsv"))
            .expect("resources/query_ref/cases.tsv is missing");
        let expected_text = std::fs::read_to_string(dir.join("expected.tsv"))
            .expect("resources/query_ref/expected.tsv is missing");

        let mut expected: HashMap<String, Vec<String>> = HashMap::new();
        for line in expected_text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<String> = line.split('\t').map(|f| f.to_string()).collect();
            assert_eq!(fields.len(), 9, "bad expected line {line:?}");
            expected.insert(fields[0].clone(), fields);
        }

        let mut cases = Vec::new();
        for line in cases_text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 4, "bad case line {line:?}");
            let id = fields[0].to_string();
            let object_flag = fields[1].parse().expect("bad object flag");
            let object_id = fields[2].parse().expect("bad object id");
            let name = unescape(fields[3]);
            let rows_path = dir.join("rows").join(format!("{id}.tsv"));
            let rows_text = std::fs::read_to_string(&rows_path)
                .unwrap_or_else(|e| panic!("could not read {}: {e}", rows_path.display()));
            let expected_fields = expected
                .get(&id)
                .unwrap_or_else(|| panic!("no expected.tsv line for case {id}"))
                .clone();
            cases.push(Case {
                id,
                object_flag,
                object_id,
                name,
                rows: parse_rows(&rows_text),
                expected: expected_fields,
            });
        }

        assert!(!cases.is_empty(), "the query reference corpus is empty");
        assert_eq!(
            cases.len(),
            expected.len(),
            "cases.tsv and expected.tsv describe a different number of cases"
        );
        cases
    }

    /// Runs every case whose id matches `filter`; returns (passed, total) and
    /// panics with the details of every failing case.
    fn run_cases(filter: impl Fn(&str) -> bool) -> (usize, usize) {
        let mut total = 0usize;
        let mut passed = 0usize;
        let mut failures: Vec<String> = Vec::new();

        for case in load_cases().into_iter().filter(|case| filter(&case.id)) {
            total += 1;
            let mut problems: Vec<String> = Vec::new();
            match create_query(case.object_flag, &case.name, &case.rows, case.object_id) {
                Err(error) => problems.push(format!("create_query failed: {error}")),
                Ok(query) => {
                    let expected = &case.expected;

                    if query.query_type().name() != expected[1] {
                        problems.push(format!(
                            "type: expected {:?}, got {:?}",
                            expected[1],
                            query.query_type().name()
                        ));
                    }
                    if query.object_flag().to_string() != expected[2] {
                        problems.push(format!(
                            "object_flag: expected {:?}, got {:?}",
                            expected[2],
                            query.object_flag()
                        ));
                    }
                    if query.is_hidden().to_string() != expected[3] {
                        problems.push(format!(
                            "hidden: expected {:?}, got {:?}",
                            expected[3],
                            query.is_hidden()
                        ));
                    }
                    if query.object_id().to_string() != expected[4] {
                        problems.push(format!(
                            "object_id: expected {:?}, got {:?}",
                            expected[4],
                            query.object_id()
                        ));
                    }
                    let expected_name = unescape(&expected[5]);
                    if query.name() != expected_name {
                        problems.push(format!(
                            "name: expected {expected_name:?}, got {:?}",
                            query.name()
                        ));
                    }
                    compare(
                        "owner_access",
                        expected_text(&expected[6]),
                        actual_text(
                            query
                                .get_owner_access_type()
                                .map(|access| access.to_string()),
                        ),
                        &mut problems,
                    );
                    compare(
                        "parameters",
                        expected_params(&expected[7]),
                        actual_params(query.get_parameters()),
                        &mut problems,
                    );
                    compare(
                        "sql",
                        expected_text(&expected[8]),
                        actual_text(query.to_sql_string()),
                        &mut problems,
                    );
                }
            }

            if problems.is_empty() {
                passed += 1;
            } else {
                failures.push(format!("case {}: {}", case.id, problems.join("; ")));
            }
        }

        assert!(total > 0, "no cases matched the filter");
        if !failures.is_empty() {
            panic!(
                "{} of {} query cases failed:\n{}",
                failures.len(),
                total,
                failures.join("\n")
            );
        }
        (passed, total)
    }

    #[test]
    fn corpus_select() {
        assert_eq!(run_cases(|id| id.starts_with("select_")), (55, 55));
    }

    #[test]
    fn corpus_crosstab() {
        assert_eq!(run_cases(|id| id.starts_with("crosstab_")), (7, 7));
    }

    #[test]
    fn corpus_union() {
        assert_eq!(run_cases(|id| id.starts_with("union_")), (8, 8));
    }

    #[test]
    fn corpus_delete() {
        assert_eq!(run_cases(|id| id.starts_with("delete_")), (5, 5));
    }

    #[test]
    fn corpus_update() {
        assert_eq!(run_cases(|id| id.starts_with("update_")), (6, 6));
    }

    #[test]
    fn corpus_append() {
        assert_eq!(run_cases(|id| id.starts_with("append_")), (9, 9));
    }

    #[test]
    fn corpus_maketable() {
        assert_eq!(run_cases(|id| id.starts_with("maketable_")), (3, 3));
    }

    #[test]
    fn corpus_passthrough() {
        assert_eq!(run_cases(|id| id.starts_with("passthrough")), (3, 3));
    }

    #[test]
    fn corpus_data_definition() {
        assert_eq!(run_cases(|id| id.starts_with("datadef")), (2, 2));
    }

    #[test]
    fn corpus_unknown() {
        assert_eq!(
            run_cases(|id| id.starts_with("unknown_") || id == "type_mismatch"),
            (4, 4)
        );
    }

    #[test]
    fn corpus_ordering_and_inference() {
        assert_eq!(
            run_cases(|id| id.starts_with("order_") || id == "row_type_inference"),
            (5, 5)
        );
    }

    #[test]
    fn corpus_is_complete() {
        let cases = load_cases();
        assert_eq!(
            cases.len(),
            107,
            "unexpected number of cases in resources/query_ref/cases.tsv"
        );

        let count =
            |filter: &dyn Fn(&str) -> bool| cases.iter().filter(|case| filter(&case.id)).count();
        let select = count(&|id| id.starts_with("select_"));
        let crosstab = count(&|id| id.starts_with("crosstab_"));
        let union = count(&|id| id.starts_with("union_"));
        let delete = count(&|id| id.starts_with("delete_"));
        let update = count(&|id| id.starts_with("update_"));
        let append = count(&|id| id.starts_with("append_"));
        let maketable = count(&|id| id.starts_with("maketable_"));
        let passthrough = count(&|id| id.starts_with("passthrough"));
        let datadef = count(&|id| id.starts_with("datadef"));
        let unknown = count(&|id| id.starts_with("unknown_") || id == "type_mismatch");
        let ordering = count(&|id| id.starts_with("order_") || id == "row_type_inference");
        assert_eq!(
            select
                + crosstab
                + union
                + delete
                + update
                + append
                + maketable
                + passthrough
                + datadef
                + unknown
                + ordering,
            cases.len()
        );
    }

    // -- real-file fixtures -------------------------------------------------
    //
    // `resources/realquery_ref/<tag>.txt` is the Java jackcess dump of an
    // Access-created database, written by `tools/verify-realquery.ps1`.  It
    // holds the raw `MSysQueries` rows (`ROW|...`) and the query records Java
    // renders from them (`QUERY|...`, `SQL|...`/`SQLERROR|...`), so the whole
    // query model is checked against real rows without opening a database.

    fn real_ref_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/realquery_ref")
    }

    /// The escaping `RealQueryDump.java`/`realquery.rs` apply to every field
    /// (the inverse of [`unescape`]).
    fn escape(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        for character in text.chars() {
            match character {
                '\\' => result.push_str("\\\\"),
                '\t' => result.push_str("\\t"),
                '\r' => result.push_str("\\r"),
                '\n' => result.push_str("\\n"),
                '\0' => result.push_str("\\0"),
                other => result.push(other),
            }
        }
        result
    }

    /// A dump field: `<null>` is SQL `NULL`, anything else is escaped text.
    fn dump_field(field: &str) -> Option<String> {
        if field == "<null>" {
            None
        } else {
            Some(unescape(field))
        }
    }

    fn dump_number(field: &str) -> Option<i32> {
        if field == "<null>" {
            None
        } else {
            Some(
                field
                    .parse()
                    .unwrap_or_else(|e| panic!("bad number {field:?}: {e}")),
            )
        }
    }

    /// A `QUERY|...` record and the `SQL|...`/`SQLERROR|...` record after it.
    struct RealQuery {
        name: String,
        type_name: String,
        object_flag: i32,
        object_id: i32,
        hidden: bool,
        /// the escaped `ownerAccess` field (or `ERROR:<tag>`)
        owner_access: String,
        /// the escaped `parameters` field (or `ERROR:<tag>`)
        parameters: String,
        /// the escaped SQL record, or the Java exception tag of `SQLERROR`
        sql: Option<std::result::Result<String, String>>,
    }

    /// Parses a dump into the query records and the raw rows grouped by object
    /// id.
    fn parse_real_dump(text: &str) -> (Vec<RealQuery>, HashMap<i32, Vec<QueryRow>>) {
        let mut queries: Vec<RealQuery> = Vec::new();
        let mut rows: HashMap<i32, Vec<QueryRow>> = HashMap::new();

        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("ROW|") {
                let fields: Vec<&str> = rest.split('|').collect();
                assert_eq!(fields.len(), 8, "bad ROW line {line:?}");
                let value = |index: usize, prefix: &str| {
                    fields[index]
                        .strip_prefix(prefix)
                        .unwrap_or_else(|| panic!("bad ROW field {prefix:?} in {line:?}"))
                };
                let object_id = dump_number(value(0, "objectId=")).expect("row object id");
                let row = QueryRow {
                    // the dump prints the Java Byte, so 0xFF is reported as -1
                    attribute: dump_number(value(1, "attribute=")).map(|value| value as u8),
                    expression: dump_field(value(2, "expression=")),
                    flag: dump_number(value(3, "flag=")).map(|value| value as i16),
                    lv_extra: dump_number(value(4, "extra=")),
                    name1: dump_field(value(5, "name1=")),
                    name2: dump_field(value(6, "name2=")),
                    object_id: Some(object_id),
                    order: {
                        let order = value(7, "order=");
                        if order == "<null>" {
                            None
                        } else {
                            assert_eq!(order.len() % 2, 0, "bad order hex {order:?}");
                            Some(
                                (0..order.len() / 2)
                                    .map(|index| {
                                        u8::from_str_radix(&order[index * 2..index * 2 + 2], 16)
                                            .unwrap_or_else(|e| {
                                                panic!("bad order hex {order:?}: {e}")
                                            })
                                    })
                                    .collect(),
                            )
                        }
                    },
                };
                rows.entry(object_id).or_default().push(row);
            } else if let Some(rest) = line.strip_prefix("QUERY|") {
                let (name, rest) = rest.split_once('|').expect("QUERY name");
                let rest = rest.strip_prefix("type=").expect("QUERY type");
                let (type_name, rest) = rest.split_once("|objectFlag=").expect("QUERY flag");
                let (object_flag, rest) = rest.split_once("|objectId=").expect("QUERY id");
                let (object_id, rest) = rest.split_once("|hidden=").expect("QUERY hidden");
                let (hidden, rest) = rest.split_once("|ownerAccess=").expect("QUERY owner");
                let (owner_access, parameters) =
                    rest.split_once("|parameters=").expect("QUERY params");
                queries.push(RealQuery {
                    name: unescape(name),
                    type_name: type_name.to_string(),
                    object_flag: object_flag.parse().expect("QUERY object flag"),
                    object_id: object_id.parse().expect("QUERY object id"),
                    hidden: hidden.parse().expect("QUERY hidden"),
                    owner_access: owner_access.to_string(),
                    parameters: parameters.to_string(),
                    sql: None,
                });
            } else if let Some(rest) = line.strip_prefix("SQL|") {
                queries.last_mut().expect("SQL without QUERY").sql = Some(Ok(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix("SQLERROR|") {
                let (tag, _message) = rest.split_once('|').expect("SQLERROR record");
                queries.last_mut().expect("SQLERROR without QUERY").sql =
                    Some(Err(tag.to_string()));
            }
        }
        (queries, rows)
    }

    /// The problems of one fixture query: empty when the Rust model reproduces
    /// the Java dump record exactly.
    fn check_real_query(query: &RealQuery, rows: &[QueryRow]) -> Vec<String> {
        let mut problems = Vec::new();
        let model = match create_query(query.object_flag, &query.name, rows, query.object_id) {
            Ok(model) => model,
            Err(error) => return vec![format!("create_query failed: {error}")],
        };
        if model.query_type().name() != query.type_name {
            problems.push(format!(
                "type: expected {:?}, got {:?}",
                query.type_name,
                model.query_type().name()
            ));
        }
        if model.object_flag() != query.object_flag {
            problems.push(format!(
                "object_flag: expected {}, got {}",
                query.object_flag,
                model.object_flag()
            ));
        }
        if model.object_id() != query.object_id {
            problems.push(format!(
                "object_id: expected {}, got {}",
                query.object_id,
                model.object_id()
            ));
        }
        if model.is_hidden() != query.hidden {
            problems.push(format!(
                "hidden: expected {}, got {}",
                query.hidden,
                model.is_hidden()
            ));
        }
        if model.name() != query.name {
            problems.push(format!(
                "name: expected {:?}, got {:?}",
                query.name,
                model.name()
            ));
        }

        let actual_owner = match model.get_owner_access_type() {
            Ok(access) => escape(access),
            Err(error) => format!("ERROR:{}", error_tag(&error)),
        };
        if actual_owner != query.owner_access {
            problems.push(format!(
                "owner_access: expected {:?}, got {:?}",
                query.owner_access, actual_owner
            ));
        }
        let actual_parameters = match model.get_parameters() {
            Ok(parameters) => escape(&parameters.join("|")),
            Err(error) => format!("ERROR:{}", error_tag(&error)),
        };
        if actual_parameters != query.parameters {
            problems.push(format!(
                "parameters: expected {:?}, got {:?}",
                query.parameters, actual_parameters
            ));
        }
        match (&query.sql, model.to_sql_string()) {
            (Some(Ok(expected)), Ok(actual)) => {
                if escape(&actual) != *expected {
                    problems.push(format!(
                        "sql: expected {expected:?}, got {:?}",
                        escape(&actual)
                    ));
                }
            }
            (Some(Err(expected_tag)), Err(error)) => {
                if error_tag(&error) != expected_tag {
                    problems.push(format!(
                        "sql error: expected {expected_tag}, got {}",
                        error_tag(&error)
                    ));
                }
            }
            (Some(Ok(expected)), Err(error)) => {
                problems.push(format!("sql: expected {expected:?}, got error {error}"))
            }
            (Some(Err(expected_tag)), Ok(actual)) => problems.push(format!(
                "sql: expected error {expected_tag}, got {:?}",
                escape(&actual)
            )),
            (None, _) => problems.push("fixture has no SQL record".to_string()),
        }
        problems
    }

    fn run_real_fixture(tag: &str, expected_queries: usize) {
        let path = real_ref_dir().join(format!("{tag}.txt"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "could not read {}: {e} (regenerate it with tools\\verify-realquery.ps1)",
                path.display()
            )
        });
        let (queries, rows) = parse_real_dump(&text);
        assert_eq!(
            queries.len(),
            expected_queries,
            "{tag}: unexpected number of queries in the fixture"
        );
        let mut failures = Vec::new();
        for query in &queries {
            let empty = Vec::new();
            let query_rows = rows.get(&query.object_id).unwrap_or(&empty);
            let problems = check_real_query(query, query_rows);
            if !problems.is_empty() {
                failures.push(format!("{}: {}", query.name, problems.join("; ")));
            }
        }
        assert!(
            failures.is_empty(),
            "{tag}: {} of {} real queries failed:\n{}",
            failures.len(),
            queries.len(),
            failures.join("\n")
        );
    }

    #[test]
    fn real_file_query_v2000() {
        run_real_fixture("queryTestV2000", 9);
    }

    #[test]
    fn real_file_query_v2003() {
        run_real_fixture("queryV2003", 9);
    }

    #[test]
    fn real_file_query_v2007() {
        run_real_fixture("queryV2007", 9);
    }

    #[test]
    fn real_file_query_v2010() {
        run_real_fixture("queryV2010", 9);
    }

    /// The ODBC-linked database is also a real Access application: 56 saved
    /// (form/report source) queries, mostly with derived tables.
    #[test]
    fn real_file_linked_odbc() {
        run_real_fixture("linkedOdbcV2007", 56);
    }

    /// The Access 97 database has the same nine saved queries as the later
    /// formats; its `MSysQueries` rows also carry the `START`/`END` attribute
    /// rows (the `END` row has the byte 0xFF, reported as -1) and NUL-terminated
    /// DDL text.
    #[test]
    fn real_file_query_v1997() {
        run_real_fixture("queryV1997", 9);
    }

    // -- direct API tests ---------------------------------------------------

    fn type_row(flag: i16) -> QueryRow {
        QueryRow {
            attribute: Some(TYPE_ATTRIBUTE),
            flag: Some(flag),
            ..QueryRow::default()
        }
    }

    fn column_row(expression: &str) -> QueryRow {
        QueryRow {
            attribute: Some(COLUMN_ATTRIBUTE),
            expression: Some(expression.to_string()),
            ..QueryRow::default()
        }
    }

    fn table_row(name: &str) -> QueryRow {
        QueryRow {
            attribute: Some(TABLE_ATTRIBUTE),
            name1: Some(name.to_string()),
            ..QueryRow::default()
        }
    }

    #[test]
    fn create_query_mirrors_the_java_type_selection() {
        // a select flagged as hidden
        let query = create_query(0x08, "q", &[type_row(1)], 42).unwrap();
        assert_eq!(query.query_type(), QueryType::Select);
        assert!(query.is_hidden());
        assert_eq!(query.object_id(), 42);
        assert_eq!(query.object_flag(), 0x08);
        assert_eq!(query.name(), "q");
        assert_eq!(query.rows().len(), 1);

        // the object flag decides when it names a type
        assert_eq!(
            create_query(APPEND_QUERY_OBJECT_FLAG, "q", &[type_row(3)], 1)
                .unwrap()
                .query_type(),
            QueryType::Append
        );

        // ... and the rows override a plain (0) object flag
        assert_eq!(
            create_query(0, "q", &[type_row(4)], 1)
                .unwrap()
                .query_type(),
            QueryType::Update
        );

        // a type row which contradicts the object flag is UNKNOWN
        let unknown = create_query(APPEND_QUERY_OBJECT_FLAG, "q", &[type_row(1)], 1).unwrap();
        assert_eq!(unknown.query_type(), QueryType::Unknown);
        assert!(matches!(
            unknown.to_sql_string(),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn query_types_default_from_the_object_flag() {
        let types = [
            (QueryType::Select, SELECT_QUERY_OBJECT_FLAG),
            (QueryType::MakeTable, MAKE_TABLE_QUERY_OBJECT_FLAG),
            (QueryType::Append, APPEND_QUERY_OBJECT_FLAG),
            (QueryType::Update, UPDATE_QUERY_OBJECT_FLAG),
            (QueryType::Delete, DELETE_QUERY_OBJECT_FLAG),
            (QueryType::CrossTab, CROSS_TAB_QUERY_OBJECT_FLAG),
            (QueryType::DataDefinition, DATA_DEF_QUERY_OBJECT_FLAG),
            (QueryType::Passthrough, PASSTHROUGH_QUERY_OBJECT_FLAG),
            (QueryType::Union, UNION_QUERY_OBJECT_FLAG),
        ];
        for (query_type, object_flag) in types {
            assert_eq!(QueryType::from_value(query_type.value()), Some(query_type));
            assert_eq!(
                create_query(object_flag, "q", &[], 1).unwrap().query_type(),
                query_type
            );
        }
        assert_eq!(QueryType::Unknown.value(), -1);
        assert_eq!(QueryType::Unknown.object_flag(), -1);
        assert_eq!(QueryType::from_value(0), None);
        assert_eq!(QueryType::from_value(255), None);
    }

    #[test]
    fn text_parameters_report_their_size() {
        let rows = vec![
            type_row(1),
            QueryRow {
                attribute: Some(PARAMETER_ATTRIBUTE),
                flag: Some(TEXT_FLAG),
                name1: Some("A".to_string()),
                lv_extra: Some(30),
                ..QueryRow::default()
            },
            QueryRow {
                attribute: Some(PARAMETER_ATTRIBUTE),
                flag: Some(TEXT_FLAG),
                name1: Some("B".to_string()),
                lv_extra: Some(0),
                ..QueryRow::default()
            },
            column_row("X"),
            table_row("T"),
        ];
        let query = create_query(0, "q", &rows, 1).unwrap();
        assert_eq!(
            query.get_parameters().unwrap(),
            vec!["A Text(30)".to_string(), "B Text".to_string()]
        );
        assert_eq!(
            query.to_sql_string().unwrap(),
            format!("PARAMETERS A Text(30), B Text;{NEWLINE}SELECT X{NEWLINE}FROM T;")
        );
    }

    #[test]
    fn quote_rules_match_the_java_pattern() {
        let mut builder = String::new();
        to_optional_quoted_expr(&mut builder, "dbo.My Table", true);
        assert_eq!(builder, "dbo.[My Table]");

        let mut builder = String::new();
        to_optional_quoted_expr(&mut builder, "[Already]", true);
        assert_eq!(builder, "[Already]");

        let mut builder = String::new();
        to_optional_quoted_expr(&mut builder, "a.b.", true);
        assert_eq!(builder, "a.b");

        let mut builder = String::new();
        to_optional_quoted_expr(&mut builder, ".", true);
        assert_eq!(builder, "");

        // \W is ASCII-only, so a non-ASCII letter is quotable
        let mut builder = String::new();
        to_optional_quoted_expr(&mut builder, "Té", true);
        assert_eq!(builder, "[Té]");

        let mut builder = String::new();
        to_parameter_name(&mut builder, Some("Forms!My Form!Field"));
        assert_eq!(builder, "Forms![My Form]!Field");
    }

    #[test]
    fn rows_without_order_are_not_sorted() {
        let rows = vec![column_row("B"), column_row("A"), type_row(1)];
        let query = create_query(0, "q", &rows, 1).unwrap();
        assert_eq!(query.to_sql_string().unwrap(), "SELECT B, A;");
    }

    // -- query properties and name maps -------------------------------------

    /// A scratch database for the property loading tests.
    fn props_test_db(name: &str) -> (Database, PathBuf) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let db = Database::create(crate::format::FileFormat::V2003, &path).unwrap();
        (db, path)
    }

    /// A full row for a system table from `(column, value)` pairs (the column
    /// order of a system table is not part of the public API).
    fn system_row(db: &mut Database, table_name: &str, values: &[(&str, Value)]) -> Vec<Value> {
        let page = db
            .table_lookup
            .get(&table_name.to_uppercase())
            .unwrap_or_else(|| panic!("no system table {table_name}"))
            .0;
        db.load_table_by_page(page).unwrap();
        let table = db.tables.get(&page).unwrap();
        let mut row = vec![Value::Null; table.columns.len()];
        for (name, value) in values {
            let idx = table
                .column_index(name)
                .unwrap_or_else(|| panic!("{table_name} has no column {name}"));
            row[idx] = value.clone();
        }
        row
    }

    /// Adds a `Type = 5` (query) catalog row with the given stored `LvProp`.
    fn add_query_catalog_row(
        db: &mut Database,
        object_id: i32,
        name: &str,
        lv_prop: Option<&[u8]>,
    ) {
        let parent = db.table_parent_id;
        let mut values = vec![
            ("Id", Value::Long(object_id)),
            ("ParentId", Value::Long(parent)),
            ("Name", Value::Text(name.to_string())),
            ("Type", Value::Int(5)),
            ("Flags", Value::Long(0)),
        ];
        if let Some(blob) = lv_prop {
            values.push(("LvProp", Value::Bytes(blob.to_vec())));
        }
        let row = system_row(db, "MSysObjects", &values);
        db.add_system_row("MSysObjects", row).unwrap();
    }

    /// The property blob of a query with the given object level properties.
    fn property_blob(
        db: &Database,
        object_id: i32,
        props: &[(&str, DataType, Value)],
        name_map: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut maps = PropertyMaps::new(object_id);
        {
            let map = maps.get_default();
            for (name, data_type, value) in props {
                map.put_value_typed(name, *data_type, value.clone());
            }
            if let Some(bytes) = name_map {
                map.put_value_typed(
                    QUERY_NAME_MAP_PROP,
                    DataType::Ole,
                    Value::Bytes(bytes.to_vec()),
                );
            }
        }
        maps.write_with_charset(&db.format, db.charset).unwrap()
    }

    /// Appends a name map record (`NameMapImpl.toBytes` layout).
    fn push_name_map_record(
        out: &mut Vec<u8>,
        type_code: i32,
        guid: [u8; 16],
        payload: [u8; 16],
        name: &str,
    ) {
        out.extend_from_slice(&guid);
        out.extend_from_slice(&type_code.to_le_bytes());
        out.extend_from_slice(&payload);
        for unit in name.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }

    /// A query name map with one table record (and its date), one column
    /// record and an end record carrying a writer tag.
    fn sample_name_map() -> Vec<u8> {
        const MAGIC: i32 = 0x550E_CC0A;
        let mut table_guid = [0u8; 16];
        for (index, byte) in table_guid.iter_mut().enumerate() {
            *byte = (index + 1) as u8;
        }
        let date = crate::datenums::LocalDateTime::new(2020, 1, 2, 3, 4, 5, 123_000_000);
        let mut table_payload = [0u8; 16];
        table_payload[0..8].copy_from_slice(&crate::datenums::to_date_double(date).to_le_bytes());
        let mut column_guid = [0u8; 16];
        for (index, byte) in column_guid.iter_mut().enumerate() {
            *byte = (index + 0x21) as u8;
        }
        let mut end_payload = [0u8; 16];
        end_payload[0..4].copy_from_slice(&0x1234_5678i32.to_le_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        push_name_map_record(&mut out, 0, table_guid, table_payload, "Data");
        out.extend_from_slice(&0u32.to_le_bytes());
        push_name_map_record(&mut out, 7, column_guid, table_guid, "Name");
        out.extend_from_slice(&0u32.to_le_bytes());
        push_name_map_record(&mut out, 12, [0u8; 16], end_payload, "");
        out
    }

    #[test]
    fn query_without_a_database_has_an_empty_property_map() {
        // `QueryImpl.create(String, List, int)` has no database, so Java cannot
        // read the properties at all; the port reports the empty default map
        let query = create_query(0, "q", &[type_row(1)], 42).unwrap();
        assert!(query.get_properties().unwrap().is_empty());
        assert!(query.property_maps_error().is_none());
        // the group holds the object level map `getProperties()` creates
        assert_eq!(query.get_property_maps().unwrap().len(), 1);
        assert_eq!(
            query
                .get_property_maps()
                .unwrap()
                .default_map()
                .unwrap()
                .name(),
            crate::property::DEFAULT_NAME
        );
        assert!(query.get_name_map().unwrap().is_none());

        // a map installed directly is visible through every accessor
        let mut maps = PropertyMaps::new(42);
        maps.get_default().put_value_typed(
            crate::property::DESCRIPTION_PROP,
            DataType::Text,
            Value::Text("described".to_string()),
        );
        let mut query = query;
        query.set_property_maps(maps);
        assert_eq!(
            query
                .get_properties()
                .unwrap()
                .get_trimmed_string(crate::property::DESCRIPTION_PROP),
            Some("described")
        );
        assert!(query.get_name_map().unwrap().is_none());
    }

    #[test]
    fn query_properties_load_from_the_catalog_row() {
        let (mut db, _path) = props_test_db("queryprops_load.mdb");
        let name_map = sample_name_map();
        let blob = property_blob(
            &db,
            9001,
            &[
                (
                    crate::property::DESCRIPTION_PROP,
                    DataType::Text,
                    Value::Text("a description".to_string()),
                ),
                ("MyByte", DataType::Byte, Value::Byte(0xFB)),
                ("MyInt", DataType::Int, Value::Int(-300)),
                ("MyLong", DataType::Long, Value::Long(70000)),
                ("MyBool", DataType::Boolean, Value::Bool(true)),
            ],
            Some(&name_map),
        );
        add_query_catalog_row(&mut db, 9001, "qryProps", Some(&blob));
        add_query_catalog_row(&mut db, 9002, "qryNoProps", None);

        // the raw storage is exposed for verification
        let blobs = db.get_object_property_blobs().unwrap();
        assert_eq!(blobs.get(&9001).cloned().flatten(), Some(blob.clone()));
        assert!(blobs.contains_key(&9002));
        assert_eq!(blobs.get(&9002).cloned().flatten(), None);

        // Java reads the properties lazily; the port keeps the same error
        // behaviour without failing `get_queries`
        let mut queries = db.get_queries_with_properties().unwrap();
        assert_eq!(queries.len(), 2);
        let query = queries
            .iter_mut()
            .find(|query| query.name() == "qryProps")
            .unwrap();
        assert!(query.property_maps_error().is_none());
        let props = query.get_properties().unwrap();
        assert_eq!(props.len(), 6);
        assert_eq!(
            props.get_trimmed_string(crate::property::DESCRIPTION_PROP),
            Some("a description")
        );
        assert_eq!(props.get_value("MyByte"), Some(&Value::Byte(0xFB)));
        assert_eq!(props.get_value("MyInt"), Some(&Value::Int(-300)));
        assert_eq!(props.get_value("MyLong"), Some(&Value::Long(70000)));
        assert_eq!(props.get_value("MyBool"), Some(&Value::Bool(true)));
        assert!(props.is_ddl(crate::property::DESCRIPTION_PROP));

        let name_map = query.get_name_map().unwrap().expect("name map");
        assert_eq!(name_map.records().len(), 3);
        assert_eq!(name_map.get_writer_tag(), Some(0x1234_5678));
        let table = name_map.get_table_records()[0];
        assert_eq!(table.get_name(), "Data");
        assert_eq!(
            table.get_date(),
            Some(crate::datenums::LocalDateTime::new(
                2020,
                1,
                2,
                3,
                4,
                5,
                123_000_000
            ))
        );
        let column = name_map.get_column_records()[0];
        assert_eq!(column.get_name(), "Name");
        assert_eq!(column.get_parent_guid(), table.get_guid());
        // a query name map cannot be changed
        let mut record = table.clone();
        assert!(matches!(record.set_name("x"), Err(Error::Unsupported(_))));

        // a query without a stored property value has the empty default map
        let empty = queries
            .iter()
            .find(|query| query.name() == "qryNoProps")
            .unwrap();
        let empty_props = empty.get_properties().unwrap();
        assert!(empty_props.is_empty());
        assert_eq!(empty.get_property_maps().unwrap().len(), 1);
        assert!(empty.get_name_map().unwrap().is_none());

        // loading a single query works the same way
        let mut query = queries[0].clone();
        query.set_property_maps(PropertyMaps::new(query.object_id()));
        db.load_query_properties(&mut query).unwrap();
        assert_eq!(query.get_properties().unwrap().len(), 6);

        // a query whose object id has no catalog row at all is empty, like
        // `DatabaseImpl.getPropertiesForObject` with a null row
        let mut ghost = create_query(0, "qryGhost", &[], 999_999).unwrap();
        db.load_query_properties(&mut ghost).unwrap();
        assert!(ghost.get_properties().unwrap().is_empty());
        assert!(ghost.get_name_map().unwrap().is_none());
        assert!(ghost.property_maps_error().is_none());
    }

    #[test]
    fn malformed_query_property_storage_is_reported_lazily() {
        let (mut db, _path) = props_test_db("queryprops_bad.mdb");
        add_query_catalog_row(&mut db, 9101, "qryBadProps", Some(b"XXXX\x01\x02\x03"));
        // a value shorter than the type tag (Java throws an unchecked
        // IndexOutOfBoundsException for it; the port treats it as empty)
        add_query_catalog_row(&mut db, 9102, "qryShortProps", Some(b"MR"));

        let mut queries = db.get_queries_with_properties().unwrap();
        let query = queries
            .iter_mut()
            .find(|query| query.name() == "qryBadProps")
            .unwrap();
        // `getQueries()` succeeds (Java reads the blob lazily) ...
        assert!(query.property_maps_error().is_some());
        // ... and the error is reported by `getProperties()`
        assert_eq!(
            query.property_maps_error(),
            Some("Unknown property map type 58 58 58 58")
        );
        assert_eq!(
            query.get_properties().unwrap_err().to_string(),
            "Unknown property map type 58 58 58 58"
        );
        assert_eq!(
            query.get_name_map().unwrap_err().to_string(),
            "Unknown property map type 58 58 58 58"
        );
        assert!(matches!(query.get_property_maps(), Err(Error::Jackcess(_))));
        // the failed group still holds the default map, and the query can be
        // given a working one
        let mut maps = PropertyMaps::new(query.object_id());
        maps.get_default().put_value_typed(
            crate::property::DESCRIPTION_PROP,
            DataType::Text,
            Value::Text("replaced".to_string()),
        );
        query.set_property_maps(maps);
        assert!(query.property_maps_error().is_none());
        assert_eq!(query.get_properties().unwrap().len(), 1);

        let short = queries
            .iter()
            .find(|query| query.name() == "qryShortProps")
            .unwrap();
        assert!(short.get_properties().unwrap().is_empty());
    }

    #[test]
    fn malformed_query_name_map_is_reported_by_get_name_map() {
        let (mut db, _path) = props_test_db("queryprops_badmap.mdb");
        let blob = property_blob(
            &db,
            9201,
            &[(
                "MyText",
                DataType::Text,
                Value::Text("broken map".to_string()),
            )],
            Some(&[0x11, 0x22, 0x33, 0x44]),
        );
        add_query_catalog_row(&mut db, 9201, "qryBadNameMap", Some(&blob));

        let mut queries = db.get_queries_with_properties().unwrap();
        let query = queries.first_mut().unwrap();
        // the properties themselves decode, so only the name map fails
        assert_eq!(query.get_properties().unwrap().len(), 2);
        assert_eq!(
            query.get_name_map().unwrap_err().to_string(),
            "Unknown name map type 11 22 33 44"
        );

        // a name map which is the magic alone has no records
        let mut maps = PropertyMaps::new(query.object_id());
        maps.get_default().put_value_typed(
            QUERY_NAME_MAP_PROP,
            DataType::Ole,
            Value::Bytes(0x550E_CC0Au32.to_le_bytes().to_vec()),
        );
        query.set_property_maps(maps);
        let name_map = query.get_name_map().unwrap().expect("magic only map");
        assert!(name_map.records().is_empty());
        assert_eq!(name_map.get_writer_tag(), None);

        // a non binary value cannot be a name map (Java casts, and throws a
        // ClassCastException; the port reports an illegal state)
        let mut maps = PropertyMaps::new(query.object_id());
        maps.get_default().put_value_typed(
            QUERY_NAME_MAP_PROP,
            DataType::Text,
            Value::Text("not a name map".to_string()),
        );
        query.set_property_maps(maps);
        assert!(matches!(query.get_name_map(), Err(Error::IllegalState(_))));
    }

    #[test]
    fn query_properties_save_round_trip() {
        let (mut db, path) = props_test_db("queryprops_save.mdb");
        let blob = property_blob(
            &db,
            9301,
            &[("MyText", DataType::Text, Value::Text("before".to_string()))],
            Some(&sample_name_map()),
        );
        add_query_catalog_row(&mut db, 9301, "qrySave", Some(&blob));
        db.flush().unwrap();

        let mut queries = db.get_queries_with_properties().unwrap();
        let query = queries.first_mut().unwrap();
        assert_eq!(
            query.get_properties().unwrap().get_value("MyText"),
            Some(&Value::Text("before".to_string()))
        );
        // update through the live map (Java's getProperties().put + save)
        query.get_properties_mut().unwrap().put_value_typed(
            "MyText",
            DataType::Text,
            Value::Text("after".to_string()),
        );
        query
            .get_properties_mut()
            .unwrap()
            .remove(QUERY_NAME_MAP_PROP);
        assert!(query.get_name_map().unwrap().is_none());
        db.save_query_properties(query).unwrap();
        db.flush().unwrap();
        drop(queries);
        drop(db);

        // the values are stored in the catalog row and read back from it
        let mut db = Database::open_path(&path).unwrap();
        let mut queries = db.get_queries_with_properties().unwrap();
        let query = queries.first_mut().unwrap();
        assert_eq!(
            query.get_properties().unwrap().get_value("MyText"),
            Some(&Value::Text("after".to_string()))
        );
        assert!(query.get_name_map().unwrap().is_none());
    }

    /// The checked in Java dumps of the real query databases are re-rendered
    /// from their stored property blobs: this checks the property and name map
    /// decoders against Access-created data without opening the databases.
    #[test]
    fn real_file_query_property_fixtures() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/queryprops_ref");
        let mut fixture_files = 0usize;
        let mut fixture_queries = 0usize;
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("resources/queryprops_ref is missing")
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("txt"))
            .collect();
        entries.sort();
        assert!(!entries.is_empty(), "the query property fixtures are empty");

        for path in entries {
            fixture_files += 1;
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                text.starts_with("JACKCESS-QUERYPROPS 1\n"),
                "{} has no dump header",
                path.display()
            );

            // group the lines into per query blocks
            struct Block {
                name: String,
                object_id: i32,
                storage: Option<Vec<u8>>,
                lines: Vec<String>,
            }
            let mut blocks: Vec<Block> = Vec::new();
            for line in text.lines() {
                let line = line.trim_end_matches('\r');
                if line == "JACKCESS-QUERYPROPS 1" {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("QUERY|") {
                    let (name, id) = rest
                        .rsplit_once("|objectId=")
                        .unwrap_or_else(|| panic!("bad QUERY line {line:?}"));
                    blocks.push(Block {
                        name: unescape(name),
                        object_id: id.parse().expect("QUERY object id"),
                        storage: None,
                        lines: vec![line.to_string()],
                    });
                } else if let Some(rest) = line.strip_prefix("STORAGE|") {
                    let block = blocks.last_mut().expect("STORAGE without QUERY");
                    block.storage = if rest == "<null>" {
                        None
                    } else {
                        Some(parse_hex(rest))
                    };
                    block.lines.push(line.to_string());
                } else if let Some(block) = blocks.last_mut() {
                    block.lines.push(line.to_string());
                } else {
                    panic!("record before the first QUERY: {line:?}");
                }
            }
            assert!(!blocks.is_empty(), "{} holds no queries", path.display());

            for block in blocks {
                fixture_queries += 1;
                let expected = block.lines.join("\n");
                let mut query = create_query(0, &block.name, &[], block.object_id).unwrap();
                if let Some(blob) = block.storage.as_deref() {
                    // a real Access file stores a decodable blob; the malformed
                    // cases are covered by the harness and the tests above
                    assert!(
                        !expected.contains("PROPSERROR|"),
                        "{}: {} has a malformed property blob",
                        path.display(),
                        block.name
                    );
                    let format = if blob.starts_with(b"KKD\0") {
                        crate::format::JetFormat::version_3()
                    } else {
                        crate::format::JetFormat::version_4()
                    };
                    let maps = PropertyMaps::read(blob, &format).unwrap();
                    query.set_property_maps(maps);
                }
                let mut rendered = String::new();
                dump_query_properties(&query, block.storage.as_deref(), &mut rendered);
                assert_eq!(
                    rendered.trim_end_matches('\n'),
                    expected,
                    "{}: {} does not re-render from its stored blob",
                    path.display(),
                    block.name
                );
            }
        }
        assert_eq!(fixture_files, 6, "unexpected number of fixtures");
        assert_eq!(
            fixture_queries, 101,
            "unexpected number of real query property records"
        );
    }
}
