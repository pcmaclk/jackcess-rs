//! Port of `RelationshipBuilder`, `Relationship`/`RelationshipImpl`,
//! `impl.RelationshipCreator` and the relationship handling of
//! `impl.DatabaseImpl` (`MSysRelationships`, `createRelationshipName`,
//! `writeRelationship`, `collectRelationships`).
//!
//! A relationship is stored as one row of `MSysRelationships` per matched
//! column pair, plus a catalog row in `MSysObjects` (type 8) and the access
//! control entries of that catalog object.  When referential integrity is
//! enforced, jackcess also creates (or reuses) a unique foreign key index on
//! the primary table and a plain foreign key index on the secondary table;
//! each index carries a [`ForeignKeyReference`] to the other one.
//!
//! # Java name mapping (the `Relationship` object API)
//!
//! | Java (`com.healthmarketscience.jackcess.Relationship`) | Rust |
//! |---|---|
//! | `getName()` | field [`Relationship::name`] |
//! | `getFromTable()` / `getToTable()` | [`Relationship::get_from_table`] / [`Relationship::get_to_table`] |
//! | `getFromColumns()` / `getToColumns()` | [`Relationship::get_from_columns`] / [`Relationship::get_to_columns`] |
//! | `isOneToOne()` / `hasReferentialIntegrity()` | [`Relationship::is_one_to_one`] / [`Relationship::has_referential_integrity`] |
//! | `cascadeUpdates()` / `cascadeDeletes()` / `cascadeNullOnDelete()` | [`Relationship::cascade_updates`] / [`Relationship::cascade_deletes`] / [`Relationship::cascade_null_on_delete`] |
//! | `isLeftOuterJoin()` / `isRightOuterJoin()` | [`Relationship::is_left_outer_join`] / [`Relationship::is_right_outer_join`] |
//! | `getJoinType()` | [`Relationship::join_type`] ([`JoinType::name`] gives Java's constant name) |

use std::collections::HashSet;

use crate::database::{
    Database, REL_COL_COLUMN_COUNT, REL_COL_COLUMN_INDEX, REL_COL_FLAGS, REL_COL_FROM_COLUMN,
    REL_COL_FROM_TABLE, REL_COL_NAME, REL_COL_TO_COLUMN, REL_COL_TO_TABLE, SYSTEM_OBJECT_FLAGS,
    TABLE_SYSTEM_RELATIONSHIPS, TYPE_RELATIONSHIP,
};
use crate::error::{Error, Result};
use crate::index::{ForeignKeyReference, IndexImpl};
use crate::table::{IndexFeature, Table};
use crate::tablecreator::IndexSpec;
use crate::tableupdater::{self, IGNORED_PRIMARY_INDEX_FLAGS, IGNORED_SECONDARY_INDEX_FLAGS};
use crate::value::Value;

/// flag indicating a one-to-one relationship
pub const ONE_TO_ONE_FLAG: i32 = 0x0000_0001;
/// flag indicating that referential integrity is *not* enforced
pub const NO_REFERENTIAL_INTEGRITY_FLAG: i32 = 0x0000_0002;
/// flag indicating cascading updates (requires referential integrity)
pub const CASCADE_UPDATES_FLAG: i32 = 0x0000_0100;
/// flag indicating cascading deletes (requires referential integrity)
pub const CASCADE_DELETES_FLAG: i32 = 0x0000_1000;
/// flag indicating cascading null on delete (requires referential integrity)
pub const CASCADE_NULL_FLAG: i32 = 0x0000_2000;
/// flag indicating a left outer join
pub const LEFT_OUTER_JOIN_FLAG: i32 = 0x0100_0000;
/// flag indicating a right outer join
pub const RIGHT_OUTER_JOIN_FLAG: i32 = 0x0200_0000;

/// `RelationshipBuilder.CASCADE_FLAGS`
const CASCADE_FLAGS: i32 = CASCADE_DELETES_FLAG | CASCADE_UPDATES_FLAG | CASCADE_NULL_FLAG;

/// The preferred join type of a relationship (`Relationship.JoinType`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JoinType {
    /// an inner join (the default)
    #[default]
    Inner,
    LeftOuter,
    RightOuter,
}

impl JoinType {
    /// The Java enum constant name (`JoinType.name()`).
    pub fn name(self) -> &'static str {
        match self {
            JoinType::Inner => "INNER",
            JoinType::LeftOuter => "LEFT_OUTER",
            JoinType::RightOuter => "RIGHT_OUTER",
        }
    }
}

/// `RelationshipBuilder.JOIN_FLAGS`
const JOIN_FLAGS: i32 = LEFT_OUTER_JOIN_FLAG | RIGHT_OUTER_JOIN_FLAG;

/// `RelationshipBuilder`: the definition of a relationship to create.
///
/// The "primary" side is jackcess' `from` table (the one holding the unique
/// key) and the "secondary" side its `to` table (the one holding the foreign
/// key).  Column pairs are added in order, the n-th primary column being
/// matched against the n-th secondary column.
#[derive(Clone, Debug, Default)]
pub struct RelationshipSpec {
    /// explicit relationship name, `None` means "use the standard Access
    /// naming convention" (primary table name + secondary table name)
    pub name: Option<String>,
    /// the "from" table (the one with the unique key)
    pub primary_table: String,
    /// the columns of the primary table, aligned with `secondary_columns`
    pub primary_columns: Vec<String>,
    /// the "to" table (the one with the foreign key)
    pub secondary_table: String,
    /// the columns of the secondary table, aligned with `primary_columns`
    pub secondary_columns: Vec<String>,
    /// enforce referential integrity (requires a unique index on the primary
    /// side)
    pub referential_integrity: bool,
    /// request the one-to-one flag.  jackcess derives this flag from the
    /// indexes of the two tables; `None` (the default) does exactly that.
    /// `Some(true)` fails unless the derived value is `true`, `Some(false)`
    /// suppresses the flag (a Rust only extension, jackcess always derives
    /// it).
    pub one_to_one: Option<bool>,
    /// cascade updates from the primary to the secondary table
    pub cascade_updates: bool,
    /// cascade deletes from the primary to the secondary table
    pub cascade_deletes: bool,
    /// set the foreign key columns to null when the primary row is deleted
    pub cascade_null_on_delete: bool,
    /// the preferred join type (jackcess' `setJoinType`, inner by default)
    pub join_type: JoinType,
}

impl RelationshipSpec {
    pub fn new(
        primary_table: impl Into<String>,
        secondary_table: impl Into<String>,
    ) -> RelationshipSpec {
        RelationshipSpec {
            name: None,
            primary_table: primary_table.into(),
            primary_columns: Vec::new(),
            secondary_table: secondary_table.into(),
            secondary_columns: Vec::new(),
            referential_integrity: false,
            one_to_one: None,
            cascade_updates: false,
            cascade_deletes: false,
            cascade_null_on_delete: false,
            join_type: JoinType::Inner,
        }
    }

    /// `RelationshipBuilder.addColumns`
    pub fn add_columns(
        mut self,
        primary_column: impl Into<String>,
        secondary_column: impl Into<String>,
    ) -> RelationshipSpec {
        self.primary_columns.push(primary_column.into());
        self.secondary_columns.push(secondary_column.into());
        self
    }

    /// `RelationshipBuilder.setName`
    pub fn with_name(mut self, name: impl Into<String>) -> RelationshipSpec {
        self.name = Some(name.into());
        self
    }

    /// `RelationshipBuilder.setReferentialIntegrity`
    pub fn set_referential_integrity(mut self) -> RelationshipSpec {
        self.referential_integrity = true;
        self
    }

    /// `RelationshipBuilder.setCascadeUpdates`
    pub fn set_cascade_updates(mut self) -> RelationshipSpec {
        self.cascade_updates = true;
        self
    }

    /// `RelationshipBuilder.setCascadeDeletes`
    pub fn set_cascade_deletes(mut self) -> RelationshipSpec {
        self.cascade_deletes = true;
        self
    }

    /// `RelationshipBuilder.setCascadeNullOnDelete`
    pub fn set_cascade_null_on_delete(mut self) -> RelationshipSpec {
        self.cascade_null_on_delete = true;
        self
    }

    /// `RelationshipBuilder.setJoinType`
    pub fn set_join_type(mut self, join_type: JoinType) -> RelationshipSpec {
        self.join_type = join_type;
        self
    }

    /// The relationship flags the builder would use (`RelationshipBuilder
    /// .getFlags`), before the one-to-one flag is derived.
    pub fn flags(&self) -> i32 {
        let mut flags = if self.referential_integrity {
            0
        } else {
            NO_REFERENTIAL_INTEGRITY_FLAG
        };
        if self.cascade_updates {
            flags |= CASCADE_UPDATES_FLAG;
        }
        if self.cascade_deletes {
            flags |= CASCADE_DELETES_FLAG;
        }
        if self.cascade_null_on_delete {
            flags |= CASCADE_NULL_FLAG;
        }
        flags &= !JOIN_FLAGS;
        match self.join_type {
            JoinType::Inner => {}
            JoinType::LeftOuter => flags |= LEFT_OUTER_JOIN_FLAG,
            JoinType::RightOuter => flags |= RIGHT_OUTER_JOIN_FLAG,
        }
        flags
    }
}

