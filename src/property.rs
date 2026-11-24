//! Port of the Jackcess property map subsystem (`PropertyMap`,
//! `PropertyMaps`, `PropertyMapImpl` and the property block format of
//! `PropertyMaps.Handler`).
//!
//! Property maps are the serialized property blobs which Access stores as the
//! `LvProp` (Jet4/Ace) or `Lv` (Jet3) value of a catalog row (`MSysObjects`).
//! A blob starts with a four byte type tag, `"MR2\0"` for Jet4/Ace and
//! `"KKD\0"` for Jet3 (see [`crate::format::PROPERTY_MAP_TYPES`]), followed by
//! a sequence of length prefixed "chunks":
//!
//! ```text
//! chunk      := len:i32 type:i16 <len-6 bytes of payload>
//! type 0x80  := property name list: { nameLen:i16 <utf16/ansi name> }*
//! type 0x00  := values of the object level ("default") map
//! type 0x01  := values of a column map
//! type 0x02  := values of an index map
//! values     := nameBlock  { valueEntry }*
//! nameBlock  := len:i32 ( nameLen:i16 <name> )?    -- len 6 == empty name
//! valueEntry := len:i16 flags:u8 type:u8 nameIdx:i16 dataLen:i16 <data>
//! ```
//!
//! The property names of a blob are stored once, in the name list chunk; the
//! value entries refer to them by index.  A blob may hold several maps: the
//! object level map (whose name is the empty string) plus one map per column
//! or index, named after that column/index.
//!
//! [`PropertyMap`] models a single map (the required, simple API); the whole
//! blob, including the column/index grouping, is modelled by [`PropertyMaps`].
//! A [`PropertyMap`] read from a blob which holds several maps is the object
//! level map, which is where table level properties such as `Description`
//! live.
//!
//! Property values are encoded with the same per-`DataType` encodings which
//! `ColumnImpl` uses for ordinary column values, except that property values
//! are always inline (there are no long value pages) and text property values
//! are never Unicode compressed.  See `PropertyMapImpl`'s "PropColumn" in
//! the Java library for the details.
//!
//! # Usage
//!
//! ```ignore
//! // read the object level properties of a catalog row
//! let props = PropertyMap::read(lv_prop_bytes, &format)?;
//! let description = props.get_trimmed_string(DESCRIPTION_PROP);
//!
//! // read/update every map of a row (object, column and index properties)
//! let mut maps = PropertyMaps::read(lv_prop_bytes, &format)?;
//! maps.get_default().put_value(DESCRIPTION_PROP, Value::Text("a table".into()))?;
//! maps.get_or_insert("Name", MAP_TYPE_COLUMN)
//!     .put_value(FORMAT_PROP, Value::Text("@".into()))?;
//! let new_bytes = maps.write(&format)?; // or store it with `maps.save(&mut db)?`
//! ```
//!
//! # Conventions and limitations
//!
//! * The flag byte of a property (the DDL and skip-handler bits, and whatever
//!   else Access stored there) is kept by the owning [`PropertyMap`], see
//!   [`PropertyMap::flags`].  [`PropertyMap::put`] derives it from the property
//!   name the way `PropertyMapImpl.put(String, DataType, Object)` does, while
//!   [`PropertyMap::put_stored`] keeps a byte exactly as read from a file.
//! * Date values use the "local date double" convention of
//!   [`crate::datenums`] (the same convention as `column.rs`), converted
//!   through the database's [`DateContext`] exactly like Java's property
//!   column (a `PropertyMaps.PropColumn` asks its `DatabaseImpl` for the
//!   `DateTimeFactory`, so a `DATE` database reads a date property as the
//!   `DateExt` analogue [`crate::value::Value::DateBits`] and writes it through
//!   `DefaultDateTimeFactory.toDateDouble`).  The context taking methods are
//!   [`PropertyMap::read_with_context`] / [`PropertyMap::write_with_context`]
//!   (and the [`PropertyMaps`] equivalents); the older methods keep the
//!   jackcess defaults (`LOCAL_DATE_TIME` and the default zone).  A numeric
//!   value written into a date property is an epoch millisecond count, like
//!   `ColumnImpl.toDateLong`.
//! * `NUMERIC` values are held as an [`crate::value::Value::Decimal`] with an
//!   `i128` unscaled value (the Java library uses an arbitrary precision
//!   `BigInteger`; a value which does not fit in an `i128` is rejected) and the
//!   default property precision of 18 digits is enforced, as the Java
//!   "PropColumn" does.  `MONEY` values are scaled to 4 decimal places and must
//!   fit in an `i64`.
//! * `TEXT` property values are limited to the data type's maximum length
//!   (255 characters for a Jet4 database, 510 for Jet3, as the Java property
//!   column sees it) and `MEMO` values are unlimited, as in the Java library.
//! * Binary property values must be [`crate::value::Value::Bytes`]; the Java
//!   library can also serialize arbitrary objects into a binary property,
//!   which this port does not attempt.  `BIG_BINARY` property values are
//!   passed through as raw bytes (the Java code writes such a value as a bare
//!   int, which no real database contains).
//! * `UNSUPPORTED_FIXEDLEN`/`UNSUPPORTED_VARLEN` property values are passed
//!   through as raw bytes; the Java library raises an exception for them.
//! * [`PropertyMaps::save`] writes a group back into the database through the
//!   catalog row machinery of [`crate::database::Database`] (the port of
//!   `PropertyMaps.Handler.save` + `DatabaseImpl`'s catalog plumbing); the
//!   blob encoding itself is available without a database through
//!   [`PropertyMaps::write`].
//! * The `DisplayControl`/`TextFormat`/`IMEMode`/`IMESentenceMode` value tables
//!   are provided as data only (`DISPLAY_CONTROL_VALUES` and friends); there is
//!   no Rust equivalent of the Java `EnumValue` interface.

use crate::bytebuf::ByteBuf;
use crate::column::DateContext;
use crate::datatype::DataType;
use crate::datenums::{self, LocalDateTime};
use crate::error::{Error, Result};
use crate::format::{Charset, JetFormat, PROPERTY_MAP_TYPES};
use crate::value::Value;

// ---------------------------------------------------------------------------
// Property name constants (`PropertyMap`)
// ---------------------------------------------------------------------------

// database and summary properties
pub const ACCESS_VERSION_PROP: &str = "AccessVersion";
pub const BUILD_PROP: &str = "Build";
pub const PROJ_VER_PROP: &str = "ProjVer";
pub const HAS_OFFLINE_LISTS_PROP: &str = "HasOfflineLists";
pub const ANSI_QUERY_MODE_PROP: &str = "ANSI Query Mode";
pub const AUTO_COMPACT_PROP: &str = "Auto Compact";
pub const TRACK_NAME_AUTO_CORRECT_PROP: &str = "Track Name AutoCorrect Info";
pub const PERFORM_NAME_AUTO_CORRECT_PROP: &str = "Perform Name AutoCorrect";
pub const CHECK_TRUNCATED_NUM_FIELDS_PROP: &str = "CheckTruncatedNumFields";
pub const PICTURE_STORAGE_FORMAT_PROP: &str = "Picture Property Storage Format";
pub const THEME_RESOURCE_NAME_PROP: &str = "Theme Resource Name";
pub const THEMED_FORM_CONTROLS_PROP: &str = "Themed Form Controls";
pub const SHOW_DOCUMENT_TABS_PROP: &str = "ShowDocumentTabs";
pub const USE_MDI_MODE_PROP: &str = "UseMDIMode";
pub const WEB_DESIGN_MODE_PROP: &str = "WebDesignMode";
pub const DESIGN_WITH_DATA_PROP: &str = "DesignWithData";
pub const ALLOW_BUILT_IN_TOOLBARS_PROP: &str = "AllowBuiltInToolbars";
pub const ALLOW_DATASHEET_SCHEMA_PROP: &str = "AllowDatasheetSchema";
pub const ALLOW_FULL_MENUS_PROP: &str = "AllowFullMenus";
pub const ALLOW_SHORTCUT_MENUS_PROP: &str = "AllowShortcutMenus";
pub const ALLOW_SPECIAL_KEYS_PROP: &str = "AllowSpecialKeys";
pub const ALLOW_TOOLBAR_CHANGES_PROP: &str = "AllowToolbarChanges";
pub const START_UP_SHOW_DB_WINDOW_PROP: &str = "StartUpShowDBWindow";
pub const START_UP_SHOW_STATUS_BAR_PROP: &str = "StartUpShowStatusBar";
pub const USE_APP_ICON_FOR_FRM_RPT_PROP: &str = "UseAppIconForFrmRpt";
pub const NAV_PANE_CATEGORY_PROP: &str = "NavPane Category";
pub const NAV_PANE_CLOSED_PROP: &str = "NavPane Closed";
pub const NAV_PANE_SORT_BY_PROP: &str = "NavPane Sort By";
pub const NAV_PANE_VIEW_BY_PROP: &str = "NavPane View By";
pub const NAV_PANE_WIDTH_PROP: &str = "NavPane Width";
pub const NAV_PANE_SEARCH_BAR_PROP: &str = "Show Navigation Pane Search Bar";
pub const SHOW_VALUES_LIMIT_PROP: &str = "Show Values Limit";
pub const SHOW_VALUES_IN_INDEXED_PROP: &str = "Show Values in Indexed";
pub const SHOW_VALUES_IN_NON_INDEXED_PROP: &str = "Show Values in Non-Indexed";
pub const SHOW_VALUES_IN_REMOTE_PROP: &str = "Show Values in Remote";
pub const REPLICATE_PROJECT_PROP: &str = "ReplicateProject";
pub const TITLE_PROP: &str = "Title";
pub const AUTHOR_PROP: &str = "Author";
pub const COMPANY_PROP: &str = "Company";

// column properties
pub const DEFAULT_VALUE_PROP: &str = "DefaultValue";
pub const REQUIRED_PROP: &str = "Required";
pub const ALLOW_ZERO_LEN_PROP: &str = "AllowZeroLength";
pub const DECIMAL_PLACES_PROP: &str = "DecimalPlaces";
pub const FORMAT_PROP: &str = "Format";
pub const INPUT_MASK_PROP: &str = "InputMask";
pub const CAPTION_PROP: &str = "Caption";
pub const VALIDATION_RULE_PROP: &str = "ValidationRule";
pub const VALIDATION_TEXT_PROP: &str = "ValidationText";
pub const GUID_PROP: &str = "GUID";
pub const DESCRIPTION_PROP: &str = "Description";
pub const RESULT_TYPE_PROP: &str = "ResultType";
pub const EXPRESSION_PROP: &str = "Expression";
pub const ALLOW_MULTI_VALUE_PROP: &str = "AllowMultipleValues";
pub const ROW_SOURCE_TYPE_PROP: &str = "RowSourceType";
pub const ROW_SOURCE_PROP: &str = "RowSource";
pub const BOUND_COLUMN_PROP: &str = "BoundColumn";
pub const COLUMN_COUNT_PROP: &str = "ColumnCount";
pub const COLUMN_WIDTHS_PROP: &str = "ColumnWidths";
pub const LIMIT_TO_LIST_PROP: &str = "LimitToList";
pub const DISPLAY_CONTROL_PROP: &str = "DisplayControl";
pub const TEXT_FORMAT_PROP: &str = "TextFormat";
pub const IME_MODE_PROP: &str = "IMEMode";
pub const IME_SENTENCE_MODE_PROP: &str = "IMESentenceMode";
pub const UNICODE_COMPRESSION_PROP: &str = "UnicodeCompression";
pub const APPEND_ONLY_PROP: &str = "AppendOnly";
pub const TEXT_ALIGN_PROP: &str = "TextAlign";
pub const AGGREGATE_TYPE_PROP: &str = "AggregateType";
pub const CURRENCY_LCID_PROP: &str = "CurrencyLCID";
pub const COLUMN_HIDDEN_PROP: &str = "ColumnHidden";
pub const COLUMN_ORDER_PROP: &str = "ColumnOrder";
pub const COLUMN_WIDTH_PROP: &str = "ColumnWidth";

// table and query properties
pub const NAME_MAP_PROP: &str = "NameMap";
pub const DEFAULT_VIEW_PROP: &str = "DefaultView";
pub const ORIENTATION_PROP: &str = "Orientation";
pub const ORDER_BY_ON_PROP: &str = "OrderByOn";
pub const ORDER_BY_ON_LOAD_PROP: &str = "OrderByOnLoad";
pub const FILTER_ON_LOAD_PROP: &str = "FilterOnLoad";
pub const TOTALS_ROW_PROP: &str = "TotalsRow";
pub const HIDE_NEW_FIELD_PROP: &str = "HideNewField";
pub const PUBLISH_TO_WEB_PROP: &str = "PublishToWeb";
pub const READ_ONLY_WHEN_DISCONNECTED_PROP: &str = "ReadOnlyWhenDisconnected";
pub const DISPLAY_VIEWS_ON_SHAREPOINT_PROP: &str = "DisplayViewsOnSharePointSite";
pub const BACK_TINT_PROP: &str = "BackTint";
pub const BACK_SHADE_PROP: &str = "BackShade";
pub const ALTERNATE_BACK_TINT_PROP: &str = "AlternateBackTint";
pub const ALTERNATE_BACK_SHADE_PROP: &str = "AlternateBackShade";
pub const ALTERNATE_BACK_THEME_COLOR_INDEX_PROP: &str = "AlternateBackThemeColorIndex";
pub const DATASHEET_FORE_THEME_COLOR_INDEX_PROP: &str = "DatasheetForeThemeColorIndex";
pub const DATASHEET_GRIDLINES_THEME_COLOR_INDEX_PROP: &str = "DatasheetGridlinesThemeColorIndex";
pub const THEME_FONT_INDEX_PROP: &str = "ThemeFontIndex";

pub const QUERY_NAME_MAP_PROP: &str = "DOL";
pub const ODBC_TIMEOUT_PROP: &str = "ODBCTimeout";
pub const RECORD_LOCKS_PROP: &str = "RecordLocks";
pub const RECORDSET_TYPE_PROP: &str = "RecordsetType";
pub const USE_TRANSACTION_PROP: &str = "UseTransaction";
pub const FAIL_ON_ERROR_PROP: &str = "FailOnError";
pub const LOG_MESSAGES_PROP: &str = "LogMessages";

// ---------------------------------------------------------------------------
// `PropertyMapImpl` constants
// ---------------------------------------------------------------------------

/// Property flag bit for the DDL argument of the DAO `CreateProperty` method.
/// A user cannot change or delete a DDL property without `dbSecWriteDef`
/// permission.
pub const DDL_FLAG: u8 = 0x01;
/// Property flag bit which tells MS Access to store the value without running
/// the handler bound to the property name, so that writing the property
/// records a state instead of bringing it about.  MS Access sets it on its own
/// account and no DAO call produces it.
pub const SKIP_HANDLER_FLAG: u8 = 0x80;

// ---------------------------------------------------------------------------
// `PropertyMaps` constants
// ---------------------------------------------------------------------------

/// The name of the "default" (object level) properties of a [`PropertyMaps`].
pub const DEFAULT_NAME: &str = "";
/// Block type of the object level ("default") property values.
pub const MAP_TYPE_DEFAULT: i16 = 0x00;
/// Block type of a column's property values.
pub const MAP_TYPE_COLUMN: i16 = 0x01;
/// Block type of an index's property values.
pub const MAP_TYPE_INDEX: i16 = 0x02;
/// Block type of the property name list.
const PROPERTY_NAME_LIST: i16 = 0x80;
/// The length of a value entry without its data.
const VALUE_HEADER_LENGTH: usize = 8;
/// The longest value entry.  Its length is a short, which is read as signed.
const MAX_VALUE_LENGTH: usize = i16::MAX as usize;

