//! Port of `com.healthmarketscience.jackcess.DatabaseBuilder` (and the static
//! builder factories `newDatabase`/`newTable`/`newColumn`/`newIndex`/
//! `newPrimaryKey`/`newRelationship`), plus the database/table iterable
//! builders.
//!
//! # Java name mapping
//!
//! The Java API is overloaded and uses `File`/`Path`/`Charset`/`Calendar`
//! types which have no direct Rust counterparts, so the Rust surface renames a
//! few entry points and keeps the rest identical:
//!
//! | Java | Rust |
//! |---|---|
//! | `new DatabaseBuilder()` | [`DatabaseBuilder::new`] |
//! | `new DatabaseBuilder(File)` / `new DatabaseBuilder(Path)` | [`DatabaseBuilder::new_at`] (`File` and `Path` are both `Path`) |
//! | `DatabaseBuilder.newDatabase()` | [`DatabaseBuilder::new_database`] |
//! | `DatabaseBuilder.newDatabase(File/Path)` | [`DatabaseBuilder::new_database_at`] |
//! | `DatabaseBuilder.open(File/Path)` (static) | [`DatabaseBuilder::open_path`] |
//! | `DatabaseBuilder.create(FileFormat, File)` (static) | [`DatabaseBuilder::create_file`] |
//! | `builder.open()` / `builder.create()` | [`DatabaseBuilder::open`] / [`DatabaseBuilder::create`] |
//! | `setFile(File)` / `setPath(Path)` | [`DatabaseBuilder::set_file`] / [`DatabaseBuilder::set_path`] |
//! | `setReadOnly` / `setAutoSync` | [`DatabaseBuilder::set_read_only`] / [`DatabaseBuilder::set_auto_sync`] |
//! | `setCharset` / `setTimeZone` | [`DatabaseBuilder::set_charset`] / [`DatabaseBuilder::set_time_zone`] |
//! | `setCodecProvider` | [`DatabaseBuilder::set_codec_provider`] |
//! | `setFileFormat` / `setChannel` | [`DatabaseBuilder::set_file_format`] / [`DatabaseBuilder::set_channel`] |
//! | `putDatabaseProperty` / `putSummaryProperty` / `putUserDefinedProperty` | [`DatabaseBuilder::put_database_property`] / [`DatabaseBuilder::put_summary_property`] / [`DatabaseBuilder::put_user_defined_property`] (and the `_typed` forms) |
//! | `setIgnoreBrokenSystemCatalogIndex` | [`DatabaseBuilder::set_ignore_broken_system_catalog_index`] |
//! | `createDateFormat` / `toCompatibleCalendar` | [`create_date_format`] / [`to_compatible_calendar`] |
//! | `newTable(String)` / `newTable(String, boolean)` | [`new_table`] / [`new_table_escaped`] |
//! | `newColumn(String)` / `newColumn(String, DataType)` | [`new_column_untyped`] / [`new_column`] |
//! | `newIndex(String)` / `newPrimaryKey(String...)` | [`new_index`] / [`new_primary_key`] |
//! | `newRelationship(String, String)` | [`new_relationship`] |
//! | `TableIterableBuilder` | [`TableIterableBuilder`] |
//! | `TableBuilder.escapeIdentifier` / `isReservedWord` | [`escape_identifier`] / [`is_reserved_word`] |
//!
//! The builder factories return the port's existing mutable definition types
//! ([`crate::tablecreator::TableSpec`], [`crate::column::ColumnSpec`],
//! [`crate::tablecreator::IndexSpec`], [`crate::relationship::RelationshipSpec`])
//! rather than new builder classes, because those are the values
//! [`crate::tablecreator::create_table`] and
//! [`Database::add_relationship`](crate::database::Database::add_relationship)
//! consume.  Java's `ColumnBuilder.setType` has no port method (the
//! [`ColumnSpec`] is typed from the start), and Java's `TableBuilder.addColumn`
//! is the `Vec` push of [`TableSpec::columns`].
//!
//! # Iteration
//!
//! Java's `Database` is `Iterable<Table>` and `TableIterableBuilder` is
//! `Iterable<Table>`.  A Rust iterator cannot yield `&mut Table` values which
//! come from the `&mut Database` it borrows (a lending iterator), so the port
//! splits the two halves:
//!
//! * [`TableIterableBuilder::table_names`] is the `getTableNames` half (the
//!   names in Java's iteration order);
//! * [`TableIterableBuilder::for_each_table`] is the `iterator`/`stream` half:
//!   it visits each selected table in Java's order through a callback;
//! * [`crate::database::Database::tables`] / `tables_with_system` hand out the
//!   local tables as `Vec<&mut Table>` in Java's order (linked tables cannot be
//!   in that vector, see their documentation);
//! * [`crate::database::Database::new_table_meta_data_iterable`] is the
//!   owning iterator over `TableMetaData`, which needs no database borrow.

use std::path::PathBuf;

use crate::column::ColumnSpec;
use crate::database::{Database, DatabaseProperties, OpenSettings};
use crate::datatype::DataType;
use crate::error::{Error, Result};
use crate::format::{Charset, FileFormat};
use crate::pagechannel::FileIo;
use crate::relationship::RelationshipSpec;
use crate::tablecreator::{IndexSpec, TableSpec};
use crate::value::Value;

/// `IndexBuilder.PRIMARY_KEY_NAME`: the name MS Access gives the primary key
/// index.
pub const PRIMARY_KEY_NAME: &str = "PrimaryKey";

/// `Database.DEFAULT_AUTO_SYNC`: jackcess flushes every write unless the caller
/// turns auto-sync off.
pub const DEFAULT_AUTO_SYNC: bool = crate::database::DEFAULT_AUTO_SYNC;

/// Port of `DatabaseBuilder`: a builder style class for opening or creating a
/// [`Database`].
///
/// ```no_run
/// use jackcess::builder::DatabaseBuilder;
/// use jackcess::format::FileFormat;
///
/// # fn main() -> jackcess::Result<()> {
/// // DatabaseBuilder.open(new File("test.mdb"))
/// let db = DatabaseBuilder::open_path("test.mdb")?;
///
/// // new DatabaseBuilder(new File("new.mdb"))
/// //   .setFileFormat(FileFormat::V2003)
/// //   .create()
/// let mut db = DatabaseBuilder::new_at("new.mdb")
///     .set_file_format(FileFormat::V2003)
///     .create()?;
/// db.close()?;
/// # Ok(())
/// # }
/// ```
///
/// The Rust builder is the same value-with-chained-setters as the Java one:
/// every `set*`/`put*` consumes and returns `Self`, and [`Self::open`] /
/// [`Self::create`] finish the chain.  (The builder is not `Clone`: a
/// configured channel and codec provider are owned trait objects.)
pub struct DatabaseBuilder {
    /// `DatabaseBuilder._mdbFile`: the file to open or create
    path: Option<PathBuf>,
    /// `DatabaseBuilder._readOnly`
    read_only: bool,
    /// `DatabaseBuilder._autoSync`
    auto_sync: bool,
    /// `DatabaseBuilder._charset`
    charset: Option<Charset>,
    /// `DatabaseBuilder._timeZone`
    time_zone: Option<crate::datenums::TimeZone>,
    /// `DatabaseBuilder._codecProvider`
    codec_provider: Option<Box<dyn crate::crypt::CodecProvider>>,
    /// `DatabaseBuilder._fileFormat`
    file_format: Option<FileFormat>,
    /// `DatabaseBuilder._channel`
    channel: Option<Box<dyn FileIo>>,
    /// `DatabaseBuilder._dbProps` / `_summaryProps` / `_userProps`
    properties: DatabaseProperties,
    /// `DatabaseBuilder._ignoreBrokenSystemCatalogIndex`
    ignore_broken_system_catalog_index: bool,
}