/// `Relationship`/`RelationshipImpl`: a relationship read from (or created in)
/// a database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relationship {
    pub name: String,
    /// the "from" table of the relationship (the primary side)
    pub primary_table: String,
    /// the "to" table of the relationship (the secondary side)
    pub secondary_table: String,
    pub primary_columns: Vec<String>,
    pub secondary_columns: Vec<String>,
    pub flags: i32,
}

impl Relationship {
    fn has_flag(&self, flag: i32) -> bool {
        (self.flags & flag) != 0
    }

    pub fn is_one_to_one(&self) -> bool {
        self.has_flag(ONE_TO_ONE_FLAG)
    }

    pub fn has_referential_integrity(&self) -> bool {
        !self.has_flag(NO_REFERENTIAL_INTEGRITY_FLAG)
    }

    pub fn cascade_updates(&self) -> bool {
        self.has_flag(CASCADE_UPDATES_FLAG)
    }

    pub fn cascade_deletes(&self) -> bool {
        self.has_flag(CASCADE_DELETES_FLAG)
    }

    pub fn cascade_null_on_delete(&self) -> bool {
        self.has_flag(CASCADE_NULL_FLAG)
    }

    pub fn is_left_outer_join(&self) -> bool {
        self.has_flag(LEFT_OUTER_JOIN_FLAG)
    }

    pub fn is_right_outer_join(&self) -> bool {
        self.has_flag(RIGHT_OUTER_JOIN_FLAG)
    }

    pub fn join_type(&self) -> JoinType {
        if self.is_left_outer_join() {
            JoinType::LeftOuter
        } else if self.is_right_outer_join() {
            JoinType::RightOuter
        } else {
            JoinType::Inner
        }
    }

    /// `Relationship.getFromTable()`: the "from" (primary) table of the
    /// relationship.
    ///
    /// Java keeps the `Table` object; the port stores the table name and
    /// resolves it through the database it was read from, so the database is
    /// passed in.
    pub fn get_from_table<'a>(&self, db: &'a mut Database) -> Result<&'a mut Table> {
        db.get_table(&self.primary_table)
    }

    /// `Relationship.getToTable()`: the "to" (secondary, foreign key) table of
    /// the relationship (see [`Relationship::get_from_table`] for why the
    /// database is passed in).
    pub fn get_to_table<'a>(&self, db: &'a mut Database) -> Result<&'a mut Table> {
        db.get_table(&self.secondary_table)
    }

    /// `Relationship.getFromColumns()`: the "from" columns of the relationship,
    /// in the order of [`Relationship::primary_columns`].
    pub fn get_from_columns(&self, db: &mut Database) -> Result<Vec<crate::column::Column>> {
        resolve_columns(db, &self.primary_table, &self.primary_columns)
    }

    /// `Relationship.getToColumns()`: the "to" columns of the relationship,
    /// aligned with [`Relationship::get_from_columns`].
    pub fn get_to_columns(&self, db: &mut Database) -> Result<Vec<crate::column::Column>> {
        resolve_columns(db, &self.secondary_table, &self.secondary_columns)
    }
}

/// Resolves the stored column names of a relationship against its table.
///
/// `RelationshipImpl` keeps the actual `Column` objects (resolved while the
/// relationship is read); the port keeps the names and resolves them on
/// demand.  A name which the table does not hold is an
/// `IllegalArgumentException`, like the other column lookups of the port.
fn resolve_columns(
    db: &mut Database,
    table_name: &str,
    names: &[String],
) -> Result<Vec<crate::column::Column>> {
    let table = db.get_table(table_name)?;
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let column = table.get_column(name).cloned().ok_or_else(|| {
            Error::IllegalArgument(format!(
                "Column {} does not exist in table {}",
                name, table_name
            ))
        })?;
        columns.push(column);
    }
    Ok(columns)
}

/// `DatabaseImpl.toLookupName`: the upper case form used for every name
/// comparison.
fn to_lookup_name(name: &str) -> String {
    name.to_uppercase()
}

/// `StringUtil.truncate`: truncates by UTF-16 code units, like
/// `String.substring`.  (Java can split a surrogate pair here, which a Rust
/// `String` cannot hold; such a name would be truncated one unit earlier.)
fn truncate(name: &str, max_length: usize) -> String {
    if name.encode_utf16().count() <= max_length {
        return name.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in name.chars() {
        let width = ch.len_utf16();
        if used + width > max_length {
            break;
        }
        out.push(ch);
        used += width;
    }
    out
}

/// `DatabaseImpl.validateIdentifierName` for a relationship name.
fn validate_relationship_name(name: &str, max_length: usize) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::IllegalArgument(
            "relationship must have non-blank name".into(),
        ));
    }
    if name.encode_utf16().count() > max_length {
        return Err(Error::IllegalArgument(format!(
            "relationship name is longer than max length of {}: {}",
            max_length, name
        )));
    }
    // `DatabaseImpl.INVALID_IDENTIFIER_CHARS`
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '.' | '!' | '`' | ']' | '['))
    {
        return Err(Error::IllegalArgument(format!(
            "relationship name '{}' contains invalid characters",
            name
        )));
    }
    if name.starts_with(' ') {
        return Err(Error::IllegalArgument(format!(
            "relationship name '{}' cannot start with a space character",
            name
        )));
    }
    Ok(())
}

/// `RelationshipCreator.withErrorContext`.
fn with_error_context(
    msg: &str,
    primary_table: &str,
    primary_columns: &[String],
    secondary_table: &str,
    secondary_columns: &[String],
) -> String {
    format!(
        "{}(Rel={}[{}] -> {}[{}])",
        msg,
        primary_table,
        primary_columns.join(", "),
        secondary_table,
        secondary_columns.join(", ")
    )
}

/// The columns of a table, resolved by name (`RelationshipCreator.getColumns`).
fn get_columns(
    table: &Table,
    table_name: &str,
    names: &[String],
) -> Result<Vec<crate::column::Column>> {
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        match table.get_column(name) {
            Some(col) => columns.push(col.clone()),
            // jackcess ends up with a null in its column list and only fails
            // later with a NullPointerException; report it properly instead
            None => {
                return Err(Error::IllegalArgument(format!(
                    "column named {} not found in table {}",
                    name, table_name
                )))
            }
        }
    }
    Ok(columns)
}

/// `DatabaseImpl.createRelationshipName`: the standard Access relationship
/// name, shortened and made unique.
fn create_relationship_name(
    db: &mut Database,
    spec: &RelationshipSpec,
    primary_table_name: &str,
    secondary_page: i32,
) -> Result<String> {
    let max_id_len = db.format.max_index_name_length;
    let secondary_table_name = db
        .tables
        .get(&secondary_page)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| spec.secondary_table.clone());
    let orig_name = match &spec.name {
        Some(name) => name.clone(),
        None => format!(
            "{}{}",
            truncate(primary_table_name, (max_id_len / 2).saturating_sub(3)),
            secondary_table_name
        ),
    };
    let orig_name = truncate(&orig_name, max_id_len.saturating_sub(3));

    // now ensure the name is unique
    let mut names: HashSet<String> = HashSet::new();
    for name in db.catalog_object_names(TYPE_RELATIONSHIP)? {
        names.insert(to_lookup_name(&name));
    }
    if spec.referential_integrity {
        // the relationship name is also an index name on the secondary table,
        // so those names have to be free as well
        let table = db.tables.get(&secondary_page).unwrap();
        for index in &table.indexes {
            names.insert(to_lookup_name(&index.name));
        }
    }

    let base_name = to_lookup_name(&orig_name);
    let mut name = base_name.clone();
    let mut i = 0;
    while names.contains(&name) {
        i += 1;
        name = format!("{}{}", base_name, i);
    }
    Ok(if i == 0 {
        orig_name
    } else {
        format!("{}{}", orig_name, i)
    })
}

/// `RelationshipCreator.createPrimaryIndexName`: the primary side's supporting
/// index is called `.rB`, `.rC`, ... (`.r[` is skipped for `.ra`).
fn create_primary_index_name(table: &Table) -> String {
    create_primary_index_name_from(&tableupdater::get_index_names(table))
}

fn create_primary_index_name_from(index_names: &HashSet<String>) -> String {
    let mut suffix = b'B';
    loop {
        let name = format!(".r{}", suffix as char);
        if !index_names.contains(&to_lookup_name(&name)) {
            return name;
        }
        let mut next = suffix + 1;
        if next == b'[' {
            next = b'a';
        }
        suffix = next;
    }
}

/// `RelationshipCreator.createFKReference`
fn create_fk_reference(
    is_primary: bool,
    other_idx_num: i32,
    primary_page: i32,
    secondary_page: i32,
    flags: i32,
) -> ForeignKeyReference {
    let (table_type, other_table_num) = if is_primary {
        (IndexImpl::FK_PRIMARY_TABLE_TYPE, secondary_page)
    } else {
        (IndexImpl::FK_SECONDARY_TABLE_TYPE, primary_page)
    };
    ForeignKeyReference::new(
        table_type,
        other_idx_num,
        other_table_num,
        (flags & CASCADE_UPDATES_FLAG) != 0,
        (flags & CASCADE_DELETES_FLAG) != 0,
        (flags & CASCADE_NULL_FLAG) != 0,
    )
}