/// The type and DDL flag MS Access uses for the common properties, i.e.
/// `PropertyMapImpl.DEFAULT_TYPES`.
static DEFAULT_TYPES: &[(&str, DataType, bool)] = &[
    // database and summary properties
    (ACCESS_VERSION_PROP, DataType::Text, false),
    (BUILD_PROP, DataType::Long, false),
    (PROJ_VER_PROP, DataType::Int, false),
    (HAS_OFFLINE_LISTS_PROP, DataType::Int, false),
    (ANSI_QUERY_MODE_PROP, DataType::Long, true),
    (AUTO_COMPACT_PROP, DataType::Long, true),
    (TRACK_NAME_AUTO_CORRECT_PROP, DataType::Long, true),
    (PERFORM_NAME_AUTO_CORRECT_PROP, DataType::Long, true),
    (CHECK_TRUNCATED_NUM_FIELDS_PROP, DataType::Long, true),
    (PICTURE_STORAGE_FORMAT_PROP, DataType::Long, true),
    (THEME_RESOURCE_NAME_PROP, DataType::Text, true),
    (THEMED_FORM_CONTROLS_PROP, DataType::Long, false),
    (SHOW_DOCUMENT_TABS_PROP, DataType::Boolean, true),
    (USE_MDI_MODE_PROP, DataType::Byte, true),
    (WEB_DESIGN_MODE_PROP, DataType::Byte, true),
    (DESIGN_WITH_DATA_PROP, DataType::Boolean, true),
    (ALLOW_BUILT_IN_TOOLBARS_PROP, DataType::Boolean, true),
    (ALLOW_DATASHEET_SCHEMA_PROP, DataType::Boolean, true),
    (ALLOW_FULL_MENUS_PROP, DataType::Boolean, true),
    (ALLOW_SHORTCUT_MENUS_PROP, DataType::Boolean, true),
    (ALLOW_SPECIAL_KEYS_PROP, DataType::Boolean, true),
    (ALLOW_TOOLBAR_CHANGES_PROP, DataType::Boolean, true),
    (START_UP_SHOW_DB_WINDOW_PROP, DataType::Boolean, true),
    (START_UP_SHOW_STATUS_BAR_PROP, DataType::Boolean, true),
    (USE_APP_ICON_FOR_FRM_RPT_PROP, DataType::Boolean, true),
    (NAV_PANE_CATEGORY_PROP, DataType::Long, true),
    (NAV_PANE_CLOSED_PROP, DataType::Long, true),
    (NAV_PANE_SORT_BY_PROP, DataType::Long, true),
    (NAV_PANE_VIEW_BY_PROP, DataType::Long, true),
    (NAV_PANE_WIDTH_PROP, DataType::Long, true),
    (NAV_PANE_SEARCH_BAR_PROP, DataType::Long, true),
    (SHOW_VALUES_LIMIT_PROP, DataType::Long, true),
    (SHOW_VALUES_IN_INDEXED_PROP, DataType::Long, true),
    (SHOW_VALUES_IN_NON_INDEXED_PROP, DataType::Long, true),
    (SHOW_VALUES_IN_REMOTE_PROP, DataType::Long, true),
    (REPLICATE_PROJECT_PROP, DataType::Boolean, false),
    (TITLE_PROP, DataType::Text, false),
    (AUTHOR_PROP, DataType::Text, false),
    (COMPANY_PROP, DataType::Text, false),
    // column properties
    (DEFAULT_VALUE_PROP, DataType::Memo, true),
    (REQUIRED_PROP, DataType::Boolean, true),
    (ALLOW_ZERO_LEN_PROP, DataType::Boolean, true),
    (DECIMAL_PLACES_PROP, DataType::Byte, true),
    (FORMAT_PROP, DataType::Text, true),
    (INPUT_MASK_PROP, DataType::Text, true),
    (CAPTION_PROP, DataType::Memo, true),
    (VALIDATION_RULE_PROP, DataType::Text, true),
    (VALIDATION_TEXT_PROP, DataType::Text, true),
    (GUID_PROP, DataType::Binary, true),
    (DESCRIPTION_PROP, DataType::Text, true),
    (RESULT_TYPE_PROP, DataType::Byte, true),
    (EXPRESSION_PROP, DataType::Memo, true),
    (ALLOW_MULTI_VALUE_PROP, DataType::Boolean, true),
    (ROW_SOURCE_TYPE_PROP, DataType::Text, true),
    (ROW_SOURCE_PROP, DataType::Memo, true),
    (BOUND_COLUMN_PROP, DataType::Int, true),
    (COLUMN_COUNT_PROP, DataType::Int, true),
    (COLUMN_WIDTHS_PROP, DataType::Text, true),
    (LIMIT_TO_LIST_PROP, DataType::Boolean, true),
    (DISPLAY_CONTROL_PROP, DataType::Int, true),
    (TEXT_FORMAT_PROP, DataType::Byte, true),
    (IME_MODE_PROP, DataType::Byte, true),
    (IME_SENTENCE_MODE_PROP, DataType::Byte, true),
    (UNICODE_COMPRESSION_PROP, DataType::Boolean, true),
    (APPEND_ONLY_PROP, DataType::Boolean, true),
    (TEXT_ALIGN_PROP, DataType::Byte, false),
    (AGGREGATE_TYPE_PROP, DataType::Long, false),
    (CURRENCY_LCID_PROP, DataType::Long, false),
    (COLUMN_HIDDEN_PROP, DataType::Boolean, false),
    (COLUMN_ORDER_PROP, DataType::Int, false),
    (COLUMN_WIDTH_PROP, DataType::Int, false),
    // table and query properties
    (NAME_MAP_PROP, DataType::Ole, false),
    (DEFAULT_VIEW_PROP, DataType::Byte, false),
    (ORIENTATION_PROP, DataType::Byte, true),
    (ORDER_BY_ON_PROP, DataType::Boolean, false),
    (ORDER_BY_ON_LOAD_PROP, DataType::Boolean, false),
    (FILTER_ON_LOAD_PROP, DataType::Boolean, false),
    (TOTALS_ROW_PROP, DataType::Boolean, false),
    (HIDE_NEW_FIELD_PROP, DataType::Boolean, false),
    (PUBLISH_TO_WEB_PROP, DataType::Byte, false),
    (READ_ONLY_WHEN_DISCONNECTED_PROP, DataType::Boolean, false),
    (DISPLAY_VIEWS_ON_SHAREPOINT_PROP, DataType::Byte, false),
    (BACK_TINT_PROP, DataType::Float, false),
    (BACK_SHADE_PROP, DataType::Float, false),
    (ALTERNATE_BACK_TINT_PROP, DataType::Float, false),
    (ALTERNATE_BACK_SHADE_PROP, DataType::Float, false),
    (ALTERNATE_BACK_THEME_COLOR_INDEX_PROP, DataType::Long, false),
    (DATASHEET_FORE_THEME_COLOR_INDEX_PROP, DataType::Long, false),
    (
        DATASHEET_GRIDLINES_THEME_COLOR_INDEX_PROP,
        DataType::Long,
        false,
    ),
    (THEME_FONT_INDEX_PROP, DataType::Long, false),
    (QUERY_NAME_MAP_PROP, DataType::Ole, false),
    (ODBC_TIMEOUT_PROP, DataType::Int, true),
    (RECORD_LOCKS_PROP, DataType::Byte, true),
    (RECORDSET_TYPE_PROP, DataType::Byte, true),
    (USE_TRANSACTION_PROP, DataType::Boolean, true),
    (FAIL_ON_ERROR_PROP, DataType::Boolean, true),
    (LOG_MESSAGES_PROP, DataType::Boolean, true),
];

/// Values of `PropertyMap.DisplayControl`.
pub static DISPLAY_CONTROL_VALUES: &[(&str, i16)] = &[
    ("BOUND_OBJECT_FRAME", 108),
    ("CHECK_BOX", 106),
    ("COMBO_BOX", 111),
    ("COMMAND_BUTTON", 104),
    ("CUSTOM_CONTROL", 119),
    ("EMPTY_CELL", 127),
    ("IMAGE", 103),
    ("LABEL", 100),
    ("LINE", 102),
    ("LIST_BOX", 110),
    ("NAVIGATION_BUTTON", 130),
    ("NAVIGATION_CONTROL", 129),
    ("OBJECT_FRAME", 114),
    ("OPTION_BUTTON", 105),
    ("OPTION_GROUP", 107),
    ("PAGE", 124),
    ("PAGE_BREAK", 118),
    ("RECTANGLE", 101),
    ("SUB_FORM", 112),
    ("TAB_CTL", 123),
    ("TEXT_BOX", 109),
    ("TOGGLE_BUTTON", 122),
    ("WEB_BROWSER", 128),
];

/// Values of `PropertyMap.TextFormat`.
pub static TEXT_FORMAT_VALUES: &[(&str, i16)] = &[("HTMLRICHTEXT", 1), ("PLAIN", 0)];

/// Values of `PropertyMap.IMEMode`.
pub static IME_MODE_VALUES: &[(&str, i16)] = &[
    ("NOCONTROL", 0),
    ("ON", 1),
    ("OFF", 2),
    ("DISABLE", 3),
    ("HIRAGANA", 4),
    ("KATAKANA", 5),
    ("KATAKANAHALF", 6),
    ("ALPHAFULL", 7),
    ("ALPHA", 8),
    ("HANGULFULL", 9),
    ("HANGUL", 10),
];

/// Values of `PropertyMap.IMESentenceMode`.
pub static IME_SENTENCE_MODE_VALUES: &[(&str, i16)] =
    &[("NORMAL", 0), ("PLURAL", 1), ("SPEAKING", 2), ("NONE", 3)];

/// `PropertyMapImpl.DEFAULT_TYPES` lookup for a property name.
fn default_def(name: &str) -> Option<&'static (&'static str, DataType, bool)> {
    DEFAULT_TYPES.iter().find(|(n, _, _)| *n == name)
}

/// `DatabaseImpl.toLookupName`: property names are matched case insensitively.
fn lookup_name(name: &str) -> String {
    name.to_uppercase()
}

/// Info about a property defined in a `PropertyMap`, i.e.
/// `PropertyMap.Property` / `PropertyMapImpl.PropertyImpl`.
///
/// As in the Java library the value is mutable, and the DDL/skip-handler flag
/// byte is tracked separately by the [`PropertyMap`] which owns the property
/// (see [`PropertyMap::flags`]), so that a property parsed from a database can
/// be written back with the exact flag byte it had.
#[derive(Clone, Debug, PartialEq)]
pub struct Property {
    pub name: String,
    pub data_type: DataType,
    pub value: Value,
}

impl Property {
    pub fn new(name: impl Into<String>, data_type: DataType, value: Value) -> Property {
        Property {
            name: name.into(),
            data_type,
            value,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn data_type(&self) -> DataType {
        self.data_type
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn set_value(&mut self, value: Value) {
        self.value = value;
    }
}

impl std::fmt::Display for Property {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}[{}]={}",
            self.name,
            self.data_type.get_name(),
            format_value(&self.value)
        )
    }
}

/// A property together with the flag byte exactly as it is stored in the file.
#[derive(Clone, Debug)]
struct Entry {
    prop: Property,
    flags: u8,
}

/// Map of properties for a database object, i.e. `PropertyMapImpl`.
///
/// The map keeps its properties in insertion order (Java's `LinkedHashMap`),
/// and property names are matched case insensitively.
#[derive(Clone, Debug, Default)]
pub struct PropertyMap {
    /// the name of this map (`PropertyMaps.DEFAULT_NAME` for object level
    /// properties)
    name: String,
    /// the block type of this map (`MAP_TYPE_DEFAULT`, `MAP_TYPE_COLUMN`,
    /// `MAP_TYPE_INDEX`)
    map_type: i16,
    props: Vec<Entry>,
}

impl PropertyMap {
    /// A new, empty object level property map.
    pub fn new() -> PropertyMap {
        PropertyMap::with_name(DEFAULT_NAME, MAP_TYPE_DEFAULT)
    }

    pub fn with_name(name: impl Into<String>, map_type: i16) -> PropertyMap {
        PropertyMap {
            name: name.into(),
            map_type,
            props: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    pub fn map_type(&self) -> i16 {
        self.map_type
    }

    pub fn set_map_type(&mut self, map_type: i16) {
        self.map_type = map_type;
    }

    /// Reads a property map from the bytes of a property blob.
    ///
    /// A blob may hold the properties of an object, its columns and its
    /// indexes (see [`PropertyMaps`]).  This returns the object level
    /// ("default") map when there is one, otherwise the first map in the
    /// blob.  Use [`PropertyMaps::read`] to get all of them.
    ///
    /// An empty blob (or one shorter than the four byte type tag) yields an
    /// empty map; a longer value whose tag is unknown is an error, exactly
    /// like the Java handler's `IOException`.  (Java throws an unchecked
    /// `IndexOutOfBoundsException` for the short value; see `read_maps`.)
    pub fn read(blob: &[u8], format: &JetFormat) -> Result<PropertyMap> {
        PropertyMap::read_with_charset(blob, format, default_charset(format))
    }

    /// [`PropertyMap::read`] with an explicit database charset (the Java
    /// library uses the charset of the database, which may come from the file
    /// header for Jet3).
    pub fn read_with_charset(
        blob: &[u8],
        format: &JetFormat,
        charset: Charset,
    ) -> Result<PropertyMap> {
        PropertyMap::read_with_context(blob, format, charset, DateContext::default())
    }

    /// [`PropertyMap::read_with_charset`] with the database's date/time
    /// conversion context, i.e. `PropertyMaps.Handler` with its `_database`:
    /// a `SHORT_DATE_TIME` property value is decoded with the configured
    /// `DateTimeType` (`PropertyMaps.PropColumn.getDatabase()
    /// .getDateTimeFactory().fromDateBits`).
    pub fn read_with_context(
        blob: &[u8],
        format: &JetFormat,
        charset: Charset,
        context: DateContext,
    ) -> Result<PropertyMap> {
        let maps = read_maps(blob, format, charset, context)?;
        if maps.is_empty() {
            return Ok(PropertyMap::new());
        }
        let idx = maps
            .iter()
            .position(|m| m.name == DEFAULT_NAME && m.map_type == MAP_TYPE_DEFAULT)
            .or_else(|| maps.iter().position(|m| m.name == DEFAULT_NAME))
            .unwrap_or(0);
        Ok(maps.into_iter().nth(idx).unwrap())
    }

    /// Writes this map as a property blob.
    ///
    /// An empty map writes no bytes at all (the Java library returns `null`
    /// for a blob with no property names).
    pub fn write(&self, format: &JetFormat) -> Result<Vec<u8>> {
        self.write_with_charset(format, default_charset(format))
    }

    pub fn write_with_charset(&self, format: &JetFormat, charset: Charset) -> Result<Vec<u8>> {
        self.write_with_context(format, charset, DateContext::default())
    }

    /// [`PropertyMap::write_with_charset`] with the database's date/time
    /// conversion context: a `SHORT_DATE_TIME` property value is encoded with
    /// the configured `DateTimeType`
    /// (`PropertyMaps.PropColumn.getDatabase().getDateTimeFactory()
    /// .toDateDouble`).
    pub fn write_with_context(
        &self,
        format: &JetFormat,
        charset: Charset,
        context: DateContext,
    ) -> Result<Vec<u8>> {
        write_maps(std::slice::from_ref(self), format, charset, context)
    }

    /// `PropertyMap.get`: the property with the given name, if any.
    pub fn get(&self, name: &str) -> Option<&Property> {
        let lookup = lookup_name(name);
        self.props
            .iter()
            .find(|e| lookup_name(&e.prop.name) == lookup)
            .map(|e| &e.prop)
    }

    /// The value of the property with the given name, if any.
    pub fn get_value(&self, name: &str) -> Option<&Value> {
        self.get(name).map(|p| &p.value)
    }

    /// The value of the property with the given name, if any, otherwise the
    /// given default value (`PropertyMap.getValue(name, defaultValue)`).
    pub fn get_value_or<'a>(&'a self, name: &str, default_value: &'a Value) -> &'a Value {
        match self.get(name) {
            Some(prop) if !prop.value.is_null() => &prop.value,
            _ => default_value,
        }
    }

    /// [`PropertyMap::get_value_or`] under the Java overload's descriptive name
    /// (`getValue(name, defaultValue)`).
    pub fn get_value_default<'a>(&'a self, name: &str, default_value: &'a Value) -> &'a Value {
        self.get_value_or(name, default_value)
    }

    /// The string value of the property with the given name, trimmed to a
    /// non-empty value (`PropertyMaps.getTrimmedStringProperty`).
    pub fn get_trimmed_string(&self, name: &str) -> Option<&str> {
        match self.get_value(name) {
            Some(Value::Text(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }
            _ => None,
        }
    }

    /// Puts a property into this map.
    ///
    /// The flag byte is derived from the property name as `PropertyMapImpl`
    /// does for `put(String, DataType, Object)`: known DDL properties get
    /// [`DDL_FLAG`], everything else gets `0`.
    pub fn put(&mut self, prop: Property) {
        self.put_with_flags(prop, 0);
    }

    /// Puts a property into this map with the given flag byte, to which the
    /// default DDL flag of a common property is added (`PropertyMapImpl.put`).
    pub fn put_with_flags(&mut self, prop: Property, flags: u8) {
        let mut flags = flags;
        if let Some((_, _, true)) = default_def(&prop.name) {
            flags |= DDL_FLAG;
        }
        self.insert(prop, flags);
    }

    /// Puts a property into this map with the type and flag byte exactly as
    /// they are stored in the file (`PropertyMapImpl.putStored`).
    pub fn put_stored(&mut self, prop: Property, flags: u8) {
        self.insert(prop, flags);
    }

    fn insert(&mut self, prop: Property, flags: u8) {
        let lookup = lookup_name(&prop.name);
        if let Some(entry) = self
            .props
            .iter_mut()
            .find(|e| lookup_name(&e.prop.name) == lookup)
        {
            // like LinkedHashMap.put, an existing property keeps its position
            entry.prop = prop;
            entry.flags = flags;
        } else {
            self.props.push(Entry { prop, flags });
        }
    }

    /// `PropertyMap.put(String name, Object value)`: creates (or updates) a
    /// property, determining its type from the name and value.
    pub fn put_value(&mut self, name: &str, value: Value) -> Result<()> {
        let data_type = match default_def(name) {
            Some((_, dt, _)) => *dt,
            None => infer_type(name, &value)?,
        };
        self.put(Property::new(name, data_type, value));
        Ok(())
    }

    /// `PropertyMap.put(String name, DataType type, Object value)`.
    pub fn put_value_typed(&mut self, name: &str, data_type: DataType, value: Value) {
        self.put(Property::new(name, data_type, value));
    }

    /// Alias of [`PropertyMap::put_value_typed`] using the Java overload's
    /// three argument shape.
    pub fn put_typed(&mut self, name: &str, data_type: DataType, value: Value) {
        self.put_value_typed(name, data_type, value);
    }

    /// `PropertyMap.put(String name, DataType type, Object value, boolean isDdl)`.
    pub fn put_typed_ddl(&mut self, name: &str, data_type: DataType, value: Value, is_ddl: bool) {
        self.put_with_flags(
            Property::new(name, data_type, value),
            Self::to_flags(is_ddl, false),
        );
    }

    /// `PropertyMap.put(String name, DataType type, Object value, boolean
    /// isDdl, boolean isSkipHandler)`.
    pub fn put_typed_ddl_skip_handler(
        &mut self,
        name: &str,
        data_type: DataType,
        value: Value,
        is_ddl: bool,
        is_skip_handler: bool,
    ) {
        self.put_with_flags(
            Property::new(name, data_type, value),
            Self::to_flags(is_ddl, is_skip_handler),
        );
    }

    /// `PropertyMapImpl.toFlags(boolean, boolean)`: the DDL and skip-handler
    /// bits of a property.
    pub fn to_flags(is_ddl: bool, is_skip_handler: bool) -> u8 {
        (if is_ddl { DDL_FLAG } else { 0 })
            | (if is_skip_handler {
                SKIP_HANDLER_FLAG
            } else {
                0
            })
    }

    /// Removes the property with the given name (`PropertyMap.remove`).
    pub fn remove(&mut self, name: &str) -> Option<Property> {
        let lookup = lookup_name(name);
        let idx = self
            .props
            .iter()
            .position(|e| lookup_name(&e.prop.name) == lookup)?;
        Some(self.props.remove(idx).prop)
    }

    /// The number of properties in this map (`PropertyMap.getSize`).
    pub fn len(&self) -> usize {
        self.props.len()
    }

    /// `PropertyMap.getSize()`.
    pub fn size(&self) -> usize {
        self.len()
    }

    pub fn is_empty(&self) -> bool {
        self.props.is_empty()
    }

    pub fn clear(&mut self) {
        self.props.clear();
    }

    /// The properties of this map, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Property> {
        self.props.iter().map(|e| &e.prop)
    }

    /// `PropertyMap.stream()`: the properties of this map as an iterator (the
    /// Rust equivalent of Java's `Stream<Property>`).
    pub fn stream(&self) -> impl Iterator<Item = &Property> {
        self.iter()
    }

    /// `PropertyMap.save()`: persists this map through its owning
    /// [`PropertyMaps`] group and the database.
    ///
    /// The Java implementation asks the map's owner to save itself
    /// (`PropertyMapImpl.getOwner().save()`); a Rust [`PropertyMap`] read or
    /// built on its own has no owner pointer, so the group is passed in.  The
    /// map is copied into the group (by name and block type) before saving.
    pub fn save(&self, maps: &mut PropertyMaps, db: &mut crate::database::Database) -> Result<()> {
        let target = maps.get_or_insert(&self.name, self.map_type);
        *target = self.clone();
        maps.save(db)
    }

    /// The property names of this map, in insertion order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.props.iter().map(|e| e.prop.name.as_str())
    }

    /// The flag byte of the property with the given name, exactly as stored in
    /// the file (`PropertyMapImpl.PropertyImpl.getFlags`).
    pub fn flags(&self, name: &str) -> Option<u8> {
        let lookup = lookup_name(name);
        self.props
            .iter()
            .find(|e| lookup_name(&e.prop.name) == lookup)
            .map(|e| e.flags)
    }

    /// Sets the flag byte of an existing property.
    pub fn set_flags(&mut self, name: &str, flags: u8) -> bool {
        let lookup = lookup_name(name);
        match self
            .props
            .iter_mut()
            .find(|e| lookup_name(&e.prop.name) == lookup)
        {
            Some(entry) => {
                entry.flags = flags;
                true
            }
            None => false,
        }
    }

    /// `PropertyMap.Property.isDdl` for the property with the given name.
    pub fn is_ddl(&self, name: &str) -> bool {
        self.flags(name)
            .map(|f| (f & DDL_FLAG) != 0)
            .unwrap_or(false)
    }

    /// `PropertyMap.Property.isSkipHandler` for the property with the given
    /// name.
    pub fn is_skip_handler(&self, name: &str) -> bool {
        self.flags(name)
            .map(|f| (f & SKIP_HANDLER_FLAG) != 0)
            .unwrap_or(false)
    }

    /// Adds all of the given properties (`PropertyMap.putAll`).
    pub fn put_all(&mut self, props: impl IntoIterator<Item = Property>) {
        for prop in props {
            self.put(prop);
        }
    }

    fn entry(&self, name: &str) -> Option<&Entry> {
        let lookup = lookup_name(name);
        self.props
            .iter()
            .find(|e| lookup_name(&e.prop.name) == lookup)
    }
}

impl<'a> IntoIterator for &'a PropertyMap {
    type Item = &'a Property;
    type IntoIter = PropertyIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        PropertyIter {
            inner: self.props.iter(),
        }
    }
}