impl std::fmt::Debug for DatabaseBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseBuilder")
            .field("path", &self.path)
            .field("read_only", &self.read_only)
            .field("auto_sync", &self.auto_sync)
            .field("charset", &self.charset)
            .field("time_zone", &self.time_zone)
            .field("file_format", &self.file_format)
            .field("has_codec_provider", &self.codec_provider.is_some())
            .field("has_channel", &self.channel.is_some())
            .field("properties", &self.properties)
            .field(
                "ignore_broken_system_catalog_index",
                &self.ignore_broken_system_catalog_index,
            )
            .finish()
    }
}

impl Default for DatabaseBuilder {
    fn default() -> Self {
        DatabaseBuilder::new()
    }
}

impl DatabaseBuilder {
    /// `new DatabaseBuilder()`: a builder with no file and every Java default
    /// (`setReadOnly(false)`, auto-sync on, no charset/time zone/codec/format
    /// override, no properties).
    pub fn new() -> DatabaseBuilder {
        DatabaseBuilder {
            path: None,
            read_only: false,
            auto_sync: DEFAULT_AUTO_SYNC,
            charset: None,
            time_zone: None,
            codec_provider: None,
            file_format: None,
            channel: None,
            properties: DatabaseProperties::new(),
            ignore_broken_system_catalog_index: false,
        }
    }

    /// `new DatabaseBuilder(Path)` / `new DatabaseBuilder(File)`: a builder for
    /// the given file.
    pub fn new_at(path: impl Into<PathBuf>) -> DatabaseBuilder {
        DatabaseBuilder {
            path: Some(path.into()),
            ..DatabaseBuilder::new()
        }
    }

    /// `DatabaseBuilder.newDatabase()`: the same as [`DatabaseBuilder::new`]
    /// (the Java convenience factory).
    pub fn new_database() -> DatabaseBuilder {
        DatabaseBuilder::new()
    }

    /// `DatabaseBuilder.newDatabase(Path)` / `newDatabase(File)`.
    pub fn new_database_at(path: impl Into<PathBuf>) -> DatabaseBuilder {
        DatabaseBuilder::new_at(path)
    }

    /// `DatabaseBuilder.open(File)` / `DatabaseBuilder.open(Path)` (the static
    /// entry points): opens the file with the Java defaults.
    pub fn open_path(path: impl Into<PathBuf>) -> Result<Database> {
        DatabaseBuilder::new_at(path).open()
    }

    /// `DatabaseBuilder.create(FileFormat, File)` (the static entry point).
    pub fn create_file(file_format: FileFormat, path: impl Into<PathBuf>) -> Result<Database> {
        DatabaseBuilder::new_at(path)
            .set_file_format(file_format)
            .create()
    }

    /// `DatabaseBuilder.setPath(Path)`.
    pub fn set_path(mut self, path: impl Into<PathBuf>) -> DatabaseBuilder {
        self.path = Some(path.into());
        self
    }

    /// `DatabaseBuilder.setFile(File)`: identical to [`Self::set_path`],
    /// because the port represents `java.io.File` with a [`std::path::Path`].
    pub fn set_file(self, path: impl Into<PathBuf>) -> DatabaseBuilder {
        self.set_path(path)
    }

    /// `DatabaseBuilder.setReadOnly(boolean)` (`open` only, like Java).
    pub fn set_read_only(mut self, read_only: bool) -> DatabaseBuilder {
        self.read_only = read_only;
        self
    }

    /// `DatabaseBuilder.setAutoSync(boolean)`: whether every write is flushed
    /// to the file system when it completes.
    pub fn set_auto_sync(mut self, auto_sync: bool) -> DatabaseBuilder {
        self.auto_sync = auto_sync;
        self
    }

    /// `DatabaseBuilder.setCharset(Charset)`: the charset used for text when
    /// the database format/header does not name one.  Java's `null` (use the
    /// default) is [`Self::clear_charset`].
    pub fn set_charset(mut self, charset: Charset) -> DatabaseBuilder {
        self.charset = Some(charset);
        self
    }

    /// `DatabaseBuilder.setCharset(null)`: use the format's default charset.
    pub fn clear_charset(mut self) -> DatabaseBuilder {
        self.charset = None;
        self
    }

    /// `DatabaseBuilder.setTimeZone(TimeZone)`: the zone used to interpret
    /// dates, either a fixed offset ([`crate::datenums::TimeZone::from_offset_minutes`])
    /// or a named IANA zone ([`crate::datenums::TimeZone::from_zone_id`]).
    pub fn set_time_zone(mut self, time_zone: crate::datenums::TimeZone) -> DatabaseBuilder {
        self.time_zone = Some(time_zone);
        self
    }

    /// `DatabaseBuilder.setTimeZone(TimeZone.getTimeZone(id))`: a fixed offset
    /// zone from its offset east of UTC.
    pub fn set_time_zone_offset_minutes(mut self, minutes: i32) -> DatabaseBuilder {
        self.time_zone = Some(crate::datenums::TimeZone::from_offset_minutes(minutes));
        self
    }

    /// `DatabaseBuilder.setTimeZone(TimeZone.getTimeZone(id))` for an IANA id
    /// (`"America/New_York"`) or one of the fixed offset forms Java's
    /// `ZoneId.of` accepts (`"UTC"`, `"GMT+05:30"`, `"+05:30"`).
    ///
    /// An id neither Java nor the port knows is an error
    /// (`java.time.DateTimeException`/`ZoneRulesException` in Java); the
    /// builder is returned unchanged on failure.
    pub fn set_time_zone_id(self, zone_id: &str) -> Result<DatabaseBuilder> {
        let time_zone = crate::datenums::TimeZone::from_zone_id(zone_id)
            .ok_or_else(|| Error::IllegalArgument(format!("Unknown time-zone ID: {}", zone_id)))?;
        Ok(self.set_time_zone(time_zone))
    }

    /// `DatabaseBuilder.setTimeZone(null)`: use the default zone.
    pub fn clear_time_zone(mut self) -> DatabaseBuilder {
        self.time_zone = None;
        self
    }

    /// `DatabaseBuilder.setCodecProvider(CodecProvider)`: the provider which
    /// handles an encoded database ([`crate::crypt::CryptCodecProvider`] for
    /// jet encoding, Office encryption or a MSISAM password).
    pub fn set_codec_provider<P>(mut self, provider: P) -> DatabaseBuilder
    where
        P: crate::crypt::CodecProvider + 'static,
    {
        self.codec_provider = Some(Box::new(provider));
        self
    }

    /// `DatabaseBuilder.setCodecProvider(null)`: no special encoding support
    /// (the Java `DefaultCodecProvider`).
    pub fn clear_codec_provider(mut self) -> DatabaseBuilder {
        self.codec_provider = None;
        self
    }

    /// `DatabaseBuilder.setFileFormat(FileFormat)` (create only).
    pub fn set_file_format(mut self, file_format: FileFormat) -> DatabaseBuilder {
        self.file_format = Some(file_format);
        self
    }

    /// `DatabaseBuilder.setChannel(FileChannel)`: use a pre-opened channel
    /// instead of a file.
    ///
    /// Like Java, the channel is not closed by [`Database::close`] (the caller
    /// keeps its handle), and [`Database::get_path`] still reports the
    /// configured path (or `None` when only a channel is configured).  Use
    /// [`crate::pagechannel::FileChannel`] or
    /// [`crate::pagechannel::MemChannel`].
    pub fn set_channel(mut self, channel: Box<dyn FileIo>) -> DatabaseBuilder {
        self.channel = Some(channel);
        self
    }

