//! Port of `com.healthmarketscience.jackcess.util.LinkResolver` and the
//! linked-table support of `DatabaseImpl` (`getLinkedDatabases`,
//! `isLinkedTable`, `getTable` through a link, and row/schema writes on a
//! linked table name).
//!
//! # Design
//!
//! Java's `DatabaseImpl` keeps the opened linkee databases in
//! `_linkedDbs` (`Map<String, Database>`) and hands out `Table` instances which
//! remember their owning `Database`, so a `Table` read through a link reads
//! from the linkee file.  The Rust port's [`crate::table::Table`] has no
//! database handle: every read takes the [`crate::pagechannel::PageChannel`]
//! explicitly.  Therefore the port stores each opened linkee as an **owned**
//! `Box<Database>` on the linker ([`crate::database::Database`]): a linkee is
//! only reachable through a mutable borrow of the linker, so
//! `Database::get_table` can return a `&mut Table` which is tied to the
//! linker's borrow and still belongs to the linkee file.  There is no
//! `Rc<RefCell<_>>` and no self-referential lifetime: the linkee is simply a
//! child value of the linker.
//!
//! Because a returned [`crate::table::Table`] does not know its database, use
//! [`crate::database::Database::read_rows_with_ids`] (which resolves a linked
//! table name to the linkee and reads with the linkee's channel) when reading
//! rows through a link.  The linked database itself is reachable with
//! [`crate::database::Database::get_linked_database_mut`].
//!
//! The table-name based write API resolves a linked name the same way and
//! delegates the whole operation to the linkee database: `add_rows`,
//! `update_row`/`update_row_partial`/`update_value`/`update_row_from_map`,
//! `delete_row`, `add_column`, `add_index`,
//! `set_table_allow_auto_number_insert`, and the complex/version-history
//! accessors.  The operation therefore uses the linkee's table definition
//! page, row ids, channel, catalog, defaults, validators, indexes, foreign
//! keys and complex value tables, exactly like `DatabaseImpl.getTable(linked)`
//! returning the linkee's `TableImpl`; the linker file itself is not written
//! by such an operation.
//!
//! Relative linkee names are resolved against the process working directory,
//! exactly like Java's `new File(linkeeFileName)` in
//! [`UNRESTRICTED`](Unrestricted), *not* against the linker file's directory.
//!
//! # `util.CustomLinkResolver`
//!
//! Java's `CustomLinkResolver` is an *abstract* base for linked data which is
//! not an Access database: an implementation maps the stored linkee file name
//! to some custom state ([`CustomLinkResolverBase::load_custom_file`]) and
//! creates the requested tables in a temporary Access database
//! ([`CustomLinkResolverBase::load_custom_table`]).  The base then returns that
//! temporary database to the linker.  It is not itself a name-to-file map; the
//! map/loader forms requested for this port are the concrete
//! [`CustomLinkResolver::from_map`] / [`CustomLinkResolver::from_loader`]
//! constructors, which live in the same public type.
//!
//! Rust has no subclassing, so the two abstract Java methods are a trait
//! ([`CustomLinkResolverBase`]) and [`CustomLinkResolver`] owns a boxed
//! implementation.  [`CustomLinkResolver`] implements [`LinkResolver`], so it
//! is installed with [`crate::database::Database::set_link_resolver`] like any
//! other resolver.  A missing custom file falls back to [`DEFAULT`], exactly
//! like Java's `DatabaseImpl.getDefaultLinkResolver()` fallback, so the error
//! message is the same denial message naming the linkee file.
//!
//! Two behaviours are adapted to this port and are documented on the methods:
//!
//! * Java's `TempDatabaseImpl` lazily calls `loadCustomTable` from the
//!   temporary database's `getTable` override.  The port's
//!   [`crate::database::Database`] has no such hook (and `database.rs` is owned
//!   by another change), so [`CustomLinkResolverBase::populate_temp_db`] eagerly
//!   loads every table name which the linker references for that linkee.  The
//!   returned database is otherwise the same temporary database.
//! * The temporary database is a real (writable) database at creation time so
//!   the tables can be loaded; when the linker is read-only it is wrapped
//!   read-only after loading, matching Java's read-only `TempDatabaseImpl` for
//!   subsequent reads/writes.  File based temporary databases delete their file
//!   when the database is dropped (Java deletes it on `close`); in-memory
//!   temporary databases use [`crate::pagechannel::MemChannel`] and never touch
//!   the file system after creation.

use std::any::Any;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::database::Database;
use crate::error::{Error, Result};
use crate::format::FileFormat;
use crate::pagechannel::{FileIo, MemChannel};

