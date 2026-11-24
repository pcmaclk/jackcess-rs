//! Port of `com.healthmarketscience.jackcess.util.ColumnValidator` /
//! `ColumnValidatorFactory`, the internal (`ColumnImpl.InternalColumnValidator`)
//! validators and the validation contexts they use.
//!
//! A column validator validates and/or manipulates a value on its way into a
//! row.  The Java interface is
//!
//! ```java
//! public interface ColumnValidator {
//!   public Object validate(Column col, Object val) throws IOException;
//! }
//! ```
//!
//! and the factory creates the default validator for a column:
//!
//! ```java
//! public interface ColumnValidatorFactory {
//!   public ColumnValidator createValidator(Column col);
//! }
//! ```
//!
//! # Java name mapping
//!
//! | Java | Rust |
//! |---|---|
//! | `util.ColumnValidator` | [`ColumnValidator`] |
//! | `util.ColumnValidatorFactory` | [`ColumnValidatorFactory`] |
//! | `util.SimpleColumnValidator` / `INSTANCE` | [`SimpleColumnValidator`] / [`SimpleColumnValidator::INSTANCE`] |
//! | `util.SimpleColumnValidatorFactory` / `INSTANCE` | [`SimpleColumnValidatorFactory`] / [`SimpleColumnValidatorFactory::INSTANCE`] |
//! | `impl.ColumnImpl.getColumnValidator()` / `setColumnValidator(...)` | [`ColumnValidatorHandle`] (the value a `Column` stores; see the integration note below) |
//! | `impl.InternalColumnValidator` | [`DefaultColumnValidator`] (the property driven chain: required, allow-zero-length, validation rule) |
//! | `impl.ColumnImpl.RequiredColValidator` | [`RequiredColumnValidator`] |
//! | `impl.ColumnImpl.NoZeroLenColValidator` | [`NoZeroLengthColumnValidator`] |
//! | `impl.ColValidatorEvalContext` | [`ValidationRuleColumnValidator`] |
//! | `Column validate(Object)` context (`ColumnImpl.withErrorContext`) | [`ColumnValidatorContext`] |
//!
//! Java's `Column` carries its owning table/database, so
//! `ColumnImpl.withErrorContext` can build the
//! `"<msg> (Db=...;Table=...;Column=...)"` text.  The Rust [`crate::column::Column`]
//! is a plain value without an owner, so that information is supplied by the
//! caller through a [`ColumnValidatorContext`].  The resulting message is
//! byte-identical to the Java one when the context is populated (the database
//! name is the database file's simple name, exactly like
//! `DatabaseImpl.getName()`).
//!
//! # Default behaviour
//!
//! As in the Java library the default (`SimpleColumnValidator`) returns the
//! value unchanged.  The property driven checks are preserved exactly:
//!
//! * the `Required` property rejects a null value
//!   (`"Missing value for required column"`),
//! * the `AllowZeroLength` property (textual columns, default `true`) rejects
//!   the empty string (`"Zero length string is not allowed"`) and hands the
//!   value's text form to the outer checks,
//! * a `ValidationRule` property evaluates as a field rule
//!   (`[ThisColumn]` implicit left value); a false/`Null` result rejects the
//!   value with `ValidationText` (or
//!   `"Invalid column value '<value>'"`),
//! * the user (external) validator always runs first, then the internal checks
//!   in Java's wrapping order (`InternalColumnValidator.validate` delegates
//!   before it validates), and autonumber/calculated columns have no internal
//!   checks at all (`reloadPropertiesValidators` returns early for them).
//!
//! # Integration
//!
//! [`crate::column::Column`] carries the external validator slot
//! (`pub column_validator: Option<ColumnValidatorHandle>`) with
//! `get_column_validator()` (defaulting to [`SimpleColumnValidator`]) and
//! `set_column_validator(Option<...>)`.  The property driven checks are built
//! by [`DefaultColumnValidator`]:
//!
//! ```ignore
//! let context = ColumnValidatorContext::for_column(&col)
//!     .with_database_and_table(db_name, table_name)
//!     .with_zone(Some(db.tz));
//! let value = DefaultColumnValidator::for_column(&col)   // uses the column's
//!     .with_rule(rule)                                  // external validator
//!     .validate(&context, value)?;
//! ```
//!
//! `Table`'s row path (`crate::table`) still applies the same three checks
//! inline (`Column::validate_value` plus `Table::validate_column_rule`); a
//! follow-up in `table.rs` should validate through
//! [`DefaultColumnValidator::for_column`] instead, so a custom
//! `Column::column_validator` runs on `Table::add_row`/`update_row` (Java's
//! `ColumnImpl.validate`).
//!
//! A `Database` can store the factory as
//! `Option<ColumnValidatorFactoryHandle>` (default
//! [`SimpleColumnValidatorFactory`]); `getColumnValidatorFactory` /
//! `setColumnValidatorFactory` are owned by the database workstream.