    /// `DatabaseBuilder.putDatabaseProperty(name, value)`: sets a core
    /// database property, inferring its type like
    /// [`crate::property::create_property`].
    pub fn put_database_property(
        mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<DatabaseBuilder> {
        let props = std::mem::take(&mut self.properties);
        self.properties = props.put_database_property(name, value)?;
        Ok(self)
    }

    /// `DatabaseBuilder.putDatabaseProperty(name, type, value)`.
    pub fn put_database_property_typed(
        mut self,
        name: impl Into<String>,
        data_type: DataType,
        value: Value,
    ) -> DatabaseBuilder {
        let props = std::mem::take(&mut self.properties);
        self.properties = props.put_database_property_typed(name, data_type, value);
        self
    }

    /// `DatabaseBuilder.putSummaryProperty(name, value)`.
    pub fn put_summary_property(
        mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<DatabaseBuilder> {
        let props = std::mem::take(&mut self.properties);
        self.properties = props.put_summary_property(name, value)?;
        Ok(self)
    }

    /// `DatabaseBuilder.putSummaryProperty(name, type, value)`.
    pub fn put_summary_property_typed(
        mut self,
        name: impl Into<String>,
        data_type: DataType,
        value: Value,
    ) -> DatabaseBuilder {
        let props = std::mem::take(&mut self.properties);
        self.properties = props.put_summary_property_typed(name, data_type, value);
        self
    }

    /// `DatabaseBuilder.putUserDefinedProperty(name, value)`.
    pub fn put_user_defined_property(
        mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<DatabaseBuilder> {
        let props = std::mem::take(&mut self.properties);
        self.properties = props.put_user_defined_property(name, value)?;
        Ok(self)
    }

    /// `DatabaseBuilder.putUserDefinedProperty(name, type, value)`.
    pub fn put_user_defined_property_typed(
        mut self,
        name: impl Into<String>,
        data_type: DataType,
        value: Value,
    ) -> DatabaseBuilder {
        let props = std::mem::take(&mut self.properties);
        self.properties = props.put_user_defined_property_typed(name, data_type, value);
        self
    }

    /// `DatabaseBuilder.setIgnoreBrokenSystemCatalogIndex(boolean)`.
    pub fn set_ignore_broken_system_catalog_index(mut self, ignore: bool) -> DatabaseBuilder {
        self.ignore_broken_system_catalog_index = ignore;
        self
    }

    /// The properties [`Self::create`] applies, in the group order of
    /// `DatabaseBuilder.create` (database, summary, user defined).
    pub fn properties(&self) -> &DatabaseProperties {
        &self.properties
    }

    /// The [`OpenSettings`] this builder configures (Java's
    /// `DatabaseImpl.open`/`create` argument list).
    pub fn open_settings(&self) -> OpenSettings<'_> {
        OpenSettings {
            read_only: self.read_only,
            auto_sync: Some(self.auto_sync),
            charset: self.charset,
            time_zone: self.time_zone,
            codec_provider: self.codec_provider.as_deref(),
            file_format: self.file_format,
            ignore_broken_system_catalog_index: self.ignore_broken_system_catalog_index,
            // `DatabaseBuilder.setChannel` documents that the caller keeps the
            // channel: Java does not close it
            close_channel: self.channel.is_none(),
        }
    }

    /// `DatabaseBuilder.open()`: opens the configured database.
    ///
    /// This is `DatabaseImpl.open(path, readOnly, channel, autoSync, charset,
    /// timeZone, codecProvider, ignoreBrokenSystemCatalogIndex)`: the charset
    /// and time zone are installed before the system catalog is read, the
    /// read-only flag (and the formats jackcess cannot write) select the
    /// read-only mode, and the configured properties are *not* applied (Java
    /// only applies them in [`Self::create`]).
    pub fn open(self) -> Result<Database> {
        let DatabaseBuilder {
            path,
            channel,
            codec_provider,
            properties: _,
            read_only,
            auto_sync,
            charset,
            time_zone,
            file_format,
            ignore_broken_system_catalog_index,
        } = self;
        let settings = OpenSettings {
            read_only,
            auto_sync: Some(auto_sync),
            charset,
            time_zone,
            codec_provider: codec_provider.as_deref(),
            file_format,
            ignore_broken_system_catalog_index,
            close_channel: channel.is_none(),
        };
        match channel {
            Some(channel) => Database::open_with_channel_settings(channel, &settings),
            None => {
                let path = path.as_deref().ok_or_else(|| {
                    Error::IllegalArgument(
                        "no file configured for DatabaseBuilder::open".to_string(),
                    )
                })?;
                Database::open_path_with_settings(path, &settings)
            }
        }
    }

    /// `DatabaseBuilder.create()`: creates the configured database, then puts
    /// the configured database, summary and user defined properties and saves
    /// each group (in that order, like Java).
    ///
    /// The file format is required (Java's `DatabaseImpl.create` fails on a
    /// `null` format); the read-only flag is ignored, like Java.
    pub fn create(self) -> Result<Database> {
        let DatabaseBuilder {
            path,
            channel,
            codec_provider,
            properties,
            read_only: _,
            auto_sync,
            charset,
            time_zone,
            file_format,
            ignore_broken_system_catalog_index,
        } = self;
        let file_format = file_format.ok_or_else(|| {
            Error::IllegalArgument(
                "no file format configured for DatabaseBuilder::create".to_string(),
            )
        })?;
        let settings = OpenSettings {
            read_only: false,
            auto_sync: Some(auto_sync),
            charset,
            time_zone,
            codec_provider: codec_provider.as_deref(),
            file_format: Some(file_format),
            ignore_broken_system_catalog_index,
            close_channel: channel.is_none(),
        };
        let mut db = match channel {
            Some(channel) => {
                Database::create_with_channel_settings(file_format, channel, &settings)?
            }
            None => {
                let path = path.as_deref().ok_or_else(|| {
                    Error::IllegalArgument(
                        "no file configured for DatabaseBuilder::create".to_string(),
                    )
                })?;
                Database::create_with_settings(file_format, path, &settings)?
            }
        };
        db.apply_create_properties(&properties)?;
        Ok(db)
    }
}

// ---------------------------------------------------------------------------
// static builder factories
// ---------------------------------------------------------------------------

/// `DatabaseBuilder.newTable(String)`: a [`TableSpec`] for the named table.
pub fn new_table(name: impl Into<String>) -> TableSpec {
    TableSpec::new(name, Vec::new())
}

/// `DatabaseBuilder.newTable(String, boolean escapeIdentifiers)`: like
/// [`new_table`], with the table name escaped when it is an Access reserved
/// word.
///
/// Java's `TableBuilder` also escapes every column and index name added after
/// the flag is set; [`TableSpec`] has no add hook, so call [`escape_identifier`]
/// for those names (or push them before handing the spec to the creator).
pub fn new_table_escaped(name: impl Into<String>, escape_identifiers: bool) -> TableSpec {
    let name = name.into();
    let name = if escape_identifiers {
        escape_identifier(&name)
    } else {
        name
    };
    TableSpec::new(name, Vec::new())
}

/// `DatabaseBuilder.newColumn(String, DataType)`: a [`ColumnSpec`] for the
/// named, typed column.
pub fn new_column(name: impl Into<String>, data_type: DataType) -> ColumnSpec {
    ColumnSpec::new(name, data_type)
}

/// `DatabaseBuilder.newColumn(String)`: the Java form which leaves the column
/// type unset until `ColumnBuilder.setType` is called.
///
/// The port's [`ColumnSpec`] is typed from the start (the table creator needs
/// it), so this returns the column with the Java default type of a plain
/// `TEXT` column; assign [`ColumnSpec::data_type`] (a public field) to the
/// wanted type before creating the table.
pub fn new_column_untyped(name: impl Into<String>) -> ColumnSpec {
    ColumnSpec::new(name, DataType::Text)
}

/// `DatabaseBuilder.newIndex(String)`: an [`IndexSpec`] with no columns; push
/// `(column name, ascending)` pairs onto [`IndexSpec::columns`], or use
/// [`crate::tablecreator::IndexSpec`]'s builder methods for the flags.
pub fn new_index(name: impl Into<String>) -> IndexSpec {
    IndexSpec::new(name, Vec::new())
}

/// `DatabaseBuilder.newPrimaryKey(String...)`: a primary key [`IndexSpec`]
/// named [`PRIMARY_KEY_NAME`] over the given columns (ascending).
pub fn new_primary_key<S: AsRef<str>>(column_names: &[S]) -> IndexSpec {
    IndexSpec::primary_key(
        PRIMARY_KEY_NAME,
        column_names
            .iter()
            .map(|name| (name.as_ref().to_string(), true))
            .collect(),
    )
}

/// `DatabaseBuilder.newRelationship(String fromTable, String toTable)`: a
/// [`RelationshipSpec`] for the two tables.
///
/// Java's `fromTable` is the table with the unique key (the port's
/// [`RelationshipSpec::primary_table`]) and `toTable` the table with the
/// foreign key ([`RelationshipSpec::secondary_table`]).
pub fn new_relationship(
    from_table: impl Into<String>,
    to_table: impl Into<String>,
) -> RelationshipSpec {
    RelationshipSpec::new(from_table, to_table)
}

/// `DatabaseBuilder.newRelationship(Table fromTable, Table toTable)`: the
/// overload which takes the tables themselves.
pub fn new_relationship_tables(
    from_table: &crate::table::Table,
    to_table: &crate::table::Table,
) -> RelationshipSpec {
    RelationshipSpec::new(from_table.name.clone(), to_table.name.clone())
}

// ---------------------------------------------------------------------------
// date format compatibility
// ---------------------------------------------------------------------------

/// `DatabaseBuilder.createDateFormat(String)`: the Rust equivalent of Java's
/// `SimpleDateFormat` configured with a proleptic Gregorian calendar.
///
/// Access uses proleptic Gregorian dates for every date, while a Java
/// `SimpleDateFormat` defaults to the Julian calendar before 1582-10-15;
/// `toCompatibleCalendar` removes that switch.  `chrono`'s date arithmetic is
/// proleptic Gregorian for every date, so [`SimpleDateFormat`] needs no switch
/// and formats/parses with `pattern` as is.
///
/// The pattern uses the Java `SimpleDateFormat` letters (the common subset:
/// `y`, `M`, `d`, `D`, `H`, `h`, `m`, `s`, `S`, `E`, `a`, `z`, `Z`, `X`, `u`,
/// `w`; quoted literals and `''` are handled).  They are translated to the
/// equivalent `chrono` specifiers internally, so `yyyy-MM-dd HH:mm:ss.SSS`
/// means what it means in Java.
pub fn create_date_format(pattern: &str) -> SimpleDateFormat {
    SimpleDateFormat::new(pattern)
}

/// The Rust equivalent of Java's `DatabaseBuilder.toCompatibleCalendar(Calendar)`.
///
/// The Java method sets the Gregorian change of a `GregorianCalendar` to
/// `Long.MIN_VALUE`, i.e. it makes the calendar use proleptic Gregorian dates
/// (Access' behaviour).  The port has no `java.util.Calendar`: `chrono`'s
/// calendar is proleptic Gregorian for every date, so there is nothing to
/// convert.  Kept for API parity; returns the given value unchanged.
pub fn to_compatible_calendar<T>(calendar: T) -> T {
    calendar
}

/// `java.text.SimpleDateFormat` with the Access compatible calendar of
/// [`create_date_format`].
///
/// The value carries the Java `SimpleDateFormat` pattern; [`SimpleDateFormat::format`]
/// and [`SimpleDateFormat::parse`] translate it to the equivalent `chrono`
/// specifiers and use `chrono`'s proleptic Gregorian calendar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimpleDateFormat {
    pattern: String,
}

impl SimpleDateFormat {
    /// `new SimpleDateFormat(pattern)` followed by
    /// [`to_compatible_calendar`].
    pub fn new(pattern: impl Into<String>) -> SimpleDateFormat {
        SimpleDateFormat {
            pattern: pattern.into(),
        }
    }

