//! Rust side of the utility API harness (`tools/verify-utilapi.ps1`).
//!
//! Prints the same canonical, sorted `key=value` table as
//! `java-src/UtilApiGen.java` for the API surface added by the utility API
//! workstream:
//!
//! * `DataType` conversions (`fromByte`, `fromSQLType`, size/scale/precision
//!   validation and clamping, unit sizes, fixed sizes),
//! * `PropertyMap`'s Java put/get forms and the property blob bytes,
//! * `OleBlob.Builder` output (the exact `OleUtil.createBlob` bytes),
//! * `EvalConfig`'s defaults (locale, temporal/numeric config, function lookup,
//!   bindings),
//! * `ColumnValidator`'s built-in Required / AllowZeroLength / ValidationRule
//!   chain, the external validator and the validator factory.
//!
//! Usage: `utilapi <dir>` (the directory is accepted for parity with the Java
//! oracle; the Rust side only writes nothing and deletes nothing).

use std::io::Cursor;

use jackcess::column::{Column, ValidationRule};
use jackcess::datatype::DataType;
use jackcess::error::Error;
use jackcess::expr::{
    Bindings, EvalConfig, ExprValue, NumericConfig, NumericType, TemporalConfig, TemporalType,
    ValueKind,
};
use jackcess::format::{FileFormat, JetFormat};
use jackcess::ole::OleBlobBuilder;
use jackcess::property::{format_value as property_format_value, PropertyMap};
use jackcess::validator::{
    ColumnValidator, ColumnValidatorContext, ColumnValidatorFactoryHandle, ColumnValidatorHandle,
    DefaultColumnValidator,
};
use jackcess::value::Value;

fn main() {
    let mut app = App::default();
    app.data_types();
    app.ole_blobs();
    app.eval_config();
    app.property_maps();
    app.validators();
    app.print();
}

#[derive(Default)]
struct App {
    lines: Vec<String>,
}

impl App {
    fn emit(&mut self, key: impl std::fmt::Display, value: impl std::fmt::Display) {
        self.lines.push(format!("{key}={value}"));
    }

    fn print(mut self) {
        self.lines.sort();
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        print!("{out}");
    }

    // -----------------------------------------------------------------------
    // DataType
    // -----------------------------------------------------------------------