/// `IndexBuilder.addColumns(name)` for every column, ascending.
fn index_columns(names: &[String]) -> Vec<(String, bool)> {
    names.iter().map(|n| (n.clone(), true)).collect()
}

/// The foreign key index of one side of the relationship, added through the
/// "internal" [`crate::tableupdater::add_index_internal`] path.
fn add_fk_index(
    db: &mut Database,
    table_page: i32,
    spec: IndexSpec,
    reference: ForeignKeyReference,
    ignored_flags: u8,
) -> Result<()> {
    let table = db
        .tables
        .get_mut(&table_page)
        .ok_or_else(|| Error::IllegalState("relation table not loaded".into()))?;
    tableupdater::add_index_internal(
        table,
        &mut db.channel,
        spec,
        Some(reference),
        ignored_flags,
        0,
    )?;
    Ok(())
}

/// `RelationshipCreator.createRelationship`: validates the definition and
/// writes the relationship (and, if integrity is enforced, its indexes).
pub(crate) fn create_relationship(
    db: &mut Database,
    spec: RelationshipSpec,
) -> Result<Relationship> {
    let format = db.format.clone();

    // ------------------------------------------------------------------
    // validate (`RelationshipCreator.validate`)
    // ------------------------------------------------------------------
    let primary_page = match db.table_lookup_entry(&spec.primary_table) {
        Some((page, _)) => {
            db.table_page(&spec.primary_table)?;
            page
        }
        None => {
            return Err(Error::IllegalArgument(with_error_context(
                "Two valid tables are required in relationship",
                &spec.primary_table,
                &spec.primary_columns,
                &spec.secondary_table,
                &spec.secondary_columns,
            )))
        }
    };
    let secondary_page = match db.table_lookup_entry(&spec.secondary_table) {
        Some((page, _)) => {
            db.table_page(&spec.secondary_table)?;
            page
        }
        None => {
            return Err(Error::IllegalArgument(with_error_context(
                "Two valid tables are required in relationship",
                &spec.primary_table,
                &spec.primary_columns,
                &spec.secondary_table,
                &spec.secondary_columns,
            )))
        }
    };

    if let Some(name) = &spec.name {
        validate_relationship_name(name, format.max_index_name_length)?;
    }

    let primary_columns = {
        let table = db.tables.get(&primary_page).unwrap();
        get_columns(table, &spec.primary_table, &spec.primary_columns)?
    };
    let secondary_columns = {
        let table = db.tables.get(&secondary_page).unwrap();
        get_columns(table, &spec.secondary_table, &spec.secondary_columns)?
    };

    // jackcess keeps the Table and Column objects, so every name it writes is
    // the one stored in the database, not the spelling the caller passed in
    let primary_table_name = db.tables.get(&primary_page).unwrap().name.clone();
    let secondary_table_name = db.tables.get(&secondary_page).unwrap().name.clone();
    let primary_col_names: Vec<String> = primary_columns.iter().map(|c| c.name.clone()).collect();
    let secondary_col_names: Vec<String> =
        secondary_columns.iter().map(|c| c.name.clone()).collect();

    let ctx = |msg: &str| {
        with_error_context(
            msg,
            &spec.primary_table,
            &spec.primary_columns,
            &spec.secondary_table,
            &spec.secondary_columns,
        )
    };

    if primary_columns.is_empty() || secondary_columns.is_empty() {
        return Err(Error::IllegalArgument(ctx(
            "Missing columns in relationship",
        )));
    }
    if primary_columns.len() != secondary_columns.len() {
        return Err(Error::IllegalArgument(ctx(
            "Must have same number of columns on each side of relationship",
        )));
    }
    for (pcol, scol) in primary_columns.iter().zip(secondary_columns.iter()) {
        if pcol.data_type != scol.data_type {
            return Err(Error::IllegalArgument(ctx(
                "Matched columns must have the same data type",
            )));
        }
    }

    if !spec.referential_integrity {
        if (spec.flags() & CASCADE_FLAGS) != 0 {
            return Err(Error::IllegalArgument(ctx(
                "Cascade flags cannot be enabled if referential integrity is not enforced",
            )));
        }
    } else {
        // for now, a unique index on the primary table is required (just like
        // access does)
        let primary_idx = {
            let table = db.tables.get(&primary_page).unwrap();
            table.find_index_for_columns(&primary_col_names, IndexFeature::ExactUniqueOnly)
        };
        if primary_idx.is_none() {
            return Err(Error::IllegalArgument(ctx(
                "Missing unique index on primary table required to enforce integrity",
            )));
        }

        // while relationships can have "dupe" columns, indexes (and therefore
        // integrity enforced relationships) cannot
        let primary_set: HashSet<String> = primary_col_names
            .iter()
            .map(|n| to_lookup_name(n))
            .collect();
        let secondary_set: HashSet<String> = secondary_col_names
            .iter()
            .map(|n| to_lookup_name(n))
            .collect();
        if primary_set.len() != primary_col_names.len()
            || secondary_set.len() != secondary_col_names.len()
        {
            return Err(Error::IllegalArgument(ctx(
                "Cannot have duplicate columns in an integrity enforced relationship",
            )));
        }

        // a table can be related to itself, but access refuses to enforce
        // integrity where a column is matched against itself
        if primary_page == secondary_page {
            for (pcol, scol) in primary_columns.iter().zip(secondary_columns.iter()) {
                if pcol.name.eq_ignore_ascii_case(&scol.name) {
                    return Err(Error::IllegalArgument(ctx(&format!(
                        "Cannot enforce integrity on column {} matched against itself",
                        pcol.name
                    ))));
                }
            }
        }

        // check referential integrity: every non-null secondary key must exist
        // in the primary table
        check_referential_integrity(db, &spec, primary_page, secondary_page)?;
    }

    // `RelationshipCreator.createRelationship`: determine the one-to-one flag
    // from the indexes of both tables
    let derived_one_to_one = {
        let primary_unique = {
            let table = db.tables.get(&primary_page).unwrap();
            table
                .find_index_for_columns(&primary_col_names, IndexFeature::ExactUniqueOnly)
                .is_some()
        };
        let secondary_unique = if primary_unique {
            let table = db.tables.get(&secondary_page).unwrap();
            table
                .find_index_for_columns(&secondary_col_names, IndexFeature::ExactUniqueOnly)
                .is_some()
        } else {
            false
        };
        primary_unique && secondary_unique
    };
    let one_to_one = match spec.one_to_one {
        None => derived_one_to_one,
        Some(true) if !derived_one_to_one => {
            return Err(Error::IllegalArgument(ctx(
                "Cannot enforce a one-to-one relationship without unique indexes on both sides",
            )))
        }
        Some(value) => value,
    };
    let mut flags = spec.flags();
    if one_to_one {
        flags |= ONE_TO_ONE_FLAG;
    }

    // ------------------------------------------------------------------
    // write (`RelationshipCreator.createRelationship`,
    //        `DatabaseImpl.writeRelationship`)
    // ------------------------------------------------------------------
    db.channel.start_exclusive_write()?;
    let result = (|| -> Result<Relationship> {
        let name = create_relationship_name(db, &spec, &primary_table_name, secondary_page)?;

        // the rows of MSysRelationships, one per matched column pair
        write_relationship_rows(
            db,
            &name,
            flags,
            &primary_table_name,
            &primary_col_names,
            &secondary_table_name,
            &secondary_col_names,
        )?;

        // the catalog object of the relationship and its access control
        // entries
        let rel_obj_id = db.next_free_synthetic_id()?;
        let rel_parent_id = db.rel_parent_id;
        crate::tablecreator::add_to_system_catalog(
            db,
            &name,
            rel_obj_id,
            TYPE_RELATIONSHIP,
            0,
            None,
            None,
            rel_parent_id,
        )?;
        crate::tablecreator::add_to_access_control_entries(db, rel_obj_id, rel_parent_id)?;

        if spec.referential_integrity {
            // each index carries the number of the other one, so both numbers
            // are worked out before either index is added.  a table related to
            // itself takes both of them, one slot apart, and ms access writes
            // the secondary one first
            let same_table = primary_page == secondary_page;
            let secondary_idx_num = db.tables.get(&secondary_page).unwrap().logical_index_count;
            let primary_idx_num =
                db.tables.get(&primary_page).unwrap().logical_index_count + i32::from(same_table);

            // the secondary side's index uses the relationship name and is not
            // unique
            let mut secondary_spec =
                IndexSpec::new(name.clone(), index_columns(&secondary_col_names));
            secondary_spec.index_type = IndexImpl::FOREIGN_KEY_INDEX_TYPE;
            add_fk_index(
                db,
                secondary_page,
                secondary_spec,
                create_fk_reference(false, primary_idx_num, primary_page, secondary_page, flags),
                IGNORED_SECONDARY_INDEX_FLAGS,
            )?;

            // the primary side's index is unique and gets the auto-generated
            // name
            let primary_index_name = {
                let table = db.tables.get(&primary_page).unwrap();
                create_primary_index_name(table)
            };
            let mut primary_spec =
                IndexSpec::new(primary_index_name, index_columns(&primary_col_names)).unique();
            primary_spec.index_type = IndexImpl::FOREIGN_KEY_INDEX_TYPE;
            add_fk_index(
                db,
                primary_page,
                primary_spec,
                create_fk_reference(true, secondary_idx_num, primary_page, secondary_page, flags),
                IGNORED_PRIMARY_INDEX_FLAGS,
            )?;
        }

        Ok(Relationship {
            name,
            primary_table: primary_table_name.clone(),
            secondary_table: secondary_table_name.clone(),
            primary_columns: primary_col_names.clone(),
            secondary_columns: secondary_col_names.clone(),
            flags,
        })
    })();
    let finish = db.channel.finish_write();
    match result {
        Ok(rel) => {
            finish?;
            Ok(rel)
        }
        Err(e) => {
            let _ = finish;
            Err(e)
        }
    }
}