    /// `SimpleDateFormat.toPattern()`.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// `SimpleDateFormat.applyPattern(String)`.
    pub fn set_pattern(&mut self, pattern: impl Into<String>) {
        self.pattern = pattern.into();
    }

    /// The `chrono` pattern `pattern()` translates to.
    pub fn chrono_pattern(&self) -> String {
        java_pattern_to_chrono(&self.pattern)
    }

    /// `SimpleDateFormat.format(Date)` for the port's local date/time value.
    pub fn format(&self, value: crate::datenums::LocalDateTime) -> String {
        let pattern = self.chrono_pattern();
        match naive_from_local(value) {
            Some(naive) => naive.format(&pattern).to_string(),
            // a year chrono cannot represent; Java formats it anyway, so fall
            // back to a debug rendering rather than panicking
            None => format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                value.year, value.month, value.day, value.hour, value.minute, value.second
            ),
        }
    }

    /// `SimpleDateFormat.parse(String)` for the port's local date/time value.
    ///
    /// Java's `SimpleDateFormat` fills the fields the pattern does not name
    /// with the calendar defaults (1970-01-01 00:00:00), so a date-only pattern
    /// parses to midnight and a time-only pattern to 1970-01-01; the port
    /// reproduces that by falling back to a date or time parse.  A pattern
    /// which matches nothing reports a `ParseException`, like Java.
    pub fn parse(&self, text: &str) -> Result<crate::datenums::LocalDateTime> {
        let pattern = self.chrono_pattern();
        if let Ok(value) = chrono::NaiveDateTime::parse_from_str(text, &pattern) {
            return Ok(local_from_naive(value));
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(text, &pattern) {
            if let Some(midnight) = date.and_hms_opt(0, 0, 0) {
                return Ok(local_from_naive(midnight));
            }
        }
        if let Ok(time) = chrono::NaiveTime::parse_from_str(text, &pattern) {
            if let Some(date) = chrono::NaiveDate::from_ymd_opt(1970, 1, 1) {
                return Ok(local_from_naive(date.and_time(time)));
            }
        }
        Err(Error::Jackcess(
            "ParseException: unparseable date".to_string(),
        ))
    }
}

/// Translates a Java `SimpleDateFormat` pattern to the equivalent `chrono`
/// (strftime style) pattern.
///
/// The letters which do not name a field in Java are literals (Java's
/// `SimpleDateFormat` resolves exactly the letters `GuyMDdFEaHkKhmsSzZXwWD`);
/// quoted text and `''` keep their Java meaning.  The translated specifiers are
/// the ones with the same rendering:
///
/// | Java | chrono |
/// |---|---|
/// | `y`, `yy`, `yyyy` | `%Y`, `%y`, `%Y` |
/// | `M`, `MM`, `MMM`, `MMMM` | `%m`, `%m`, `%b`, `%B` |
/// | `d` | `%d` |
/// | `D` | `%j` |
/// | `H` / `h` | `%H` / `%I` |
/// | `m` / `s` | `%M` / `%S` |
/// | `S`, `SS`, `SSS` | `%1f`, `%2f`, `%3f` (fractional seconds) |
/// | `E`, `EEEE` | `%a`, `%A` |
/// | `a` | `%p` |
/// | `z`, `Z`, `X` | `%z` |
/// | `u` / `w` | `%u` / `%V` |
pub fn java_pattern_to_chrono(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut literal = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // Java: '' is a literal quote, '...' quotes literal text
            if i + 1 < chars.len() && chars[i + 1] == '\'' {
                out.push('\'');
                i += 2;
                continue;
            }
            literal = !literal;
            i += 1;
            continue;
        }
        if literal || !is_java_pattern_letter(c) {
            push_chrono_literal(&mut out, c);
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < chars.len() && chars[end] == c {
            end += 1;
        }
        let count = end - i;
        match c {
            'y' if count == 2 => out.push_str("%y"),
            'y' => out.push_str("%Y"),
            'M' if count == 1 => out.push_str("%-m"),
            'M' if count == 2 => out.push_str("%m"),
            'M' if count == 3 => out.push_str("%b"),
            'M' => out.push_str("%B"),
            'd' if count == 1 => out.push_str("%-d"),
            'd' => out.push_str("%d"),
            'D' => out.push_str("%j"),
            'H' if count == 1 => out.push_str("%-H"),
            'H' => out.push_str("%H"),
            'h' if count == 1 => out.push_str("%-I"),
            'h' => out.push_str("%I"),
            'k' if count == 1 => out.push_str("%-H"),
            'k' => out.push_str("%H"),
            'K' if count == 1 => out.push_str("%-I"),
            'K' => out.push_str("%I"),
            'm' if count == 1 => out.push_str("%-M"),
            'm' => out.push_str("%M"),
            's' if count == 1 => out.push_str("%-S"),
            's' => out.push_str("%S"),
            'S' => out.push_str(&format!("%{}f", count.min(9))),
            'E' if count >= 4 => out.push_str("%A"),
            'E' => out.push_str("%a"),
            'a' => out.push_str("%p"),
            'z' | 'Z' | 'X' => out.push_str("%z"),
            'u' => out.push_str("%u"),
            'w' => out.push_str("%V"),
            // the remaining Java letters (era, week of month, day of week in
            // month) have no chrono equivalent: render them literally
            _ => {
                for _ in 0..count {
                    push_chrono_literal(&mut out, c);
                }
            }
        }
        i = end;
    }
    out
}