use std::sync::Arc;

use crate::column::{to_char_sequence, Column, ValidationRule};
use crate::datenums::TimeZone;
use crate::error::{Error, Result};
use crate::expr::{ExprError, ExprValue};
use crate::value::Value;

/// `com.healthmarketscience.jackcess.util.ColumnValidator`.
///
/// A validator may return a different value (Java's `Object` return); an
/// invalid value is reported as [`Error::InvalidValue`]
/// (`InvalidValueException`).  `validate` takes `&self` because the Java
/// interface does not require an exclusive receiver; a stateful validator uses
/// interior mutability.
pub trait ColumnValidator {
    /// `ColumnValidator.validate(Column col, Object val)`: validates and/or
    /// manipulates the given potential new value for the given column.
    fn validate(&self, context: &ColumnValidatorContext<'_>, value: Value) -> Result<Value>;

    /// Whether this is the default (value preserving) validator.
    fn is_simple(&self) -> bool {
        false
    }

    /// The Java `toString()` of a validator (`InternalColumnValidator.toString`
    /// builds `{custom=...;required=true;...}`).
    fn description(&self) -> String {
        "custom".to_string()
    }
}

/// `com.healthmarketscience.jackcess.util.ColumnValidatorFactory`: creates the
/// validator of a column when none was set explicitly.
pub trait ColumnValidatorFactory {
    /// `ColumnValidatorFactory.createValidator(Column col)`: the validator for
    /// the column, or `None` to use the default
    /// ([`SimpleColumnValidator`], like Java's `null` return).
    fn create_validator(&self, column: &Column) -> Option<ColumnValidatorHandle>;
}

/// `com.healthmarketscience.jackcess.util.SimpleColumnValidator`: returns the
/// value unchanged.
#[derive(Clone, Copy, Debug, Default)]
pub struct SimpleColumnValidator;

impl SimpleColumnValidator {
    /// `SimpleColumnValidator.INSTANCE`.
    pub const INSTANCE: SimpleColumnValidator = SimpleColumnValidator;

    /// A shared handle of the default validator.
    pub fn handle() -> ColumnValidatorHandle {
        ColumnValidatorHandle::new(SimpleColumnValidator::INSTANCE)
    }
}

impl ColumnValidator for SimpleColumnValidator {
    fn validate(&self, _context: &ColumnValidatorContext<'_>, value: Value) -> Result<Value> {
        Ok(value)
    }

    fn is_simple(&self) -> bool {
        true
    }

    fn description(&self) -> String {
        "simple".to_string()
    }
}

/// `com.healthmarketscience.jackcess.util.SimpleColumnValidatorFactory`: the
/// default factory, which returns [`SimpleColumnValidator::INSTANCE`] for every
/// column.
#[derive(Clone, Copy, Debug, Default)]
pub struct SimpleColumnValidatorFactory;

impl SimpleColumnValidatorFactory {
    /// `SimpleColumnValidatorFactory.INSTANCE`.
    pub const INSTANCE: SimpleColumnValidatorFactory = SimpleColumnValidatorFactory;
}

impl ColumnValidatorFactory for SimpleColumnValidatorFactory {
    fn create_validator(&self, _column: &Column) -> Option<ColumnValidatorHandle> {
        Some(SimpleColumnValidator::handle())
    }
}

/// A shared, type-erased [`ColumnValidator`].
///
/// Java stores a plain validator object in `ColumnImpl._validator`.  The Rust
/// [`Column`] is `Clone` (a column value is copied out of a table all the
/// time), so the validator slot is a cheaply clonable handle instead of a
/// `Box`; this keeps `#[derive(Clone, Debug)]` working on `Column` without
/// requiring every custom validator to be `Debug`.
#[derive(Clone)]
pub struct ColumnValidatorHandle(Arc<dyn ColumnValidator>);

impl std::fmt::Debug for ColumnValidatorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ColumnValidator({})", self.0.description())
    }
}

impl ColumnValidatorHandle {
    /// Wraps a validator (`Column.setColumnValidator(newValidator)`).
    pub fn new(validator: impl ColumnValidator + 'static) -> ColumnValidatorHandle {
        ColumnValidatorHandle(Arc::new(validator))
    }