/// `RelationshipCreator.validate`'s referential integrity check: every
/// non-null secondary key has to match a primary row.
///
/// jackcess looks the secondary values up with the primary index cursor; the
/// keys are therefore encoded with the primary index' column descriptors (and
/// compared as encoded keys, just like the index does).
fn check_referential_integrity(
    db: &mut Database,
    spec: &RelationshipSpec,
    primary_page: i32,
    secondary_page: i32,
) -> Result<()> {
    let format = db.format.clone();
    let charset = db.charset;
    // the primary index' `ColumnDescriptor` asks the database for the
    // `DateTimeFactory`/zone a `SHORT_DATE_TIME` key is encoded with
    let date_context = crate::column::DateContext::new(db.date_time_type, db.tz);

    let key_info = primary_columns_key_info(db, primary_page, spec)?;

    let primary_keys: HashSet<Vec<u8>> = {
        let rows = db.read_table_rows(primary_page)?;
        let mut keys = HashSet::new();
        for row in rows.into_iter().flatten() {
            let mut entry = Vec::new();
            for (col, flags) in &key_info {
                let idx = crate::index::IndexData::row_value_index(col);
                let value = row.get(idx).unwrap_or(&Value::Null);
                let desc = crate::index::ColumnDescriptor::with_date_context(
                    col.clone(),
                    *flags,
                    &format,
                    date_context,
                );
                desc.write_value(value, &mut entry, &format, charset)?;
            }
            let max_len = crate::index::MAX_KEY_UNITS * format.size_text_field_unit;
            keys.insert(crate::index::truncate_entry_bytes(&entry, max_len));
        }
        keys
    };

    let secondary_columns = {
        let table = db.tables.get(&secondary_page).unwrap();
        get_columns(table, &spec.secondary_table, &spec.secondary_columns)?
    };
    let rows = db.read_table_rows(secondary_page)?;
    for row in rows.into_iter().flatten() {
        let mut values = Vec::with_capacity(secondary_columns.len());
        let mut has_values = false;
        for col in &secondary_columns {
            let value = row.get(col.column_index).cloned().unwrap_or(Value::Null);
            has_values = has_values || value != Value::Null;
            values.push(value);
        }
        if !has_values {
            // we can ignore null entries
            continue;
        }
        if !primary_keys.contains(&encode_ordered(
            &values,
            &format,
            charset,
            &key_info,
            date_context,
        )) {
            return Err(Error::Jackcess(with_error_context(
                "Integrity constraint violation found for relationship",
                &spec.primary_table,
                &spec.primary_columns,
                &spec.secondary_table,
                &spec.secondary_columns,
            )));
        }
    }
    Ok(())
}

/// The (column, flags) description of the primary table's unique index, used
/// to encode search keys.
fn primary_columns_key_info(
    db: &Database,
    primary_page: i32,
    spec: &RelationshipSpec,
) -> Result<Vec<(crate::column::Column, u8)>> {
    let table = db
        .tables
        .get(&primary_page)
        .ok_or_else(|| Error::IllegalState("primary table not loaded".into()))?;
    let primary_names = spec.primary_columns.clone();
    let idx = table
        .find_index_for_columns(&primary_names, IndexFeature::ExactUniqueOnly)
        .ok_or_else(|| {
            Error::IllegalArgument(
                "Missing unique index on primary table required to enforce integrity".into(),
            )
        })?;
    let data = table.index_data_for(idx).unwrap();
    Ok(data
        .columns
        .iter()
        .map(|c| (c.column.clone(), c.flags))
        .collect())
}

/// Encodes the given values with the (column, flags) list of the primary key
/// index, in order.
fn encode_ordered(
    values: &[Value],
    format: &crate::format::JetFormat,
    charset: crate::format::Charset,
    key_info: &[(crate::column::Column, u8)],
    date_context: crate::column::DateContext,
) -> Vec<u8> {
    let mut entry = Vec::new();
    for (i, (col, flags)) in key_info.iter().enumerate() {
        let value = values.get(i).unwrap_or(&Value::Null);
        let desc = crate::index::ColumnDescriptor::with_date_context(
            col.clone(),
            *flags,
            format,
            date_context,
        );
        if desc
            .write_value(value, &mut entry, format, charset)
            .is_err()
        {
            return Vec::new();
        }
    }
    let max_len = crate::index::MAX_KEY_UNITS * format.size_text_field_unit;
    crate::index::truncate_entry_bytes(&entry, max_len)
}

/// `DatabaseImpl.writeRelationship`: the `MSysRelationships` rows of one
/// relationship.
#[allow(clippy::too_many_arguments)]
fn write_relationship_rows(
    db: &mut Database,
    name: &str,
    flags: i32,
    primary_table_name: &str,
    primary_columns: &[String],
    secondary_table_name: &str,
    secondary_columns: &[String],
) -> Result<()> {
    let page = db
        .table_lookup_entry(TABLE_SYSTEM_RELATIONSHIPS)
        .map(|(p, _)| p)
        .ok_or_else(|| {
            Error::Jackcess(format!(
                "Could not find system table {}",
                TABLE_SYSTEM_RELATIONSHIPS
            ))
        })?;
    db.table_page(TABLE_SYSTEM_RELATIONSHIPS)?;

    let (
        column_count,
        ccolumn,
        grbit_col,
        icolumn,
        name_col,
        from_table_col,
        from_col_col,
        to_table_col,
        to_col_col,
    ) = {
        let table = db.tables.get(&page).unwrap();
        (
            table.columns.len(),
            table.column_index(REL_COL_COLUMN_COUNT),
            table.column_index(REL_COL_FLAGS),
            table.column_index(REL_COL_COLUMN_INDEX),
            table.column_index(REL_COL_NAME),
            table.column_index(REL_COL_FROM_TABLE),
            table.column_index(REL_COL_FROM_COLUMN),
            table.column_index(REL_COL_TO_TABLE),
            table.column_index(REL_COL_TO_COLUMN),
        )
    };
    let need = |idx: Option<usize>, name: &str| -> Result<usize> {
        idx.ok_or_else(|| {
            Error::Jackcess(format!(
                "{} has no column named {}",
                TABLE_SYSTEM_RELATIONSHIPS, name
            ))
        })
    };
    let ccolumn = need(ccolumn, REL_COL_COLUMN_COUNT)?;
    let grbit_col = need(grbit_col, REL_COL_FLAGS)?;
    let icolumn = need(icolumn, REL_COL_COLUMN_INDEX)?;
    let name_col = need(name_col, REL_COL_NAME)?;
    let from_table_col = need(from_table_col, REL_COL_FROM_TABLE)?;
    let from_col_col = need(from_col_col, REL_COL_FROM_COLUMN)?;
    let to_table_col = need(to_table_col, REL_COL_TO_TABLE)?;
    let to_col_col = need(to_col_col, REL_COL_TO_COLUMN)?;

    let num_cols = primary_columns.len();
    let mut rows = Vec::with_capacity(num_cols);
    for i in 0..num_cols {
        let mut row = vec![Value::Null; column_count];
        row[ccolumn] = Value::Long(num_cols as i32);
        row[grbit_col] = Value::Long(flags);
        row[icolumn] = Value::Long(i as i32);
        row[name_col] = Value::Text(name.to_string());
        row[from_table_col] = Value::Text(primary_table_name.to_string());
        row[from_col_col] = Value::Text(primary_columns[i].clone());
        row[to_table_col] = Value::Text(secondary_table_name.to_string());
        row[to_col_col] = Value::Text(secondary_columns[i].clone());
        rows.push(row);
    }
    // `DatabaseImpl.writeRelationship` writes the rows with the *batch*
    // `Table.addRows(rows)`, so a failure is the `BatchUpdateException` of the
    // `MSysRelationships` table (and its original cause), not the bare failure
    let tz = db.tz;
    let table = db.tables.get_mut(&page).unwrap();
    table.add_rows_batch(&mut db.channel, &rows, &tz)
}