/// `true` for the letters Java's `SimpleDateFormat` treats as pattern letters.
fn is_java_pattern_letter(c: char) -> bool {
    matches!(
        c,
        'G' | 'u'
            | 'y'
            | 'M'
            | 'w'
            | 'W'
            | 'D'
            | 'd'
            | 'F'
            | 'E'
            | 'a'
            | 'H'
            | 'k'
            | 'K'
            | 'h'
            | 'm'
            | 's'
            | 'S'
            | 'z'
            | 'Z'
            | 'X'
    )
}

/// Appends a literal character to a chrono pattern (a literal `%` must be
/// escaped).
fn push_chrono_literal(out: &mut String, c: char) {
    if c == '%' {
        out.push_str("%%");
    } else {
        out.push(c);
    }
}

fn naive_from_local(value: crate::datenums::LocalDateTime) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDate::from_ymd_opt(value.year, value.month, value.day)
        .and_then(|date| date.and_hms_nano_opt(value.hour, value.minute, value.second, value.nano))
}

fn local_from_naive(naive: chrono::NaiveDateTime) -> crate::datenums::LocalDateTime {
    use chrono::{Datelike, Timelike};
    crate::datenums::LocalDateTime::new(
        naive.year(),
        naive.month(),
        naive.day(),
        naive.hour(),
        naive.minute(),
        naive.second(),
        naive.nanosecond(),
    )
}

// ---------------------------------------------------------------------------
// identifiers
// ---------------------------------------------------------------------------

/// `TableBuilder.escapeIdentifier(String)`: prefixes the name with `"x"` when
/// it is an Access reserved word.
pub fn escape_identifier(name: &str) -> String {
    if is_reserved_word(name) {
        format!("x{}", name)
    } else {
        name.to_string()
    }
}

/// `TableBuilder.isReservedWord(String)`: whether the name is one of Access'
/// reserved words (matched case insensitively, like Java's
/// `StringUtil.toLowerCase`).
pub fn is_reserved_word(name: &str) -> bool {
    let lower = name.to_lowercase();
    RESERVED_WORDS.contains(&lower.as_str())
}

/// `TableBuilder.ReservedWords.VALUES`: the Access reserved words.
const RESERVED_WORDS: &[&str] = &[
    "add",
    "all",
    "alphanumeric",
    "alter",
    "and",
    "any",
    "application",
    "as",
    "asc",
    "assistant",
    "autoincrement",
    "avg",
    "between",
    "binary",
    "bit",
    "boolean",
    "by",
    "byte",
    "char",
    "character",
    "column",
    "compactdatabase",
    "constraint",
    "container",
    "count",
    "counter",
    "create",
    "createdatabase",
    "createfield",
    "creategroup",
    "createindex",
    "createobject",
    "createproperty",
    "createrelation",
    "createtabledef",
    "createuser",
    "createworkspace",
    "currency",
    "currentuser",
    "database",
    "date",
    "datetime",
    "delete",
    "desc",
    "description",
    "disallow",
    "distinct",
    "distinctrow",
    "document",
    "double",
    "drop",
    "echo",
    "else",
    "end",
    "eqv",
    "error",
    "exists",
    "exit",
    "false",
    "field",
    "fields",
    "fillcache",
    "float",
    "float4",
    "float8",
    "foreign",
    "form",
    "forms",
    "from",
    "full",
    "function",
    "general",
    "getobject",
    "getoption",
    "gotopage",
    "group",
    "group by",
    "guid",
    "having",
    "idle",
    "ieeedouble",
    "ieeesingle",
    "if",
    "ignore",
    "imp",
    "in",
    "index",
    "indexes",
    "inner",
    "insert",
    "inserttext",
    "int",
    "integer",
    "integer1",
    "integer2",
    "integer4",
    "into",
    "is",
    "join",
    "key",
    "lastmodified",
    "left",
    "level",
    "like",
    "logical",
    "logical1",
    "long",
    "longbinary",
    "longtext",
    "macro",
    "match",
    "max",
    "min",
    "mod",
    "memo",
    "module",
    "money",
    "move",
    "name",
    "newpassword",
    "no",
    "not",
    "null",
    "number",
    "numeric",
    "object",
    "oleobject",
    "off",
    "on",
    "openrecordset",
    "option",
    "or",
    "order",
    "outer",
    "owneraccess",
    "parameter",
    "parameters",
    "partial",
    "percent",
    "pivot",
    "primary",
    "procedure",
    "property",
    "queries",
    "query",
    "quit",
    "real",
    "recalc",
    "recordset",
    "references",
    "refresh",
    "refreshlink",
    "registerdatabase",
    "relation",
    "repaint",
    "repairdatabase",
    "report",
    "reports",
    "requery",
    "right",
    "screen",
    "section",
    "select",
    "set",
    "setfocus",
    "setoption",
    "short",
    "single",
    "smallint",
    "some",
    "sql",
    "stdev",
    "stdevp",
    "string",
    "sum",
    "table",
    "tabledef",
    "tabledefs",
    "tableid",
    "text",
    "time",
    "timestamp",
    "top",
    "transform",
    "true",
    "type",
    "union",
    "unique",
    "update",
    "user",
    "value",
    "values",
    "var",
    "varp",
    "varbinary",
    "varchar",
    "where",
    "with",
    "workspace",
    "xor",
    "year",
    "yes",
    "yesno",
];

// ---------------------------------------------------------------------------
// table iterable builder
// ---------------------------------------------------------------------------

/// Port of `com.healthmarketscience.jackcess.util.TableIterableBuilder`: the
/// filter of Java's `Database.newIterable()`.
///
/// By default the normal (non-system) tables and the linked tables are
/// included and the system tables are not, exactly like the Java builder.
/// Unlike the Java class the Rust value does not keep the database handle: the
/// database is passed to [`Self::table_names`] / [`Self::for_each_table`] when
/// the iteration happens (a `&mut Database` cannot be stored and used later
/// without wrapping the whole builder in a borrow).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableIterableBuilder {
    include_normal_tables: bool,
    include_system_tables: bool,
    include_linked_tables: bool,
}

impl Default for TableIterableBuilder {
    fn default() -> Self {
        TableIterableBuilder::new()
    }
}

impl TableIterableBuilder {
    /// `new TableIterableBuilder(db)`: the Java defaults (`includeNormalTables
    /// = true`, `includeSystemTables = false`, `includeLinkedTables = true`).
    pub fn new() -> TableIterableBuilder {
        TableIterableBuilder {
            include_normal_tables: true,
            include_system_tables: false,
            include_linked_tables: true,
        }
    }