    fn data_types(&mut self) {
        let sizes = [
            -1, 0, 1, 2, 4, 8, 16, 17, 42, 255, 256, 510, 511, 1000, 3992,
        ];
        let scales = [-1, 0, 1, 4, 5, 18, 19, 28, 29];
        let v3 = JetFormat::version_3();

        for t in DataType::values().iter().copied() {
            let name = t.get_name();
            self.emit(format!("dtype.{name}.value"), t.value() as i8);
            match t.try_get_sql_type() {
                Ok(sql) => self.emit(format!("dtype.{name}.sql"), sql),
                Err(error) => self.emit(
                    format!("dtype.{name}.sql"),
                    format!("ERR:{}", error_message(&error)),
                ),
            }
            match t.try_get_fixed_size() {
                Ok(size) => self.emit(format!("dtype.{name}.fixed"), size),
                Err(error) => self.emit(
                    format!("dtype.{name}.fixed"),
                    format!("ERR:{}", error_message(&error)),
                ),
            }
            match t.try_get_fixed_size_with_length(Some(7)) {
                Ok(size) => self.emit(format!("dtype.{name}.fixed7"), size),
                Err(error) => self.emit(
                    format!("dtype.{name}.fixed7"),
                    format!("ERR:{}", error_message(&error)),
                ),
            }
            self.emit(format!("dtype.{name}.minSize"), t.get_min_size());
            self.emit(format!("dtype.{name}.defaultSize"), t.get_default_size());
            self.emit(format!("dtype.{name}.maxSize"), t.get_max_size());
            self.emit(format!("dtype.{name}.minScale"), t.get_min_scale());
            self.emit(format!("dtype.{name}.defaultScale"), t.get_default_scale());
            self.emit(format!("dtype.{name}.maxScale"), t.get_max_scale());
            self.emit(format!("dtype.{name}.minPrecision"), t.get_min_precision());
            self.emit(
                format!("dtype.{name}.defaultPrecision"),
                t.get_default_precision(),
            );
            self.emit(format!("dtype.{name}.maxPrecision"), t.get_max_precision());
            self.emit(
                format!("dtype.{name}.hasScalePrecision"),
                t.has_scale_precision(),
            );
            self.emit(format!("dtype.{name}.unitSize"), t.get_unit_size());
            self.emit(
                format!("dtype.{name}.unitSizeV3"),
                t.get_unit_size_with(Some(&v3)),
            );
            self.emit(
                format!("dtype.{name}.toUnitSize20"),
                t.to_unit_size(20, None),
            );
            self.emit(
                format!("dtype.{name}.toUnitSize20V3"),
                t.to_unit_size(20, Some(&v3)),
            );
            self.emit(
                format!("dtype.{name}.fromUnitSize10"),
                t.from_unit_size(10, None),
            );
            self.emit(
                format!("dtype.{name}.fromUnitSize10V3"),
                t.from_unit_size(10, Some(&v3)),
            );
            self.emit(
                format!("dtype.{name}.isVariableLength"),
                t.is_variable_length(),
            );
            self.emit(
                format!("dtype.{name}.isTrueVariableLength"),
                t.is_true_variable_length(),
            );
            self.emit(format!("dtype.{name}.isLongValue"), t.is_long_value());
            self.emit(format!("dtype.{name}.isTextual"), t.is_textual());
            self.emit(
                format!("dtype.{name}.mayBeAutoNumber"),
                t.may_be_auto_number(),
            );
            self.emit(
                format!("dtype.{name}.isMultipleAutoNumberAllowed"),
                t.is_multiple_auto_number_allowed(),
            );
            self.emit(format!("dtype.{name}.isUnsupported"), t.is_unsupported());
            for size in sizes {
                self.emit(
                    format!("dtype.{name}.validSize.{size}"),
                    t.is_valid_size(size),
                );
                self.emit(
                    format!("dtype.{name}.toValidSize.{size}"),
                    t.to_valid_size(size),
                );
            }
            for scale in scales {
                self.emit(
                    format!("dtype.{name}.validScale.{scale}"),
                    t.is_valid_scale(scale),
                );
                self.emit(
                    format!("dtype.{name}.toValidScale.{scale}"),
                    t.to_valid_scale(scale),
                );
                self.emit(
                    format!("dtype.{name}.validPrecision.{scale}"),
                    t.is_valid_precision(scale),
                );
                self.emit(
                    format!("dtype.{name}.toValidPrecision.{scale}"),
                    t.to_valid_precision(scale),
                );
            }
        }

        for b in 0u16..=255 {
            let b = b as u8;
            let key = b as i8;
            match DataType::try_from_byte(b) {
                Ok(t) => self.emit(format!("fromByte.{key}"), format!("OK:{}", t.get_name())),
                Err(error) => self.emit(
                    format!("fromByte.{key}"),
                    format!("ERR:{}", error_message(&error)),
                ),
            }
        }

        let sql_types_list = [
            -16, -15, -9, -7, -6, -5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 91, 92,
            93, 2004, 2005, 2011, 2013, 2014,
        ];
        let formats: [(Option<FileFormat>, &str); 10] = [
            (None, "null"),
            (Some(FileFormat::V1997), "V1997"),
            (Some(FileFormat::GenericJet4), "GENERIC_JET4"),
            (Some(FileFormat::V2000), "V2000"),
            (Some(FileFormat::V2003), "V2003"),
            (Some(FileFormat::V2007), "V2007"),
            (Some(FileFormat::V2010), "V2010"),
            (Some(FileFormat::V2016), "V2016"),
            (Some(FileFormat::V2019), "V2019"),
            (Some(FileFormat::Msisam), "MSISAM"),
        ];
        let lengths = [0, 255, 256, 300];
        for sql_type in sql_types_list {
            for length in lengths {
                for (format, label) in formats {
                    let key = format!("fromSQLType.{sql_type}.{length}.{label}");
                    match DataType::from_sql_type_with_format(sql_type, length, format) {
                        Ok(t) => self.emit(key, t.get_name()),
                        Err(error) => self.emit(key, format!("ERR:{}", error_message(&error))),
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // OleBlob.Builder
    // -----------------------------------------------------------------------

    fn ole_blobs(&mut self) {
        let content = b"hello ole".to_vec();

        let mut builder = OleBlobBuilder::new();
        builder
            .set_simple_package_bytes(content.clone())
            .set_simple_package_file_name("c.txt")
            .set_simple_package_file_path("C:\\c.txt");
        self.dump_blob("ole.simple_bytes", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_simple_package_bytes(Vec::new())
            .set_simple_package_file_name("e.txt")
            .set_simple_package_file_path("C:\\e.txt");
        self.dump_blob("ole.simple_empty", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_simple_package_file_name("caf\u{e9}.txt")
            .set_simple_package_file_path("C:\\data\\caf\u{e9}.txt");
        self.dump_blob("ole.simple_names", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_simple_package_bytes(b"abc".to_vec())
            .set_simple_package_file_name("a.txt")
            .set_simple_package_file_path("C:\\a.txt");
        self.dump_blob("ole.simple_all", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_simple_package_bytes(content.clone())
            .set_simple_package_file_name("\u{65e5}\u{672c}\u{8a9e}.txt")
            .set_simple_package_file_path("C:\\\u{65e5}\u{672c}\u{8a9e}.txt");
        self.dump_blob("ole.simple_japanese", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_simple_package_stream(Cursor::new(b"stream".to_vec()), 6)
            .expect("read stream")
            .set_simple_package_file_name("s.txt")
            .set_simple_package_file_path("C:\\s.txt");
        self.dump_blob("ole.simple_stream", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_link_path("C:\\data\\caf\u{e9}.txt")
            .set_link_file_name("caf\u{e9}.txt");
        self.dump_blob("ole.link", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_other_bytes(vec![1u8, 2, 3, 4])
            .set_package_pretty_name("Packager Shell Object")
            .set_package_class_name("Package");
        self.dump_blob("ole.other", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_other_bytes(vec![9u8, 8])
            .set_package_pretty_name("My Pretty")
            .set_package_class_name("My.Class")
            .set_package_type_name("MyType");
        self.dump_blob("ole.other_names", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_simple_package_bytes(content.clone())
            .set_simple_package_file_name("t.txt")
            .set_simple_package_file_path("C:\\t.txt")
            .set_package_type_name("OnlyType");
        self.dump_blob("ole.simple_only_type", builder.to_blob().unwrap());

        let mut builder = OleBlobBuilder::new();
        builder
            .set_simple_package_bytes(content)
            .set_simple_package_file_name("c2.txt")
            .set_simple_package_file_path("C:\\c2.txt")
            .set_package_class_name("OnlyClass");
        self.dump_blob("ole.simple_only_class", builder.to_blob().unwrap());

        match OleBlobBuilder::new().to_blob() {
            Ok(_) => self.emit("ole.no_type", "OK"),
            Err(error) => self.emit(
                "ole.no_type",
                format!("ERR:{}:{}", error_class(&error), error_message(&error)),
            ),
        }
    }

    fn dump_blob(&mut self, prefix: &str, blob: jackcess::ole::OleBlob) {
        let bytes = blob.bytes();
        self.emit(format!("{prefix}.len"), bytes.len());
        self.emit(format!("{prefix}.hex"), hex(bytes));

        let content = blob.content().expect("blob content");
        self.emit(format!("{prefix}.ctype"), content.content_type().name());
        if let Some(pretty) = content.pretty_name() {
            self.emit(format!("{prefix}.pretty"), utf8_hex(pretty));
        }
        if let Some(class) = content.class_name() {
            self.emit(format!("{prefix}.class"), utf8_hex(class));
        }
        if let Some(type_name) = content.type_name() {
            self.emit(format!("{prefix}.type_name"), utf8_hex(type_name));
        }
        if let Some(simple) = content.as_simple_package() {
            self.emit(format!("{prefix}.file_name"), utf8_hex(simple.file_name()));
            self.emit(format!("{prefix}.file_path"), utf8_hex(simple.file_path()));
            self.emit(
                format!("{prefix}.local_file_path"),
                utf8_hex(simple.local_file_path()),
            );
            self.emit(format!("{prefix}.content_len"), simple.length());
            self.emit(
                format!("{prefix}.content"),
                hex(simple.data().expect("simple package data")),
            );
        } else if let Some(link) = content.as_link() {
            self.emit(format!("{prefix}.file_name"), utf8_hex(link.file_name()));
            self.emit(format!("{prefix}.file_path"), utf8_hex(link.file_path()));
            self.emit(format!("{prefix}.link_path"), utf8_hex(link.link_path()));
        } else if let Some(other) = content.as_other() {
            self.emit(format!("{prefix}.content_len"), other.length());
            self.emit(
                format!("{prefix}.content"),
                hex(other.data().expect("other content data")),
            );
        }
    }

    // -----------------------------------------------------------------------
    // EvalConfig
    // -----------------------------------------------------------------------

    fn eval_config(&mut self) {
        let mut config = EvalConfig::default();
        self.dump_eval_config("eval.default", &config);

        config.set_locale("de_DE");
        self.emit("eval.set.locale", config.locale());

        config.set_temporal_config(TemporalConfig::new(
            "dd.MM.uuuu",
            "EEEE, dd. MMMM uuuu",
            "HH:mm:ss",
            "HH:mm:ss",
            '.',
            ':',
            "de_DE",
        ));
        self.dump_temporal("eval.set.temporal", config.temporal_config());

        config.set_numeric_config(NumericConfig::new(3, false, true, false, 0, "de_DE"));
        self.dump_numeric("eval.set.numeric", config.numeric_config());

        let mut bindings = Bindings::new();
        let _ = bindings.put("flag", ExprValue::Text("yes".to_string()));
        config.set_bindings(bindings);
        self.emit("eval.set.bindings.empty", config.bindings().is_empty());
        self.emit("eval.set.bindings.size", config.bindings().len());
        self.emit(
            "eval.set.bindings.flag",
            fmt_expr_value(config.bindings().get("flag").unwrap()),
        );
    }

    fn dump_eval_config(&mut self, prefix: &str, config: &EvalConfig) {
        self.emit(format!("{prefix}.locale"), config.locale());
        self.dump_temporal(&format!("{prefix}.temporal"), config.temporal_config());
        self.dump_numeric(&format!("{prefix}.numeric"), config.numeric_config());
        self.emit(
            format!("{prefix}.bindings.empty"),
            config.bindings().is_empty(),
        );
        self.emit(format!("{prefix}.bindings.size"), config.bindings().len());
        for name in [
            "Abs",
            "abs",
            "Format$",
            "DLookup",
            "nper",
            "NoSuchFunction",
            "",
        ] {
            let value = match config.function_lookup().get_function(name) {
                Some(info) => info.name().to_string(),
                None => "none".to_string(),
            };
            self.emit(format!("{prefix}.func.{name}"), value);
        }
    }

    fn dump_temporal(&mut self, prefix: &str, config: &TemporalConfig) {
        self.emit(format!("{prefix}.locale"), config.locale());
        self.emit(format!("{prefix}.date_format"), config.date_format());
        self.emit(
            format!("{prefix}.long_date_format"),
            config.get_date_time_format(TemporalType::LongDate),
        );
        self.emit(format!("{prefix}.time_format_12"), config.time_format_12());
        self.emit(format!("{prefix}.time_format_24"), config.time_format_24());
        self.emit(
            format!("{prefix}.datetime_format_12"),
            config.date_time_format_12(),
        );
        self.emit(
            format!("{prefix}.datetime_format_24"),
            config.date_time_format_24(),
        );
        self.emit(
            format!("{prefix}.default_date_format"),
            config.default_date_format(),
        );
        self.emit(
            format!("{prefix}.default_time_format"),
            config.default_time_format(),
        );
        self.emit(
            format!("{prefix}.default_datetime_format"),
            config.default_date_time_format(),
        );
        self.emit(
            format!("{prefix}.date_separator"),
            config.date_separator() as u32,
        );
        self.emit(
            format!("{prefix}.time_separator"),
            config.time_separator() as u32,
        );
        let am_pm = config.am_pm_strings();
        self.emit(
            format!("{prefix}.am_pm"),
            format!("{},{}", utf8_hex(am_pm[0]), utf8_hex(am_pm[1])),
        );
        for type_ in TEMPORAL_TYPES {
            let name = type_.name();
            self.emit(
                format!("{prefix}.format.{name}"),
                config.get_date_time_format(type_),
            );
            self.emit(
                format!("{prefix}.format.{name}.default"),
                type_.default_type().name(),
            );
            self.emit(
                format!("{prefix}.format.{name}.value_type"),
                value_kind_name(type_.value_type()),
            );
            self.emit(
                format!("{prefix}.format.{name}.date_only"),
                type_.is_date_only(),
            );
            self.emit(
                format!("{prefix}.format.{name}.time_only"),
                type_.is_time_only(),
            );
            self.emit(
                format!("{prefix}.format.{name}.includes_date"),
                type_.includes_date(),
            );
            self.emit(
                format!("{prefix}.format.{name}.includes_time"),
                type_.includes_time(),
            );
        }
    }

    fn dump_numeric(&mut self, prefix: &str, config: &NumericConfig) {
        self.emit(
            format!("{prefix}.num_dec_digits"),
            config.num_decimal_digits(),
        );
        self.emit(
            format!("{prefix}.include_leading_digit"),
            config.include_leading_digit(),
        );
        self.emit(
            format!("{prefix}.use_parens"),
            config.use_parens_for_negatives(),
        );
        self.emit(
            format!("{prefix}.use_currency_parens"),
            config.use_parens_for_currency_negatives(),
        );
        self.emit(
            format!("{prefix}.num_group_digits"),
            config.num_grouping_digits(),
        );
        for type_ in NUMERIC_TYPES {
            self.emit(
                format!("{prefix}.format.{}", numeric_type_name(type_)),
                escape(&config.get_number_format(type_)),
            );
        }
    }

    // -----------------------------------------------------------------------
    // PropertyMap
    // -----------------------------------------------------------------------

    fn property_maps(&mut self) {
        let mut props = PropertyMap::new();
        self.emit("props.initial.size", props.size());
        self.emit("props.initial.empty", props.is_empty());

        props
            .put_value("Title", Value::Text("The Title".into()))
            .unwrap();
        props
            .put_value("Author", Value::Text("An Author".into()))
            .unwrap();
        props
            .put_value("CustomText", Value::Text("some text".into()))
            .unwrap();
        props.put_value("CustomLong", Value::Long(42)).unwrap();
        props.put_value("CustomBool", Value::Bool(true)).unwrap();
        props.put_value("CustomByte", Value::Byte(3)).unwrap();
        props.put_value("CustomShort", Value::Int(4)).unwrap();
        props.put_value("CustomInt", Value::Long(5)).unwrap();
        props.put_value("CustomFloat", Value::Float(1.5)).unwrap();
        props.put_value("CustomDouble", Value::Double(2.5)).unwrap();
        props.put_value("CustomLong64", Value::BigInt(6)).unwrap();
        props
            .put_value("CustomBytes", Value::Bytes(vec![1, 2, 3]))
            .unwrap();
        props.put_typed("Typed", DataType::Int, Value::Int(7));
        props.put_typed_ddl("DdlProp", DataType::Text, Value::Text("x".into()), true);
        props.put_typed_ddl_skip_handler("SkipProp", DataType::Long, Value::Long(8), false, true);
        props.put_typed_ddl_skip_handler(
            "BothProp",
            DataType::Boolean,
            Value::Bool(true),
            true,
            true,
        );
        props.put_typed_ddl(
            "Description",
            DataType::Text,
            Value::Text("desc".into()),
            false,
        );

        self.emit("props.size", props.size());
        self.emit("props.empty", props.is_empty());
        self.emit(
            "props.value.custom",
            fmt_value(props.get_value("CustomLong").unwrap()),
        );
        self.emit(
            "props.value.default",
            fmt_value(props.get_value_default("NoSuchProp", &Value::Text("fallback".into()))),
        );
        self.emit(
            "props.value.null",
            match props.get_value("NoSuchProp") {
                Some(value) => fmt_value(value),
                None => "null".to_string(),
            },
        );
        for prop in props.iter() {
            let prefix = format!("props.{}", prop.name);
            self.emit(format!("{prefix}.type"), prop.data_type.get_name());
            self.emit(format!("{prefix}.ddl"), props.is_ddl(&prop.name));
            self.emit(format!("{prefix}.skip"), props.is_skip_handler(&prop.name));
            self.emit(format!("{prefix}.value"), fmt_value(&prop.value));
        }

        let blob = props.write(&JetFormat::version_4()).unwrap();
        self.emit("props.blob", hex(&blob));
        // Java's `PropertyMap.save()` writes this same map and the blob is
        // unchanged by the save (the oracle checks both forms), so the Rust
        // side reports the same bytes for the post-save dump.
        self.emit("props.blob.after_save", hex(&blob));
        let removed = props.remove("CustomText").unwrap();
        self.emit("props.removed", format!("o:{removed}"));
        self.emit("props.size.after_remove", props.size());
    }

    // -----------------------------------------------------------------------
    // ColumnValidator
    // -----------------------------------------------------------------------

    fn validators(&mut self) {
        let mut text_col = column_spec("c", DataType::Text);
        text_col.allow_zero_length = true;
        let num_col = column_spec("n", DataType::Long);

        // Required = true
        text_col.required = true;
        let validator = DefaultColumnValidator::for_column(&text_col);
        self.validate("val.required.null", &validator, &text_col, Value::Null);
        self.validate(
            "val.required.value",
            &validator,
            &text_col,
            Value::Text("x".into()),
        );

        // Required = false, AllowZeroLength = false
        text_col.required = false;
        text_col.allow_zero_length = false;
        let validator = DefaultColumnValidator::for_column(&text_col);
        self.validate("val.zero.null", &validator, &text_col, Value::Null);
        self.validate(
            "val.zero.empty",
            &validator,
            &text_col,
            Value::Text(String::new()),
        );
        self.validate(
            "val.zero.value",
            &validator,
            &text_col,
            Value::Text("x".into()),
        );
        self.validate("val.zero.number", &validator, &text_col, Value::Long(12));

        // an expression rule with a ValidationText
        text_col.allow_zero_length = true;
        let rule = ValidationRule::field("Len([c]) = 3", Some("c must have 3 chars"));
        let validator = DefaultColumnValidator::for_column(&text_col).with_rule(Some(rule));
        self.validate(
            "val.rule.valid",
            &validator,
            &text_col,
            Value::Text("abc".into()),
        );
        self.validate(
            "val.rule.invalid",
            &validator,
            &text_col,
            Value::Text("ab".into()),
        );

        // the default rejection message (`Invalid column value '<value>'`)
        let rule = ValidationRule::field("[c] = \"abc\"", None);
        let validator = DefaultColumnValidator::for_column(&text_col).with_rule(Some(rule));
        self.validate(
            "val.rule.default_message",
            &validator,
            &text_col,
            Value::Text("ab".into()),
        );
        self.validate(
            "val.rule.default_valid",
            &validator,
            &text_col,
            Value::Text("abc".into()),
        );
        self.validate("val.rule.null", &validator, &text_col, Value::Null);

        // a numeric column compares as a number
        let rule = ValidationRule::field("[n] > 3", None);
        let validator = DefaultColumnValidator::for_column(&num_col).with_rule(Some(rule));
        self.validate(
            "val.rule.number_default_message",
            &validator,
            &num_col,
            Value::Long(2),
        );
        self.validate(
            "val.rule.number_valid",
            &validator,
            &num_col,
            Value::Long(4),
        );

        // a rule may only read "this" column
        let rule = ValidationRule::field("[Other] = 1", None);
        let validator = DefaultColumnValidator::for_column(&text_col).with_rule(Some(rule));
        self.validate(
            "val.rule.other_field",
            &validator,
            &text_col,
            Value::Long(1),
        );

        // a custom (external) validator runs before the internal checks
        let validator = DefaultColumnValidator::new(ColumnValidatorHandle::new(Doubler));
        self.validate("val.external.value", &validator, &text_col, Value::Long(21));

        text_col.required = true;
        let validator =
            DefaultColumnValidator::new(ColumnValidatorHandle::new(Nuller)).with_required(true);
        self.validate(
            "val.external.required",
            &validator,
            &text_col,
            Value::Long(5),
        );
        text_col.required = false;

        // the column validator factory
        let factory = ColumnValidatorFactoryHandle::new(Factory);
        let external = factory
            .create_validator(&text_col)
            .expect("the factory returns a validator");
        let validator = DefaultColumnValidator::new(external);
        self.validate(
            "val.factory",
            &validator,
            &text_col,
            Value::Text("x".into()),
        );
    }

    fn validate(
        &mut self,
        key: &str,
        validator: &DefaultColumnValidator,
        column: &Column,
        value: Value,
    ) {
        let context = ColumnValidatorContext::for_column(column)
            .with_database_and_table("validators.mdb", "T");
        match validator.validate(&context, value) {
            Ok(value) => self.emit(key, format!("OK:{}", fmt_value(&value))),
            Err(error) => self.emit(
                key,
                format!("ERR:{}:{}", error_class(&error), error_message(&error)),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

const TEMPORAL_TYPES: [TemporalType; 14] = [
    TemporalType::Date,
    TemporalType::Time,
    TemporalType::DateTime,
    TemporalType::Time12,
    TemporalType::Time24,
    TemporalType::DateTime12,
    TemporalType::DateTime24,
    TemporalType::GeneralDate,
    TemporalType::LongDate,
    TemporalType::MediumDate,
    TemporalType::ShortDate,
    TemporalType::LongTime,
    TemporalType::MediumTime,
    TemporalType::ShortTime,
];

const NUMERIC_TYPES: [NumericType; 6] = [
    NumericType::Currency,
    NumericType::Fixed,
    NumericType::Standard,
    NumericType::Percent,
    NumericType::Scientific,
    NumericType::Euro,
];

fn numeric_type_name(type_: NumericType) -> &'static str {
    type_.name()
}

fn value_kind_name(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Date => "DATE",
        ValueKind::Time => "TIME",
        ValueKind::DateTime => "DATE_TIME",
        ValueKind::Null => "NULL",
        ValueKind::Text => "STRING",
        ValueKind::Long => "LONG",
        ValueKind::Double => "DOUBLE",
        ValueKind::Decimal => "BIG_DEC",
    }
}

/// A test column with the values `Column::read` leaves for a table definition.
fn column_spec(name: &str, data_type: DataType) -> Column {
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

/// The Java exception class names of the port's error variants.
fn error_class(error: &Error) -> &'static str {
    match error {
        Error::InvalidValue(_) => "InvalidValueException",
        Error::Jackcess(_) => "JackcessException",
        Error::IllegalArgument(_) => "IllegalArgumentException",
        Error::IllegalState(_) => "IllegalStateException",
        Error::Unsupported(_) => "UnsupportedOperationException",
        Error::ConstraintViolation(_) => "ConstraintViolationException",
        Error::InvalidCredentials(_) => "InvalidCredentialsException",
        Error::BatchUpdate { .. } => "BatchUpdateException",
        Error::NumberFormat(_) => "NumberFormatException",
        Error::Io(_) => "IOException",
        Error::BufferUnderflow(_) => "BufferUnderflowException",
        Error::BufferOverflow(_) => "BufferOverflowException",
        Error::InvalidMark => "InvalidMarkException",
    }
}

/// The Java exception message of a port error (the same text Java's
/// `Throwable.getMessage()` holds).
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
        | Error::NumberFormat(message)
        | Error::BufferUnderflow(message)
        | Error::BufferOverflow(message) => message.clone(),
        Error::BatchUpdate { message, .. } => message.clone(),
        Error::InvalidMark => String::new(),
    }
}

fn fmt_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => format!("b:{value}"),
        Value::Byte(value) => format!("n:{}", *value as i8),
        Value::Int(value) => format!("n:{value}"),
        Value::Long(value) => format!("n:{value}"),
        Value::BigInt(value) => format!("n:{value}"),
        Value::Float(value) => format!("f:{:x}", value.to_bits()),
        Value::Double(value) => format!("d:{:x}", value.to_bits()),
        Value::Text(value) => format!("s:{}", escape(value)),
        Value::Bytes(value) => format!("x:{}", hex(value)),
        Value::Decimal { unscaled, scale } => {
            let text = jackcess::expr::Decimal::from_unscaled(*unscaled, *scale)
                .map(|decimal| decimal.to_plain_string())
                .unwrap_or_else(|_| format!("{unscaled}E-{scale}"));
            format!("dec:{text}")
        }
        other => format!("o:{}", property_format_value(other)),
    }
}

fn fmt_expr_value(value: &ExprValue) -> String {
    match value {
        ExprValue::Null => "null".to_string(),
        ExprValue::Bool(value) => format!("b:{value}"),
        ExprValue::Byte(value) => format!("n:{value}"),
        ExprValue::Int(value) => format!("n:{value}"),
        ExprValue::Long(value) => format!("n:{value}"),
        ExprValue::BigInt(value) => format!("n:{value}"),
        ExprValue::Double(value) => format!("d:{:x}", value.to_bits()),
        ExprValue::Decimal(value) => format!("dec:{}", value.to_plain_string()),
        ExprValue::Text(value) => format!("s:{}", escape(value)),
        ExprValue::DateTime { value, .. } => format!("o:{value:?}"),
    }
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 || (ch as u32) > 0x7e => {
                // Java chars are UTF-16 code units; the harness only escapes
                // BMP text (non-BMP would be two \uXXXX escapes in Java).
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out
}

fn utf8_hex(value: &str) -> String {
    hex(value.as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

struct Doubler;

impl ColumnValidator for Doubler {
    fn validate(
        &self,
        _context: &ColumnValidatorContext<'_>,
        value: Value,
    ) -> jackcess::Result<Value> {
        Ok(match value {
            Value::Long(value) => Value::Long(value * 2),
            value => value,
        })
    }
}

struct Nuller;

impl ColumnValidator for Nuller {
    fn validate(
        &self,
        _context: &ColumnValidatorContext<'_>,
        _value: Value,
    ) -> jackcess::Result<Value> {
        Ok(Value::Null)
    }
}

struct Factory;

impl jackcess::validator::ColumnValidatorFactory for Factory {
    fn create_validator(&self, _column: &Column) -> Option<ColumnValidatorHandle> {
        Some(ColumnValidatorHandle::new(Prefixer))
    }
}

struct Prefixer;

impl ColumnValidator for Prefixer {
    fn validate(
        &self,
        _context: &ColumnValidatorContext<'_>,
        value: Value,
    ) -> jackcess::Result<Value> {
        Ok(match value {
            Value::Text(value) => Value::Text(format!("F{value}")),
            value => value,
        })
    }
}