/// `DatabaseImpl.getRelationships`: reads the relationships back out of
/// `MSysRelationships`.
///
/// With a table given, the relationships where it is the primary (from) table
/// are collected first, then the ones where it is the secondary (to) table,
/// like jackcess' two cursor passes.  (jackcess orders each pass by the
/// `MSysRelationships` index on the table name; this port keeps the table row
/// order instead.)
pub(crate) fn read_relationships(
    db: &mut Database,
    table1: Option<&str>,
    table2: Option<&str>,
    include_system_tables: bool,
) -> Result<Vec<Relationship>> {
    let page = db
        .table_lookup_entry(TABLE_SYSTEM_RELATIONSHIPS)
        .map(|(p, _)| p)
        .ok_or_else(|| {
            Error::Jackcess(format!(
                "Could not find system table {}",
                TABLE_SYSTEM_RELATIONSHIPS
            ))
        })?;
    let rows = db.read_table_rows(page)?;
    // jackcess is given Table objects, so the tables named by the caller are
    // loaded (and their columns resolved) up front
    if let Some(name) = table1 {
        db.table_page(name)?;
    }
    if let Some(name) = table2 {
        db.table_page(name)?;
    }
    let indices = {
        let table = db.tables.get(&page).unwrap();
        (
            table.column_index(REL_COL_COLUMN_COUNT),
            table.column_index(REL_COL_FLAGS),
            table.column_index(REL_COL_COLUMN_INDEX),
            table.column_index(REL_COL_NAME),
            table.column_index(REL_COL_FROM_TABLE),
            table.column_index(REL_COL_FROM_COLUMN),
            table.column_index(REL_COL_TO_TABLE),
            table.column_index(REL_COL_TO_COLUMN),
        )
    };
    let (
        ccolumn,
        grbit_col,
        icolumn,
        name_col,
        from_table_col,
        from_col_col,
        to_table_col,
        to_col_col,
    ) = indices;

    let mut relationships: Vec<Relationship> = Vec::new();
    let mut orders: Vec<Vec<Option<(String, String)>>> = Vec::new();
    let indices = (
        ccolumn,
        grbit_col,
        icolumn,
        name_col,
        from_table_col,
        from_col_col,
        to_table_col,
        to_col_col,
    );

    if let Some(t1) = table1 {
        collect_relationships(
            db,
            &rows,
            indices,
            Some(t1),
            table2,
            include_system_tables,
            &mut relationships,
            &mut orders,
        )?;
        collect_relationships(
            db,
            &rows,
            indices,
            table2,
            Some(t1),
            include_system_tables,
            &mut relationships,
            &mut orders,
        )?;
    } else {
        collect_relationships(
            db,
            &rows,
            indices,
            None,
            None,
            include_system_tables,
            &mut relationships,
            &mut orders,
        )?;
    }

    // fill in the columns
    for (i, rel) in relationships.iter_mut().enumerate() {
        if let Some(order) = orders.get(i) {
            rel.primary_columns = order
                .iter()
                .map(|c| c.as_ref().map(|(from, _)| from.clone()).unwrap_or_default())
                .collect();
            rel.secondary_columns = order
                .iter()
                .map(|c| c.as_ref().map(|(_, to)| to.clone()).unwrap_or_default())
                .collect();
        }
    }
    Ok(relationships)
}

type RelColumnIndices = (
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
);

fn row_text(row: &[Value], idx: Option<usize>) -> Option<String> {
    idx.and_then(|i| row.get(i))
        .and_then(|v| v.as_text())
        .map(|s| s.to_string())
}

fn row_int(row: &[Value], idx: Option<usize>) -> Option<i32> {
    idx.and_then(|i| row.get(i))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
}

/// `DatabaseImpl.collectRelationships`
#[allow(clippy::too_many_arguments)]
fn collect_relationships(
    db: &mut Database,
    rows: &[Option<Vec<Value>>],
    indices: RelColumnIndices,
    from_table: Option<&str>,
    to_table: Option<&str>,
    include_system_tables: bool,
    relationships: &mut Vec<Relationship>,
    orders: &mut Vec<Vec<Option<(String, String)>>>,
) -> Result<()> {
    let (
        ccolumn,
        grbit_col,
        icolumn,
        name_col,
        from_table_col,
        from_col_col,
        to_table_col,
        to_col_col,
    ) = indices;
    for row in rows.iter().flatten() {
        let from_name = row_text(row, from_table_col);
        let to_name = row_text(row, to_table_col);
        let matches = |wanted: Option<&str>, actual: Option<&String>| match (wanted, actual) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(w), Some(a)) => w.eq_ignore_ascii_case(a),
        };
        if !matches(from_table, from_name.as_ref()) || !matches(to_table, to_name.as_ref()) {
            continue;
        }

        let rel_name = match row_text(row, name_col) {
            Some(name) => name,
            None => continue,
        };

        // find the existing relationship with this name, if any
        let existing = relationships
            .iter()
            .position(|r| r.name.eq_ignore_ascii_case(&rel_name));

        let from_name = from_name.unwrap_or_default();
        let to_name = to_name.unwrap_or_default();
        let from_col_name = row_text(row, from_col_col).unwrap_or_default();
        let to_col_name = row_text(row, to_col_col).unwrap_or_default();

        // resolve the tables named by the row (the caller's tables are used
        // as they are, matching jackcess)
        let rel_from_table = match from_table {
            Some(name) => name.to_string(),
            None => match resolve_table(db, &from_name, include_system_tables)? {
                Some(name) => name,
                // invalid table or ignoring system tables, just ignore
                None => continue,
            },
        };
        let rel_to_table = match to_table {
            Some(name) => name.to_string(),
            None => match resolve_table(db, &to_name, include_system_tables)? {
                Some(name) => name,
                None => continue,
            },
        };

        // a column which is not there is ignored as well
        let (from_has, to_has) = {
            let from_page = db.table_lookup_entry(&rel_from_table).map(|(p, _)| p);
            let to_page = db.table_lookup_entry(&rel_to_table).map(|(p, _)| p);
            let from_has = match from_page {
                Some(p) => db
                    .tables
                    .get(&p)
                    .map(|t| t.get_column(&from_col_name).is_some())
                    .unwrap_or(false),
                None => false,
            };
            let to_has = match to_page {
                Some(p) => db
                    .tables
                    .get(&p)
                    .map(|t| t.get_column(&to_col_name).is_some())
                    .unwrap_or(false),
                None => false,
            };
            (from_has, to_has)
        };
        if !from_has || !to_has {
            continue;
        }

        let idx = match existing {
            Some(idx) => idx,
            None => {
                let num_cols = row_int(row, ccolumn).unwrap_or(0);
                let flags = row_int(row, grbit_col).unwrap_or(0);
                if num_cols < 0 {
                    return Err(Error::Jackcess(format!(
                        "invalid column count {} in {}",
                        num_cols, TABLE_SYSTEM_RELATIONSHIPS
                    )));
                }
                relationships.push(Relationship {
                    name: rel_name,
                    primary_table: rel_from_table.clone(),
                    secondary_table: rel_to_table.clone(),
                    primary_columns: Vec::new(),
                    secondary_columns: Vec::new(),
                    flags,
                });
                orders.push(vec![None; num_cols as usize]);
                relationships.len() - 1
            }
        };

        let col_idx = row_int(row, icolumn).unwrap_or(0);
        if col_idx < 0 || col_idx as usize >= orders[idx].len() {
            return Err(Error::Jackcess(format!(
                "invalid column index {} in {}",
                col_idx, TABLE_SYSTEM_RELATIONSHIPS
            )));
        }
        orders[idx][col_idx as usize] = Some((from_col_name, to_col_name));
    }
    Ok(())
}