    /// `TableIterableBuilder.isIncludeNormalTables()`.
    pub fn is_include_normal_tables(&self) -> bool {
        self.include_normal_tables
    }

    /// `TableIterableBuilder.isIncludeSystemTables()`.
    pub fn is_include_system_tables(&self) -> bool {
        self.include_system_tables
    }

    /// `TableIterableBuilder.isIncludeLinkedTables()`.
    pub fn is_include_linked_tables(&self) -> bool {
        self.include_linked_tables
    }

    /// `TableIterableBuilder.setIncludeNormalTables(boolean)`.
    pub fn set_include_normal_tables(mut self, include: bool) -> TableIterableBuilder {
        self.include_normal_tables = include;
        self
    }

    /// `TableIterableBuilder.setIncludeSystemTables(boolean)`.
    pub fn set_include_system_tables(mut self, include: bool) -> TableIterableBuilder {
        self.include_system_tables = include;
        self
    }

    /// `TableIterableBuilder.setIncludeLinkedTables(boolean)`.
    pub fn set_include_linked_tables(mut self, include: bool) -> TableIterableBuilder {
        self.include_linked_tables = include;
        self
    }

    /// `TableIterableBuilder.withLocalUserTablesOnly()`: only the non-linked
    /// normal tables.
    pub fn with_local_user_tables_only(mut self) -> TableIterableBuilder {
        self.include_normal_tables = true;
        self.include_system_tables = false;
        self.include_linked_tables = false;
        self
    }

    /// `TableIterableBuilder.withSystemTablesOnly()`: only the system tables.
    pub fn with_system_tables_only(mut self) -> TableIterableBuilder {
        self.include_normal_tables = false;
        self.include_system_tables = true;
        self.include_linked_tables = false;
        self
    }

    /// `TableIterableBuilder.iterator()`'s name list: the selected table names
    /// in Java's iteration order (case-insensitive name order).
    pub fn table_names(&self, db: &mut Database) -> Result<Vec<String>> {
        db.get_table_names_filtered(
            self.include_normal_tables,
            self.include_system_tables,
            self.include_linked_tables,
        )
    }

    /// The number of tables [`Self::table_names`] would yield.
    pub fn count(&self, db: &mut Database) -> Result<usize> {
        Ok(self.table_names(db)?.len())
    }