    /// Wraps an already shared validator.
    pub fn from_arc(validator: Arc<dyn ColumnValidator>) -> ColumnValidatorHandle {
        ColumnValidatorHandle(validator)
    }

    /// The shared pointer behind the handle.
    pub fn as_arc(&self) -> &Arc<dyn ColumnValidator> {
        &self.0
    }

    /// The validator itself.
    // Part of the handle's public API, mirroring the `Arc` accessors.
    #[expect(
        clippy::should_implement_trait,
        reason = "public API keeps the `as_ref` name used by the handle's callers"
    )]
    pub fn as_ref(&self) -> &dyn ColumnValidator {
        self.0.as_ref()
    }

    /// `ColumnValidator.validate(Column, Object)`.
    pub fn validate(&self, context: &ColumnValidatorContext<'_>, value: Value) -> Result<Value> {
        self.0.validate(context, value)
    }

    /// Whether this is the value preserving default validator.
    pub fn is_simple(&self) -> bool {
        self.0.is_simple()
    }

    /// The Java `toString()` of the wrapped validator.
    pub fn description(&self) -> String {
        self.0.description()
    }
}

/// A shared [`ColumnValidatorFactory`] (the value a `Database` stores for
/// `getColumnValidatorFactory`).
#[derive(Clone)]
pub struct ColumnValidatorFactoryHandle(Arc<dyn ColumnValidatorFactory>);

impl std::fmt::Debug for ColumnValidatorFactoryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ColumnValidatorFactory")
    }
}

impl ColumnValidatorFactoryHandle {
    pub fn new(factory: impl ColumnValidatorFactory + 'static) -> ColumnValidatorFactoryHandle {
        ColumnValidatorFactoryHandle(Arc::new(factory))
    }

    /// The default factory (`SimpleColumnValidatorFactory.INSTANCE`).
    pub fn simple() -> ColumnValidatorFactoryHandle {
        ColumnValidatorFactoryHandle::new(SimpleColumnValidatorFactory::INSTANCE)
    }

    pub fn create_validator(&self, column: &Column) -> Option<ColumnValidatorHandle> {
        self.0.create_validator(column)
    }
}

impl Default for ColumnValidatorFactoryHandle {
    fn default() -> ColumnValidatorFactoryHandle {
        ColumnValidatorFactoryHandle::simple()
    }
}

/// The context of a validation (`ColumnImpl.withErrorContext`'s
/// database/table/column information plus the database zone the expression
/// rule is evaluated in).
///
/// `Db=<name>;Table=<name>;Column=<name>` is the exact suffix the Java library
/// appends to validator messages; the database name is the file's simple name
/// (`DatabaseImpl.getName`).
#[derive(Clone, Debug)]
pub struct ColumnValidatorContext<'a> {
    column: &'a Column,
    /// the `Db=...;Table=...;Column=...` text, when the caller supplied it
    suffix: Option<String>,
    /// the database zone (`DatabaseImpl.getZoneId`) used to convert date
    /// values for an expression rule; `None` uses UTC
    zone: Option<TimeZone>,
}

impl<'a> ColumnValidatorContext<'a> {
    /// A standalone context for the column (validator messages have no
    /// database/table suffix, unlike Java where they always do).
    pub fn new(column: &'a Column) -> ColumnValidatorContext<'a> {
        ColumnValidatorContext {
            column,
            suffix: None,
            zone: None,
        }
    }

    /// Alias of [`ColumnValidatorContext::new`].
    pub fn for_column(column: &'a Column) -> ColumnValidatorContext<'a> {
        ColumnValidatorContext::new(column)
    }

    /// Supplies the database/table names used by Java's error context.
    pub fn with_database_and_table(
        mut self,
        database_name: &str,
        table_name: &str,
    ) -> ColumnValidatorContext<'a> {
        self.suffix = Some(format!(
            "Db={};Table={};Column={}",
            database_name, table_name, self.column.name
        ));
        self
    }

    /// Supplies a pre-built context suffix (the text inside the parentheses).
    pub fn with_context_suffix(mut self, suffix: impl Into<String>) -> ColumnValidatorContext<'a> {
        self.suffix = Some(suffix.into());
        self
    }