/// `DatabaseImpl.getTable(name, includeSystemTables)`: the table name as
/// stored in the catalog, if the table exists and may be used.
fn resolve_table(
    db: &mut Database,
    name: &str,
    include_system_tables: bool,
) -> Result<Option<String>> {
    let (page, flags) = match db.table_lookup_entry(name) {
        Some(entry) => entry,
        None => return Ok(None),
    };
    if !include_system_tables && (flags & SYSTEM_OBJECT_FLAGS) != 0 {
        return Ok(None);
    }
    db.table_page(name)?;
    // the catalog name is what the caller asked for (case insensitively); use
    // the real name of the loaded table
    Ok(db.tables.get(&page).map(|t| t.name.clone()))
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

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnSpec;
    use crate::datatype::DataType;
    use crate::format::FileFormat;
    use crate::tablecreator::TableSpec;

    fn create_test_db(name: &str) -> (std::path::PathBuf, Database) {
        let path = test_tmp_dir().join(format!("jackcess_rel_{}_{}.mdb", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        (path, db)
    }

    /// `P(ID autonumber pk, Name text)` and `C(ID autonumber pk, P_ID long,
    /// Note text)`, the tables of the `relate` scenario.
    fn make_tables(db: &mut Database) {
        let p = TableSpec::new(
            "P",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&p).expect("create P");

        let c = TableSpec::new(
            "C",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("P_ID", DataType::Long),
                ColumnSpec::new("Note", DataType::Text).with_length(50),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&c).expect("create C");
    }

    #[test]
    fn relationship_flags_and_name_follow_jackcess() {
        let (path, mut db) = create_test_db("flags");
        make_tables(&mut db);
        let rows: Vec<Vec<Value>> = (0..3)
            .map(|i| vec![Value::Null, Value::Text(format!("p{}", i))])
            .collect();
        db.add_rows("P", &rows).expect("add P rows");
        let rows: Vec<Vec<Value>> = (0..3)
            .map(|i| {
                vec![
                    Value::Null,
                    Value::Long(i + 1),
                    Value::Text(format!("note {}", i)),
                ]
            })
            .collect();
        db.add_rows("C", &rows).expect("add C rows");

        let rel = db
            .add_relationship(
                RelationshipSpec::new("P", "C")
                    .add_columns("ID", "P_ID")
                    .set_referential_integrity(),
            )
            .expect("add relationship");

        // the standard Access relationship name is the two table names
        assert_eq!(rel.name, "PC");
        assert_eq!(rel.primary_table, "P");
        assert_eq!(rel.secondary_table, "C");
        assert_eq!(rel.primary_columns, vec!["ID".to_string()]);
        assert_eq!(rel.secondary_columns, vec!["P_ID".to_string()]);
        // integrity enforced, not one to one, no cascades
        assert_eq!(rel.flags, 0);
        assert!(rel.has_referential_integrity());
        assert!(!rel.is_one_to_one());
        assert!(!rel.cascade_updates());
        assert!(!rel.cascade_deletes());

        // the supporting indexes: a unique `.rB` foreign key index on P which
        // reuses the primary key index data, and a `PC` foreign key index on C
        // with its own index data
        {
            let p_page = db.table_lookup_entry("P").unwrap().0;
            let p = db.tables.get(&p_page).unwrap();
            assert_eq!(p.logical_index_count, 2);
            assert_eq!(p.index_count, 1);
            let idx = p
                .indexes
                .iter()
                .find(|i| i.name == ".rB")
                .expect(".rB index");
            assert_eq!(idx.index_type, IndexImpl::FOREIGN_KEY_INDEX_TYPE);
            assert_eq!(idx.index_data_number, 0);
            let reference = idx.reference.expect("fk reference");
            assert_eq!(reference.table_type, IndexImpl::FK_PRIMARY_TABLE_TYPE);
            assert_eq!(reference.other_index_number, 1);
            assert_eq!(
                reference.other_table_page_number,
                db.table_lookup_entry("C").unwrap().0
            );
            assert!(!reference.cascade_updates);
            assert!(!reference.cascade_deletes);
            assert!(!reference.cascade_null);
        }
        {
            let c_page = db.table_lookup_entry("C").unwrap().0;
            let c = db.tables.get(&c_page).unwrap();
            assert_eq!(c.logical_index_count, 2);
            assert_eq!(c.index_count, 2);
            let idx = c.indexes.iter().find(|i| i.name == "PC").expect("PC index");
            assert_eq!(idx.index_type, IndexImpl::FOREIGN_KEY_INDEX_TYPE);
            assert_eq!(idx.index_data_number, 1);
            let reference = idx.reference.expect("fk reference");
            assert_eq!(reference.table_type, IndexImpl::FK_SECONDARY_TABLE_TYPE);
            assert_eq!(reference.other_index_number, 1);
            assert_eq!(
                reference.other_table_page_number,
                db.table_lookup_entry("P").unwrap().0
            );
        }

        // the relationship reads back
        let rels = db.relationships("P").expect("read relationships");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0], rel);
        let rels = db.relationships("C").expect("read relationships");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0], rel);
        let rels = db
            .relationships_between("C", "P")
            .expect("read relationships");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0], rel);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn relationship_cascade_flags_reach_the_fk_index() {
        let (path, mut db) = create_test_db("cascade");
        make_tables(&mut db);
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Text(format!("p{}", i))])
            .collect();
        db.add_rows("P", &rows).expect("add P rows");
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Long(i + 1), Value::Text("n".into())])
            .collect();
        db.add_rows("C", &rows).expect("add C rows");

        let rel = db
            .add_relationship(
                RelationshipSpec::new("P", "C")
                    .add_columns("ID", "P_ID")
                    .set_referential_integrity()
                    .set_cascade_updates()
                    .set_cascade_deletes(),
            )
            .expect("add relationship");
        assert_eq!(rel.flags, CASCADE_UPDATES_FLAG | CASCADE_DELETES_FLAG);

        for table_name in ["P", "C"] {
            let page = db.table_lookup_entry(table_name).unwrap().0;
            let table = db.tables.get(&page).unwrap();
            let idx = table
                .indexes
                .iter()
                .find(|i| i.is_foreign_key())
                .expect("foreign key index");
            let reference = idx.reference.expect("fk reference");
            assert!(reference.cascade_updates);
            assert!(reference.cascade_deletes);
            assert!(!reference.cascade_null);
        }

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn second_relationship_gets_a_unique_name_and_a_second_catalog_object() {
        let (path, mut db) = create_test_db("second");
        make_tables(&mut db);
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Text(format!("p{}", i))])
            .collect();
        db.add_rows("P", &rows).expect("add P rows");
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Long(i + 1), Value::Text("n".into())])
            .collect();
        db.add_rows("C", &rows).expect("add C rows");

        let rel1 = db
            .add_relationship(
                RelationshipSpec::new("P", "C")
                    .add_columns("ID", "P_ID")
                    .set_referential_integrity(),
            )
            .expect("first relationship");
        assert_eq!(rel1.name, "PC");
        // the second relationship cannot use the same name (it is also an
        // index name on C), so jackcess appends a counter
        let rel2 = db
            .add_relationship(
                RelationshipSpec::new("P", "C")
                    .add_columns("ID", "P_ID")
                    .set_referential_integrity(),
            )
            .expect("second relationship");
        assert_eq!(rel2.name, "PC1");

        // the catalog objects use synthetic ids which count down from the
        // smallest integer
        let names = db.catalog_object_names(TYPE_RELATIONSHIP).expect("names");
        assert_eq!(names, vec!["PC".to_string(), "PC1".to_string()]);
        let rels = db.relationships_all().expect("relationships");
        assert_eq!(rels.len(), 2);
        assert_eq!(rels[0].name, "PC");
        assert_eq!(rels[1].name, "PC1");

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    fn read_page(db: &mut Database, page: i32) -> crate::bytebuf::ByteBuf {
        let mut buf = db.channel.create_page_buffer();
        db.channel.read_page(&mut buf, page).expect("read page");
        buf
    }

    /// The raw bytes of the given row of a data page.
    fn raw_row(db: &mut Database, page_number: i32, row_num: usize) -> Vec<u8> {
        let page = read_page(db, page_number);
        let page_size = db.format.page_size;
        let start =
            (page.get_i16_at(14 + 2 * row_num).expect("row start") as u16 & 0x1FFF) as usize;
        let end = if row_num == 0 {
            page_size
        } else {
            (page
                .get_i16_at(14 + 2 * (row_num - 1))
                .expect("previous row start") as u16
                & 0x1FFF) as usize
        };
        page.get_bytes_at(start, end - start).expect("row bytes")
    }

    /// The offset of the first index data definition block of a table
    /// definition (`size_tdef_header` + the row count definitions + the column
    /// definitions and their names).
    fn index_data_start(db: &Database, tdef_page: i32) -> usize {
        let format = db.format.clone();
        let table = db.tables.get(&tdef_page).unwrap();
        let names_len: usize = table
            .columns
            .iter()
            .map(|c| crate::column::calculate_name_length(&c.name))
            .sum();
        format.offset_index_def_block
            + table.index_count as usize * format.size_index_definition
            + table.columns.len() * format.size_column_def_block
            + names_len
    }

    /// The offset of the first logical index definition block, followed by the
    /// index names.
    fn index_info_start(db: &Database, tdef_page: i32) -> usize {
        let format = db.format.clone();
        let table = db.tables.get(&tdef_page).unwrap();
        index_data_start(db, tdef_page)
            + table.index_count as usize * format.size_index_column_block
    }

    /// Reads a table definition name (a 2 byte length prefix then the UTF-16
    /// characters).
    fn read_definition_name(page: &crate::bytebuf::ByteBuf, offset: usize) -> String {
        let len = page.get_i16_at(offset).expect("name length") as usize;
        let units: Vec<u16> = (0..len / 2)
            .map(|i| page.get_u16_at(offset + 2 + 2 * i).expect("name char"))
            .collect();
        String::from_utf16(&units).expect("utf16 name")
    }

    /// The relationship row layout and the foreign key index definitions, down
    /// to the bytes of the table definition pages.
    #[test]
    fn relationship_row_and_fk_index_bytes_are_pinned() {
        let (path, mut db) = create_test_db("bytes");
        make_tables(&mut db);
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Text(format!("p{}", i))])
            .collect();
        db.add_rows("P", &rows).expect("add P rows");
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Long(i + 1), Value::Text("n".into())])
            .collect();
        db.add_rows("C", &rows).expect("add C rows");

        db.add_relationship(
            RelationshipSpec::new("P", "C")
                .add_columns("ID", "P_ID")
                .set_referential_integrity()
                .set_cascade_updates(),
        )
        .expect("add relationship");

        let format = db.format.clone();

        // ------------------------------------------------------------------
        // the MSysRelationships row: the payload, and the exact row bytes the
        // encoder produces for it
        // ------------------------------------------------------------------
        let rels_page = db.table_lookup_entry(TABLE_SYSTEM_RELATIONSHIPS).unwrap().0;
        let table = db.tables.get(&rels_page).unwrap();
        assert_eq!(table.columns.len(), 8);
        assert_eq!(table.row_count, 1);
        let column_index = |name: &str| table.column_index(name).expect("relationship column");
        let (ccolumn, grbit, icolumn, name_col, from_table, from_col, to_table, to_col) = (
            column_index(REL_COL_COLUMN_COUNT),
            column_index(REL_COL_FLAGS),
            column_index(REL_COL_COLUMN_INDEX),
            column_index(REL_COL_NAME),
            column_index(REL_COL_FROM_TABLE),
            column_index(REL_COL_FROM_COLUMN),
            column_index(REL_COL_TO_TABLE),
            column_index(REL_COL_TO_COLUMN),
        );

        let data_page = {
            let pages: Vec<i32> = table.owned_pages.as_ref().unwrap().iter_pages().collect();
            pages.into_iter().min().expect("data page")
        };
        let data_page_bytes = read_page(&mut db, data_page);
        let values = {
            let table = db.tables.get(&rels_page).unwrap();
            table
                .read_row(&mut db.channel, &data_page_bytes, 0)
                .expect("read row")
        };
        assert_eq!(values[ccolumn], Value::Long(1));
        assert_eq!(values[grbit], Value::Long(CASCADE_UPDATES_FLAG));
        assert_eq!(values[icolumn], Value::Long(0));
        assert_eq!(values[name_col], Value::Text("PC".into()));
        // szObject/szColumn hold the "to" (secondary) side, szReferencedObject
        // and szReferencedColumn the "from" (primary) side
        assert_eq!(values[from_table], Value::Text("P".into()));
        assert_eq!(values[from_col], Value::Text("ID".into()));
        assert_eq!(values[to_table], Value::Text("C".into()));
        assert_eq!(values[to_col], Value::Text("P_ID".into()));

        // re-encoding the row reproduces the stored bytes exactly
        let raw = raw_row(&mut db, data_page, 0);
        let mut buf = db.channel.create_page_buffer();
        buf.set_position(0).unwrap();
        buf.set_limit(format.page_size).unwrap();
        let tz = db.tz;
        {
            let table = db.tables.get_mut(&rels_page).unwrap();
            table
                .create_row(&mut db.channel, &values, &mut buf, 0, &tz)
                .expect("re-encode row");
        }
        assert_eq!(buf.remaining_bytes().to_vec(), raw);

        // ------------------------------------------------------------------
        // the supporting indexes of the primary table (P)
        // ------------------------------------------------------------------
        let p_page = db.table_lookup_entry("P").unwrap().0;
        let c_page = db.table_lookup_entry("C").unwrap().0;
        {
            let table = db.tables.get(&p_page).unwrap();
            assert_eq!(table.index_count, 1);
            assert_eq!(table.logical_index_count, 2);
        }
        let p_tdef = read_page(&mut db, p_page);
        assert_eq!(p_tdef.get_i32_at(format.offset_num_index_slots).unwrap(), 2);
        assert_eq!(p_tdef.get_i32_at(format.offset_num_indexes).unwrap(), 1);
        // the new index reuses the primary key index data, so the flags of
        // that index data are unchanged: unique + required + unknown
        assert_eq!(
            p_tdef
                .get_u8_at(index_data_start(&db, p_page) + 46)
                .unwrap(),
            0x89
        );
        let info = index_info_start(&db, p_page);
        let slot = info + format.size_index_info_block; // the second logical index
        assert_eq!(
            p_tdef.get_i32_at(slot).unwrap(),
            crate::table::MAGIC_TABLE_NUMBER
        );
        assert_eq!(p_tdef.get_i32_at(slot + 4).unwrap(), 1); // index number
        assert_eq!(p_tdef.get_i32_at(slot + 8).unwrap(), 0); // index data number
        assert_eq!(p_tdef.get_u8_at(slot + 12).unwrap(), 1); // FK_PRIMARY_TABLE_TYPE
        assert_eq!(p_tdef.get_i32_at(slot + 13).unwrap(), 1); // other index number
        assert_eq!(p_tdef.get_i32_at(slot + 17).unwrap(), c_page); // other table
        assert_eq!(p_tdef.get_u8_at(slot + 21).unwrap(), 1); // cascade updates
        assert_eq!(p_tdef.get_u8_at(slot + 22).unwrap(), 0); // cascade deletes
        assert_eq!(p_tdef.get_u8_at(slot + 23).unwrap(), 2); // FOREIGN_KEY_INDEX_TYPE
        assert_eq!(p_tdef.get_i32_at(slot + 24).unwrap(), 0);
        // the index names: "PrimaryKey" then the generated ".rB"
        let names = info + 2 * format.size_index_info_block;
        assert_eq!(read_definition_name(&p_tdef, names), "PrimaryKey");
        assert_eq!(
            read_definition_name(
                &p_tdef,
                names + crate::column::calculate_name_length("PrimaryKey")
            ),
            ".rB"
        );
        // the definition grew by exactly one index slot and its name
        let expected = format.size_tdef_header
            + format.size_index_definition
            + 2 * format.size_column_def_block
            + crate::column::calculate_name_length("ID")
            + crate::column::calculate_name_length("Name")
            + format.size_index_column_block
            + 2 * format.size_index_info_block
            + crate::column::calculate_name_length("PrimaryKey")
            + crate::column::calculate_name_length(".rB")
            + format.size_tdef_trailer;
        assert_eq!(p_tdef.get_i32_at(8).unwrap() as usize, expected);

        // ------------------------------------------------------------------
        // the supporting index of the secondary table (C)
        // ------------------------------------------------------------------
        {
            let table = db.tables.get(&c_page).unwrap();
            assert_eq!(table.index_count, 2);
            assert_eq!(table.logical_index_count, 2);
        }
        let c_tdef = read_page(&mut db, c_page);
        assert_eq!(c_tdef.get_i32_at(format.offset_num_indexes).unwrap(), 2);
        // the new index data is not unique
        assert_eq!(
            c_tdef
                .get_u8_at(index_data_start(&db, c_page) + format.size_index_column_block + 46)
                .unwrap(),
            0x80
        );
        let info = index_info_start(&db, c_page);
        let slot = info + format.size_index_info_block;
        assert_eq!(c_tdef.get_i32_at(slot + 4).unwrap(), 1); // index number
        assert_eq!(c_tdef.get_i32_at(slot + 8).unwrap(), 1); // its own index data
        assert_eq!(c_tdef.get_u8_at(slot + 12).unwrap(), 2); // FK_SECONDARY_TABLE_TYPE
        assert_eq!(c_tdef.get_i32_at(slot + 13).unwrap(), 1); // other index number
        assert_eq!(c_tdef.get_i32_at(slot + 17).unwrap(), p_page); // other table
        assert_eq!(c_tdef.get_u8_at(slot + 21).unwrap(), 1); // cascade updates
        assert_eq!(c_tdef.get_u8_at(slot + 23).unwrap(), 2); // FOREIGN_KEY_INDEX_TYPE
                                                             // the secondary index is named after the relationship
        let names = info + 2 * format.size_index_info_block;
        assert_eq!(
            read_definition_name(
                &c_tdef,
                names + crate::column::calculate_name_length("PrimaryKey")
            ),
            "PC"
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A relationship without referential integrity only writes the
    /// `MSysRelationships` rows and the catalog object: no indexes are added.
    #[test]
    fn relationship_without_integrity_adds_no_indexes() {
        let (path, mut db) = create_test_db("nointegrity");
        make_tables(&mut db);
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Text(format!("p{}", i))])
            .collect();
        db.add_rows("P", &rows).expect("add P rows");
        // no matching primary rows are needed without integrity
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Long(100 + i), Value::Text("n".into())])
            .collect();
        db.add_rows("C", &rows).expect("add C rows");
        let (p_page, _) = db.table_lookup_entry("P").unwrap();
        let (c_page, _) = db.table_lookup_entry("C").unwrap();

        let rel = db
            .add_relationship(RelationshipSpec::new("P", "C").add_columns("ID", "P_ID"))
            .expect("add relationship");
        assert_eq!(rel.flags, NO_REFERENTIAL_INTEGRITY_FLAG);
        assert!(!rel.has_referential_integrity());
        assert!(!rel.is_one_to_one());
        {
            let p = db.tables.get(&p_page).unwrap();
            let c = db.tables.get(&c_page).unwrap();
            assert_eq!(p.logical_index_count, 1);
            assert_eq!(c.logical_index_count, 1);
        }
        let rels = db.relationships_all().expect("relationships");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0], rel);

        // the join type is an independent flag (`RelationshipBuilder.setJoinType`)
        let joined = db
            .add_relationship(
                RelationshipSpec::new("P", "C")
                    .add_columns("ID", "P_ID")
                    .set_join_type(JoinType::LeftOuter),
            )
            .expect("add outer join relationship");
        assert_eq!(
            joined.flags,
            NO_REFERENTIAL_INTEGRITY_FLAG | LEFT_OUTER_JOIN_FLAG
        );
        assert_eq!(joined.join_type(), JoinType::LeftOuter);
        assert!(joined.is_left_outer_join());
        assert!(!joined.is_right_outer_join());

        // cascades require integrity, just like jackcess
        let err = db
            .add_relationship(
                RelationshipSpec::new("P", "C")
                    .add_columns("ID", "P_ID")
                    .set_cascade_deletes(),
            )
            .expect_err("cascade without integrity must fail");
        assert!(err.to_string().contains("Cascade flags"), "{}", err);
        // ... and integrity requires a unique index on the primary side
        let err = db
            .add_relationship(
                RelationshipSpec::new("C", "P")
                    .add_columns("P_ID", "ID")
                    .set_referential_integrity(),
            )
            .expect_err("missing unique index must fail");
        assert!(err.to_string().contains("Missing unique index"), "{}", err);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The relationships of a freshly re-opened database read back with their
    /// table and column names (the same as the ones jackcess writes).
    #[test]
    fn relationships_read_back_after_reopening() {
        let (path, mut db) = create_test_db("reopen");
        make_tables(&mut db);
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Text(format!("p{}", i))])
            .collect();
        db.add_rows("P", &rows).expect("add P rows");
        let rows: Vec<Vec<Value>> = (0..2)
            .map(|i| vec![Value::Null, Value::Long(i + 1), Value::Text("n".into())])
            .collect();
        db.add_rows("C", &rows).expect("add C rows");
        let created = db
            .add_relationship(
                RelationshipSpec::new("P", "C")
                    .add_columns("ID", "P_ID")
                    .set_referential_integrity()
                    .set_cascade_deletes(),
            )
            .expect("add relationship");
        db.flush().expect("flush");
        drop(db);

        let mut db = Database::open_path(&path).expect("reopen database");
        // asking for a table's relationships on a fresh database must load the
        // tables (and therefore resolve the columns)
        let rels = db.relationships("C").expect("read relationships");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0], created);
        assert!(rels[0].cascade_deletes());
        // ... and the foreign key index metadata survived the round trip
        let p_page = db.table_lookup_entry("P").unwrap().0;
        let p = db.get_table("P").expect("P");
        assert_eq!(p.name, "P");
        let index = db
            .tables
            .get(&p_page)
            .unwrap()
            .indexes
            .iter()
            .find(|i| i.is_foreign_key())
            .cloned()
            .expect("foreign key index");
        let reference = index.reference.expect("fk reference");
        assert!(reference.is_primary_table());
        assert!(reference.cascade_deletes);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A table related to itself: the two supporting indexes take index
    /// numbers one apart and reference each other (jackcess writes the
    /// secondary one first).
    #[test]
    fn self_relationship_indexes_reference_each_other() {
        let (path, mut db) = create_test_db("selfrel");
        let spec = TableSpec::new(
            "E",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Parent_ID", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(30),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).expect("create E");
        let rows: Vec<Vec<Value>> = (0..4)
            .map(|i| {
                vec![
                    Value::Null,
                    if i == 0 { Value::Null } else { Value::Long(i) },
                    Value::Text(format!("e{}", i)),
                ]
            })
            .collect();
        db.add_rows("E", &rows).expect("add E rows");

        let rel = db
            .add_relationship(
                RelationshipSpec::new("E", "E")
                    .add_columns("ID", "Parent_ID")
                    .set_referential_integrity(),
            )
            .expect("add self relationship");
        assert_eq!(rel.primary_table, "E");
        assert_eq!(rel.secondary_table, "E");

        let page = db.table_lookup_entry("E").unwrap().0;
        let table = db.tables.get(&page).unwrap();
        assert_eq!(table.logical_index_count, 3);
        let secondary = table
            .indexes
            .iter()
            .find(|i| i.index_number == 1)
            .expect("secondary index");
        assert_eq!(secondary.index_type, IndexImpl::FOREIGN_KEY_INDEX_TYPE);
        let primary = table
            .indexes
            .iter()
            .find(|i| i.index_number == 2)
            .expect("primary index");
        assert_eq!(primary.index_type, IndexImpl::FOREIGN_KEY_INDEX_TYPE);
        // each one points at the other
        let secondary_ref = secondary.reference.expect("secondary reference");
        let primary_ref = primary.reference.expect("primary reference");
        assert_eq!(secondary_ref.table_type, IndexImpl::FK_SECONDARY_TABLE_TYPE);
        assert_eq!(secondary_ref.other_index_number, 2);
        assert_eq!(primary_ref.table_type, IndexImpl::FK_PRIMARY_TABLE_TYPE);
        assert_eq!(primary_ref.other_index_number, 1);
        assert_eq!(secondary_ref.other_table_page_number, page);
        assert_eq!(primary_ref.other_table_page_number, page);

        // a self relationship is read back once, not once per cursor pass
        let rels = db.relationships("E").expect("read relationships");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0], rel);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `createPrimaryIndexName`: the auto-generated primary index name walks up
    /// the alphabet from `.rB`, skipping `[`.
    #[test]
    fn primary_index_name_skips_taken_names() {
        let names =
            |list: &[&str]| -> HashSet<String> { list.iter().map(|n| to_lookup_name(n)).collect() };
        assert_eq!(create_primary_index_name_from(&names(&[])), ".rB");
        assert_eq!(
            create_primary_index_name_from(&names(&["PrimaryKey", ".rB", ".rC", ".rD"])),
            ".rE"
        );
        // ".rZ" is followed by ".ra" (the '[' between them is skipped)
        assert_eq!(
            create_primary_index_name_from(&names(&[
                ".rB", ".rC", ".rD", ".rE", ".rF", ".rG", ".rH", ".rI", ".rJ", ".rK", ".rL", ".rM",
                ".rN", ".rO", ".rP", ".rQ", ".rR", ".rS", ".rT", ".rU", ".rV", ".rW", ".rX", ".rY",
                ".rZ",
            ])),
            ".ra"
        );
    }

    /// `Relationship.getFromTable` / `getToTable` / `getFromColumns` /
    /// `getToColumns`: Java keeps the resolved `Table`/`Column` objects, the
    /// port stores the names and resolves them through the database.
    #[test]
    fn java_relationship_object_accessors_resolve_tables_and_columns() {
        let (path, mut db) = create_test_db("objapi");
        make_tables(&mut db);
        db.add_rows("P", &[vec![Value::Null, Value::Text("p1".into())]])
            .expect("add P row");
        db.add_rows(
            "C",
            &[vec![
                Value::Null,
                Value::Long(1),
                Value::Text("note".into()),
            ]],
        )
        .expect("add C row");

        let rel = db
            .add_relationship(
                RelationshipSpec::new("P", "C")
                    .add_columns("ID", "P_ID")
                    .set_referential_integrity()
                    .set_cascade_updates()
                    .set_join_type(JoinType::LeftOuter),
            )
            .expect("add relationship");
        assert_eq!(rel.get_from_table(&mut db).unwrap().get_name(), "P");
        assert_eq!(rel.get_to_table(&mut db).unwrap().get_name(), "C");
        let from = rel.get_from_columns(&mut db).expect("from columns");
        assert_eq!(from.len(), 1);
        assert_eq!(from[0].name, "ID");
        assert_eq!(from[0].data_type, DataType::Long);
        let to = rel.get_to_columns(&mut db).expect("to columns");
        assert_eq!(to.len(), 1);
        assert_eq!(to[0].name, "P_ID");
        assert_eq!(rel.join_type(), JoinType::LeftOuter);

        // the relationship read back from the catalog resolves the same way
        let rels = db.relationships("P").expect("read relationships");
        let read_back = rels
            .iter()
            .find(|candidate| candidate.name == rel.name)
            .expect("the created relationship");
        assert_eq!(read_back.get_from_columns(&mut db).unwrap()[0].name, "ID");
        assert_eq!(read_back.get_to_columns(&mut db).unwrap()[0].name, "P_ID");
        // a table/column which does not exist is an error, not Java's null
        let missing = Relationship {
            name: "Missing".to_string(),
            primary_table: "Nope".to_string(),
            secondary_table: "C".to_string(),
            primary_columns: Vec::new(),
            secondary_columns: Vec::new(),
            flags: 0,
        };
        assert!(missing.get_from_table(&mut db).is_err());
        let bad_column = Relationship {
            primary_table: "P".to_string(),
            primary_columns: vec!["Nope".to_string()],
            ..missing.clone()
        };
        assert!(matches!(
            bad_column.get_from_columns(&mut db),
            Err(Error::IllegalArgument(message))
                if message == "Column Nope does not exist in table P"
        ));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