/// Iterator over the properties of a [`PropertyMap`].
pub struct PropertyIter<'a> {
    inner: std::slice::Iter<'a, Entry>,
}

impl<'a> Iterator for PropertyIter<'a> {
    type Item = &'a Property;

    fn next(&mut self) -> Option<&'a Property> {
        self.inner.next().map(|e| &e.prop)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl std::fmt::Display for PropertyMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = if self.name.is_empty() {
            "<DEFAULT>"
        } else {
            &self.name
        };
        write!(f, "{} {{", name)?;
        for (i, entry) in self.props.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            let mut flags = String::new();
            if entry.flags & DDL_FLAG != 0 {
                flags.push_str(":ddl");
            }
            if entry.flags & SKIP_HANDLER_FLAG != 0 {
                flags.push_str(":nohandler");
            }
            write!(
                f,
                "{}[{}{}]={}",
                entry.prop.name,
                entry.prop.data_type.get_name(),
                flags,
                format_value(&entry.prop.value)
            )?;
        }
        write!(f, "}}")
    }
}

/// Collection of property maps read from a single property blob, i.e.
/// `PropertyMaps`: the object level ("default") map plus one map per column
/// and one map per index.
///
/// The maps are keyed by their name and block type, because an index is named
/// after its column by default, so a table can hold a column map and an index
/// map with the same name.
#[derive(Clone, Debug, Default)]
pub struct PropertyMaps {
    maps: Vec<PropertyMap>,
    /// whether the group was read from a blob which held no properties (a new
    /// table's property blob, or a value which is absent/zeroed).
    ///
    /// Java keeps one `PropertyMaps` instance per table and creates a map for
    /// every column of a new table before anything is stored, so the block
    /// order of a *first* write is the column creation order of `TableImpl`
    /// (calculated columns first, then the rest).  The port rebuilds the group
    /// from the blob, so it remembers that the blob was empty to reproduce that
    /// order on the first write (see `Database::write_property_maps`).
    read_from_empty: bool,
    /// the object id of the owning catalog row, when known
    object_id: i32,
    /// the row which holds the property blob in its `LvProp` column, when the
    /// owning catalog row is known (`PropertyMaps._rowId`)
    row_id: Option<crate::table::RowId>,
    /// for a database level property group whose catalog row does not exist,
    /// the name of the catalog object ("MSysDb", "SummaryInfo",
    /// "UserDefined").  Saving such a group creates the row first.
    db_object_name: Option<String>,
    /// for a group read as a table's property maps (`Table.getPropertyMaps`),
    /// the table definition page, so a save can refresh the table's column
    /// validators/defaults (`TableImpl.propertiesUpdated`)
    table_owner_page: Option<i32>,
}

impl PropertyMaps {
    pub fn new(object_id: i32) -> PropertyMaps {
        PropertyMaps {
            maps: Vec::new(),
            read_from_empty: true,
            object_id,
            row_id: None,
            db_object_name: None,
            table_owner_page: None,
        }
    }

    /// Reads all the property maps from the bytes of a property blob (always
    /// returns a result, which is empty for an empty blob).
    pub fn read(blob: &[u8], format: &JetFormat) -> Result<PropertyMaps> {
        PropertyMaps::read_with_charset(blob, format, default_charset(format))
    }

    pub fn read_with_charset(
        blob: &[u8],
        format: &JetFormat,
        charset: Charset,
    ) -> Result<PropertyMaps> {
        PropertyMaps::read_with_context(blob, format, charset, DateContext::default())
    }

    /// [`PropertyMaps::read_with_charset`] with the database's date/time
    /// conversion context, i.e. `PropertyMaps.Handler` with its `_database`
    /// (a `SHORT_DATE_TIME` property is decoded with the configured
    /// `DateTimeType`).
    pub fn read_with_context(
        blob: &[u8],
        format: &JetFormat,
        charset: Charset,
        context: DateContext,
    ) -> Result<PropertyMaps> {
        let maps = read_maps(blob, format, charset, context)?;
        Ok(PropertyMaps {
            read_from_empty: maps.iter().all(|map| map.is_empty()),
            maps,
            object_id: 0,
            row_id: None,
            db_object_name: None,
            table_owner_page: None,
        })
    }

    /// Writes all the property maps as a property blob (`PropertyMaps.Handler
    /// .write`).  A group with no properties writes no bytes at all.
    pub fn write(&self, format: &JetFormat) -> Result<Vec<u8>> {
        self.write_with_charset(format, default_charset(format))
    }

    pub fn write_with_charset(&self, format: &JetFormat, charset: Charset) -> Result<Vec<u8>> {
        self.write_with_context(format, charset, DateContext::default())
    }

    /// [`PropertyMaps::write_with_charset`] with the database's date/time
    /// conversion context (a `SHORT_DATE_TIME` property is encoded with the
    /// configured `DateTimeType`).
    pub fn write_with_context(
        &self,
        format: &JetFormat,
        charset: Charset,
        context: DateContext,
    ) -> Result<Vec<u8>> {
        write_maps(&self.maps, format, charset, context)
    }

    pub fn object_id(&self) -> i32 {
        self.object_id
    }

    /// Whether the group was read from a property blob which held no
    /// properties (see [`PropertyMaps::read_from_empty`]).
    pub fn read_from_empty(&self) -> bool {
        self.read_from_empty
    }

    pub fn set_object_id(&mut self, object_id: i32) {
        self.object_id = object_id;
    }

    /// The catalog row which holds this group's property blob, when it is
    /// known (`PropertyMaps._rowId`).
    pub fn row_id(&self) -> Option<crate::table::RowId> {
        self.row_id
    }

    pub fn set_row_id(&mut self, row_id: Option<crate::table::RowId>) {
        self.row_id = row_id;
    }

    /// The name of the database level catalog object this group belongs to,
    /// for a group whose catalog row had to be created on save.
    pub fn db_object_name(&self) -> Option<&str> {
        self.db_object_name.as_deref()
    }

    pub fn set_db_object_name(&mut self, name: Option<impl Into<String>>) {
        self.db_object_name = name.map(|n| n.into());
    }

    /// The table definition page when this group is a table's property maps
    /// (see [`Database::get_table_properties`](crate::database::Database::
    /// get_table_properties)).
    pub fn table_owner_page(&self) -> Option<i32> {
        self.table_owner_page
    }

    pub fn set_table_owner_page(&mut self, page: Option<i32>) {
        self.table_owner_page = page;
    }

    /// Saves this group to the database (`PropertyMaps.save`): the blob is
    /// written back as the `LvProp` value of the catalog row the group was read
    /// from.  An empty group is stored as a null value, like the Java library
    /// (whose `write` returns `null` for a group with no property names).
    ///
    /// A database level group whose catalog row does not exist (a database
    /// created without Access's `SummaryInfo`/`UserDefined` rows) creates the
    /// row first; the group is then bound to the created row, so saving it
    /// again updates that row instead of creating another one.  The Java
    /// library has no such fallback and refuses to save such a group.
    pub fn save(&mut self, db: &mut crate::database::Database) -> Result<()> {
        db.write_property_maps(self)
    }

    pub fn len(&self) -> usize {
        self.maps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.maps.is_empty()
    }

    /// The object level ("default") map, if this group has one.
    pub fn default_map(&self) -> Option<&PropertyMap> {
        self.maps.iter().find(|m| m.name == DEFAULT_NAME)
    }

    /// The object level ("default") map, creating it if necessary
    /// (`PropertyMaps.getDefault`).
    pub fn get_default(&mut self) -> &mut PropertyMap {
        self.get_or_insert_with_type(DEFAULT_NAME, MAP_TYPE_DEFAULT)
    }

    /// The column map with the given name, if any (`PropertyMaps.get`).
    pub fn get(&self, name: &str) -> Option<&PropertyMap> {
        self.find(name, MAP_TYPE_COLUMN)
    }

    /// The index map with the given name, if any (`PropertyMaps.getIndex`).
    pub fn get_index(&self, name: &str) -> Option<&PropertyMap> {
        self.find(name, MAP_TYPE_INDEX)
    }

    /// The column map with the given name, creating it if necessary
    /// (`PropertyMaps.get`).
    pub fn get_mut(&mut self, name: &str) -> &mut PropertyMap {
        self.get_or_insert_with_type(name, MAP_TYPE_COLUMN)
    }

    /// The index map with the given name, creating it if necessary
    /// (`PropertyMaps.getIndex`).
    pub fn get_index_mut(&mut self, name: &str) -> &mut PropertyMap {
        self.get_or_insert_with_type(name, MAP_TYPE_INDEX)
    }

    /// The map with the given name and block type, if any.
    pub fn get_map(&self, name: &str, map_type: i16) -> Option<&PropertyMap> {
        self.find(name, map_type)
    }

    /// The map with the given name and block type, creating it if necessary
    /// (`PropertyMaps.get`).
    pub fn get_or_insert(&mut self, name: &str, map_type: i16) -> &mut PropertyMap {
        self.get_or_insert_with_type(name, map_type)
    }

    fn get_or_insert_with_type(&mut self, name: &str, map_type: i16) -> &mut PropertyMap {
        let lookup = lookup_name(name);
        if let Some(idx) = self
            .maps
            .iter()
            .position(|m| m.map_type == map_type && lookup_name(&m.name) == lookup)
        {
            return &mut self.maps[idx];
        }
        self.maps.push(PropertyMap::with_name(name, map_type));
        self.maps.last_mut().unwrap()
    }

    /// Reorders the group so the given column maps come first, in the given
    /// order, creating the ones which do not exist yet.
    ///
    /// `TableImpl`'s constructor creates the property map of every column, in
    /// column order, *before* the table's own map is touched (each
    /// `ColumnImpl.initColumnValidator` -> `getProperties` allocates it).  A
    /// table created in this process therefore anchors the block order at
    /// `[column maps..., other maps...]` from the start, which is the order its
    /// property blob is written in; a table loaded from a file keeps the blob's
    /// order and appends the column maps the blob does not hold yet.  The port
    /// rebuilds the group on every access, so a table created in this process
    /// records the anchor and this method restores it.
    pub fn reorder_column_maps(&mut self, anchor: &[String]) {
        let mut reordered: Vec<PropertyMap> = Vec::with_capacity(self.maps.len().max(anchor.len()));
        for name in anchor {
            let lookup = lookup_name(name);
            let existing = self
                .maps
                .iter()
                .position(|m| m.map_type == MAP_TYPE_COLUMN && lookup_name(&m.name) == lookup);
            match existing {
                Some(idx) => reordered.push(self.maps.remove(idx)),
                None => reordered.push(PropertyMap::with_name(name.clone(), MAP_TYPE_COLUMN)),
            }
        }
        reordered.append(&mut self.maps);
        self.maps = reordered;
    }

    fn find(&self, name: &str, map_type: i16) -> Option<&PropertyMap> {
        let lookup = lookup_name(name);
        self.maps
            .iter()
            .find(|m| m.map_type == map_type && lookup_name(&m.name) == lookup)
    }

    pub fn iter(&self) -> impl Iterator<Item = &PropertyMap> {
        self.maps.iter()
    }

    /// Removes the empty maps from this group, which is what a write would
    /// skip anyway.
    pub fn remove_empty(&mut self) {
        self.maps.retain(|m| !m.is_empty());
    }
}

impl<'a> IntoIterator for &'a PropertyMaps {
    type Item = &'a PropertyMap;
    type IntoIter = std::slice::Iter<'a, PropertyMap>;

    fn into_iter(self) -> Self::IntoIter {
        self.maps.iter()
    }
}

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

/// The charset used for property names and text values.  The Java library
/// uses the database charset; for Jet3 (the `KKD` property block type) that is
/// derived from the code page in the file header.  Without the header, the
/// code page of virtually every Jet3 database, windows-1252, is used (a caller
/// which knows the database charset can pass it to the `*_with_charset`
/// methods).
fn default_charset(format: &JetFormat) -> Charset {
    if format.property_map_type.starts_with(b"KKD") && format.charset == Charset::Utf8 {
        Charset::Cp1252
    } else {
        format.charset
    }
}

/// Parses every map of a property blob (`PropertyMaps.Handler.read`).
fn read_maps(
    blob: &[u8],
    format: &JetFormat,
    charset: Charset,
    context: DateContext,
) -> Result<Vec<PropertyMap>> {
    if blob.len() < 4 {
        // an empty property value is a group with no maps.
        //
        // Note: Java reads the four byte type tag without checking the
        // remaining length, so a value of 1 to 3 bytes makes it throw an
        // unchecked `IndexOutOfBoundsException` (through
        // `ByteUtil.matchesRange`).  The port treats such a truncated value as
        // empty instead of reproducing that incidental exception (see the
        // "Conventions and limitations" section of the module documentation).
        return Ok(Vec::new());
    }

    // check for known header
    let magic = &blob[..4];
    let known_type = PROPERTY_MAP_TYPES
        .iter()
        .any(|tmp_type| magic == &tmp_type[..]);
    if !known_type {
        // a value which is all zero bytes holds no properties (an empty or
        // unused catalog value)
        if magic.iter().all(|b| *b == 0) {
            return Ok(Vec::new());
        }
        return Err(Error::Jackcess(format!(
            "Unknown property map type {}",
            hex_string_spaced(&blob[..4])
        )));
    }

    let mut bb = ByteBuf::wrap_le(blob.to_vec());
    bb.set_position(4)?;

    let mut maps: Vec<PropertyMap> = Vec::new();
    let mut prop_names: Option<Vec<String>> = None;

    // parse each data "chunk"
    while bb.has_remaining() {
        require_remaining(&bb, 6, "chunk header")?;
        let len = bb.get_i32()?;
        let block_type = bb.get_i16()?;
        let end_pos = require_end(&bb, (bb.position() as i64) + (len as i64) - 6, "chunk")?;

        let mut bb_block = bb.narrow(bb.position(), end_pos)?;

        if block_type == PROPERTY_NAME_LIST {
            prop_names = Some(read_property_names(&mut bb_block, charset)?);
        } else {
            read_property_values(
                &mut bb_block,
                prop_names.as_deref(),
                block_type,
                charset,
                format,
                context,
                &mut maps,
            )?;
        }

        bb.set_position(end_pos)?;
    }

    Ok(maps)
}

/// `PropertyMaps.Handler.readPropertyNames`.
fn read_property_names(bb_block: &mut ByteBuf, charset: Charset) -> Result<Vec<String>> {
    let mut names = Vec::new();
    while bb_block.has_remaining() {
        names.push(read_prop_name(bb_block, charset)?);
    }
    Ok(names)
}

/// `PropertyMaps.Handler.readPropName`.
fn read_prop_name(buffer: &mut ByteBuf, charset: Charset) -> Result<String> {
    require_remaining(buffer, 2, "name length")?;
    let name_length = buffer.get_i16()?;
    if name_length < 0 || name_length as usize > buffer.remaining() {
        return Err(Error::Jackcess(format!(
            "Property name is {} bytes, {} left in the block",
            name_length,
            buffer.remaining()
        )));
    }
    let name_bytes = buffer.get_bytes(name_length as usize)?;
    Ok(decode_uncompressed_text(&name_bytes, charset))
}