    /// The database zone used by an expression rule (Java:
    /// `db.getZoneId()`).
    pub fn with_zone(mut self, zone: Option<TimeZone>) -> ColumnValidatorContext<'a> {
        self.zone = zone;
        self
    }

    /// The column being validated.
    pub fn column(&self) -> &'a Column {
        self.column
    }

    /// The database/table/column suffix, if one was supplied.
    pub fn context_suffix(&self) -> Option<&str> {
        self.suffix.as_deref()
    }

    /// The database zone, defaulting to UTC (the port's zones are fixed
    /// offsets; Java always has the database zone).
    pub fn zone(&self) -> TimeZone {
        self.zone.unwrap_or_else(TimeZone::utc)
    }

    /// `ColumnImpl.withErrorContext(msg)`: `"<msg> (Db=...;Table=...;Column=...)"`,
    /// or just the message when the context is detached.
    pub fn error_context(&self, message: impl std::fmt::Display) -> String {
        match &self.suffix {
            Some(suffix) => format!("{message} ({suffix})"),
            None => message.to_string(),
        }
    }
}

/// Port of `ColumnImpl.RequiredColValidator`: delegates first, then rejects a
/// null value.
#[derive(Clone, Debug)]
pub struct RequiredColumnValidator {
    delegate: ColumnValidatorHandle,
}

impl RequiredColumnValidator {
    pub fn new(delegate: ColumnValidatorHandle) -> RequiredColumnValidator {
        RequiredColumnValidator { delegate }
    }

    /// The wrapped ("external") validator.
    pub fn external(&self) -> &ColumnValidatorHandle {
        &self.delegate
    }
}

impl ColumnValidator for RequiredColumnValidator {
    fn validate(&self, context: &ColumnValidatorContext<'_>, value: Value) -> Result<Value> {
        let value = self.delegate.validate(context, value)?;
        if value.is_null() {
            return Err(Error::InvalidValue(
                context.error_context("Missing value for required column"),
            ));
        }
        Ok(value)
    }

    fn description(&self) -> String {
        let mut out = String::from("{");
        if !self.delegate.is_simple() {
            out.push_str(&format!("custom={};", self.delegate.description()));
        }
        out.push_str("required=true}");
        out
    }
}

/// Port of `ColumnImpl.NoZeroLenColValidator`: delegates first, then rejects an
/// empty string for a non-null value and returns the text form
/// (`ColumnImpl.toCharSequence`) the way the Java validator does.
#[derive(Clone, Debug)]
pub struct NoZeroLengthColumnValidator {
    delegate: ColumnValidatorHandle,
}

impl NoZeroLengthColumnValidator {
    pub fn new(delegate: ColumnValidatorHandle) -> NoZeroLengthColumnValidator {
        NoZeroLengthColumnValidator { delegate }
    }

    /// The wrapped ("external") validator.
    pub fn external(&self) -> &ColumnValidatorHandle {
        &self.delegate
    }
}

impl ColumnValidator for NoZeroLengthColumnValidator {
    fn validate(&self, context: &ColumnValidatorContext<'_>, value: Value) -> Result<Value> {
        let value = self.delegate.validate(context, value)?;
        if value.is_null() {
            // oddly enough null is allowed for non zero length strings
            return Ok(value);
        }
        let text = match to_char_sequence(&value) {
            Ok(text) => text,
            Err(_) => {
                return Err(Error::InvalidValue(
                    context.error_context("Value cannot be converted to text"),
                ))
            }
        };
        if text.is_empty() {
            return Err(Error::InvalidValue(
                context.error_context("Zero length string is not allowed"),
            ));
        }
        Ok(Value::Text(text))
    }

    fn description(&self) -> String {
        let mut out = String::from("{");
        if !self.delegate.is_simple() {
            out.push_str(&format!("custom={};", self.delegate.description()));
        }
        out.push_str("allowZeroLength=false}");
        out
    }
}

/// Port of `impl.ColValidatorEvalContext.toColumnValidator`: a field
/// validation rule as a column validator.
///
/// The rule is parsed lazily like the Java `RawExpr`: a parse failure is
/// reported when the rule is evaluated, not when the validator is built.
#[derive(Clone, Debug)]
pub struct ValidationRuleColumnValidator {
    delegate: ColumnValidatorHandle,
    rule: ValidationRule,
}

impl ValidationRuleColumnValidator {
    pub fn new(
        delegate: ColumnValidatorHandle,
        rule: ValidationRule,
    ) -> ValidationRuleColumnValidator {
        ValidationRuleColumnValidator { delegate, rule }
    }

    /// The wrapped ("external") validator.
    pub fn external(&self) -> &ColumnValidatorHandle {
        &self.delegate
    }

    /// The validation rule.
    pub fn rule(&self) -> &ValidationRule {
        &self.rule
    }
}