    /// `TableIterableBuilder.iterator()` / `stream()`: visits every selected
    /// table in Java's iteration order.
    ///
    /// Each table is loaded (or taken from the database cache) by name exactly
    /// like `DatabaseImpl.TableIterator.next()` calls `getTable(name, true)`, so
    /// a system table is included when the filter asks for it and a linked table
    /// is opened from its linkee database.  The callback is the Rust equivalent
    /// of the iterator/stream body.
    pub fn for_each_table<F>(&self, db: &mut Database, mut f: F) -> Result<()>
    where
        F: FnMut(&mut crate::table::Table) -> Result<()>,
    {
        for name in self.table_names(db)? {
            let table = db.get_table(&name)?;
            f(table)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Tests of the builder surface: the Java defaults, the property groups,
    //! the reserved word escaping, the date format compatibility and the table
    //! iterable filters.

    use super::*;
    use crate::database::TableMetaDataType;
    use crate::datenums::LocalDateTime;
    use crate::tablecreator::create_table;

    fn test_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn test_path(name: &str) -> std::path::PathBuf {
        let path = test_tmp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    /// `DatabaseBuilder.newDatabase()`, `newDatabase(Path)` and the static
    /// `open`/`create` entry points.
    #[test]
    fn java_entry_points_and_defaults() {
        let builder = DatabaseBuilder::new();
        assert!(builder.auto_sync);
        assert!(!builder.read_only);
        assert!(builder.path.is_none());
        assert!(builder.charset.is_none());
        assert!(builder.time_zone.is_none());
        assert!(builder.codec_provider.is_none());
        assert!(builder.file_format.is_none());
        assert!(builder.channel.is_none());

        let builder = DatabaseBuilder::new_database().set_path("x.mdb");
        assert_eq!(builder.path.as_deref(), Some(std::path::Path::new("x.mdb")));
        let builder = DatabaseBuilder::new_database_at("y.mdb");
        assert_eq!(builder.path.as_deref(), Some(std::path::Path::new("y.mdb")));
        // setFile and setPath are the same in the port
        let builder = DatabaseBuilder::new().set_file("z.mdb").set_path("w.mdb");
        assert_eq!(builder.path.as_deref(), Some(std::path::Path::new("w.mdb")));
    }

    /// `create()` requires a format and a file; `open()` requires a file.
    #[test]
    fn missing_configuration_is_reported() {
        let err = match DatabaseBuilder::new()
            .set_file_format(FileFormat::V2003)
            .create()
        {
            Ok(_) => panic!("create without a file must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no file configured"), "{err}");
        let err = match DatabaseBuilder::new().open() {
            Ok(_) => panic!("open without a file must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no file configured"), "{err}");
        let path = test_path("builder_noformat.accdb");
        let err = match DatabaseBuilder::new_at(&path).create() {
            Ok(_) => panic!("create without a format must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no file format"), "{err}");
    }

    /// `create()` writes the template, applies the property groups and can be
    /// opened again with `DatabaseBuilder::open`.
    #[test]
    fn create_applies_properties_and_reopens() {
        let path = test_path("builder_props.accdb");
        let db = DatabaseBuilder::new_at(&path)
            .set_file_format(FileFormat::V2016)
            .put_database_property_typed(
                crate::property::TITLE_PROP,
                DataType::Text,
                Value::Text("Builder DB".to_string()),
            )
            .put_summary_property_typed(
                crate::property::AUTHOR_PROP,
                DataType::Text,
                Value::Text("the author".to_string()),
            )
            .put_user_defined_property_typed("MyFlag", DataType::Boolean, Value::Bool(true))
            .create()
            .unwrap();
        assert_eq!(db.get_file(), Some(path.as_path()));
        assert!(db.is_open());
        drop(db);

        let mut db = DatabaseBuilder::open_path(&path).unwrap();
        assert_eq!(db.get_file(), Some(path.as_path()));
        let mut props = db.get_database_properties().unwrap();
        assert_eq!(
            props
                .get_default()
                .get(crate::property::TITLE_PROP)
                .unwrap()
                .value,
            Value::Text("Builder DB".to_string())
        );
        let mut props = db.get_summary_properties().unwrap();
        assert_eq!(
            props
                .get_default()
                .get(crate::property::AUTHOR_PROP)
                .unwrap()
                .value,
            Value::Text("the author".to_string())
        );
        let mut props = db.get_user_defined_properties().unwrap();
        assert_eq!(
            props.get_default().get("MyFlag").unwrap().value,
            Value::Bool(true)
        );
        db.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    /// `setAutoSync`, `setCharset`, `setTimeZone` and
    /// `setIgnoreBrokenSystemCatalogIndex` reach the opened database, and the
    /// charset/time zone are installed before the catalog is read.
    #[test]
    fn open_options_reach_the_database() {
        let path = test_path("builder_options.mdb");
        DatabaseBuilder::new_at(&path)
            .set_file_format(FileFormat::V2003)
            .create()
            .unwrap();

        let mut db = DatabaseBuilder::new_at(&path)
            .set_auto_sync(false)
            .set_charset(Charset::Cp1252)
            .set_time_zone_offset_minutes(-300)
            .set_ignore_broken_system_catalog_index(true)
            .open()
            .unwrap();
        assert!(!db.channel.auto_sync);
        assert_eq!(db.get_charset(), Charset::Cp1252);
        assert_eq!(db.get_zone_id(), "GMT-05:00".to_string());
        assert_eq!(db.get_zone_info().offset_minutes, -300);
        assert_eq!(db.get_zone_info().raw_offset_ms, -300 * 60_000);
        assert!(!db.get_zone_info().is_named);
        assert!(db.is_ignore_broken_system_catalog_index());

        // `Database.getEvalConfig()`: the Java `DBEvalContext` defaults, with
        // the port's zone extension following the database zone
        let config = db.get_eval_config();
        assert_eq!(config.locale(), "en_US");
        assert_eq!(
            config.zone().map(|zone| zone.zone_id()),
            Some("GMT-05:00".to_string())
        );
        assert!(config.bindings().is_empty());
        assert!(config.function_lookup().get_function("UCase").is_some());
        assert!(config
            .function_lookup()
            .get_function("NoSuchFunc")
            .is_none());
        // `Database.getColumnValidatorFactory()`: the default simple factory
        let factory = db.get_column_validator_factory();
        db.set_column_validator_factory(Some(factory));
        // `setColumnValidatorFactory(null)` restores the default
        db.set_column_validator_factory(None);
        let _ = db.get_column_validator_factory();
        // a zone change (Database.setZoneId) keeps the config in sync
        db.set_zone_id("America/New_York").unwrap();
        assert_eq!(
            db.get_eval_config().zone().map(|zone| zone.zone_id()),
            Some("America/New_York".to_string())
        );
        std::fs::remove_file(&path).ok();
    }

    /// `setTimeZoneId` accepts the named and fixed offset ids and rejects an
    /// unknown one, like Java's `ZoneId.of` / `TimeZone.getTimeZone`.
    #[test]
    fn time_zone_ids() {
        let builder = DatabaseBuilder::new()
            .set_time_zone_id("America/New_York")
            .unwrap();
        let zone = builder.time_zone.unwrap();
        assert!(zone.is_named());
        assert_eq!(zone.zone_id(), "America/New_York".to_string());

        let builder = DatabaseBuilder::new()
            .set_time_zone_id("GMT+05:30")
            .unwrap();
        assert_eq!(builder.time_zone.unwrap().offset_minutes(), 330);

        let builder = DatabaseBuilder::new().set_time_zone(crate::datenums::TimeZone::utc());
        assert_eq!(builder.time_zone.unwrap().offset_minutes(), 0);

        let err = DatabaseBuilder::new()
            .set_time_zone_id("Not/AZone")
            .unwrap_err();
        assert!(err.to_string().contains("Unknown time-zone ID"), "{err}");
    }

    /// `createDateFormat`/`toCompatibleCalendar`: chrono is proleptic Gregorian,
    /// so the date format behaves like the Java `SimpleDateFormat` on dates
    /// before the Julian/Gregorian switch as well.
    #[test]
    fn date_format_is_proleptic_gregorian() {
        let format = create_date_format("yyyy-MM-dd HH:mm:ss");
        let value = LocalDateTime::new(1582, 10, 4, 12, 34, 56, 0);
        assert_eq!(format.format(value), "1582-10-04 12:34:56");
        assert_eq!(format.parse("1582-10-04 12:34:56").unwrap(), value);

        // a modern value with nanoseconds; Java's SimpleDateFormat has
        // millisecond precision, so `SSS` keeps three digits
        let format = create_date_format("yyyy-MM-dd HH:mm:ss.SSS");
        let value = LocalDateTime::new(2024, 2, 29, 1, 2, 3, 400_000_000);
        assert_eq!(format.format(value), "2024-02-29 01:02:03.400");

        let mut format = format;
        format.set_pattern("MM/dd/yyyy");
        assert_eq!(format.pattern(), "MM/dd/yyyy");
        assert_eq!(
            format.format(LocalDateTime::of_date(2001, 12, 31)),
            "12/31/2001"
        );
        // toCompatibleCalendar is a no-op in the port
        assert_eq!(to_compatible_calendar(42), 42);
    }

    /// `escapeIdentifier` / `isReservedWord` follow `TableBuilder`.
    #[test]
    fn reserved_words_are_escaped() {
        assert!(is_reserved_word("TABLE"));
        assert!(is_reserved_word("Date"));
        assert!(!is_reserved_word("MyTable"));
        assert_eq!(escape_identifier("table"), "xtable");
        assert_eq!(escape_identifier("MyTable"), "MyTable");
        let spec = new_table_escaped("select", true);
        assert_eq!(spec.name, "xselect");
        let spec = new_table_escaped("select", false);
        assert_eq!(spec.name, "select");
    }

    /// The static factories produce the definition types the creators consume.
    #[test]
    fn builder_factories() {
        let table = new_table("T").with_index(new_primary_key(&["ID"]));
        assert_eq!(table.name, "T");
        assert_eq!(table.indexes.len(), 1);
        assert_eq!(table.indexes[0].name, PRIMARY_KEY_NAME);
        assert!(table.indexes[0].flags & crate::index::UNIQUE_INDEX_FLAG != 0);
        assert!(table.indexes[0].flags & crate::index::REQUIRED_INDEX_FLAG != 0);
        assert_eq!(
            table.indexes[0].index_type,
            crate::index::IndexImpl::PRIMARY_KEY_INDEX_TYPE
        );

        let column = new_column("Name", DataType::Text);
        assert_eq!(column.name, "Name");
        assert_eq!(column.data_type, DataType::Text);
        let column = new_column_untyped("Name");
        assert_eq!(column.data_type, DataType::Text);

        let mut index = new_index("ByNum");
        assert_eq!(index.name, "ByNum");
        index.columns.push(("Num".to_string(), true));
        assert_eq!(index.columns.len(), 1);

        let relationship = new_relationship("P", "C").add_columns("ID", "P_ID");
        assert_eq!(relationship.primary_table, "P");
        assert_eq!(relationship.secondary_table, "C");
        assert_eq!(relationship.primary_columns, vec!["ID".to_string()]);
        assert_eq!(relationship.secondary_columns, vec!["P_ID".to_string()]);
    }

    /// `newRelationship(Table, Table)` uses the table names.
    #[test]
    fn relationship_from_tables() {
        let path = test_path("builder_reltables.accdb");
        let mut db = DatabaseBuilder::new_at(&path)
            .set_file_format(FileFormat::V2016)
            .create()
            .unwrap();
        let mut parent = new_table("Parent").with_index(new_primary_key(&["ID"]));
        parent.columns = vec![new_column("ID", DataType::Long).with_auto_number(true)];
        create_table(&mut db, &parent).unwrap();
        let table = db.get_table("Parent").unwrap();
        let name = table.name.clone();
        let spec = new_relationship_tables(&*table, &*table);
        assert_eq!(spec.primary_table, name);
        assert_eq!(spec.secondary_table, name);
        db.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    /// The table iterable filters: the default, local-only, system-only and the
    /// metadata iterable (catalog order, system tables included).
    #[test]
    fn table_iterables_and_metadata() {
        let path = test_path("builder_iter.mdb");
        let mut db = DatabaseBuilder::new_at(&path)
            .set_file_format(FileFormat::V2003)
            .create()
            .unwrap();
        let mut alpha = TableSpec::new("Alpha", Vec::new());
        alpha.columns = vec![new_column("ID", DataType::Long).with_auto_number(true)];
        create_table(&mut db, &alpha).unwrap();
        let mut zulu = TableSpec::new("Zulu", Vec::new());
        zulu.columns = vec![new_column("ID", DataType::Long)];
        create_table(&mut db, &zulu).unwrap();

        let iterable = db.new_iterable();
        assert!(iterable.is_include_normal_tables());
        assert!(!iterable.is_include_system_tables());
        assert!(iterable.is_include_linked_tables());
        assert_eq!(
            iterable.table_names(&mut db).unwrap(),
            vec!["Alpha", "Zulu"]
        );
        assert_eq!(iterable.count(&mut db).unwrap(), 2);

        let local = db.new_iterable().with_local_user_tables_only();
        assert_eq!(local.table_names(&mut db).unwrap(), vec!["Alpha", "Zulu"]);

        let system = db.new_iterable().with_system_tables_only();
        let system_names = system.table_names(&mut db).unwrap();
        assert!(system_names.contains(&"MSysObjects".to_string()));
        assert!(!system_names.contains(&"Alpha".to_string()));

        let all = db
            .new_iterable()
            .set_include_system_tables(true)
            .table_names(&mut db)
            .unwrap();
        assert!(all.contains(&"Alpha".to_string()));
        assert!(all.contains(&"MSysObjects".to_string()));

        // `getTableNames` and the filtered variants
        assert_eq!(db.get_table_names().unwrap(), vec!["Alpha", "Zulu"]);
        assert!(db
            .get_system_table_names()
            .unwrap()
            .contains(&"MSysObjects".to_string()));

        // `Database.iterator()`'s local subset, in name order
        let tables = db.tables().unwrap();
        assert_eq!(tables.len(), 2);
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "Zulu"]);
        drop(tables);
        assert!(db.tables_with_system().unwrap().len() > 2);

        // the callback form covers every selected table in Java order
        let mut visited = Vec::new();
        db.new_iterable()
            .for_each_table(&mut db, |table| {
                visited.push(table.name.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(visited, vec!["Alpha", "Zulu"]);

        // the metadata iterable is in catalog order and holds every table type
        let metas: Vec<_> = db.new_table_meta_data_iterable().unwrap().collect();
        let meta_names: Vec<&str> = metas.iter().map(|m| m.name.as_str()).collect();
        assert!(meta_names.contains(&"Alpha"));
        assert!(meta_names.contains(&"Zulu"));
        assert!(meta_names.contains(&"MSysObjects"));
        let alpha_meta = metas.iter().find(|m| m.name == "Alpha").unwrap();
        assert_eq!(alpha_meta.table_type, TableMetaDataType::Local);
        assert!(!alpha_meta.is_linked());
        assert!(!alpha_meta.is_system());
        assert!(metas
            .iter()
            .find(|m| m.name == "MSysObjects")
            .unwrap()
            .is_system());

        // the owned stream/iterator forms
        let mut stream = db.new_table_meta_data_stream().unwrap();
        assert!(stream.next().is_some());
        let iterable = db.new_table_meta_data_iterable().unwrap();
        assert!(!iterable.is_empty());
        assert_eq!(iterable.len(), iterable.clone().count());
        assert_eq!(iterable.len(), db.table_meta_data_all().unwrap().len());

        db.close().unwrap();
        std::fs::remove_file(&path).ok();
    }

    /// `TableMetaData.open`/`getTableDefinition`: LOCAL opens the local table,
    /// LINKED opens the linkee's table and LINKED_ODBC has no open table but a
    /// local definition.
    #[test]
    fn table_meta_data_open_and_definition() {
        let dir = test_tmp_dir();
        let target_path = test_path("builder_meta_target.mdb");
        let linker_path = test_path("builder_meta_linker.mdb");
        {
            let mut target = DatabaseBuilder::new_at(&target_path)
                .set_file_format(FileFormat::V2003)
                .create()
                .unwrap();
            let mut source = TableSpec::new("Source", Vec::new());
            source.columns = vec![new_column("ID", DataType::Long).with_auto_number(true)];
            create_table(&mut target, &source).unwrap();
            target.close().unwrap();
        }
        {
            let mut linker = DatabaseBuilder::new_at(&linker_path)
                .set_file_format(FileFormat::V2003)
                .create()
                .unwrap();
            // `LinkResolver.UNRESTRICTED` resolves a relative linkee name
            // against the process working directory, so the test stores the
            // absolute target path (the Java default resolver would refuse the
            // link either way)
            let target_name = target_path.to_string_lossy().to_string();
            linker
                .create_linked_table("LinkSource", &target_name, "Source")
                .unwrap();
            linker.close().unwrap();
        }

        let mut db = DatabaseBuilder::new_at(&linker_path).open().unwrap();
        let meta = db.get_table_meta_data("LinkSource").unwrap().unwrap();
        assert_eq!(meta.table_type, TableMetaDataType::Linked);
        assert!(meta.is_linked());
        assert_eq!(meta.linked_table_name.as_deref(), Some("Source"));
        assert_eq!(
            meta.linked_db_name.as_deref(),
            Some(target_path.to_string_lossy().as_ref())
        );
        // without a resolver the link is refused (Java's default resolver)
        assert!(meta.open(&mut db).is_err());
        // a linked table has no local definition (Java returns null)
        assert!(meta.get_table_definition(&mut db).unwrap().is_none());

        // with the unrestricted resolver the linkee's table is opened
        db.set_link_resolver(crate::linked::UNRESTRICTED);
        let meta = db.get_table_meta_data("LinkSource").unwrap().unwrap();
        let table = meta.open(&mut db).unwrap().unwrap();
        assert_eq!(table.name, "Source");
        assert!(table.table_def_page_number > 0);

        // a local table opens from its table definition page
        let local = db.get_table_meta_data("MSysObjects").unwrap().unwrap();
        assert_eq!(local.table_type, TableMetaDataType::Local);
        assert!(local.is_system());
        let table = local.open(&mut db).unwrap().unwrap();
        assert_eq!(table.name, "MSysObjects");
        assert!(local.get_table_definition(&mut db).unwrap().is_none());

        db.close().unwrap();
        // the directory is only used to keep the test files together
        let _ = dir;
        std::fs::remove_file(&linker_path).ok();
        std::fs::remove_file(&target_path).ok();
    }

    /// `setChannel` databases keep the caller's channel: Java's
    /// `closeChannel=false` leaves it open, so `close()` only flushes and the
    /// database is not marked closed.
    #[test]
    fn channel_database_keeps_the_callers_channel_open() {
        let channel = Box::new(crate::pagechannel::MemChannel::new());
        let mut db = DatabaseBuilder::new()
            .set_file_format(FileFormat::V2003)
            .set_channel(channel)
            .create()
            .unwrap();
        assert!(db.is_open());
        let mut spec = TableSpec::new("T", Vec::new());
        spec.columns = vec![new_column("ID", DataType::Long)];
        create_table(&mut db, &spec).unwrap();
        db.close().unwrap();
        assert!(db.is_open());
        // a second close just flushes again, like Java's still open channel
        db.close().unwrap();
        assert_eq!(db.get_path(), None);
    }

    /// `close` flushes and marks the database closed; a second close and a
    /// flush afterwards report the Java `ClosedChannelException`.
    #[test]
    fn close_flushes_and_marks_closed() {
        let path = test_path("builder_close.mdb");
        let mut db = DatabaseBuilder::new_at(&path)
            .set_file_format(FileFormat::V2003)
            .create()
            .unwrap();
        let mut spec = TableSpec::new("T", Vec::new());
        spec.columns = vec![new_column("ID", DataType::Long)];
        create_table(&mut db, &spec).unwrap();
        db.add_rows("T", &[vec![Value::Long(1)]]).unwrap();
        assert!(db.is_open());
        assert!(!db.is_closed());
        db.close().unwrap();
        assert!(!db.is_open());
        assert!(db.is_closed());
        // a second close reports the same ClosedChannelException the Java
        // `PageChannel.close` flush throws on the closed channel
        let err = db.close().unwrap_err();
        assert!(err.to_string().contains("ClosedChannelException"), "{err}");
        let err = db.flush().unwrap_err();
        assert!(err.to_string().contains("ClosedChannelException"), "{err}");
        assert_eq!(
            err.java_to_string(),
            "java.io.IOException: ClosedChannelException"
        );

        // the row was flushed by close
        let mut reopened = DatabaseBuilder::open_path(&path).unwrap();
        let rows = reopened.read_rows_with_ids("T").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]
                .1
                .as_ref()
                .and_then(|values| values.first().cloned()),
            Some(Value::Long(1))
        );
        reopened.close().unwrap();
        std::fs::remove_file(&path).ok();
    }
}