/// `PropertyMaps.Handler.readPropertyValues`: reads the values of one block
/// into the (possibly newly created) map it belongs to.
fn read_property_values(
    bb_block: &mut ByteBuf,
    prop_names: Option<&[String]>,
    block_type: i16,
    charset: Charset,
    format: &JetFormat,
    context: DateContext,
    maps: &mut Vec<PropertyMap>,
) -> Result<()> {
    let mut map_name = DEFAULT_NAME.to_string();

    if bb_block.has_remaining() {
        // read the map name, if any
        require_remaining(bb_block, 4, "map name header")?;
        let name_block_len = bb_block.get_i32()?;
        let end_pos = require_end(
            bb_block,
            (bb_block.position() as i64) + (name_block_len as i64) - 4,
            "map name",
        )?;
        if name_block_len > 6 {
            map_name = read_prop_name(bb_block, charset)?;
        }
        bb_block.set_position(end_pos)?;
    }

    let map_idx = get_or_insert_map(maps, &map_name, block_type);

    // read the values
    while bb_block.has_remaining() {
        require_remaining(bb_block, VALUE_HEADER_LENGTH, "value header")?;
        let val_len = bb_block.get_i16()?;
        let end_pos = require_end(
            bb_block,
            (bb_block.position() as i64) + (val_len as i64) - 2,
            "value",
        )?;
        let flags = bb_block.get_u8()?;
        let type_byte = bb_block.get_u8()?;
        let data_type = DataType::from_byte(type_byte)
            .ok_or_else(|| Error::Jackcess(format!("Unrecognized data type: {}", type_byte)))?;
        let name_idx = bb_block.get_i16()?;
        let data_size = bb_block.get_i16()?;

        // the name list is a chunk of its own, which a corrupt block can leave
        // out or put after the values which use it
        let prop_names = match prop_names {
            Some(names) => names,
            None => {
                return Err(Error::Jackcess(format!(
                    "Property name index {} is not one of the 0 known names",
                    name_idx
                )))
            }
        };
        if name_idx < 0 || name_idx as usize >= prop_names.len() {
            return Err(Error::Jackcess(format!(
                "Property name index {} is not one of the {} known names",
                name_idx,
                prop_names.len()
            )));
        }
        let prop_name = prop_names[name_idx as usize].clone();

        // guids seem to be marked as "binary" fields
        let col_type = if is_pseudo_guid_column(data_type, &prop_name, data_size) {
            DataType::Guid
        } else {
            data_type
        };
        // long value types are read like their simple types
        let read_type = match col_type {
            DataType::Memo => DataType::Text,
            DataType::Ole => DataType::Binary,
            other => other,
        };

        if data_size < 0 || data_size as usize > bb_block.remaining() {
            return Err(Error::Jackcess(format!(
                "Property {} holds {} bytes, {} left in the block",
                prop_name,
                data_size,
                bb_block.remaining()
            )));
        }
        // a fixed length value which is short would underflow the decode
        if !read_type.is_variable_length()
            && read_type.has_fixed_size()
            && data_size < read_type.get_fixed_size() as i16
        {
            return Err(Error::Jackcess(format!(
                "Property {} holds {} bytes, too few for {}",
                prop_name,
                data_size,
                read_type.get_name()
            )));
        }

        let data = bb_block.get_bytes(data_size as usize)?;
        let value = read_value(read_type, &prop_name, &data, charset, format, context)?;

        maps[map_idx].put_stored(Property::new(prop_name, data_type, value), flags);

        bb_block.set_position(end_pos)?;
    }

    Ok(())
}

/// Returns the index of the map with the given name and block type, creating
/// it if necessary (`PropertyMaps.get`).
fn get_or_insert_map(maps: &mut Vec<PropertyMap>, name: &str, map_type: i16) -> usize {
    let lookup = lookup_name(name);
    if let Some(idx) = maps
        .iter()
        .position(|m| m.map_type == map_type && lookup_name(&m.name) == lookup)
    {
        return idx;
    }
    maps.push(PropertyMap::with_name(name, map_type));
    maps.len() - 1
}

/// The given end position, checked against the block
/// (`PropertyMaps.Handler.requireEnd`).
fn require_end(buffer: &ByteBuf, end_pos: i64, what: &str) -> Result<usize> {
    if end_pos < buffer.position() as i64 || end_pos > buffer.limit() as i64 {
        return Err(Error::Jackcess(format!(
            "Property {} ends at {}, outside {} to {}",
            what,
            end_pos,
            buffer.position(),
            buffer.limit()
        )));
    }
    Ok(end_pos as usize)
}

/// A length in a property block is a signed field read from the file, so it can
/// promise more bytes than the block holds
/// (`PropertyMaps.Handler.requireRemaining`).
fn require_remaining(buffer: &ByteBuf, len: usize, what: &str) -> Result<()> {
    if buffer.remaining() < len {
        return Err(Error::Jackcess(format!(
            "Property {} is {} bytes, {} left in the block",
            what,
            len,
            buffer.remaining()
        )));
    }
    Ok(())
}

fn is_pseudo_guid_column(data_type: DataType, prop_name: &str, data_size: i16) -> bool {
    data_type == DataType::Binary
        && data_size == DataType::Guid.get_fixed_size() as i16
        && prop_name.eq_ignore_ascii_case(GUID_PROP)
}

// ---------------------------------------------------------------------------
// value encoding
// ---------------------------------------------------------------------------

/// Reads a property value of the given (effective) type (`PropColumn.read`).
fn read_value(
    data_type: DataType,
    prop_name: &str,
    data: &[u8],
    charset: Charset,
    format: &JetFormat,
    context: DateContext,
) -> Result<Value> {
    let mut buf = ByteBuf::wrap_le(data.to_vec());
    match data_type {
        DataType::Boolean => {
            if data.is_empty() {
                return Err(Error::Jackcess(format!(
                    "Property {} holds no data for BOOLEAN",
                    prop_name
                )));
            }
            Ok(Value::Bool(data[0] != 0))
        }
        DataType::Byte => Ok(Value::Byte(buf.get_u8()?)),
        DataType::Int => Ok(Value::Int(buf.get_i16()?)),
        DataType::Long => Ok(Value::Long(buf.get_i32()?)),
        DataType::Double => Ok(Value::Double(buf.get_f64()?)),
        DataType::Float => Ok(Value::Float(buf.get_f32()?)),
        DataType::ShortDateTime => {
            let bits = buf.get_u64()?;
            // `PropColumn.read` -> `ColumnImpl.readDateValue` ->
            // `getDateTimeFactory().fromDateBits(this, dateBits)`, where
            // `PropColumn.getDatabase()` is the mapping database
            Ok(crate::column::date_value_from_bits_in(bits, context))
        }
        DataType::Binary | DataType::BigBinary => Ok(Value::Bytes(data.to_vec())),
        DataType::Text | DataType::Memo => {
            Ok(Value::Text(decode_text_value(data, charset, format)))
        }
        DataType::Ole => Ok(Value::Bytes(data.to_vec())),
        DataType::Money => {
            if data.len() != 8 {
                return Err(Error::Jackcess(format!(
                    "Invalid money value for property {}",
                    prop_name
                )));
            }
            let raw = buf.get_i64()?;
            Ok(Value::Decimal {
                unscaled: raw as i128,
                scale: 4,
            })
        }
        DataType::Numeric => {
            let negate = buf.get_u8()? != 0;
            let mut tmp = buf.get_bytes(16)?;
            fix_numeric_byte_order(&mut tmp);
            let magnitude = u128::from_be_bytes(
                tmp.as_slice()
                    .try_into()
                    .map_err(|_| Error::Jackcess("Invalid numeric property value".to_string()))?,
            );
            let unscaled: i128 = if negate {
                if magnitude > (i128::MAX as u128) + 1 {
                    return Err(Error::Jackcess(format!(
                        "Numeric property {} is out of range for i128",
                        prop_name
                    )));
                }
                (magnitude as i128).wrapping_neg()
            } else {
                if magnitude > i128::MAX as u128 {
                    return Err(Error::Jackcess(format!(
                        "Numeric property {} is out of range for i128",
                        prop_name
                    )));
                }
                magnitude as i128
            };
            Ok(Value::Decimal { unscaled, scale: 0 })
        }
        DataType::Guid => Ok(Value::Text(read_guid_value(data)?)),
        DataType::ExtDateTime => Ok(Value::DateTime(read_extended_date_value(data)?)),
        DataType::BigInt => Ok(Value::BigInt(buf.get_i64()?)),
        DataType::ComplexType => Ok(Value::Long(buf.get_i32()?)),
        DataType::UnsupportedFixedLen | DataType::UnsupportedVarLen => {
            Ok(Value::Bytes(data.to_vec()))
        }
    }
}

/// Writes a property value of the given type (`PropColumn.write`).
fn write_value(
    prop: &Property,
    charset: Charset,
    format: &JetFormat,
    context: DateContext,
) -> Result<Vec<u8>> {
    let mut data_type = prop.data_type;
    // guids seem to be marked as "binary" fields
    if data_type == DataType::Binary
        && prop.name.eq_ignore_ascii_case(GUID_PROP)
        && is_guid_value(&prop.value)
    {
        data_type = DataType::Guid;
    }

    let mut buffer = ByteBuf::allocate_order(64, crate::bytebuf::ByteOrder::LittleEndian);
    let mut out: Vec<u8> = Vec::new();
    match data_type {
        DataType::Boolean => {
            // the boolean property column writes the int value of the boolean
            out.push(match to_bool(&prop.value)? {
                true => 0xFF,
                false => 0x00,
            });
        }
        DataType::Byte => out.push(to_number_i64(&prop.value)? as u8),
        DataType::Int => buffer.put_i16(to_number_i64(&prop.value)? as i16)?,
        DataType::Long => buffer.put_i32(to_number_i64(&prop.value)? as i32)?,
        DataType::Money => buffer.put_i64(money_value(&prop.value)?)?,
        DataType::Float => buffer.put_f32(to_f64(&prop.value)? as f32)?,
        DataType::Double => buffer.put_f64(to_f64(&prop.value)?)?,
        DataType::ShortDateTime => buffer.put_u64(date_value_bits(&prop.value, context)?)?,
        DataType::Binary
        | DataType::Ole
        | DataType::BigBinary
        | DataType::UnsupportedFixedLen
        | DataType::UnsupportedVarLen => {
            out = to_byte_array(&prop.value)?;
        }
        DataType::Text => {
            let max_chars =
                DataType::Text.to_unit_size(DataType::Text.get_max_size(), Some(format));
            out = encode_text_value(&prop.value, 0, max_chars, charset)?;
        }
        DataType::Memo => {
            // the length of the whole value entry is checked when it is written
            out = encode_text_value(&prop.value, 0, i32::MAX, charset)?;
        }
        DataType::Guid => out = write_guid_value(&prop.value)?,
        DataType::Numeric => out = write_numeric_value(&prop.value)?,
        DataType::BigInt => buffer.put_i64(to_number_i64(&prop.value)?)?,
        DataType::ExtDateTime => out = write_extended_date_value(&prop.value)?,
        DataType::ComplexType => buffer.put_i32(to_number_i64(&prop.value)? as i32)?,
    }

    if out.is_empty() {
        let n = buffer.position();
        out.extend_from_slice(&buffer.backing()[..n]);
    }
    Ok(out)
}

/// `ColumnImpl.decodeTextValue`.
fn decode_text_value(data: &[u8], charset: Charset, format: &JetFormat) -> String {
    // the 0xFF, 0xFE sequence indicates that unicode compression is used.  a
    // format which holds text in a code page has no compressed form.
    let is_compressed = !format.code_page_charset
        && data.len() > 1
        && data[0] == TEXT_COMPRESSION_HEADER[0]
        && data[1] == TEXT_COMPRESSION_HEADER[1];

    if !is_compressed {
        return decode_uncompressed_text(data, charset);
    }

    // this is a whacky compression combo that switches back and forth between
    // compressed/uncompressed using a 0x00 byte (starting in compressed mode)
    let mut text = String::new();
    let mut data_start = TEXT_COMPRESSION_HEADER.len();
    let mut data_end = data_start;
    let mut in_compressed_mode = true;
    while data_end < data.len() {
        if data[data_end] == 0x00 {
            decode_text_segment(
                data,
                data_start,
                data_end,
                in_compressed_mode,
                charset,
                &mut text,
            );
            in_compressed_mode = !in_compressed_mode;
            data_end += 1;
            data_start = data_end;
        } else {
            // an uncompressed character is two bytes wide and the byte which
            // marks the change back is one, so the 00 of a character such as
            // "d", which is 64 00, is not one.  stepping over the whole
            // character is what tells the two apart
            data_end += if in_compressed_mode { 1 } else { 2 };
        }
    }
    decode_text_segment(
        data,
        data_start,
        data.len(),
        in_compressed_mode,
        charset,
        &mut text,
    );
    text
}

fn decode_text_segment(
    data: &[u8],
    data_start: usize,
    data_end: usize,
    in_compressed_mode: bool,
    charset: Charset,
    text: &mut String,
) {
    if data_end <= data_start {
        return;
    }
    if in_compressed_mode {
        let mut tmp = Vec::with_capacity((data_end - data_start) * 2);
        for &b in &data[data_start..data_end] {
            tmp.push(b);
            tmp.push(0);
        }
        text.push_str(&decode_uncompressed_text(&tmp, charset));
    } else {
        text.push_str(&decode_uncompressed_text(
            &data[data_start..data_end],
            charset,
        ));
    }
}

/// Header used to indicate unicode text compression.
const TEXT_COMPRESSION_HEADER: [u8; 2] = [0xFF, 0xFE];

/// `ColumnImpl.decodeUncompressedText`.
fn decode_uncompressed_text(text_bytes: &[u8], charset: Charset) -> String {
    String::from_utf16_lossy(&charset.decode(text_bytes))
}

/// `ColumnImpl.encodeUncompressedText`.
fn encode_uncompressed_text(text: &str, charset: Charset) -> Vec<u8> {
    // the charset's `encode` is `Charset.encode(String)` for every code page
    // (UTF-16LE/UTF-8, the single byte windows/ISO/DOS/IBM pages and the East
    // Asian multi byte pages), including Java's '?' for an unmappable char
    charset.encode(text)
}

/// `ColumnImpl.encodeTextValue` as used by a property column: the length is
/// checked, but the value is never unicode compressed (property columns are
/// not "compressed unicode" columns).
fn encode_text_value(
    value: &Value,
    min_chars: i32,
    max_chars: i32,
    charset: Charset,
) -> Result<Vec<u8>> {
    let text = value_to_text(value)?;
    let len = text.encode_utf16().count() as i64;
    if len > max_chars as i64 || len < min_chars as i64 {
        return Err(Error::IllegalArgument(format!(
            "Text is wrong length for TEXT column, max {}, min {}, got {}",
            max_chars, min_chars, len
        )));
    }
    Ok(encode_uncompressed_text(&text, charset))
}

/// `ColumnImpl.toCharSequence` (which falls back to `Object.toString()`).
fn value_to_text(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok(String::new()),
        Value::Text(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(if *b { "true" } else { "false" }.to_string()),
        Value::Byte(v) => Ok((*v as i8).to_string()),
        Value::Int(v) => Ok(v.to_string()),
        Value::Long(v) => Ok(v.to_string()),
        Value::BigInt(v) => Ok(v.to_string()),
        Value::Float(v) => Ok(format!("{}", v)),
        Value::Double(v) => Ok(format!("{}", v)),
        Value::Decimal { unscaled, scale } => Ok(format_decimal(*unscaled, *scale)),
        Value::DateTime(ldt) => Ok(format_ldt(*ldt)),
        Value::DateBits(bits) => Ok(format_ldt(ldt_from_local_date_double(*bits))),
        Value::Uuid(_) => Ok(String::new()),
        Value::Bytes(_) => Err(Error::IllegalArgument(
            "cannot convert binary data to text".to_string(),
        )),
        Value::Keep => Err(Error::IllegalArgument(
            "KEEP_VALUE is not a property value".to_string(),
        )),
    }
}

/// `BigDecimal.toString` for the simple (plain notation) cases.
fn format_decimal(unscaled: i128, scale: i32) -> String {
    let neg = unscaled < 0;
    let digits = unscaled.unsigned_abs().to_string();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if scale <= 0 {
        out.push_str(&digits);
        for _ in 0..(-scale) {
            out.push('0');
        }
    } else {
        let scale = scale as usize;
        if digits.len() <= scale {
            out.push_str("0.");
            for _ in 0..(scale - digits.len()) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let split = digits.len() - scale;
            out.push_str(&digits[..split]);
            out.push('.');
            out.push_str(&digits[split..]);
        }
    }
    out
}

/// `LocalDateTime.toString`.
fn format_ldt(ldt: LocalDateTime) -> String {
    let mut out = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        ldt.year, ldt.month, ldt.day, ldt.hour, ldt.minute, ldt.second
    );
    if ldt.nano != 0 {
        if ldt.nano.is_multiple_of(1_000_000) {
            out.push_str(&format!(".{:03}", ldt.nano / 1_000_000));
        } else {
            out.push_str(&format!(".{:09}", ldt.nano));
        }
    }
    out
}

fn to_byte_array(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::Bytes(b) => Ok(b.clone()),
        Value::Null => Ok(Vec::new()),
        _ => Err(Error::IllegalArgument(
            "binary property values must be byte arrays".to_string(),
        )),
    }
}

/// `ColumnImpl.booleanToInteger` + `byteValue` through `Number`.
fn to_bool(value: &Value) -> Result<bool> {
    match value {
        Value::Bool(b) => Ok(*b),
        // lenient: any number is truthy when it is not 0
        other => Ok(to_number(other)?.as_i64() != 0),
    }
}

/// A number which keeps integer values exact, like the Java `Number` passed to
/// `ColumnImpl.writeFixedLengthField`.
enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    fn as_i64(&self) -> i64 {
        match self {
            Number::Int(v) => *v,
            Number::Float(v) => *v as i64,
        }
    }

    fn as_f64(&self) -> f64 {
        match self {
            Number::Int(v) => *v as f64,
            Number::Float(v) => *v,
        }
    }
}