impl ColumnValidator for ValidationRuleColumnValidator {
    fn validate(&self, context: &ColumnValidatorContext<'_>, value: Value) -> Result<Value> {
        let value = self.delegate.validate(context, value)?;
        validate_rule(context, &self.rule, value)
    }

    fn description(&self) -> String {
        let mut out = String::from("{");
        if !self.delegate.is_simple() {
            out.push_str(&format!("custom={};", self.delegate.description()));
        }
        out.push_str("expression=");
        out.push_str(self.rule.text.as_deref().unwrap_or("<rule>"));
        out.push('}');
        out
    }
}

/// `ColumnImpl.validate` for the internal (property driven) validators, plus an
/// optional user ("external") validator, in Java's wrapping order:
/// external, `Required`, `AllowZeroLength`, then the validation rule.
///
/// `Column::validate_value` already applies the required/zero-length checks
/// inline; this type is the object form the Java API exposes, and is what a
/// `Column` should validate through once it carries a
/// [`ColumnValidatorHandle`].
#[derive(Clone, Debug)]
pub struct DefaultColumnValidator {
    external: ColumnValidatorHandle,
    required: bool,
    check_zero_length: bool,
    rule: Option<ValidationRule>,
    zone: Option<TimeZone>,
}

impl DefaultColumnValidator {
    /// A chain over the given external validator with no internal checks.
    pub fn new(external: ColumnValidatorHandle) -> DefaultColumnValidator {
        DefaultColumnValidator {
            external,
            required: false,
            check_zero_length: false,
            rule: None,
            zone: None,
        }
    }

    /// The property driven checks of a column
    /// (`ColumnImpl.reloadPropertiesValidators`): autonumber and calculated
    /// columns have none, otherwise `Required` and (textual columns with
    /// `AllowZeroLength` off) the zero length check.
    ///
    /// The external validator is the one stored on the column
    /// ([`crate::column::Column::get_column_validator`]), i.e.
    /// `ColumnImpl.reloadPropertiesValidators`'s `_validator =
    /// getColumnValidator()`.
    pub fn for_column(column: &Column) -> DefaultColumnValidator {
        DefaultColumnValidator::new(column.get_column_validator()).with_column_properties(column)
    }

    /// Adds the required/zero-length checks of the column (see
    /// [`DefaultColumnValidator::for_column`]).
    pub fn with_column_properties(mut self, column: &Column) -> DefaultColumnValidator {
        if column.auto_number || column.calculated {
            return self;
        }
        self.required = column.required;
        self.check_zero_length = column.data_type.is_textual() && !column.allow_zero_length;
        self
    }

    /// The external (user defined) validator.
    pub fn external(&self) -> &ColumnValidatorHandle {
        &self.external
    }

    /// `ColumnImpl.setColumnValidator` for the external slot: the internal
    /// validators stay in place.
    pub fn set_external(&mut self, external: ColumnValidatorHandle) {
        self.external = external;
    }

    /// Builder form of [`DefaultColumnValidator::set_external`].
    pub fn with_external(mut self, external: ColumnValidatorHandle) -> DefaultColumnValidator {
        self.set_external(external);
        self
    }

    /// The `Required` property check (`RequiredColValidator`).
    pub fn with_required(mut self, required: bool) -> DefaultColumnValidator {
        self.required = required;
        self
    }

    /// The `AllowZeroLength` property check (`NoZeroLenColValidator`).
    pub fn with_zero_length_check(mut self, check: bool) -> DefaultColumnValidator {
        self.check_zero_length = check;
        self
    }

    /// The field validation rule (`ColValidatorEvalContext`).
    pub fn with_rule(mut self, rule: Option<ValidationRule>) -> DefaultColumnValidator {
        self.rule = rule;
        self
    }

    /// The database zone used by the expression rule.
    pub fn with_zone(mut self, zone: Option<TimeZone>) -> DefaultColumnValidator {
        self.zone = zone;
        self
    }

    /// Turns this chain into the shared handle a `Column` stores.
    pub fn into_handle(self) -> ColumnValidatorHandle {
        ColumnValidatorHandle::new(self)
    }
}