/// Port of the `LinkResolver` functional interface.
///
/// The Java interface receives the linker `Database` and the linkee file name
/// stored in the linker's catalog and returns an opened linkee `Database`.
/// This port returns the linkee by value; the linker stores it as an owned
/// child database (see the module documentation).
pub trait LinkResolver {
    /// Returns the appropriate [`Database`] for `linkee_file_name`.
    fn resolve_linked_database(
        &self,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Database>;
}

impl<F> LinkResolver for F
where
    F: Fn(&Database, &str) -> Result<Database>,
{
    fn resolve_linked_database(
        &self,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Database> {
        self(linker, linkee_file_name)
    }
}

/// Port of `LinkResolver.DEFAULT`: refuses to open any linked database.
///
/// This is the resolver used when the application has not configured one.
/// Enabling the Java `ALLOW_LINK_RESOLUTION_PROPERTY` system property selects
/// [`UNRESTRICTED`] instead; this port does not consult that property (the
/// caller configures the policy explicitly with
/// [`crate::database::Database::set_link_resolver`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct Denied;

/// Port of `LinkResolver.UNRESTRICTED`: opens whatever file name the linker
/// catalog specifies, including absolute, network (UNC) and device paths.
///
/// The linkee is opened read-only when the linker itself is read-only and
/// read-write otherwise, matching Java's implementation
/// (`DatabaseBuilder.setReadOnly(linkerDb.isReadOnly())`).
///
/// Since the linkee file name comes from the database file itself, a database
/// from an untrusted source can use this resolver to make the library read an
/// arbitrary local file or open a network connection.  Only use this resolver
/// when the linked database file names can be trusted.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unrestricted;

/// `LinkResolver.DEFAULT`: the resolver used when none is configured.
pub const DEFAULT: Denied = Denied;

/// `LinkResolver.UNRESTRICTED`.
pub const UNRESTRICTED: Unrestricted = Unrestricted;

/// `AccessDeniedException.getMessage()` of `LinkResolver.DEFAULT`
/// (`file + ": " + reason`).
pub fn denied_resolution_message(linkee_file_name: &str) -> String {
    format!(
        "{}: Linked database resolution is disabled.  Configure a LinkResolver \
         on the Database (e.g. LinkResolver.UNRESTRICTED for trusted file \
         names) to enable it",
        linkee_file_name
    )
}

impl LinkResolver for Denied {
    fn resolve_linked_database(
        &self,
        _linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Database> {
        // Java throws a java.nio.file.AccessDeniedException whose message names
        // the linkee file; there is no AccessDenied variant in this port's
        // error type, so the exact message is carried by a Jackcess error.
        Err(Error::Jackcess(denied_resolution_message(linkee_file_name)))
    }
}

impl LinkResolver for Unrestricted {
    fn resolve_linked_database(
        &self,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Database> {
        let path = Path::new(linkee_file_name);
        // `Database::open_path` opens with create(true); a link must never
        // create the linkee file, Java throws FileNotFoundException instead.
        std::fs::metadata(path)?;
        if linker.is_read_only() {
            Database::open_path_read_only(path)
        } else {
            Database::open_path(path)
        }
    }
}

// ---------------------------------------------------------------------------
// `util.CustomLinkResolver`
// ---------------------------------------------------------------------------

/// `CustomLinkResolver.DEFAULT_FORMAT`: the format of the temporary database.
pub const DEFAULT_FORMAT: FileFormat = FileFormat::V2000;

/// `CustomLinkResolver.DEFAULT_IN_MEMORY`: temporary databases are file based
/// by default.
pub const DEFAULT_IN_MEMORY: bool = false;

/// `CustomLinkResolver.DEFAULT_TEMP_DIR`: `null` means the system temporary
/// directory.
pub const DEFAULT_TEMP_DIR: Option<&'static Path> = None;

/// `CustomLinkResolver.MEM_DB_PREFIX`.
const MEM_DB_PREFIX: &str = "memdb_";
/// `CustomLinkResolver.FILE_DB_PREFIX`.
const FILE_DB_PREFIX: &str = "linkeddb_";

/// The abstract methods of Java's `util.CustomLinkResolver`.
///
/// Java subclasses `CustomLinkResolver` and overrides `loadCustomFile` and
/// `loadCustomTable`; the other methods have the same defaults as Java.  The
/// Rust port uses this trait for the two abstract methods and
/// [`CustomLinkResolver`] for the concrete base implementation.
///
/// `create_temp_db`/`populate_temp_db` are the extension points corresponding
/// to Java's protected `createTempDb` and to the `TempDatabaseImpl.getTable`
/// hook respectively.  The map/loader constructors override `create_temp_db`
/// to open the mapped Access database directly and make `populate_temp_db` a
/// no-op (the mapped database already contains its tables).
pub trait CustomLinkResolverBase: 'static {
    /// The custom state returned by [`load_custom_file`](Self::load_custom_file)
    /// and passed back to [`load_custom_table`](Self::load_custom_table)
    /// (Java's `Object customFile`).
    type CustomFile: 'static;

    /// `getDefaultFormat`.
    fn default_format(&self) -> FileFormat {
        DEFAULT_FORMAT
    }

    /// `isDefaultInMemory`.
    fn is_default_in_memory(&self) -> bool {
        DEFAULT_IN_MEMORY
    }

    /// `getDefaultTempDirectory`.
    fn default_temp_directory(&self) -> Option<PathBuf> {
        DEFAULT_TEMP_DIR.map(Path::to_path_buf)
    }

    /// `loadCustomFile`: `Some` when `linkee_file_name` is a custom file which
    /// should be served from a temporary database, `None` to fall back to the
    /// default (denying) resolver.
    fn load_custom_file(
        &self,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Option<Self::CustomFile>>;

    /// `loadCustomTable`: create `table_name` in `temp_db` (using the normal
    /// table creation APIs) and return whether it was available.
    fn load_custom_table(
        &self,
        temp_db: &mut Database,
        custom_file: &Self::CustomFile,
        table_name: &str,
    ) -> Result<bool>;

    /// `createTempDb`: create the temporary database.  The default creates an
    /// empty database in `format`, either in memory or as a temporary file in
    /// `temp_dir`/the system temporary directory (see [`DEFAULT_FORMAT`],
    /// [`DEFAULT_IN_MEMORY`] and [`DEFAULT_TEMP_DIR`]).
    ///
    /// The returned database must still be writable at this point: the tables
    /// are loaded by [`populate_temp_db`](Self::populate_temp_db) and the base
    /// wraps it read-only afterwards when the linker is read-only.
    fn create_temp_db(
        &self,
        custom_file: &Self::CustomFile,
        format: FileFormat,
        in_memory: bool,
        temp_dir: Option<&Path>,
        read_only: bool,
    ) -> Result<Database> {
        let _ = (custom_file, read_only);
        create_temp_database(format, in_memory, temp_dir)
    }

    /// Loads the tables of a newly created temporary database.
    ///
    /// Java does this lazily from `TempDatabaseImpl.getTable`; the port has no
    /// such hook, so the default implementation loads every table name which
    /// the linker references for `linkee_file_name` (the same set of names a
    /// lazy implementation could ever be asked for by this linker).  A false
    /// return from [`load_custom_table`](Self::load_custom_table) leaves the
    /// table absent, exactly like Java.
    fn populate_temp_db(
        &self,
        temp_db: &mut Database,
        custom_file: &Self::CustomFile,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<()> {
        default_populate_temp_db(self, temp_db, custom_file, linker, linkee_file_name)
    }
}

/// The object-safe form of [`CustomLinkResolverBase`] used by
/// [`CustomLinkResolver`]; `CustomFile` is erased behind [`Any`].
trait CustomLinkResolverBaseDyn {
    fn default_format(&self) -> FileFormat;
    fn is_default_in_memory(&self) -> bool;
    fn default_temp_directory(&self) -> Option<PathBuf>;
    fn load_custom_file(
        &self,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Option<Box<dyn Any>>>;
    fn create_temp_db(
        &self,
        custom_file: &dyn Any,
        format: FileFormat,
        in_memory: bool,
        temp_dir: Option<&Path>,
        read_only: bool,
    ) -> Result<Database>;
    fn populate_temp_db(
        &self,
        temp_db: &mut Database,
        custom_file: &dyn Any,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<()>;
}

struct ErasedCustomLinkResolverBase<B: CustomLinkResolverBase> {
    base: B,
}

fn downcast_custom_file<B: CustomLinkResolverBase>(
    custom_file: &dyn Any,
) -> Result<&B::CustomFile> {
    custom_file.downcast_ref::<B::CustomFile>().ok_or_else(|| {
        Error::IllegalState(
            "custom link resolver: custom file state has an unexpected type".to_string(),
        )
    })
}

impl<B: CustomLinkResolverBase> CustomLinkResolverBaseDyn for ErasedCustomLinkResolverBase<B> {
    fn default_format(&self) -> FileFormat {
        self.base.default_format()
    }

    fn is_default_in_memory(&self) -> bool {
        self.base.is_default_in_memory()
    }

    fn default_temp_directory(&self) -> Option<PathBuf> {
        self.base.default_temp_directory()
    }

    fn load_custom_file(
        &self,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Option<Box<dyn Any>>> {
        Ok(self
            .base
            .load_custom_file(linker, linkee_file_name)?
            .map(|custom_file| Box::new(custom_file) as Box<dyn Any>))
    }

    fn create_temp_db(
        &self,
        custom_file: &dyn Any,
        format: FileFormat,
        in_memory: bool,
        temp_dir: Option<&Path>,
        read_only: bool,
    ) -> Result<Database> {
        let custom_file = downcast_custom_file::<B>(custom_file)?;
        self.base
            .create_temp_db(custom_file, format, in_memory, temp_dir, read_only)
    }

    fn populate_temp_db(
        &self,
        temp_db: &mut Database,
        custom_file: &dyn Any,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<()> {
        let custom_file = downcast_custom_file::<B>(custom_file)?;
        self.base
            .populate_temp_db(temp_db, custom_file, linker, linkee_file_name)
    }
}

/// Port of `com.healthmarketscience.jackcess.util.CustomLinkResolver`.
///
/// This is the concrete resolver installed on a [`Database`]:
///
/// ```ignore
/// // name -> file map (relative names resolve against the base directory)
/// let resolver = CustomLinkResolver::from_map_with_base_dir(
///     ".",
///     [("old/target.mdb".to_string(), PathBuf::from("moved/target.mdb"))],
/// );
/// db.set_link_resolver(resolver);
/// ```
///
/// To implement Java's abstract custom-file loader, implement
/// [`CustomLinkResolverBase`] and use [`CustomLinkResolver::from_base`].
pub struct CustomLinkResolver {
    base: Box<dyn CustomLinkResolverBaseDyn>,
}

impl CustomLinkResolver {
    /// `CustomLinkResolver.DEFAULT_FORMAT`.
    pub const DEFAULT_FORMAT: FileFormat = DEFAULT_FORMAT;

    /// `CustomLinkResolver.DEFAULT_IN_MEMORY`.
    pub const DEFAULT_IN_MEMORY: bool = DEFAULT_IN_MEMORY;

    /// `CustomLinkResolver.DEFAULT_TEMP_DIR` (`null` = system temporary
    /// directory).
    pub const DEFAULT_TEMP_DIR: Option<&'static Path> = DEFAULT_TEMP_DIR;

    /// Wraps a [`CustomLinkResolverBase`] implementation (Java's abstract
    /// `CustomLinkResolver` subclass).
    pub fn from_base<B: CustomLinkResolverBase>(base: B) -> CustomLinkResolver {
        CustomLinkResolver {
            base: Box::new(ErasedCustomLinkResolverBase { base }),
        }
    }

    /// A resolver which maps the stored linkee file name to an Access database
    /// file.  Relative mapped paths resolve against the process working
    /// directory, like Java's `new File(name)`; use
    /// [`from_map_with_base_dir`](Self::from_map_with_base_dir) to resolve them
    /// against a base directory instead.
    ///
    /// Lookup is exact first, then ASCII case-insensitive (Windows paths are
    /// case-insensitive), so a linkee name stored with a different case still
    /// finds its mapping.
    pub fn from_map<I, K, V>(entries: I) -> CustomLinkResolver
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<PathBuf>,
    {
        CustomLinkResolver {
            base: Box::new(ErasedCustomLinkResolverBase {
                base: PathBase::from_map(None, entries),
            }),
        }
    }

    /// [`from_map`](Self::from_map) with relative mapped paths resolved
    /// against `base_dir`.
    pub fn from_map_with_base_dir<I, K, V>(
        base_dir: impl Into<PathBuf>,
        entries: I,
    ) -> CustomLinkResolver
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<PathBuf>,
    {
        CustomLinkResolver {
            base: Box::new(ErasedCustomLinkResolverBase {
                base: PathBase::from_map(Some(base_dir.into()), entries),
            }),
        }
    }

    /// A resolver backed by a loader callback: it receives the linker and the
    /// stored linkee file name and returns the file to open (or `None` to fall
    /// back to [`DEFAULT`], which denies the link).
    ///
    /// Relative paths returned by the loader resolve against the process
    /// working directory; use
    /// [`from_loader_with_base_dir`](Self::from_loader_with_base_dir) for a
    /// base directory.
    pub fn from_loader<F>(loader: F) -> CustomLinkResolver
    where
        F: Fn(&Database, &str) -> Result<Option<PathBuf>> + 'static,
    {
        CustomLinkResolver {
            base: Box::new(ErasedCustomLinkResolverBase {
                base: PathBase::from_loader(None, loader),
            }),
        }
    }

    /// [`from_loader`](Self::from_loader) with relative paths resolved against
    /// `base_dir`.
    pub fn from_loader_with_base_dir<F>(
        base_dir: impl Into<PathBuf>,
        loader: F,
    ) -> CustomLinkResolver
    where
        F: Fn(&Database, &str) -> Result<Option<PathBuf>> + 'static,
    {
        CustomLinkResolver {
            base: Box::new(ErasedCustomLinkResolverBase {
                base: PathBase::from_loader(Some(base_dir.into()), loader),
            }),
        }
    }

    /// `getDefaultFormat`.
    pub fn default_format(&self) -> FileFormat {
        self.base.default_format()
    }

    /// `isDefaultInMemory`.
    pub fn is_default_in_memory(&self) -> bool {
        self.base.is_default_in_memory()
    }

    /// `getDefaultTempDirectory`.
    pub fn default_temp_directory(&self) -> Option<PathBuf> {
        self.base.default_temp_directory()
    }
}

impl LinkResolver for CustomLinkResolver {
    fn resolve_linked_database(
        &self,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Database> {
        resolve_custom_linked_database(self.base.as_ref(), linker, linkee_file_name)
    }
}

fn resolve_custom_linked_database(
    base: &dyn CustomLinkResolverBaseDyn,
    linker: &Database,
    linkee_file_name: &str,
) -> Result<Database> {
    // `CustomLinkResolver.resolveLinkedDatabase`: a non-custom file falls back
    // to the default resolver (which denies it)
    let custom_file = match base.load_custom_file(linker, linkee_file_name)? {
        Some(custom_file) => custom_file,
        None => return DEFAULT.resolve_linked_database(linker, linkee_file_name),
    };

    let format = base.default_format();
    let in_memory = base.is_default_in_memory();
    let temp_dir = base.default_temp_directory();
    let mut temp_db = base.create_temp_db(
        custom_file.as_ref(),
        format,
        in_memory,
        temp_dir.as_deref(),
        linker.is_read_only(),
    )?;
    // Java's `TempDatabaseImpl` loads tables lazily; this port loads them
    // eagerly (see `populate_temp_db`)
    base.populate_temp_db(&mut temp_db, custom_file.as_ref(), linker, linkee_file_name)?;
    if linker.is_read_only() && !temp_db.is_read_only() {
        make_read_only(&mut temp_db);
    }
    Ok(temp_db)
}

/// Default [`CustomLinkResolverBase::populate_temp_db`]: load every linkee
/// table name referenced by the linker for `linkee_file_name`.
fn default_populate_temp_db<B: CustomLinkResolverBase + ?Sized>(
    base: &B,
    temp_db: &mut Database,
    custom_file: &B::CustomFile,
    linker: &Database,
    linkee_file_name: &str,
) -> Result<()> {
    let mut candidate_names: Vec<String> = Vec::new();
    for local_name in linker.linked_table_names() {
        let info = match linker.linked_table_info(&local_name) {
            Some(info) => info,
            None => continue,
        };
        if info.linked_db_name != linkee_file_name {
            continue;
        }
        if candidate_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&info.linked_table_name))
        {
            continue;
        }
        candidate_names.push(info.linked_table_name.clone());
    }

    for table_name in candidate_names {
        // already created by an earlier candidate (names differing only by
        // case are collapsed above)
        if temp_db.find_table(&table_name)?.is_some() {
            continue;
        }
        let _ = base.load_custom_table(temp_db, custom_file, &table_name)?;
    }
    Ok(())
}

/// The map/loader backed [`CustomLinkResolverBase`] used by
/// [`CustomLinkResolver::from_map`] and [`CustomLinkResolver::from_loader`].
struct PathBase {
    base_dir: Option<PathBuf>,
    source: PathSource,
}

/// Loads the linkee path for a table name
/// (`CustomLinkResolverBase::loadCustomFile`).
type PathLoader = Box<dyn Fn(&Database, &str) -> Result<Option<PathBuf>>>;

enum PathSource {
    Map(BTreeMap<String, PathBuf>),
    Loader(PathLoader),
}

impl PathBase {
    fn from_map<I, K, V>(base_dir: Option<PathBuf>, entries: I) -> PathBase
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<PathBuf>,
    {
        let mapping = entries
            .into_iter()
            .map(|(name, path)| (name.into(), path.into()))
            .collect();
        PathBase {
            base_dir,
            source: PathSource::Map(mapping),
        }
    }

    fn from_loader<F>(base_dir: Option<PathBuf>, loader: F) -> PathBase
    where
        F: Fn(&Database, &str) -> Result<Option<PathBuf>> + 'static,
    {
        PathBase {
            base_dir,
            source: PathSource::Loader(Box::new(loader)),
        }
    }

    /// `File.isAbsolute() ? file : new File(baseDir, file)`.
    fn resolve_path(&self, path: PathBuf) -> PathBuf {
        if path.is_absolute() {
            path
        } else if let Some(base_dir) = &self.base_dir {
            base_dir.join(path)
        } else {
            path
        }
    }
}

fn lookup_mapped_path(
    mapping: &BTreeMap<String, PathBuf>,
    linkee_file_name: &str,
) -> Option<PathBuf> {
    if let Some(path) = mapping.get(linkee_file_name) {
        return Some(path.clone());
    }
    // different-case linkee name: File paths on Windows are case-insensitive
    mapping
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(linkee_file_name))
        .map(|(_, path)| path.clone())
}

impl CustomLinkResolverBase for PathBase {
    type CustomFile = PathBuf;

    fn is_default_in_memory(&self) -> bool {
        // the mapped database is opened directly; there is no temporary
        // database, and Java's `DEFAULT_IN_MEMORY` is false
        false
    }

    fn load_custom_file(
        &self,
        linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Option<Self::CustomFile>> {
        let mapped = match &self.source {
            PathSource::Map(mapping) => lookup_mapped_path(mapping, linkee_file_name),
            PathSource::Loader(loader) => loader(linker, linkee_file_name)?,
        };
        Ok(mapped.map(|path| self.resolve_path(path)))
    }

    fn load_custom_table(
        &self,
        _temp_db: &mut Database,
        _custom_file: &Self::CustomFile,
        _table_name: &str,
    ) -> Result<bool> {
        Ok(false)
    }

    fn create_temp_db(
        &self,
        custom_file: &Self::CustomFile,
        _format: FileFormat,
        _in_memory: bool,
        _temp_dir: Option<&Path>,
        read_only: bool,
    ) -> Result<Database> {
        // `Database::open_path` opens with create(true); a link must never
        // create the mapped file (Java throws FileNotFoundException)
        std::fs::metadata(custom_file)?;
        if read_only {
            Database::open_path_read_only(custom_file)
        } else {
            Database::open_path(custom_file)
        }
    }

    fn populate_temp_db(
        &self,
        _temp_db: &mut Database,
        _custom_file: &Self::CustomFile,
        _linker: &Database,
        _linkee_file_name: &str,
    ) -> Result<()> {
        // the mapped database already contains its tables
        Ok(())
    }
}

/// Creates Java's temporary database: an in-memory database or a
/// `linkeddb_*` file in `temp_dir`/the system temporary directory.
fn create_temp_database(
    format: FileFormat,
    in_memory: bool,
    temp_dir: Option<&Path>,
) -> Result<Database> {
    let dir = match temp_dir {
        Some(temp_dir) => temp_dir.to_path_buf(),
        None => std::env::temp_dir(),
    };
    let prefix = if in_memory {
        MEM_DB_PREFIX
    } else {
        FILE_DB_PREFIX
    };
    for _ in 0..64 {
        let path = dir.join(format!(
            "{}{}{}",
            prefix,
            temporary_database_id(),
            format.extension()
        ));
        if path.exists() {
            continue;
        }
        return create_temp_database_at(format, in_memory, &path);
    }
    Err(Error::Jackcess(
        "unable to allocate a temporary linked database name".to_string(),
    ))
}

fn create_temp_database_at(format: FileFormat, in_memory: bool, path: &Path) -> Result<Database> {
    match create_temp_database_at_impl(format, in_memory, path) {
        Ok(db) => Ok(db),
        Err(error) => {
            // Java's `createTempDb` `finally` deletes the temp file when the
            // temp database could not be created
            let _ = std::fs::remove_file(path);
            Err(error)
        }
    }
}

fn create_temp_database_at_impl(
    format: FileFormat,
    in_memory: bool,
    path: &Path,
) -> Result<Database> {
    let mut db = Database::create(format, path)?;
    if in_memory {
        // read the empty template into memory and delete the file, like
        // `MemFileChannel` (Java never creates a file in this mode)
        let bytes = std::fs::read(path)?;
        let old_io = std::mem::replace(&mut db.channel.io, Box::new(MemChannel::from_bytes(bytes)));
        drop(old_io);
        let _ = std::fs::remove_file(path);
    } else {
        let old_io = std::mem::replace(&mut db.channel.io, Box::new(MemChannel::new()));
        db.channel.io = Box::new(TempFileIo {
            path: path.to_path_buf(),
            inner: Some(old_io),
        });
    }
    db.file_format = Some(format);
    Ok(db)
}

/// A temporary database file which is deleted when its channel is dropped
/// (Java's `TempDatabaseImpl.close` + `deleteDbFile`).
struct TempFileIo {
    path: PathBuf,
    inner: Option<Box<dyn FileIo>>,
}

impl TempFileIo {
    fn inner_mut(&mut self) -> &mut dyn FileIo {
        self.inner.as_mut().expect("temp file channel").as_mut()
    }
}

impl FileIo for TempFileIo {
    fn size(&mut self) -> Result<u64> {
        self.inner_mut().size()
    }

    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.inner_mut().read_at(buf, offset)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<usize> {
        self.inner_mut().write_at(buf, offset)
    }

    fn truncate(&mut self, size: u64) -> Result<()> {
        self.inner_mut().truncate(size)
    }

    fn flush(&mut self) -> Result<()> {
        self.inner_mut().flush()
    }
}

impl Drop for TempFileIo {
    fn drop(&mut self) {
        // close the file before deleting it (Windows cannot delete an open
        // file), then apply Java's `deleteDbFile` prefix check
        self.inner.take();
        let is_temp_db_file = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with(FILE_DB_PREFIX))
            .unwrap_or(false);
        if is_temp_db_file {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `util.ReadOnlyFileChannel` for a temporary database: reads pass through,
/// writes fail with the exception a read-only `FileChannel` throws.
struct ReadOnlyTempIo {
    inner: Box<dyn FileIo>,
}

impl FileIo for ReadOnlyTempIo {
    fn size(&mut self) -> Result<u64> {
        self.inner.size()
    }

    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.inner.read_at(buf, offset)
    }

    fn write_at(&mut self, _buf: &[u8], _offset: u64) -> Result<usize> {
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "NonWritableChannelException",
        )))
    }

    fn truncate(&mut self, _size: u64) -> Result<()> {
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "NonWritableChannelException",
        )))
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

fn make_read_only(db: &mut Database) {
    if db.is_read_only() {
        return;
    }
    let old_io = std::mem::replace(&mut db.channel.io, Box::new(MemChannel::new()));
    db.channel.io = Box::new(ReadOnlyTempIo { inner: old_io });
    db.read_only = true;
}

fn temporary_database_id() -> u64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_ok() {
        return u64::from_le_bytes(bytes);
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0)
}

/// The linked-table metadata of one `MSysObjects` row of type 6
/// (`DatabaseImpl.LinkedTableInfo`, a `TableMetaData` with
/// `getType() == Type.LINKED`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedTableInfo {
    /// The local (linking) table name, in its catalog spelling.
    pub name: String,
    /// The synthetic (negative) object id of the catalog row, which the port
    /// uses as the linked table's "table definition page".
    pub object_id: i32,
    /// The catalog flags of the object.
    pub flags: i32,
    /// `MSysObjects.Database`: the linkee file name (may be relative).
    pub linked_db_name: String,
    /// `MSysObjects.ForeignName`: the table name inside the linkee.
    pub linked_table_name: String,
}

impl LinkedTableInfo {
    /// `TableMetaData.getType()` is `LINKED` for every linked table.
    pub fn is_linked(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnSpec;
    use crate::datatype::DataType;
    use crate::tablecreator::{IndexSpec, TableSpec};
    use crate::value::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn test_dir(stem: &str) -> PathBuf {
        let unique = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!(
                "jackcess_customlink_{}_{}_{}",
                std::process::id(),
                stem,
                unique
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn source_spec(name: &str) -> TableSpec {
        TableSpec::new(
            name,
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(50),
                ColumnSpec::new("Num", DataType::Long),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ))
    }

    fn create_source_database(path: &Path, table_name: &str, rows: &[(i32, &str, i32)]) {
        let mut db = Database::create(FileFormat::V2003, path).unwrap();
        db.create_table(&source_spec(table_name)).unwrap();
        for (id, name, num) in rows {
            db.add_rows(
                table_name,
                &[vec![
                    Value::Long(*id),
                    Value::Text((*name).to_string()),
                    Value::Long(*num),
                ]],
            )
            .unwrap();
        }
        db.flush().unwrap();
    }

    struct Fixture {
        dir: PathBuf,
        target: PathBuf,
        target_rel: String,
        loader_target: PathBuf,
        loader_rel: String,
        linker: PathBuf,
    }

    /// target + a second loader target, and a linker with links to both (by
    /// relative name, by absolute name, by a different-case name and a missing
    /// name).
    fn fixture(stem: &str) -> Fixture {
        let dir = test_dir(stem);
        let target_rel = "target.mdb".to_string();
        let loader_rel = "loader_target.mdb".to_string();
        let target = dir.join(&target_rel);
        let loader_target = dir.join(&loader_rel);
        create_source_database(
            &target,
            "Source",
            &[(1, "alpha", 10), (2, "beta", 20), (3, "gamma", 30)],
        );
        create_source_database(
            &loader_target,
            "Source",
            &[(7, "delta", 70), (8, "epsilon", 80)],
        );

        let linker = dir.join("linker.mdb");
        {
            let mut db = Database::create(FileFormat::V2003, &linker).unwrap();
            db.create_linked_table("RelLink", &target_rel, "Source")
                .unwrap();
            db.create_linked_table("AbsLink", &target.to_string_lossy(), "Source")
                .unwrap();
            db.create_linked_table("CaseLink", &target_rel.to_uppercase(), "Source")
                .unwrap();
            db.create_linked_table("MissingLink", "missing.mdb", "Source")
                .unwrap();
            db.create_linked_table("LoaderLink", &loader_rel, "Source")
                .unwrap();
            db.flush().unwrap();
        }
        Fixture {
            dir,
            target,
            target_rel,
            loader_target,
            loader_rel,
            linker,
        }
    }

    fn move_targets(fixture: &Fixture) -> (PathBuf, PathBuf) {
        let moved_dir = fixture.dir.join("moved");
        std::fs::create_dir_all(&moved_dir).unwrap();
        let moved_target = moved_dir.join(fixture.target.file_name().unwrap());
        let moved_loader = moved_dir.join(fixture.loader_target.file_name().unwrap());
        std::fs::rename(&fixture.target, &moved_target).unwrap();
        std::fs::rename(&fixture.loader_target, &moved_loader).unwrap();
        (moved_target, moved_loader)
    }

    fn read_link_rows(db: &mut Database, link_name: &str) -> Vec<Vec<Value>> {
        db.read_rows_with_ids(link_name)
            .unwrap()
            .into_iter()
            .map(|(_, values)| values.expect("a row read through a link is not deleted"))
            .collect()
    }

    fn assert_source_rows(rows: &[Vec<Value>]) {
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            vec![
                Value::Long(1),
                Value::Text("alpha".to_string()),
                Value::Long(10)
            ]
        );
        assert_eq!(
            rows[1],
            vec![
                Value::Long(2),
                Value::Text("beta".to_string()),
                Value::Long(20)
            ]
        );
        assert_eq!(
            rows[2],
            vec![
                Value::Long(3),
                Value::Text("gamma".to_string()),
                Value::Long(30)
            ]
        );
    }

    #[test]
    fn custom_link_resolver_defaults_match_java() {
        assert_eq!(DEFAULT_FORMAT, FileFormat::V2000);
        // the other two defaults are constants, so check them at compile time
        const _: () = {
            assert!(!DEFAULT_IN_MEMORY);
            assert!(DEFAULT_TEMP_DIR.is_none());
        };

        let temp_dir = test_dir("defaults");
        let resolver = CustomLinkResolver::from_base(TestBase {
            temp_dir: temp_dir.clone(),
            in_memory: false,
        });
        assert_eq!(resolver.default_format(), FileFormat::V2000);
        assert!(!resolver.is_default_in_memory());
        assert_eq!(resolver.default_temp_directory(), Some(temp_dir));
    }

    #[test]
    fn map_resolver_resolves_relative_absolute_and_different_case_names() {
        let fixture = fixture("map");
        let old_abs_target = fixture.target.clone();
        let (moved_target, _) = move_targets(&fixture);
        assert!(!fixture.target.exists());

        let mut mapping = BTreeMap::new();
        // relative mapping value, resolved against the base directory; the
        // lower-case key also serves the upper-case stored CaseLink name
        mapping.insert("target.mdb".to_string(), PathBuf::from("moved/target.mdb"));
        // absolute mapping key and value
        mapping.insert(old_abs_target.to_string_lossy().into_owned(), moved_target);
        let resolver = CustomLinkResolver::from_map_with_base_dir(fixture.dir.clone(), mapping);

        let mut db = Database::open_path(&fixture.linker).unwrap();
        db.set_link_resolver(resolver);
        for link in ["RelLink", "AbsLink", "CaseLink"] {
            let rows = read_link_rows(&mut db, link);
            assert_source_rows(&rows);
        }
        // each stored linkee name was resolved (and cached) separately
        assert_eq!(db.get_linked_databases().len(), 3);
    }

    #[test]
    fn loader_callback_resolves_and_a_missing_loader_falls_back() {
        let fixture = fixture("loader");
        let (_, moved_loader) = move_targets(&fixture);
        let loader_name = fixture.loader_rel.clone();
        let moved_loader_for_closure = moved_loader.clone();
        let resolver = CustomLinkResolver::from_loader(move |_linker, name| {
            if name == loader_name {
                Ok(Some(moved_loader_for_closure.clone()))
            } else {
                Ok(None)
            }
        });

        let mut db = Database::open_path(&fixture.linker).unwrap();
        db.set_link_resolver(resolver);
        let rows = read_link_rows(&mut db, "LoaderLink");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            vec![
                Value::Long(7),
                Value::Text("delta".to_string()),
                Value::Long(70)
            ]
        );
        assert_eq!(
            rows[1],
            vec![
                Value::Long(8),
                Value::Text("epsilon".to_string()),
                Value::Long(80)
            ]
        );

        // the loader returns None for this linkee -> LinkResolver.DEFAULT
        db.set_link_resolver(CustomLinkResolver::from_loader(|_, _| Ok(None)));
        match db.get_table("RelLink") {
            Err(Error::Jackcess(message)) => {
                assert_eq!(message, denied_resolution_message(&fixture.target_rel));
            }
            Ok(_) => panic!("a missing loader mapping must deny the link"),
            Err(error) => panic!("unexpected error: {}", error),
        }
    }

    #[test]
    fn a_missing_mapping_uses_the_default_denial_message() {
        let fixture = fixture("missingmap");
        let mut db = Database::open_path(&fixture.linker).unwrap();
        db.set_link_resolver(CustomLinkResolver::from_map(Vec::<(String, PathBuf)>::new()));
        match db.get_table("RelLink") {
            Err(Error::Jackcess(message)) => {
                assert_eq!(message, denied_resolution_message(&fixture.target_rel));
                assert!(message.starts_with(&fixture.target_rel));
                assert!(message.contains("LinkResolver.UNRESTRICTED"));
            }
            Ok(_) => panic!("a missing mapping must deny the link"),
            Err(error) => panic!("unexpected error: {}", error),
        }
        assert!(db.get_linked_databases().is_empty());
    }

    #[test]
    fn a_mapping_to_a_missing_file_reports_an_io_error() {
        let fixture = fixture("missingfile");
        let missing = fixture.dir.join("does_not_exist.mdb");
        let resolver = CustomLinkResolver::from_map([(fixture.target_rel.clone(), missing)]);
        let mut db = Database::open_path(&fixture.linker).unwrap();
        db.set_link_resolver(resolver);
        match db.get_table("RelLink") {
            Err(Error::Io(error)) => {
                assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            }
            Ok(_) => panic!("a missing mapped file must fail"),
            Err(error) => panic!("unexpected error: {}", error),
        }
        assert!(db.get_linked_databases().is_empty());
    }

    #[test]
    fn a_read_only_linker_opens_the_mapped_database_read_only() {
        let fixture = fixture("readonly");
        let (moved_target, _) = move_targets(&fixture);
        let resolver = CustomLinkResolver::from_map([(fixture.target_rel.clone(), moved_target)]);
        let mut db = Database::open_path_read_only(&fixture.linker).unwrap();
        db.set_link_resolver(resolver);
        {
            let table = db.get_table("RelLink").unwrap();
            assert_eq!(table.name, "Source");
        }
        let linked = db
            .get_linked_database(&fixture.target_rel)
            .expect("the linkee is cached");
        assert!(linked.is_read_only());
    }

    /// Java's `CustomLinkResolverTest.TestLinkResolver` shape: two abstract
    /// methods, in-memory temporary database, missing custom tables allowed.
    struct TestBase {
        temp_dir: PathBuf,
        in_memory: bool,
    }

    impl CustomLinkResolverBase for TestBase {
        type CustomFile = String;

        fn is_default_in_memory(&self) -> bool {
            self.in_memory
        }

        fn default_temp_directory(&self) -> Option<PathBuf> {
            Some(self.temp_dir.clone())
        }

        fn load_custom_file(
            &self,
            _linker: &Database,
            linkee_file_name: &str,
        ) -> Result<Option<Self::CustomFile>> {
            if linkee_file_name == "custom_source.txt" {
                Ok(Some(linkee_file_name.to_string()))
            } else {
                Ok(None)
            }
        }

        fn load_custom_table(
            &self,
            temp_db: &mut Database,
            custom_file: &Self::CustomFile,
            table_name: &str,
        ) -> Result<bool> {
            if table_name != "Custom1" || custom_file != "custom_source.txt" {
                return Ok(false);
            }
            temp_db.create_table(&TableSpec::new(
                "Custom1",
                vec![
                    ColumnSpec::new("id", DataType::Long),
                    ColumnSpec::new("data1", DataType::Text).with_length(50),
                ],
            ))?;
            for i in 0..3i32 {
                temp_db.add_rows(
                    "Custom1",
                    &[vec![Value::Long(i), Value::Text(format!("row{}", i))]],
                )?;
            }
            Ok(true)
        }
    }

    fn add_custom_links(linker_path: &Path) {
        let mut linker = Database::open_path(linker_path).unwrap();
        linker
            .create_linked_table("CustomLink", "custom_source.txt", "Custom1")
            .unwrap();
        linker
            .create_linked_table("CustomMissingTable", "custom_source.txt", "MissingTable")
            .unwrap();
        linker
            .create_linked_table("CustomFallback", "unknown_source.txt", "X")
            .unwrap();
        linker.flush().unwrap();
    }

    #[test]
    fn custom_base_loads_tables_and_reports_missing_tables() {
        let fixture = fixture("custombase");
        add_custom_links(&fixture.linker);
        let temp_dir = test_dir("custombase_temp");
        let resolver = CustomLinkResolver::from_base(TestBase {
            temp_dir: temp_dir.clone(),
            in_memory: true,
        });
        let mut db = Database::open_path(&fixture.linker).unwrap();
        db.set_link_resolver(resolver);

        let rows = read_link_rows(&mut db, "CustomLink");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            vec![Value::Long(0), Value::Text("row0".to_string())]
        );
        assert_eq!(
            rows[1],
            vec![Value::Long(1), Value::Text("row1".to_string())]
        );
        assert_eq!(
            rows[2],
            vec![Value::Long(2), Value::Text("row2".to_string())]
        );

        // `load_custom_table` returned false: the table is absent (Java's
        // getTable would return null; the port's non-Option API is find_table)
        assert!(db.find_table("CustomMissingTable").unwrap().is_none());

        // no custom file -> the default resolver denies the link
        match db.get_table("CustomFallback") {
            Err(Error::Jackcess(message)) => {
                assert_eq!(message, denied_resolution_message("unknown_source.txt"));
            }
            Ok(_) => panic!("an unknown custom file must be denied"),
            Err(error) => panic!("unexpected error: {}", error),
        }

        // the in-memory temporary database never leaves a file behind
        let temp_name = db
            .get_linked_database("custom_source.txt")
            .expect("the temporary database is cached")
            .name
            .clone();
        assert!(temp_name.starts_with("memdb_"), "name = {}", temp_name);
        assert!(!temp_dir.join(temp_name).exists());
    }

    #[test]
    fn file_temp_database_file_is_deleted_when_the_database_drops() {
        let fixture = fixture("tempfile");
        add_custom_links(&fixture.linker);
        let temp_dir = test_dir("tempfile_store");
        let resolver = CustomLinkResolver::from_base(TestBase {
            temp_dir: temp_dir.clone(),
            in_memory: false,
        });
        let mut db = Database::open_path(&fixture.linker).unwrap();
        db.set_link_resolver(resolver);
        {
            let table = db.get_table("CustomLink").unwrap();
            assert_eq!(table.name, "Custom1");
        }
        let temp_name = db
            .get_linked_database("custom_source.txt")
            .expect("the temporary database is cached")
            .name
            .clone();
        assert!(temp_name.starts_with("linkeddb_"), "name = {}", temp_name);
        let temp_path = temp_dir.join(&temp_name);
        assert!(temp_path.exists(), "temporary database file is missing");
        drop(db);
        assert!(
            !temp_path.exists(),
            "temporary database file was not deleted on drop"
        );
    }
}