/// `ColumnImpl.toNumber`.
fn to_number(value: &Value) -> Result<Number> {
    match value {
        Value::Null => Ok(Number::Int(0)),
        Value::Bool(b) => Ok(Number::Int(if *b { -1 } else { 0 })),
        Value::Byte(v) => Ok(Number::Int(*v as i64)),
        Value::Int(v) => Ok(Number::Int(*v as i64)),
        Value::Long(v) => Ok(Number::Int(*v as i64)),
        Value::BigInt(v) => Ok(Number::Int(*v)),
        Value::Float(v) => Ok(Number::Float(*v as f64)),
        Value::Double(v) => Ok(Number::Float(*v)),
        Value::Decimal { unscaled, scale } => {
            Ok(Number::Float(*unscaled as f64 / 10f64.powi(*scale)))
        }
        Value::DateTime(ldt) => Ok(Number::Float(datenums::to_date_double(*ldt))),
        Value::DateBits(bits) => Ok(Number::Float(*bits)),
        Value::Text(s) => {
            Ok(Number::Float(s.trim().parse::<f64>().map_err(|_| {
                Error::IllegalArgument(format!("not a number: {}", s))
            })?))
        }
        Value::Bytes(_) | Value::Uuid(_) => Err(Error::IllegalArgument(
            "cannot convert binary data to a number".to_string(),
        )),
        Value::Keep => Err(Error::IllegalArgument(
            "KEEP_VALUE is not a property value".to_string(),
        )),
    }
}

fn to_number_i64(value: &Value) -> Result<i64> {
    Ok(to_number(value)?.as_i64())
}

fn to_f64(value: &Value) -> Result<f64> {
    Ok(to_number(value)?.as_f64())
}

/// Parses a decimal string the way `new BigDecimal(String)` does for plain
/// notation (no exponent), returning the unscaled value and scale.
fn parse_decimal_text(text: &str) -> Option<(i128, i32)> {
    let s = text.trim();
    let (negative, rest) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((int_part, frac_part)) => (int_part, frac_part),
        None => (rest, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{}{}", int_part, frac_part);
    let digits = digits.trim_start_matches('0');
    let magnitude: i128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    Some((
        if negative { -magnitude } else { magnitude },
        frac_part.len() as i32,
    ))
}

/// The exact decimal value of a text property value: `ColumnImpl.toBigDecimal`
/// parses a string exactly rather than through a double.
fn text_decimal_parts(value: &Value) -> Option<(i128, i32)> {
    match value {
        Value::Text(s) => parse_decimal_text(s),
        _ => None,
    }
}

/// Moves a decimal value to the given scale without rounding, like
/// `BigDecimal.setScale` (which fails when a digit would be lost).
fn rescale_exact(unscaled: i128, scale: i32, target_scale: i32) -> Option<i128> {
    let mut unscaled = unscaled;
    let mut scale = scale;
    while scale > target_scale {
        if unscaled % 10 != 0 {
            return None;
        }
        unscaled /= 10;
        scale -= 1;
    }
    while scale < target_scale {
        unscaled = unscaled.checked_mul(10)?;
        scale += 1;
    }
    Some(unscaled)
}

/// `ColumnImpl.toBigDecimal` reduced to the exact, in-range cases: returns the
/// value scaled to 4 decimal places as the `MONEY` unscaled long
/// (`writeCurrencyValue`).
fn money_value(value: &Value) -> Result<i64> {
    let exact = match value {
        Value::Decimal { unscaled, scale } => Some(rescale_exact(*unscaled, *scale, 4)),
        other => {
            text_decimal_parts(other).map(|(unscaled, scale)| rescale_exact(unscaled, scale, 4))
        }
    };
    if let Some(exact) = exact {
        return i64::try_from(exact.ok_or_else(|| {
            Error::IllegalArgument(format!(
                "Currency value '{}' out of range",
                format_value(value)
            ))
        })?)
        .map_err(|_| {
            Error::IllegalArgument(format!(
                "Currency value '{}' out of range",
                format_value(value)
            ))
        });
    }
    let d = to_f64(value)?;
    let scaled = d * 10_000.0;
    if !scaled.is_finite()
        || scaled.fract() != 0.0
        || scaled < i64::MIN as f64
        || scaled > i64::MAX as f64
    {
        return Err(Error::IllegalArgument(format!(
            "Currency value '{}' out of range",
            d
        )));
    }
    Ok(scaled as i64)
}

/// The unscaled integer value of a `NUMERIC` property (`BigDecimal.setScale(0)`
/// in `writeNumericValue`).
fn numeric_unscaled(value: &Value) -> Result<i128> {
    let exact = match value {
        Value::Decimal { unscaled, scale } => Some(rescale_exact(*unscaled, *scale, 0)),
        other => {
            text_decimal_parts(other).map(|(unscaled, scale)| rescale_exact(unscaled, scale, 0))
        }
    };
    if let Some(exact) = exact {
        return exact.ok_or_else(|| {
            Error::IllegalArgument(format!(
                "Numeric value '{}' out of range",
                format_value(value)
            ))
        });
    }
    let d = to_f64(value)?;
    if !d.is_finite() || d.fract() != 0.0 {
        return Err(Error::IllegalArgument(format!(
            "Numeric value '{}' out of range",
            d
        )));
    }
    if d < i128::MIN as f64 || d > i128::MAX as f64 {
        return Err(Error::IllegalArgument(
            "Numeric value out of range".to_string(),
        ));
    }
    Ok(d as i128)
}

/// `ColumnImpl.writeNumericValue`: a sign byte plus 16 bytes of unscaled
/// decimal digits.
fn write_numeric_value(value: &Value) -> Result<Vec<u8>> {
    let unscaled = numeric_unscaled(value)?;
    let negative = unscaled < 0;
    let magnitude = unscaled.unsigned_abs();

    // check the precision of the property numeric column, which is the
    // default precision of the NUMERIC type (18)
    let precision = magnitude.to_string().len() as i32;
    if precision > DataType::Numeric.get_default_precision() {
        return Err(Error::IllegalArgument(format!(
            "Numeric value is too big for specified precision {}: {}",
            DataType::Numeric.get_default_precision(),
            magnitude
        )));
    }

    // convert to unscaled little endian bytes
    let mut int_val_bytes = magnitude.to_be_bytes().to_vec();
    fix_numeric_byte_order(&mut int_val_bytes);

    let mut out = Vec::with_capacity(17);
    out.push(if negative { NUMERIC_NEGATIVE_BYTE } else { 0 });
    out.extend_from_slice(&int_val_bytes);
    Ok(out)
}

/// `ColumnImpl.NUMERIC_NEGATIVE_BYTE`
const NUMERIC_NEGATIVE_BYTE: u8 = 0x80;

/// `ColumnImpl.fixNumericByteOrder`: reverses each 4 byte segment.
fn fix_numeric_byte_order(bytes: &mut [u8]) {
    let mut i = 0;
    while i + 4 <= bytes.len() {
        bytes[i..i + 4].reverse();
        i += 4;
    }
}

/// `ColumnImpl.writeDateValue` through the database's date/time factory
/// (`PropertyMaps.PropColumn.getDatabase().getDateTimeFactory().toDateDouble`).
///
/// The conversion (and, in particular, the time zone) is the one
/// [`crate::column::date_value_bits_in`] implements for the configured
/// [`DateContext`]: a [`Value::DateBits`] keeps its original bits in both
/// modes, a [`Value::DateTime`] is already a wall clock value, and any other
/// numeric value is a `java.util.Date`-style epoch instant which the
/// `DateContext` converts with the database zone.
fn date_value_bits(value: &Value, context: DateContext) -> Result<u64> {
    match value {
        Value::Null => Ok(0f64.to_bits()),
        // `DateExt.getDateBits()`: the original bits, in both modes
        Value::DateBits(bits) => Ok(bits.to_bits()),
        // `toDateDouble(LocalDateTime)`, which does not consult the zone
        Value::DateTime(_) => crate::column::date_value_bits_in(value, context),
        Value::Bool(_)
        | Value::Byte(_)
        | Value::Int(_)
        | Value::Long(_)
        | Value::BigInt(_)
        | Value::Float(_)
        | Value::Double(_)
        | Value::Decimal { .. } => {
            // `toDateLong` returns epoch milliseconds for a number, then both
            // factories apply their conversion (and the `DATE` one its zone
            // offset)
            let millis = to_number_i64(value)?;
            let double = match context.date_time_type {
                crate::database::DateTimeType::LocalDateTime => {
                    datenums::to_date_double(context.time_zone.local_date_time_from_millis(millis))
                }
                crate::database::DateTimeType::Date => {
                    datenums::to_date_double_from_millis(millis, &context.time_zone)
                }
            };
            Ok(double.to_bits())
        }
        other => Err(Error::IllegalArgument(format!(
            "cannot convert {} to a date",
            format_value(other)
        ))),
    }
}

/// Days from 0001-01-01 (the "extended" date base) to 1970-01-01.
const EPOCH_DAY_0001_01_01: i64 = -719_162;

/// `ColumnImpl.readExtendedDateValue`.
fn read_extended_date_value(data: &[u8]) -> Result<LocalDateTime> {
    if data.len() < 39 {
        return Err(Error::Jackcess(
            "Invalid extended date property value".to_string(),
        ));
    }
    let num_days = read_ext_date_long(data, 0, 19)?;
    let seconds = read_ext_date_long(data, 20, 12)?;
    let nanos = read_ext_date_long(data, 32, 7)? * 100;
    Ok(ext_date_ldt(num_days, seconds, nanos))
}

fn read_ext_date_long(data: &[u8], start: usize, num_chars: usize) -> Result<i64> {
    let mut val: i64 = 0;
    for i in 0..num_chars {
        let digit = data[start + i];
        let inc = (digit as i64) - ('0' as i64);
        val = val
            .checked_mul(10)
            .and_then(|v| v.checked_add(inc))
            .ok_or_else(|| Error::Jackcess("Invalid extended date property value".to_string()))?;
    }
    Ok(val)
}

fn ext_date_ldt(num_days: i64, seconds: i64, nanos: i64) -> LocalDateTime {
    let total_secs = seconds + num_days * datenums::SECONDS_PER_DAY;
    let days = EPOCH_DAY_0001_01_01 + total_secs.div_euclid(datenums::SECONDS_PER_DAY);
    let sec_of_day = total_secs.rem_euclid(datenums::SECONDS_PER_DAY);
    let mut ldt = LocalDateTime::from_epoch_day(days);
    ldt.hour = (sec_of_day / 3600) as u32;
    ldt.minute = ((sec_of_day % 3600) / 60) as u32;
    ldt.second = (sec_of_day % 60) as u32;
    ldt.nano = nanos as u32;
    ldt
}

/// `ColumnImpl.writeExtendedDateValue`.
fn write_extended_date_value(value: &Value) -> Result<Vec<u8>> {
    let ldt = match value {
        Value::Null => LocalDateTime::of_date(1, 1, 1),
        Value::DateTime(ldt) => *ldt,
        Value::DateBits(bits) => ldt_from_local_date_double(*bits),
        Value::Bool(_)
        | Value::Byte(_)
        | Value::Int(_)
        | Value::Long(_)
        | Value::BigInt(_)
        | Value::Float(_)
        | Value::Double(_)
        | Value::Decimal { .. } => LocalDateTime::from_epoch_millis(to_number_i64(value)?),
        other => {
            return Err(Error::IllegalArgument(format!(
                "cannot convert {} to a date",
                format_value(other)
            )))
        }
    };
    let num_days = ldt.to_epoch_day() - EPOCH_DAY_0001_01_01;
    let num_seconds = ldt.second_of_day();
    let nanos = (ldt.nano / 100) as i64;

    // format: <19digits>:<19digits>:7 0x00
    let mut out = Vec::with_capacity(42);
    write_ext_date_long(&mut out, num_days, 19);
    out.push(b':');
    write_ext_date_long(&mut out, num_seconds, 12);
    write_ext_date_long(&mut out, nanos, 7);
    out.push(b':');
    out.push(b'7');
    out.push(0x00);
    Ok(out)
}

fn write_ext_date_long(out: &mut Vec<u8>, mut val: i64, num_chars: usize) {
    let start = out.len();
    out.resize(start + num_chars, b'0');
    for i in (0..num_chars).rev() {
        // Java truncates the (possibly negative) value the same way
        out[start + i] = b'0'.wrapping_add((val % 10) as u8);
        val /= 10;
    }
}

/// `ColumnImpl.ldtFromLocalDateDouble`.
fn ldt_from_local_date_double(value: f64) -> LocalDateTime {
    let (secs, nanos) = datenums::duration_from_local_date_double(value);
    let base = LocalDateTime::of_date(1899, 12, 30);
    let base_secs = base.to_epoch_day() * datenums::SECONDS_PER_DAY;
    let total = base_secs + secs;
    let days = total.div_euclid(datenums::SECONDS_PER_DAY);
    let sec = total.rem_euclid(datenums::SECONDS_PER_DAY);
    let mut ldt = LocalDateTime::from_epoch_day(days);
    ldt.hour = (sec / 3600) as u32;
    ldt.minute = ((sec % 3600) / 60) as u32;
    ldt.second = (sec % 60) as u32;
    ldt.nano = nanos as u32;
    ldt
}

/// `ColumnImpl.readGUIDValue`: formats the guid the way MS Access writes one,
/// braced and in upper case hex.
fn read_guid_value(data: &[u8]) -> Result<String> {
    if data.len() < 16 {
        return Err(Error::Jackcess("Invalid GUID property value".to_string()));
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&data[..16]);
    // the first 3 guid components are integer components which respect
    // endianness, so swap 4-byte int, 2-byte int, 2-byte int
    bytes[..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    Ok(format!(
        "{{{}-{}-{}-{}-{}}}",
        hex_string(&bytes[0..4]),
        hex_string(&bytes[4..6]),
        hex_string(&bytes[6..8]),
        hex_string(&bytes[8..10]),
        hex_string(&bytes[10..16])
    ))
}

/// `ColumnImpl.GUID_PATTERN`: an optional braced, optionally whitespace padded
/// 8-4-4-4-12 group of hex digits.
fn parse_guid(value: &str) -> Option<[u8; 16]> {
    let s = value.trim();
    let s = if let Some(stripped) = s.strip_prefix('{') {
        stripped.strip_suffix('}')?
    } else {
        s
    };
    let groups: Vec<&str> = s.split('-').collect();
    if groups.len() != 5 {
        return None;
    }
    let lens = [8usize, 4, 4, 4, 12];
    let mut bytes = [0u8; 16];
    let mut pos = 0;
    for (group, len) in groups.iter().zip(lens.iter()) {
        if group.len() != *len || !group.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut i = 0;
        while i < *len {
            let hi = hex_val(group.as_bytes()[i])?;
            let lo = hex_val(group.as_bytes()[i + 1])?;
            bytes[pos] = (hi << 4) | lo;
            pos += 1;
            i += 2;
        }
    }
    Some(bytes)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn is_guid_value(value: &Value) -> bool {
    match value {
        Value::Text(s) => parse_guid(s).is_some(),
        Value::Uuid(_) => true,
        _ => false,
    }
}

/// `ColumnImpl.writeGUIDValue`.
fn write_guid_value(value: &Value) -> Result<Vec<u8>> {
    let bytes = match value {
        Value::Uuid(b) => *b,
        Value::Text(s) => {
            parse_guid(s).ok_or_else(|| Error::IllegalArgument(format!("Invalid GUID: {}", s)))?
        }
        other => {
            return Err(Error::IllegalArgument(format!(
                "Invalid GUID: {}",
                format_value(other)
            )))
        }
    };
    let mut out = bytes;
    // the first 3 guid components are integer components which respect
    // endianness, so swap 4-byte int, 2-byte int, 2-byte int
    out[..4].reverse();
    out[4..6].reverse();
    out[6..8].reverse();
    Ok(out.to_vec())
}

// ---------------------------------------------------------------------------
// writing
// ---------------------------------------------------------------------------

/// A value entry which has been encoded and is ready to be written.
struct ValueRecord {
    /// index of the map in the group
    map_index: usize,
    /// index of the name in the name list
    name_index: usize,
    flags: u8,
    data_type: DataType,
    data: Vec<u8>,
}

/// Serializes a group of property maps (`PropertyMaps.Handler.write`).
fn write_maps(
    maps: &[PropertyMap],
    format: &JetFormat,
    charset: Charset,
    context: DateContext,
) -> Result<Vec<u8>> {
    // grab the property names from all the maps
    let mut prop_names: Vec<String> = Vec::new();
    for prop_map in maps {
        for prop in prop_map.iter() {
            // like the LinkedHashSet of the java library, names are deduped
            // exactly (not case insensitively)
            if !prop_names.contains(&prop.name) {
                prop_names.push(prop.name.clone());
            }
        }
    }

    if prop_names.is_empty() {
        return Ok(Vec::new());
    }
    if prop_names.len() > i16::MAX as usize {
        return Err(Error::IllegalArgument(format!(
            "Too many property names: {}",
            prop_names.len()
        )));
    }

    let encoded_names: Vec<Vec<u8>> = prop_names
        .iter()
        .map(|name| encode_uncompressed_text(name, charset))
        .collect();
    for name in &encoded_names {
        if name.len() > i16::MAX as usize {
            return Err(Error::IllegalArgument(
                "Property name is too long".to_string(),
            ));
        }
    }

    // encode all the values first, so the buffer can be allocated exactly
    let mut records: Vec<ValueRecord> = Vec::new();
    for (map_index, prop_map) in maps.iter().enumerate() {
        if prop_map.is_empty() {
            continue;
        }
        for (name_index, prop_name) in prop_names.iter().enumerate() {
            let entry = match prop_map.entry(prop_name) {
                Some(entry) => entry,
                None => continue,
            };
            if entry.prop.value.is_null() {
                continue;
            }
            let data = write_value(&entry.prop, charset, format, context)?;
            if VALUE_HEADER_LENGTH + data.len() > MAX_VALUE_LENGTH {
                return Err(Error::IllegalArgument(format!(
                    "Invalid value for property {}: {} bytes is too long, max {}",
                    entry.prop.name,
                    data.len(),
                    MAX_VALUE_LENGTH - VALUE_HEADER_LENGTH
                )));
            }
            records.push(ValueRecord {
                map_index,
                name_index,
                flags: entry.flags,
                data_type: entry.prop.data_type,
                data,
            });
        }
    }

    // compute the exact size of the blob
    let mut size = 4usize; // property map type
    let names_block_len = 6 + encoded_names.iter().map(|n| 2 + n.len()).sum::<usize>();
    size += names_block_len;
    for (map_index, prop_map) in maps.iter().enumerate() {
        if prop_map.is_empty() {
            continue;
        }
        size += 6; // chunk header
        let map_name = encode_uncompressed_text(prop_map.name(), charset);
        size += 4 + 2 + map_name.len(); // map name block
        size += records
            .iter()
            .filter(|r| r.map_index == map_index)
            .map(|r| VALUE_HEADER_LENGTH + r.data.len())
            .sum::<usize>();
    }

    let mut bab = ByteBuf::allocate(size);

    bab.put_bytes(format.property_map_type)?;

    // write the full set of property names
    let names_start = bab.position();
    bab.put_i32(0)?;
    bab.put_i16(PROPERTY_NAME_LIST)?;
    for name in &encoded_names {
        bab.put_i16(name.len() as i16)?;
        bab.put_bytes(name)?;
    }
    let len = (bab.position() - names_start) as i32;
    bab.put_i32_at(names_start, len)?;

    // write all the map values
    for (map_index, prop_map) in maps.iter().enumerate() {
        if prop_map.is_empty() {
            continue;
        }
        let block_start = bab.position();
        bab.put_i32(0)?;
        bab.put_i16(prop_map.map_type())?;

        // write the map name
        let name_block_start = bab.position();
        bab.put_i32(0)?;
        let map_name = encode_uncompressed_text(prop_map.name(), charset);
        bab.put_i16(map_name.len() as i16)?;
        bab.put_bytes(&map_name)?;
        let len = (bab.position() - name_block_start) as i32;
        bab.put_i32_at(name_block_start, len)?;

        // write the map values, in the order of the name list
        for record in records.iter().filter(|r| r.map_index == map_index) {
            let val_start = bab.position();
            bab.put_i16(0)?;
            bab.put_u8(record.flags)?;
            bab.put_u8(record.data_type.value())?;
            bab.put_i16(record.name_index as i16)?;
            bab.put_i16(record.data.len() as i16)?;
            bab.put_bytes(&record.data)?;
            let len = (bab.position() - val_start) as i16;
            bab.put_i16_at(val_start, len)?;
        }

        let len = (bab.position() - block_start) as i32;
        bab.put_i32_at(block_start, len)?;
    }

    Ok(bab.backing()[..bab.position()].to_vec())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// `PropertyMapImpl.createProperty(name, value)`: creates a property whose type
/// is the well known type of the name, or is inferred from the value.
pub fn create_property(name: &str, value: Value) -> Result<Property> {
    create_property_typed(name, None, value)
}

/// `PropertyMapImpl.createProperty(name, type, value)`: creates a property for
/// a builder (`DatabaseBuilder.putDatabaseProperty`, `TableBuilder
/// .putProperty`, `ColumnBuilder.putProperty`).
pub fn create_property_typed(
    name: &str,
    data_type: Option<DataType>,
    value: Value,
) -> Result<Property> {
    let data_type = match data_type {
        Some(data_type) => data_type,
        None => match default_def(name) {
            Some((_, data_type, _)) => *data_type,
            None => infer_type(name, &value)?,
        },
    };
    Ok(Property::new(name, data_type, value))
}

/// Adds a property to a builder's property list; like the `HashMap` of the Java
/// builders, a property with the same name is replaced in place.
pub fn put_builder_property(props: &mut Vec<Property>, prop: Property) {
    match props.iter_mut().find(|p| p.name == prop.name) {
        Some(existing) => *existing = prop,
        None => props.push(prop),
    }
}

/// The order in which the Java builders store their properties in a property
/// map.
///
/// `DatabaseBuilder`, `TableBuilder` and `ColumnBuilder` collect their
/// properties in a `java.util.HashMap` and copy them into the property map with
/// `putAll`, so a property map created at build time holds its properties in
/// the iteration order of that `HashMap` (bucket order of `String.hashCode`),
/// not in the order the properties were added.  Reproducing that order keeps
/// the written property blobs byte identical to the Java library's.
///
/// A Java `HashMap` starts with 16 buckets and doubles them once the 0.75 load
/// factor is exceeded; it iterates bucket by bucket, in insertion order within
/// a bucket.  Returns the indexes of `names` in that order.
pub fn java_hashmap_order(names: &[String]) -> Vec<usize> {
    /// `String.hashCode`: 31 * h + c over the UTF-16 code units.
    fn java_string_hash(name: &str) -> i32 {
        let mut hash: i32 = 0;
        for unit in name.encode_utf16() {
            hash = hash.wrapping_mul(31).wrapping_add(unit as i32);
        }
        hash
    }

    let mut capacity = 16usize;
    while names.len() > (capacity * 3) / 4 {
        capacity *= 2;
    }
    let bucket = |idx: usize| -> usize {
        let hash = java_string_hash(&names[idx]) as u32;
        // `h ^ (h >>> 16)`, then the bucket mask
        ((hash ^ (hash >> 16)) as usize) & (capacity - 1)
    };
    let mut order: Vec<usize> = (0..names.len()).collect();
    // a stable sort keeps the insertion order of the entries in one bucket
    order.sort_by_key(|&a| bucket(a));
    order
}

/// Adds the properties of a builder to a property map in the order the Java
/// builders store them (see [`java_hashmap_order`]).
pub fn put_builder_properties(map: &mut PropertyMap, props: &[Property]) {
    let names: Vec<String> = props.iter().map(|p| p.name.clone()).collect();
    for idx in java_hashmap_order(&names) {
        map.put(props[idx].clone());
    }
}

/// The type of a property, determined from its value when the name is not one
/// of the known properties (`PropertyMapImpl.createProperty`).
fn infer_type(name: &str, value: &Value) -> Result<DataType> {
    match value {
        Value::Text(_) => Ok(DataType::Text),
        Value::Bool(_) => Ok(DataType::Boolean),
        Value::Byte(_) => Ok(DataType::Byte),
        Value::Int(_) => Ok(DataType::Int),
        Value::Long(_) => Ok(DataType::Long),
        Value::Float(_) => Ok(DataType::Float),
        Value::Double(_) => Ok(DataType::Double),
        Value::DateTime(_) | Value::DateBits(_) => Ok(DataType::ShortDateTime),
        Value::Bytes(_) => Ok(DataType::Ole),
        Value::BigInt(_) => Ok(DataType::BigInt),
        other => Err(Error::IllegalArgument(format!(
            "Could not determine type for property {} with value {}",
            name,
            format_value(other)
        ))),
    }
}

/// Renders a value for `Display`, roughly like the Java `toString` of the
/// matching Java object.
pub fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Byte(v) => (*v as i8).to_string(),
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        Value::BigInt(v) => v.to_string(),
        Value::Decimal { unscaled, scale } => format_decimal(*unscaled, *scale),
        Value::Float(v) => format!("{}", v),
        Value::Double(v) => format!("{}", v),
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => hex_string(b),
        Value::DateTime(ldt) => format_ldt(*ldt),
        Value::DateBits(bits) => format!("datebits({:016x})", bits.to_bits()),
        Value::Uuid(u) => format!(
            "{{{}-{}-{}-{}-{}}}",
            hex_string(&u[0..4]),
            hex_string(&u[4..6]),
            hex_string(&u[6..8]),
            hex_string(&u[8..10]),
            hex_string(&u[10..16])
        ),
        // `Column.KEEP_VALUE` is a String, so its Java toString is the text
        Value::Keep => "<KEEP_VALUE>".to_string(),
    }
}

fn hex_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02X}", b));
    }
    out
}