impl ColumnValidator for DefaultColumnValidator {
    fn validate(&self, context: &ColumnValidatorContext<'_>, value: Value) -> Result<Value> {
        // the chain may carry the database zone itself (Java reads it from the
        // database); prefer it when the caller did not build a zoned context
        let zoned_context;
        let context = if context.zone.is_none() && self.zone.is_some() {
            let mut cloned = context.clone();
            cloned.zone = self.zone;
            zoned_context = cloned;
            &zoned_context
        } else {
            context
        };
        // `InternalColumnValidator.validate`: the external validator runs
        // first, then the internal checks from the innermost wrapper outwards
        let mut value = self.external.validate(context, value)?;
        if self.required && value.is_null() {
            return Err(Error::InvalidValue(
                context.error_context("Missing value for required column"),
            ));
        }
        if self.check_zero_length && context.column().data_type.is_textual() && !value.is_null() {
            value = match to_char_sequence(&value) {
                Ok(text) => {
                    if text.is_empty() {
                        return Err(Error::InvalidValue(
                            context.error_context("Zero length string is not allowed"),
                        ));
                    }
                    Value::Text(text)
                }
                Err(_) => {
                    return Err(Error::InvalidValue(
                        context.error_context("Value cannot be converted to text"),
                    ))
                }
            };
        }
        if let Some(rule) = &self.rule {
            value = validate_rule(context, rule, value)?;
        }
        Ok(value)
    }

    fn description(&self) -> String {
        let mut out = String::from("{");
        let mut wrote = false;
        if !self.external.is_simple() {
            out.push_str(&format!("custom={}", self.external.description()));
            wrote = true;
        }
        if self.required {
            if wrote {
                out.push(';');
            }
            out.push_str("required=true");
            wrote = true;
        }
        if self.check_zero_length {
            if wrote {
                out.push(';');
            }
            out.push_str("allowZeroLength=false");
            wrote = true;
        }
        if let Some(rule) = &self.rule {
            if wrote {
                out.push(';');
            }
            out.push_str("expression=");
            out.push_str(rule.text.as_deref().unwrap_or("<rule>"));
        }
        out.push('}');
        out
    }
}

/// `ColValidatorEvalContext.validate`: evaluates the field rule as a condition
/// and rejects the value when the result is false/`Null`.
fn validate_rule(
    context: &ColumnValidatorContext<'_>,
    rule: &ValidationRule,
    value: Value,
) -> Result<Value> {
    let expression = match &rule.expression {
        Ok(expression) => expression,
        Err(error) => return Err(rule_eval_error(context, error)),
    };
    let column = context.column();
    // `ColValidatorEvalContext.getIdentifierValue`: a field rule can only read
    // "this" column (which it may name explicitly)
    let table_name = context_suffix_table(context);
    for identifier in expression.identifiers() {
        let collection_is_this_table = match &identifier.collection {
            None => true,
            Some(collection) => {
                table_name.is_some_and(|table| collection.eq_ignore_ascii_case(table))
            }
        };
        if !collection_is_this_table || !identifier.object.eq_ignore_ascii_case(&column.name) {
            return Err(Error::Jackcess(context.error_context(format!(
                "Cannot access other fields for {identifier}"
            ))));
        }
    }
    let this_value = value_to_expr_value(&value, context)?;
    let mut eval_context = crate::expr::EvalContext::new();
    if let Some(table) = table_name {
        eval_context.set_table_name(table);
    }
    eval_context.set_column(&column.name, this_value.clone());
    eval_context.set_this_value(this_value);
    match expression.eval_condition(&eval_context) {
        Ok(true) => Ok(value),
        Ok(false) => {
            let message = rule.message(|| {
                format!(
                    "Invalid column value '{}'",
                    crate::column::value_message_text(&value)
                )
            });
            Err(Error::InvalidValue(context.error_context(message)))
        }
        Err(error) => Err(rule_eval_error(context, &error)),
    }
}

/// The table name inside a `Db=...;Table=...;Column=...` suffix, if present.
fn context_suffix_table<'a>(context: &'a ColumnValidatorContext<'_>) -> Option<&'a str> {
    let suffix = context.context_suffix()?;
    for part in suffix.split(';') {
        if let Some(table) = part.strip_prefix("Table=") {
            return Some(table);
        }
    }
    None
}

/// `BaseEvalContext.eval`'s mapping of a rule failure: `EvalException` /
/// `ParseException` become a contextual `JackcessException`, while a construct
/// the port does not implement stays [`Error::Unsupported`].
fn rule_eval_error(context: &ColumnValidatorContext<'_>, error: &ExprError) -> Error {
    let message = match error {
        ExprError::Parse(message)
        | ExprError::Eval(message)
        | ExprError::Arithmetic(message)
        | ExprError::Unsupported(message) => message,
    };
    let contextual = context.error_context(message);
    match error {
        ExprError::Unsupported(_) => Error::Unsupported(contextual),
        _ => Error::Jackcess(contextual),
    }
}

/// `BaseEvalContext.toValue` for a column value (the same conversion the row
/// validation of `Table` uses; date doubles are interpreted in the database
/// zone).
fn value_to_expr_value(value: &Value, context: &ColumnValidatorContext<'_>) -> Result<ExprValue> {
    Ok(match value {
        Value::Null => ExprValue::Null,
        Value::Bool(value) => ExprValue::Bool(*value),
        Value::Byte(value) => ExprValue::Byte(*value as i8),
        Value::Int(value) => ExprValue::Int(*value),
        Value::Long(value) => ExprValue::Long(*value),
        Value::BigInt(value) => ExprValue::BigInt(*value),
        Value::Decimal { unscaled, scale } => ExprValue::Decimal(
            crate::expr::Decimal::from_unscaled(*unscaled, *scale).map_err(expr_error)?,
        ),
        Value::Float(value) => ExprValue::Double(*value as f64),
        Value::Double(value) => ExprValue::Double(*value),
        Value::Text(value) => ExprValue::Text(value.clone()),
        Value::DateTime(value) => ExprValue::date_time(*value),
        Value::DateBits(bits) => {
            let zone = context.zone();
            let millis = crate::datenums::from_date_double(*bits, &zone);
            ExprValue::date_time(zone.local_date_time_from_millis(millis))
        }
        other => {
            return Err(Error::Unsupported(format!(
                "Cannot use a {:?} value in an expression",
                other
            )))
        }
    })
}

fn expr_error(error: ExprError) -> Error {
    match error {
        ExprError::Unsupported(message) => Error::Unsupported(message),
        other => Error::Jackcess(other.to_string()),
    }
}