/// The hex format of `ByteUtil.toHexString(buffer, offset, size)`: the bytes
/// in upper case, separated by a space.  The property error messages use it
/// (the unspaced [`hex_string`] is the format of the guid and test dumps).
fn hex_string_spaced(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (index, b) in bytes.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{:02X}", b));
    }
    out
}

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    //! Tests against reference blobs generated by the Java library (see
    //! `resources/property_ref/src/.../PropertyRefGen.java`).  Each fixture is
    //! a `<name>.bin` property blob plus a `<name>.txt` textual dump of the
    //! values the Java library parses from it, so these tests check both the
    //! parsed data model and byte exact serialization.

    use super::*;
    use crate::datenums::TimeZone;
    use std::fs;
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/property_ref/blobs")
    }

    struct Fixture {
        format: String,
        maps: Vec<FixtureMap>,
    }

    struct FixtureMap {
        name: String,
        map_type: i16,
        props: Vec<FixtureProp>,
    }

    struct FixtureProp {
        name: String,
        data_type: DataType,
        flags: u8,
        value: String,
    }

    fn parse_fixture(text: &str) -> Fixture {
        let mut format = String::new();
        let mut maps: Vec<FixtureMap> = Vec::new();
        for line in text.lines() {
            if let Some(header) = line.strip_prefix("# ") {
                for field in header.split(' ') {
                    if let Some((key, value)) = field.split_once('=') {
                        if key == "format" {
                            format = value.to_string();
                        }
                    }
                }
            } else if line.starts_with("MAP|") {
                let parts: Vec<&str> = line.split('|').collect();
                maps.push(FixtureMap {
                    name: unescape(parts[1]),
                    map_type: parts[2].parse().unwrap(),
                    props: Vec::new(),
                });
            } else if line.starts_with("PROP|") {
                let parts: Vec<&str> = line.split('|').collect();
                maps.last_mut().unwrap().props.push(FixtureProp {
                    name: unescape(parts[1]),
                    data_type: data_type_from_name(parts[2]),
                    flags: u8::from_str_radix(parts[3], 16).unwrap(),
                    value: parts[4].to_string(),
                });
            }
        }
        Fixture { format, maps }
    }

    fn unescape(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next().unwrap() {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'p' => out.push('|'),
                other => out.push(other),
            }
        }
        out
    }

    fn data_type_from_name(name: &str) -> DataType {
        use DataType::*;
        match name {
            "BOOLEAN" => Boolean,
            "BYTE" => Byte,
            "INT" => Int,
            "LONG" => Long,
            "MONEY" => Money,
            "FLOAT" => Float,
            "DOUBLE" => Double,
            "SHORT_DATE_TIME" => ShortDateTime,
            "BINARY" => Binary,
            "TEXT" => Text,
            "OLE" => Ole,
            "MEMO" => Memo,
            "GUID" => Guid,
            "NUMERIC" => Numeric,
            "BIG_BINARY" => BigBinary,
            "COMPLEX_TYPE" => ComplexType,
            "BIG_INT" => BigInt,
            "EXT_DATE_TIME" => ExtDateTime,
            other => panic!("unknown data type {}", other),
        }
    }

    fn format_for(name: &str) -> JetFormat {
        match name {
            "jet4" => JetFormat::version_4(),
            "jet97" => JetFormat::version_3(),
            other => panic!("unknown fixture format {}", other),
        }
    }

    /// Renders a value the way `PropertyRefGen.renderValue` does.
    fn render(value: &Value) -> String {
        match value {
            Value::Null => "null:".to_string(),
            Value::Bool(b) => format!("bool:{}", b),
            Value::Byte(v) => format!("num:{}", *v as i8),
            Value::Int(v) => format!("num:{}", v),
            Value::Long(v) => format!("num:{}", v),
            Value::BigInt(v) => format!("num:{}", v),
            Value::Float(v) => format!("f32:{:08X}", v.to_bits()),
            Value::Double(v) => format!("f64:{:016X}", v.to_bits()),
            Value::Decimal { unscaled, scale } => format!("dec:{}:{}", scale, unscaled),
            Value::Text(s) => format!("str:{}", escape(s)),
            Value::Bytes(b) => format!("hex:{}", hex_string(b)),
            Value::DateTime(ldt) => format!("ldt:{}", java_ldt_string(*ldt)),
            Value::DateBits(bits) => format!("datebits:{:016X}", bits.to_bits()),
            Value::Uuid(u) => format!(
                "ldt:{{{}-{}-{}-{}-{}}}",
                hex_string(&u[0..4]),
                hex_string(&u[4..6]),
                hex_string(&u[6..8]),
                hex_string(&u[8..10]),
                hex_string(&u[10..16])
            ),
            Value::Keep => "keep".to_string(),
        }
    }

    fn escape(s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '|' => out.push_str("\\p"),
                other => out.push(other),
            }
        }
        out
    }

    /// `LocalDateTime.toString` (which `renderValue` uses).
    fn java_ldt_string(ldt: LocalDateTime) -> String {
        format_ldt(ldt)
    }

    /// Compares a parsed value with the fixture dump.  Date values are also
    /// compared exactly: Access stores "wall clock" date doubles, so the value
    /// this port decodes is the same `LocalDateTime` the Java library produced.
    fn value_matches(expected: &str, actual: &Value) -> bool {
        render(actual) == expected
    }

    fn check_fixture(name: &str, bin: &[u8], text: &str) {
        let fixture = parse_fixture(text);
        let format = format_for(&fixture.format);

        check_parsed_maps(name, bin, &fixture, &format);

        // re-serializing the parsed maps must reproduce the original bytes
        let maps = PropertyMaps::read(bin, &format).unwrap();
        let back = maps.write(&format).unwrap();
        assert_eq!(back, bin, "{}: serialized bytes", name);
    }

    /// Compares the maps parsed from the given blob with the textual dump the
    /// Java library produced for it.
    fn check_parsed_maps(name: &str, bin: &[u8], fixture: &Fixture, format: &JetFormat) {
        let maps = PropertyMaps::read(bin, format).unwrap();
        assert_eq!(maps.len(), fixture.maps.len(), "{}: number of maps", name);
        for (map, expected) in maps.iter().zip(fixture.maps.iter()) {
            assert_eq!(map.name(), expected.name, "{}: map name", name);
            assert_eq!(map.map_type(), expected.map_type, "{}: map type", name);
            assert_eq!(
                map.len(),
                expected.props.len(),
                "{}: number of properties in map {}",
                name,
                expected.name
            );
            for (prop, expected_prop) in map.iter().zip(expected.props.iter()) {
                assert_eq!(prop.name, expected_prop.name, "{}: property name", name);
                assert_eq!(
                    prop.data_type, expected_prop.data_type,
                    "{}: type of property {}",
                    name, prop.name
                );
                assert_eq!(
                    map.flags(&prop.name),
                    Some(expected_prop.flags),
                    "{}: flags of property {}",
                    name,
                    prop.name
                );
                assert!(
                    value_matches(&expected_prop.value, &prop.value),
                    "{}: value of property {}: expected {}, got {}",
                    name,
                    prop.name,
                    expected_prop.value,
                    render(&prop.value)
                );
            }
        }
    }

    #[test]
    fn java_fixtures_parse_and_roundtrip() {
        let dir = fixture_dir();
        let index = fs::read_to_string(dir.join("index.txt")).unwrap();
        let mut count = 0;
        for line in index.lines().filter(|line| !line.trim().is_empty()) {
            let parts: Vec<&str> = line.split('|').collect();
            let name = parts[0];
            let bin = fs::read(dir.join(format!("{}.bin", name))).unwrap();
            let text = fs::read_to_string(dir.join(format!("{}.txt", name))).unwrap();
            check_fixture(name, &bin, &text);
            count += 1;
        }
        assert_eq!(count, 24, "expected all reference fixtures to be checked");
    }

    /// The template databases' `MSysObjects` rows hold real property blobs.
    /// They are dumped (with the values the Java library parses out of them and
    /// the bytes the Java library writes back) by
    /// `tools/propcheck/java/.../PropTemplateDump.java`.
    ///
    /// Some of the real blobs are not round tripped byte for byte by the Java
    /// library itself (Access stores `true` booleans as 0x01 where jackcess
    /// writes 0xFF, and the name list of a blob can hold names which no value
    /// entry uses).  Those blobs are compared against the bytes the Java
    /// library writes, which is the fidelity target of this port.
    #[test]
    fn template_database_blobs_match_java() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/propcheck/fixtures");
        assert!(
            dir.is_dir(),
            "missing template fixtures; run tools/propcheck/gen.ps1 (see tools/propcheck/README.md)"
        );

        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with("_index.txt"))
                    .unwrap_or(false)
            })
            .collect();
        entries.sort();
        assert!(!entries.is_empty(), "no template fixture indexes found");

        let mut count = 0;
        let mut exact = 0;
        for index in entries {
            let index_text = fs::read_to_string(&index).unwrap();
            for line in index_text.lines().filter(|line| !line.trim().is_empty()) {
                let parts: Vec<&str> = line.split('|').collect();
                let name = parts[0];
                let bin = fs::read(dir.join(format!("{}.bin", name))).unwrap();
                let text = fs::read_to_string(dir.join(format!("{}.txt", name))).unwrap();
                let fixture = parse_fixture(&text);
                let format = format_for(&fixture.format);

                check_parsed_maps(name, &bin, &fixture, &format);

                let maps = PropertyMaps::read(&bin, &format).unwrap();
                let ours = maps.write(&format).unwrap();
                let java = fs::read(dir.join(format!("{}.java.bin", name))).unwrap();
                assert_eq!(ours, java, "{}: bytes differ from the java library", name);
                if ours == bin {
                    exact += 1;
                }
                count += 1;
            }
        }
        assert!(count >= 30, "expected the template fixtures, got {}", count);
        // most of the real blobs are round tripped exactly; the rest only
        // differ where the java library itself differs from the original bytes
        assert!(exact > 0, "no template blob round tripped exactly");
        eprintln!(
            "template property blobs: {} parsed and compared, {} also byte \
             identical to the original access bytes (the rest match the java \
             library's re-serialization)",
            count, exact
        );
    }

    #[test]
    fn single_map_fixtures_via_property_map() {
        let dir = fixture_dir();
        let index = fs::read_to_string(dir.join("index.txt")).unwrap();
        for line in index.lines().filter(|line| !line.trim().is_empty()) {
            let parts: Vec<&str> = line.split('|').collect();
            let name = parts[0];
            if parts[3] != "single" {
                continue;
            }
            let bin = fs::read(dir.join(format!("{}.bin", name))).unwrap();
            let format = format_for(parts[1]);
            let map = PropertyMap::read(&bin, &format).unwrap();
            assert_eq!(map.len(), 1, "{}: property count", name);
            assert_eq!(map.name(), DEFAULT_NAME);
            assert_eq!(map.map_type(), MAP_TYPE_DEFAULT);
            assert_eq!(map.write(&format).unwrap(), bin, "{}: bytes", name);
        }
    }

    #[test]
    fn multi_map_blob_yields_default_map() {
        let dir = fixture_dir();
        let bin = fs::read(dir.join("table_jet4.bin")).unwrap();
        let format = JetFormat::version_4();
        let map = PropertyMap::read(&bin, &format).unwrap();
        assert_eq!(map.name(), DEFAULT_NAME);
        assert_eq!(
            map.get(DESCRIPTION_PROP).unwrap().value,
            Value::Text("table description".to_string())
        );
        assert_eq!(map.get("CustomTableProp").unwrap().value, Value::Long(42));
        assert!(map.is_ddl(DESCRIPTION_PROP));
        assert!(!map.is_ddl("CustomTableProp"));
        assert_eq!(map.get("OrderByOnLoad").unwrap().value, Value::Bool(true));
        assert_eq!(map.get("DefaultView").unwrap().value, Value::Byte(2));

        let maps = PropertyMaps::read(&bin, &format).unwrap();
        let name_props = maps.get("Name").unwrap();
        assert_eq!(
            name_props.get(DESCRIPTION_PROP).unwrap().value,
            Value::Text("the name column".to_string())
        );
        // DefaultValue is a MEMO property, stored as text
        assert_eq!(
            name_props.get(DEFAULT_VALUE_PROP).unwrap().data_type,
            DataType::Memo
        );
        assert_eq!(
            name_props.get(DEFAULT_VALUE_PROP).unwrap().value,
            Value::Text("\"x\"".to_string())
        );
        let index_props = maps.get_index("IdxName").unwrap();
        assert_eq!(
            index_props.get(DESCRIPTION_PROP).unwrap().value,
            Value::Text("index description".to_string())
        );
        // an index map and a column map can share a name
        assert!(maps.get("IdxName").is_none());
        assert!(maps.get_index("Name").is_none());
    }

    #[test]
    fn empty_and_short_blobs() {
        let format = JetFormat::version_4();
        // a null/empty value and a value too short for the type tag are groups
        // with no maps (Java reads the tag unchecked and throws
        // IndexOutOfBoundsException for the latter, see `read_maps`)
        for blob in [&b""[..], &b"M"[..], &b"MR"[..], &b"MR2"[..]] {
            let map = PropertyMap::read(blob, &format).unwrap();
            assert!(map.is_empty(), "{:?}", blob);
            assert_eq!(map.name(), DEFAULT_NAME);
            let maps = PropertyMaps::read(blob, &format).unwrap();
            assert!(maps.is_empty(), "{:?}", blob);
            // an empty map writes nothing at all
            assert!(map.write(&format).unwrap().is_empty());
        }
        // the bare type tag holds no chunks, which is a valid empty group
        for blob in [&b"MR2\0"[..], &b"KKD\0"[..]] {
            assert!(PropertyMaps::read(blob, &format).unwrap().is_empty());
        }
        // an empty group writes nothing at all
        assert!(PropertyMaps::new(0).write(&format).unwrap().is_empty());
    }

    #[test]
    fn unknown_blob_type_is_an_error() {
        let format = JetFormat::version_4();
        let err = PropertyMap::read(b"XXXX\x01\x00\x00\x00", &format).unwrap_err();
        // `ByteUtil.toHexString`: upper case bytes, separated by a space
        assert_eq!(err.to_string(), "Unknown property map type 58 58 58 58");
    }

    #[test]
    fn date_property_values() {
        let dir = fixture_dir();
        let format = JetFormat::version_4();
        let map =
            PropertyMap::read(&fs::read(dir.join("type_date.bin")).unwrap(), &format).unwrap();
        // the access date value (2009-02-14T07:31:30.123 wall clock time)
        assert_eq!(
            map.get("DateProp").unwrap().value,
            Value::DateTime(LocalDateTime::new(2009, 2, 14, 7, 31, 30, 123_000_000))
        );

        let map =
            PropertyMap::read(&fs::read(dir.join("type_extdate.bin")).unwrap(), &format).unwrap();
        assert_eq!(
            map.get("ExtDateProp").unwrap().value,
            Value::DateTime(LocalDateTime::new(2009, 2, 13, 23, 31, 30, 0))
        );
    }

    #[test]
    fn guid_and_unicode_values() {
        let dir = fixture_dir();
        let format = JetFormat::version_4();
        let map =
            PropertyMap::read(&fs::read(dir.join("type_guid.bin")).unwrap(), &format).unwrap();
        assert_eq!(
            map.get("GuidProp").unwrap().value,
            Value::Text("{11223344-5566-7788-99AA-BBCCDDEEFF00}".to_string())
        );
        let map = PropertyMap::read(
            &fs::read(dir.join("type_guid_pseudo.bin")).unwrap(),
            &format,
        )
        .unwrap();
        // a "BINARY" value named GUID is really a guid
        assert_eq!(map.get(GUID_PROP).unwrap().data_type, DataType::Binary);
        assert_eq!(
            map.get(GUID_PROP).unwrap().value,
            Value::Text("{11223344-5566-7788-99AA-BBCCDDEEFF00}".to_string())
        );
        let map =
            PropertyMap::read(&fs::read(dir.join("type_unicode.bin")).unwrap(), &format).unwrap();
        assert_eq!(
            map.get("Tëst Näme").unwrap().value,
            Value::Text("välue 日本語".to_string())
        );
        // lookups are case insensitive
        assert!(map.get("tëst näme").is_some());
    }

    #[test]
    fn write_put_remove_flags() {
        let format = JetFormat::version_4();
        let mut map = PropertyMap::new();
        map.put(Property::new(
            DESCRIPTION_PROP,
            DataType::Text,
            Value::Text("a description".to_string()),
        ));
        // Description is a known DDL property
        assert_eq!(map.flags(DESCRIPTION_PROP), Some(DDL_FLAG));
        map.put(Property::new("CustomProp", DataType::Long, Value::Long(7)));
        assert_eq!(map.flags("CustomProp"), Some(0));
        assert_eq!(map.len(), 2);

        // put replaces in place and keeps the position
        map.put(Property::new(
            DESCRIPTION_PROP,
            DataType::Text,
            Value::Text("new description".to_string()),
        ));
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.names().collect::<Vec<_>>(),
            vec![DESCRIPTION_PROP, "CustomProp"]
        );

        // the default DDL flag is added to an explicitly given flag byte
        map.put_with_flags(
            Property::new(REQUIRED_PROP, DataType::Boolean, Value::Bool(true)),
            SKIP_HANDLER_FLAG,
        );
        assert_eq!(map.flags(REQUIRED_PROP), Some(DDL_FLAG | SKIP_HANDLER_FLAG));

        let blob = map.write(&format).unwrap();
        let read_back = PropertyMap::read(&blob, &format).unwrap();
        assert_eq!(read_back.len(), 3);
        assert_eq!(
            read_back.flags(REQUIRED_PROP),
            Some(DDL_FLAG | SKIP_HANDLER_FLAG)
        );
        assert_eq!(
            read_back.get(DESCRIPTION_PROP).unwrap().value,
            Value::Text("new description".to_string())
        );

        assert!(map.remove(DESCRIPTION_PROP).is_some());
        assert!(map.remove(DESCRIPTION_PROP).is_none());
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn put_value_infers_types() {
        let mut map = PropertyMap::new();
        map.put_value("TextProp", Value::Text("x".to_string()))
            .unwrap();
        map.put_value("BoolProp", Value::Bool(true)).unwrap();
        map.put_value("ByteProp", Value::Byte(3)).unwrap();
        map.put_value("IntProp", Value::Int(4)).unwrap();
        map.put_value("LongProp", Value::Long(5)).unwrap();
        map.put_value("FloatProp", Value::Float(1.5)).unwrap();
        map.put_value("DoubleProp", Value::Double(2.5)).unwrap();
        map.put_value("BigIntProp", Value::BigInt(6)).unwrap();
        map.put_value("BinaryProp", Value::Bytes(vec![1, 2, 3]))
            .unwrap();
        // known property names use their well known type
        map.put_value(BUILD_PROP, Value::Long(1234)).unwrap();
        map.put_value(DEFAULT_VALUE_PROP, Value::Text("0".to_string()))
            .unwrap();

        assert_eq!(map.get("TextProp").unwrap().data_type, DataType::Text);
        assert_eq!(map.get("BoolProp").unwrap().data_type, DataType::Boolean);
        assert_eq!(map.get("ByteProp").unwrap().data_type, DataType::Byte);
        assert_eq!(map.get("IntProp").unwrap().data_type, DataType::Int);
        assert_eq!(map.get("LongProp").unwrap().data_type, DataType::Long);
        assert_eq!(map.get("FloatProp").unwrap().data_type, DataType::Float);
        assert_eq!(map.get("DoubleProp").unwrap().data_type, DataType::Double);
        assert_eq!(map.get("BigIntProp").unwrap().data_type, DataType::BigInt);
        assert_eq!(map.get("BinaryProp").unwrap().data_type, DataType::Ole);
        assert_eq!(map.get(BUILD_PROP).unwrap().data_type, DataType::Long);
        assert_eq!(
            map.get(DEFAULT_VALUE_PROP).unwrap().data_type,
            DataType::Memo
        );
        assert!(map.get(BUILD_PROP).unwrap().value == Value::Long(1234));

        let format = JetFormat::version_4();
        let blob = map.write(&format).unwrap();
        let read_back = PropertyMap::read(&blob, &format).unwrap();
        assert_eq!(read_back.len(), map.len());
        for prop in map.iter() {
            assert_eq!(
                read_back.get(&prop.name).unwrap().data_type,
                prop.data_type,
                "{}",
                prop.name
            );
        }
    }

    #[test]
    fn write_and_read_every_type() {
        let format = JetFormat::version_4();
        let mut map = PropertyMap::new();
        let props = vec![
            Property::new("BoolProp", DataType::Boolean, Value::Bool(true)),
            Property::new("ByteProp", DataType::Byte, Value::Byte(0xF9)),
            Property::new("IntProp", DataType::Int, Value::Int(-12345)),
            Property::new("LongProp", DataType::Long, Value::Long(1234567)),
            Property::new(
                "MoneyProp",
                DataType::Money,
                Value::Decimal {
                    unscaled: -123456789,
                    scale: 4,
                },
            ),
            Property::new("FloatProp", DataType::Float, Value::Float(1.5)),
            Property::new("DoubleProp", DataType::Double, Value::Double(-2.25)),
            Property::new(
                "DateProp",
                DataType::ShortDateTime,
                Value::DateTime(LocalDateTime::new(2009, 2, 14, 7, 31, 30, 123_000_000)),
            ),
            Property::new(
                "BinaryProp",
                DataType::Binary,
                Value::Bytes(vec![0, 1, 2, 0x7F, 0x80, 0xFF]),
            ),
            Property::new(
                "TextProp",
                DataType::Text,
                Value::Text("Hello, Wörld".to_string()),
            ),
            Property::new("EmptyTextProp", DataType::Text, Value::Text(String::new())),
            Property::new(
                "MemoProp",
                DataType::Memo,
                Value::Text("Memo text\nwith ünïcode".to_string()),
            ),
            Property::new("OleProp", DataType::Ole, Value::Bytes(vec![9, 8, 7, 0, 0])),
            Property::new(
                "GuidProp",
                DataType::Guid,
                Value::Text("{11223344-5566-7788-99AA-BBCCDDEEFF00}".to_string()),
            ),
            Property::new(
                "NumericProp",
                DataType::Numeric,
                Value::Decimal {
                    unscaled: -123456789012345678,
                    scale: 0,
                },
            ),
            Property::new(
                "BigIntProp",
                DataType::BigInt,
                Value::BigInt(-9223372036854775808),
            ),
            Property::new(
                "ExtDateProp",
                DataType::ExtDateTime,
                Value::DateTime(LocalDateTime::new(2009, 2, 13, 23, 31, 30, 0)),
            ),
        ];
        for prop in props {
            map.put(prop);
        }
        let blob = map.write(&format).unwrap();
        let read_back = PropertyMap::read(&blob, &format).unwrap();
        assert_eq!(read_back.len(), map.len());
        for prop in map.iter() {
            let read_prop = read_back.get(&prop.name).unwrap();
            assert_eq!(read_prop.data_type, prop.data_type, "{}", prop.name);
            assert_eq!(read_prop.value, prop.value, "{}", prop.name);
        }
        // and it round trips byte for byte
        assert_eq!(read_back.write(&format).unwrap(), blob);
    }

    #[test]
    fn numeric_and_money_ranges() {
        let format = JetFormat::version_4();
        // money keeps its exact decimal value
        let mut map = PropertyMap::new();
        map.put(Property::new(
            "MoneyProp",
            DataType::Money,
            Value::Decimal {
                unscaled: 12345,
                scale: 4,
            },
        ));
        let blob = map.write(&format).unwrap();
        let back = PropertyMap::read(&blob, &format).unwrap();
        assert_eq!(
            back.get("MoneyProp").unwrap().value,
            Value::Decimal {
                unscaled: 12345,
                scale: 4
            }
        );

        // a value with too many decimals cannot be stored in a money property
        let mut map = PropertyMap::new();
        map.put(Property::new(
            "MoneyProp",
            DataType::Money,
            Value::Decimal {
                unscaled: 123456,
                scale: 5,
            },
        ));
        assert!(map.write(&format).is_err());

        // numeric values are limited to the default precision (18)
        let mut map = PropertyMap::new();
        map.put(Property::new(
            "NumericProp",
            DataType::Numeric,
            Value::Decimal {
                unscaled: -1234567890123456789,
                scale: 0,
            },
        ));
        assert!(map.write(&format).is_err());
    }

    #[test]
    fn text_longer_than_a_text_property_is_rejected() {
        let format = JetFormat::version_4();
        let mut map = PropertyMap::new();
        map.put(Property::new(
            "TextProp",
            DataType::Text,
            Value::Text("x".repeat(256)),
        ));
        assert!(map.write(&format).is_err());

        // a memo property has no such limit
        let mut map = PropertyMap::new();
        map.put(Property::new(
            "MemoProp",
            DataType::Memo,
            Value::Text("x".repeat(1000)),
        ));
        assert!(map.write(&format).is_ok());
    }

    #[test]
    fn display_control_values_are_ported() {
        assert_eq!(
            DISPLAY_CONTROL_VALUES
                .iter()
                .find(|(name, _)| *name == "TEXT_BOX")
                .map(|(_, v)| *v),
            Some(109)
        );
        assert_eq!(TEXT_FORMAT_VALUES[0].1, 1);
        assert_eq!(IME_MODE_VALUES.len(), 11);
        assert_eq!(IME_SENTENCE_MODE_VALUES.len(), 4);
        assert_eq!(DEFAULT_TYPES.len(), 97);
        // the common properties' default types and DDL flags
        assert_eq!(default_def(BUILD_PROP).map(|d| d.1), Some(DataType::Long));
        assert_eq!(default_def(DESCRIPTION_PROP).map(|d| d.2), Some(true));
        assert_eq!(default_def("NoSuchProperty"), None);
    }

    /// The builder property order the Java `HashMap` produces (printed by a
    /// small Java program which put the same names into a `HashMap`).
    #[test]
    fn java_hashmap_order_matches_java() {
        let names =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
        // HashMap.put("Title"); put("CustomNum") iterates CustomNum, Title
        assert_eq!(
            java_hashmap_order(&names(&["Title", "CustomNum"])),
            vec![1, 0]
        );
        assert_eq!(
            java_hashmap_order(&names(&[
                "Description",
                "Title",
                "CustomNum",
                "MyBool",
                "Author",
                "Company"
            ])),
            vec![5, 0, 2, 1, 4, 3]
        );
        assert_eq!(
            java_hashmap_order(&names(&["Format", "DefaultValue", "Description"])),
            vec![1, 0, 2]
        );
        // single entries keep their place, and names in ascending buckets keep
        // the insertion order
        assert_eq!(java_hashmap_order(&names(&["Description"])), vec![0]);
        assert_eq!(
            java_hashmap_order(&names(&[
                "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P",
                "Q", "R", "S", "T"
            ])),
            (0..20).collect::<Vec<_>>()
        );
        // a group past the 12 entry load factor of the 16 bucket map still uses
        // one consistent bucket order
        let many: Vec<String> = (0..30).map(|i| format!("Prop{}", i)).collect();
        let order = java_hashmap_order(&many);
        assert_eq!(order.len(), 30);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..30).collect::<Vec<_>>());
    }

    /// The creation time properties of the `tableprops` scenario, as the Java
    /// library writes them (`java-src/PropFixtures.java`): the column maps come
    /// first (in column order), the table's own map last, and the properties of
    /// each map are in the order the Java builders store them.
    #[test]
    fn table_property_group_builder_matches_java() {
        let dir = fixture_dir();
        let format = JetFormat::version_4();

        let prop = |name: &str, value: Value| create_property(name, value).unwrap();
        let mut maps = PropertyMaps::new(0);
        put_builder_properties(
            maps.get_mut("Name"),
            &[
                prop(DESCRIPTION_PROP, Value::Text("the name column".to_string())),
                prop(FORMAT_PROP, Value::Text("@".to_string())),
            ],
        );
        put_builder_properties(
            maps.get_mut("Value"),
            &[prop(DEFAULT_VALUE_PROP, Value::Text("1.5".to_string()))],
        );
        put_builder_properties(
            maps.get_mut("Note"),
            &[prop(
                DESCRIPTION_PROP,
                Value::Text("the note column".to_string()),
            )],
        );
        put_builder_properties(
            maps.get_default(),
            &[
                prop(DESCRIPTION_PROP, Value::Text("the test table".to_string())),
                // a MEMO property whose value spans several long value pages
                prop(ROW_SOURCE_PROP, Value::Text("abcdefghij".repeat(600))),
            ],
        );
        assert_eq!(
            maps.write(&format).unwrap(),
            fs::read(dir.join("tableprops_created_jet4.bin")).unwrap(),
            "creation time property group bytes"
        );

        // change the table description, remove a column property and change
        // another one
        maps.get_default()
            .put_value(
                DESCRIPTION_PROP,
                Value::Text("the changed table".to_string()),
            )
            .unwrap();
        assert!(maps.get_mut("Name").remove(FORMAT_PROP).is_some());
        maps.get_mut("Note")
            .put_value(DESCRIPTION_PROP, Value::Text("note changed".to_string()))
            .unwrap();
        assert_eq!(
            maps.write(&format).unwrap(),
            fs::read(dir.join("tableprops_updated_jet4.bin")).unwrap(),
            "updated property group bytes"
        );
    }

    // The database/summary/user defined property groups of the `dbprops`
    // scenario are checked against the Java fixtures by
    // `database::tests::database_property_groups_match_java_fixtures`, which
    // goes through the real `DatabaseBuilder` plumbing.

    /// Builds property maps with this port's writer for every `DataType` the
    /// Java reference generator covered and checks that the bytes are exactly
    /// the bytes the Java library wrote for the same values.
    #[test]
    fn writer_matches_java_bytes_for_every_type() {
        let dir = fixture_dir();
        let format = JetFormat::version_4();

        struct Case {
            fixture: &'static str,
            prop: Property,
            flags: u8,
        }

        let cases = vec![
            Case {
                fixture: "type_boolean",
                prop: Property::new(REQUIRED_PROP, DataType::Boolean, Value::Bool(true)),
                flags: 0,
            },
            Case {
                fixture: "type_boolean_nohandler",
                prop: Property::new("CustomBoolProp", DataType::Boolean, Value::Bool(true)),
                flags: SKIP_HANDLER_FLAG,
            },
            Case {
                fixture: "type_boolean_ddl_nohandler",
                prop: Property::new("CustomBoolProp2", DataType::Boolean, Value::Bool(false)),
                flags: DDL_FLAG | SKIP_HANDLER_FLAG,
            },
            Case {
                fixture: "type_byte",
                prop: Property::new("ByteProp", DataType::Byte, Value::Byte(0xF9)),
                flags: 0,
            },
            Case {
                fixture: "type_int",
                prop: Property::new("IntProp", DataType::Int, Value::Int(-12345)),
                flags: 0,
            },
            Case {
                fixture: "type_long",
                prop: Property::new("LongProp", DataType::Long, Value::Long(1234567)),
                flags: 0,
            },
            Case {
                fixture: "type_money",
                prop: Property::new(
                    "MoneyProp",
                    DataType::Money,
                    Value::Decimal {
                        unscaled: -123456789,
                        scale: 4,
                    },
                ),
                flags: 0,
            },
            Case {
                fixture: "type_float",
                prop: Property::new("FloatProp", DataType::Float, Value::Float(1.5)),
                flags: 0,
            },
            Case {
                fixture: "type_double",
                prop: Property::new("DoubleProp", DataType::Double, Value::Double(-2.25)),
                flags: 0,
            },
            Case {
                fixture: "type_date",
                prop: Property::new(
                    "DateProp",
                    DataType::ShortDateTime,
                    Value::DateTime(LocalDateTime::new(2009, 2, 14, 7, 31, 30, 123_000_000)),
                ),
                flags: 0,
            },
            Case {
                fixture: "type_binary",
                prop: Property::new(
                    "BinaryProp",
                    DataType::Binary,
                    Value::Bytes(vec![0, 1, 2, 0x7F, 0x80, 0xFF]),
                ),
                flags: 0,
            },
            Case {
                fixture: "type_text",
                prop: Property::new(
                    "TextProp",
                    DataType::Text,
                    Value::Text("Hello, Wörld".to_string()),
                ),
                flags: 0,
            },
            Case {
                fixture: "type_text_empty",
                prop: Property::new("EmptyTextProp", DataType::Text, Value::Text(String::new())),
                flags: 0,
            },
            Case {
                fixture: "type_memo",
                prop: Property::new(
                    "MemoProp",
                    DataType::Memo,
                    Value::Text("Memo text\nwith ünïcode".to_string()),
                ),
                flags: 0,
            },
            Case {
                fixture: "type_ole",
                prop: Property::new("OleProp", DataType::Ole, Value::Bytes(vec![9, 8, 7, 0, 0])),
                flags: 0,
            },
            Case {
                fixture: "type_guid",
                prop: Property::new(
                    "GuidProp",
                    DataType::Guid,
                    Value::Text("{11223344-5566-7788-99AA-BBCCDDEEFF00}".to_string()),
                ),
                flags: 0,
            },
            Case {
                fixture: "type_guid_pseudo",
                // a "binary" property named GUID is written as a guid
                prop: Property::new(
                    GUID_PROP,
                    DataType::Binary,
                    Value::Text("{11223344-5566-7788-99AA-BBCCDDEEFF00}".to_string()),
                ),
                flags: 0,
            },
            Case {
                fixture: "type_numeric",
                prop: Property::new(
                    "NumericProp",
                    DataType::Numeric,
                    Value::Decimal {
                        unscaled: -123456789012345678,
                        scale: 0,
                    },
                ),
                flags: 0,
            },
            Case {
                fixture: "type_bigint",
                prop: Property::new("BigIntProp", DataType::BigInt, Value::BigInt(i64::MIN)),
                flags: 0,
            },
            Case {
                fixture: "type_extdate",
                prop: Property::new(
                    "ExtDateProp",
                    DataType::ExtDateTime,
                    Value::DateTime(LocalDateTime::new(2009, 2, 13, 23, 31, 30, 0)),
                ),
                flags: 0,
            },
            Case {
                fixture: "type_unicode",
                prop: Property::new(
                    "Tëst Näme",
                    DataType::Text,
                    Value::Text("välue 日本語".to_string()),
                ),
                flags: 0,
            },
        ];

        for case in cases {
            let mut map = PropertyMap::new();
            let mut flags = case.flags;
            if default_def(&case.prop.name).map(|d| d.2).unwrap_or(false) {
                flags |= DDL_FLAG;
            }
            map.put_stored(case.prop.clone(), flags);
            let blob = map
                .write(&format)
                .unwrap_or_else(|e| panic!("{}: {}", case.fixture, e));
            let java = fs::read(dir.join(format!("{}.bin", case.fixture))).unwrap();
            assert_eq!(
                blob, java,
                "{}: bytes differ from the java library",
                case.fixture
            );
        }
    }

    #[test]
    fn named_column_map_roundtrips() {
        let format = JetFormat::version_4();
        let mut map = PropertyMap::with_name("Name", MAP_TYPE_COLUMN);
        map.put(Property::new(
            DESCRIPTION_PROP,
            DataType::Text,
            Value::Text("the name column".to_string()),
        ));
        let blob = map.write(&format).unwrap();
        let back = PropertyMap::read(&blob, &format).unwrap();
        assert_eq!(back.name(), "Name");
        assert_eq!(back.map_type(), MAP_TYPE_COLUMN);
        assert_eq!(back.write(&format).unwrap(), blob);
    }

    /// The Ace formats share the `MR2` block format with Jet4, so a blob read
    /// from a Jet4 file parses the same way with an Ace format.
    #[test]
    fn ace_formats_read_the_same_blobs() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/propcheck/fixtures");
        assert!(dir.is_dir(), "missing template fixtures");
        let bin = fs::read(dir.join("tpl2019_0.bin")).unwrap();
        let format = JetFormat::version_2019();
        let map = PropertyMap::read(&bin, &format).unwrap();
        assert_eq!(
            map.get(ACCESS_VERSION_PROP).unwrap().value,
            Value::Text("09.50".to_string())
        );
        assert!(map.is_ddl(ANSI_QUERY_MODE_PROP));
        assert_eq!(
            map.get("ShowDocumentTabs").unwrap().value,
            Value::Bool(true)
        );
        let maps = PropertyMaps::read(&bin, &format).unwrap();
        assert_eq!(
            maps.write(&format).unwrap(),
            fs::read(dir.join("tpl2019_0.java.bin")).unwrap()
        );
    }

    #[test]
    fn zero_filled_blob_is_empty() {
        let format = JetFormat::version_4();
        let map = PropertyMap::read(&[0u8; 10], &format).unwrap();
        assert!(map.is_empty());
        let maps = PropertyMaps::read(&[0u8; 510], &format).unwrap();
        assert!(maps.is_empty());
    }

    #[test]
    fn date_from_epoch_millis_value() {
        let format = JetFormat::version_4();
        let mut map = PropertyMap::new();
        map.put(Property::new(
            "DateProp",
            DataType::ShortDateTime,
            Value::BigInt(1_234_567_890_123),
        ));
        // a numeric date property is an epoch instant (`toDateLong`), which a
        // LOCAL_DATE_TIME database converts to its zone's wall clock time
        // (`LocalDateTime.ofInstant(..., getZoneId())`)
        let utc = DateContext::new(
            crate::database::DateTimeType::LocalDateTime,
            TimeZone::utc(),
        );
        let blob = map
            .write_with_context(&format, default_charset(&format), utc)
            .unwrap();
        let back =
            PropertyMap::read_with_context(&blob, &format, default_charset(&format), utc).unwrap();
        assert_eq!(
            back.get("DateProp").unwrap().value,
            Value::DateTime(LocalDateTime::from_epoch_millis(1_234_567_890_123))
        );

        let plus8 = DateContext::new(
            crate::database::DateTimeType::LocalDateTime,
            TimeZone::from_offset_minutes(480),
        );
        let blob = map
            .write_with_context(&format, default_charset(&format), plus8)
            .unwrap();
        let back = PropertyMap::read_with_context(&blob, &format, default_charset(&format), plus8)
            .unwrap();
        assert_eq!(
            back.get("DateProp").unwrap().value,
            Value::DateTime(LocalDateTime::from_epoch_millis(
                1_234_567_890_123 + 8 * 3_600_000
            ))
        );
    }

    #[test]
    fn date_property_values_use_the_configured_date_time_type() {
        let format = JetFormat::version_4();
        let charset = default_charset(&format);
        // the fixture stores 2009-02-14T07:31:30.123 wall clock time
        let blob = fs::read(fixture_dir().join("type_date.bin")).unwrap();
        let ldt = DateContext::new(
            crate::database::DateTimeType::LocalDateTime,
            TimeZone::utc(),
        );
        let date = DateContext::new(crate::database::DateTimeType::Date, TimeZone::utc());

        // LOCAL_DATE_TIME reads a LocalDateTime, DATE reads the `DateExt`
        // analogue which keeps the stored bits
        let map = PropertyMap::read_with_context(&blob, &format, charset, ldt).unwrap();
        assert_eq!(
            map.get("DateProp").unwrap().value,
            Value::DateTime(LocalDateTime::new(2009, 2, 14, 7, 31, 30, 123_000_000))
        );
        let map = PropertyMap::read_with_context(&blob, &format, charset, date).unwrap();
        let bits = map.get("DateProp").unwrap().value.clone();
        match bits {
            Value::DateBits(value) => {
                assert_eq!(
                    crate::column::ldt_from_local_date_double(value),
                    LocalDateTime::new(2009, 2, 14, 7, 31, 30, 123_000_000)
                );
            }
            other => panic!("expected DateBits, got {other:?}"),
        }

        // writing the read value back keeps the bytes in both modes (the
        // `DateExt` round trip), which is what the fixture round trip checks
        assert_eq!(
            PropertyMap::read_with_context(&blob, &format, charset, date)
                .unwrap()
                .write_with_context(&format, charset, date)
                .unwrap(),
            blob
        );

        // a sub-millisecond LocalDateTime: LOCAL_DATE_TIME keeps the
        // nanoseconds, DATE truncates to the millisecond like
        // `DefaultDateTimeFactory.toDateDouble`
        let sub_ms = Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789));
        let mut map = PropertyMap::new();
        map.put(Property::new(
            "DateProp",
            DataType::ShortDateTime,
            sub_ms.clone(),
        ));
        let ldt_blob = map.write_with_context(&format, charset, ldt).unwrap();
        let ldt_back = PropertyMap::read_with_context(&ldt_blob, &format, charset, ldt).unwrap();
        // the OLE double cannot hold every nanosecond, so the value comes back
        // through the same `durationFromLocalDateDouble` rounding Java uses
        assert_eq!(
            ldt_back.get("DateProp").unwrap().value,
            Value::DateTime(crate::column::ldt_from_local_date_double(
                datenums::to_date_double(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789))
            ))
        );
        let date_blob = map.write_with_context(&format, charset, date).unwrap();
        assert_ne!(ldt_blob, date_blob);
        let date_back = PropertyMap::read_with_context(&date_blob, &format, charset, date).unwrap();
        match date_back.get("DateProp").unwrap().value.clone() {
            Value::DateBits(value) => assert_eq!(
                crate::column::ldt_from_local_date_double(value),
                LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000)
            ),
            other => panic!("expected DateBits, got {other:?}"),
        }
        // in DATE mode writing the read value back keeps the truncated bits
        assert_eq!(
            date_back
                .write_with_context(&format, charset, date)
                .unwrap(),
            date_blob
        );
    }

    #[test]
    fn default_property_context_matches_the_old_byte_output() {
        // the default (LOCAL_DATE_TIME) context is zone independent for the
        // date value kinds the port has always written (DateTime/DateBits)
        let format = JetFormat::version_4();
        let _charset = default_charset(&format);
        let blob = fs::read(fixture_dir().join("type_date.bin")).unwrap();
        let map = PropertyMap::read(&blob, &format).unwrap();
        assert_eq!(map.write(&format).unwrap(), blob);
    }

    #[test]
    fn truncated_and_corrupt_blobs_do_not_panic() {
        let dir = fixture_dir();
        let format = JetFormat::version_4();
        let bin = fs::read(dir.join("table_jet4.bin")).unwrap();
        for len in 0..bin.len() {
            let _ = PropertyMap::read(&bin[..len], &format);
            let _ = PropertyMaps::read(&bin[..len], &format);
        }
        // flip each of the first 64 bytes of a valid blob
        for idx in 0..bin.len().min(64) {
            let mut corrupt = bin.clone();
            corrupt[idx] ^= 0xFF;
            let _ = PropertyMap::read(&corrupt, &format);
            let _ = PropertyMaps::read(&corrupt, &format);
        }
        // a values block with a name index which is out of range is an error
        let blob = fs::read(dir.join("type_long.bin")).unwrap();
        let mut corrupt = blob.clone();
        // entry: len(2) flags(1) type(1) nameIdx(2) dataLen(2) data(4)
        let pos = corrupt.len() - 8;
        corrupt[pos..pos + 2].copy_from_slice(&7i16.to_le_bytes());
        let err = PropertyMaps::read(&corrupt, &format).unwrap_err();
        assert!(err.to_string().contains("not one of"), "{}", err);
    }
}

#[cfg(all(test, feature = "fixtures"))]
mod util_api_tests {
    use super::*;

    /// `PropertyMap.put`'s four Java overloads, with the same flag byte and
    /// type resolution (`PropertyMapImpl.createProperty`).
    #[test]
    fn put_forms_match_java() {
        let mut map = PropertyMap::new();
        map.put_value(DESCRIPTION_PROP, Value::Text("custom".into()))
            .unwrap();
        // a known property takes its declared type and DDL bit
        assert_eq!(map.get(DESCRIPTION_PROP).unwrap().data_type, DataType::Text);
        assert!(map.is_ddl(DESCRIPTION_PROP));

        // an unknown property derives its type from the value
        map.put_value("CustomLong", Value::Long(7)).unwrap();
        assert_eq!(map.get("CustomLong").unwrap().data_type, DataType::Long);
        assert!(!map.is_ddl("CustomLong"));

        // put(name, DataType, value)
        map.put_typed("CustomBool", DataType::Boolean, Value::Bool(true));
        assert_eq!(map.get("CustomBool").unwrap().data_type, DataType::Boolean);

        // put(name, DataType, value, isDdl)
        map.put_typed_ddl("CustomDdl", DataType::Text, Value::Text("x".into()), true);
        assert!(map.is_ddl("CustomDdl"));
        assert!(!map.is_skip_handler("CustomDdl"));

        // put(name, DataType, value, isDdl, isSkipHandler)
        map.put_typed_ddl_skip_handler("CustomBoth", DataType::Int, Value::Int(3), true, true);
        assert_eq!(map.flags("CustomBoth"), Some(DDL_FLAG | SKIP_HANDLER_FLAG));

        // a common property gets its default DDL bit even when the explicit
        // flag is false (`createProperty` ORs it in)
        map.put_typed_ddl("Caption", DataType::Memo, Value::Text("c".into()), false);
        assert!(map.is_ddl("Caption"));
    }