#[cfg(test)]
fn test_column(name: &str, data_type: crate::datatype::DataType) -> Column {
    // the public `Column` fields, with the values a table definition read
    // leaves them at (see `Column::read`)
    Column {
        name: name.to_string(),
        data_type,
        column_number: 0,
        column_id: 0,
        column_length: 0,
        variable_length: true,
        auto_number: false,
        calculated: false,
        complex_value_fk: false,
        hidden: false,
        security_identifier: false,
        var_len_table_index: 0,
        fixed_data_offset: 0,
        flags: 0,
        ext_flags: 0,
        precision: 0,
        scale: 0,
        sort_order: None,
        code_page: 0,
        compressed_unicode: false,
        display_index: 0,
        column_index: 0,
        umap_page_number: -1,
        umap_owned_row_number: 0,
        umap_free_row_number: 0,
        append_only: false,
        complex_id: 0,
        length_in_units: -1,
        default_value: None,
        required: false,
        allow_zero_length: true,
        column_validator: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatype::DataType;

    fn column(name: &str, data_type: DataType) -> Column {
        test_column(name, data_type)
    }

    /// `SimpleColumnValidator.validate` returns the value unchanged.
    #[test]
    fn simple_validator_returns_the_value() {
        let col = column("c", DataType::Text);
        let context = ColumnValidatorContext::new(&col);
        let value = Value::Text("x".into());
        assert_eq!(
            SimpleColumnValidator::INSTANCE
                .validate(&context, value.clone())
                .unwrap(),
            value
        );
        assert!(SimpleColumnValidator::INSTANCE.is_simple());
        assert!(SimpleColumnValidator::handle().is_simple());
    }

    /// The three built-in validators, in Java's order and with Java's messages.
    #[test]
    fn builtin_validators_match_java() {
        let mut col = column("c", DataType::Text);
        col.required = true;
        col.allow_zero_length = false;
        let context =
            ColumnValidatorContext::new(&col).with_context_suffix("Db=db.mdb;Table=T;Column=c");
        let validator = DefaultColumnValidator::for_column(&col);
        assert!(validator.required);
        assert!(validator.check_zero_length);

        let error = validator.validate(&context, Value::Null).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Missing value for required column (Db=db.mdb;Table=T;Column=c)"
        );

        let error = validator
            .validate(&context, Value::Text(String::new()))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Zero length string is not allowed (Db=db.mdb;Table=T;Column=c)"
        );

        // the zero length validator hands back the text form
        assert_eq!(
            validator.validate(&context, Value::Long(12)).unwrap(),
            Value::Text("12".into())
        );

        // autonumber columns have no property driven validators
        col.auto_number = true;
        let validator = DefaultColumnValidator::for_column(&col);
        assert!(!validator.required);
        assert!(!validator.check_zero_length);
    }

    /// The user validator runs before the internal checks and can change the
    /// value (`InternalColumnValidator.validate`).
    #[test]
    fn external_validator_runs_first() {
        struct Doubler;
        impl ColumnValidator for Doubler {
            fn validate(
                &self,
                _context: &ColumnValidatorContext<'_>,
                value: Value,
            ) -> Result<Value> {
                Ok(match value {
                    Value::Long(value) => Value::Long(value * 2),
                    other => other,
                })
            }

            fn description(&self) -> String {
                "doubler".to_string()
            }
        }
        let col = column("c", DataType::Long);
        let context = ColumnValidatorContext::new(&col);
        let validator = DefaultColumnValidator::new(ColumnValidatorHandle::new(Doubler));
        assert_eq!(
            validator.validate(&context, Value::Long(21)).unwrap(),
            Value::Long(42)
        );
        assert_eq!(validator.description(), "{custom=doubler}");
    }

    /// A field rule is evaluated with `[ThisColumn]` / the column name and the
    /// `ValidationText` (or the Java default message) on rejection.
    #[test]
    fn validation_rule_messages_match_java() {
        let mut col = column("Code", DataType::Text);
        col.required = false;
        let rule = ValidationRule::field("Len([Code]) = 3", Some("Code must have 3 chars"));
        let validator = DefaultColumnValidator::for_column(&col).with_rule(Some(rule));
        let context =
            ColumnValidatorContext::new(&col).with_context_suffix("Db=db.mdb;Table=T;Column=Code");
        assert_eq!(
            validator
                .validate(&context, Value::Text("abc".into()))
                .unwrap(),
            Value::Text("abc".into())
        );
        assert_eq!(
            validator
                .validate(&context, Value::Text("ab".into()))
                .unwrap_err()
                .to_string(),
            "Code must have 3 chars (Db=db.mdb;Table=T;Column=Code)"
        );

        let rule = ValidationRule::field("[Code] > 3", None);
        let validator = DefaultColumnValidator::for_column(&col).with_rule(Some(rule));
        assert_eq!(
            validator
                .validate(&context, Value::Long(2))
                .unwrap_err()
                .to_string(),
            "Invalid column value '2' (Db=db.mdb;Table=T;Column=Code)"
        );

        // a rule which reads another field is rejected like
        // `ColValidatorEvalContext.getIdentifierValue`
        let rule = ValidationRule::field("[Other] = 1", None);
        let validator = DefaultColumnValidator::for_column(&col).with_rule(Some(rule));
        assert_eq!(
            validator
                .validate(&context, Value::Long(1))
                .unwrap_err()
                .to_string(),
            "Cannot access other fields for [Other] (Db=db.mdb;Table=T;Column=Code)"
        );
    }

    /// The factory default is the simple validator.
    #[test]
    fn simple_factory_returns_the_simple_validator() {
        let col = column("c", DataType::Text);
        let factory = ColumnValidatorFactoryHandle::simple();
        let validator = factory.create_validator(&col).expect("a validator");
        assert!(validator.is_simple());
    }
}

#[cfg(test)]
mod column_integration_tests {
    use super::*;
    use crate::datatype::DataType;

    fn column(name: &str, data_type: DataType) -> Column {
        super::test_column(name, data_type)
    }

    struct Doubler;
    impl ColumnValidator for Doubler {
        fn validate(&self, _context: &ColumnValidatorContext<'_>, value: Value) -> Result<Value> {
            Ok(match value {
                Value::Long(value) => Value::Long(value * 2),
                other => other,
            })
        }
    }

    /// `Column.getColumnValidator` / `setColumnValidator` store the external
    /// validator, and `DefaultColumnValidator::for_column` uses it
    /// (`reloadPropertiesValidators`'s `_validator = getColumnValidator()`).
    #[test]
    fn column_validator_slot_is_honored() {
        let mut col = column("c", DataType::Long);
        assert!(col.get_column_validator().is_simple());
        col.set_column_validator(Some(ColumnValidatorHandle::new(Doubler)));
        assert!(!col.get_column_validator().is_simple());
        let context = ColumnValidatorContext::new(&col);
        let validator = DefaultColumnValidator::for_column(&col);
        assert_eq!(
            validator.validate(&context, Value::Long(3)).unwrap(),
            Value::Long(6)
        );
        col.set_column_validator(None);
        assert!(col.get_column_validator().is_simple());
        let context = ColumnValidatorContext::new(&col);
        assert_eq!(
            DefaultColumnValidator::for_column(&col)
                .validate(&context, Value::Long(3))
                .unwrap(),
            Value::Long(3)
        );
    }
}