    /// size/get_value_default/stream/remove are the Java convenience accessors.
    #[test]
    fn convenience_accessors_match_java() {
        let mut map = PropertyMap::new();
        assert!(map.is_empty());
        map.put_value("A", Value::Long(1)).unwrap();
        map.put_value("B", Value::Text("b".into())).unwrap();
        assert_eq!(map.size(), 2);
        assert_eq!(map.size(), map.len());
        assert_eq!(map.get_value_default("A", &Value::Null), &Value::Long(1));
        assert_eq!(
            map.get_value_default("Missing", &Value::Long(9)),
            &Value::Long(9)
        );
        let names: Vec<&str> = map.stream().map(|p| p.name()).collect();
        assert_eq!(names, vec!["A", "B"]);
        let removed = map.remove("A").unwrap();
        assert_eq!(removed.name, "A");
        assert_eq!(map.size(), 1);
    }

    /// The blob written with the convenience forms is byte identical to the
    /// original two-argument form of the same property.
    #[test]
    fn convenience_forms_round_trip_identically() {
        let format = JetFormat::version_4();
        let mut direct = PropertyMap::new();
        direct.put(Property::new(
            "Description",
            DataType::Text,
            Value::Text("table".into()),
        ));
        direct.put(Property::new("Custom", DataType::Long, Value::Long(42)));

        let mut convenience = PropertyMap::new();
        convenience
            .put_value("Description", Value::Text("table".into()))
            .unwrap();
        convenience.put_value("Custom", Value::Long(42)).unwrap();

        assert_eq!(
            direct.write(&format).unwrap(),
            convenience.write(&format).unwrap()
        );

        // the 5 argument form with an explicit skip-handler bit writes a flags
        // byte a read maps back to the same accessors
        let mut flagged = PropertyMap::new();
        flagged.put_typed_ddl_skip_handler("Custom", DataType::Long, Value::Long(42), false, true);
        let bytes = flagged.write(&format).unwrap();
        let read = PropertyMap::read(&bytes, &format).unwrap();
        assert!(read.is_skip_handler("Custom"));
        assert!(!read.is_ddl("Custom"));
        assert_eq!(read.get_value("Custom"), Some(&Value::Long(42)));
    }
}
