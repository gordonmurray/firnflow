//! Typed metadata columns on a namespace.
//!
//! A namespace starts with the four columns Firn owns: `id`,
//! `vector`, `text`, and `_ingested_at`. Those are the only things a
//! query `filter` can reference, which limits filtering to row ids and
//! write times. Attribute columns are the caller's own scalar fields
//! (`section`, `tenant`, `language`, `published_year`), declared once
//! per namespace and then available to every filter predicate.
//!
//! # Declared, not inferred
//!
//! The set of columns and their types come from an explicit
//! declaration ([`crate::NamespaceManager::declare_attributes`]), not
//! from the shape of the first row that carries them. Inference reads
//! well in a quickstart and then fails on the second batch: a column
//! whose first value is `2024` is an integer until someone sends
//! `2024.5`, a column whose first value is absent has no type at all,
//! and neither problem is visible until the write that trips it.
//! Declaring up front means the type error lands on the declaration,
//! where the caller can see all of it at once.
//!
//! # Names are lowercase
//!
//! Attribute names are restricted to `[a-z][a-z0-9_]*`. The predicate
//! dialect is SQL, and SQL lowercases an unquoted identifier during
//! parsing, so a column declared as `Section` could only be reached
//! from a filter as `"Section"`, quotes included. Restricting the
//! declaration means every name a caller declares is a name they can
//! type straight into a predicate.

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow_array::builder::{BooleanBuilder, Float64Builder, Int64Builder, StringBuilder};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field};
use serde::{Deserialize, Serialize};

use crate::FirnflowError;

/// Maximum number of attribute columns one namespace may declare.
///
/// Every declared column is materialised in the Lance schema and read
/// back on each query, so the cap bounds both the per-row width and
/// the per-result payload. Well above the ten to twenty fields a
/// faceted corpus tends to carry.
pub const MAX_ATTRIBUTE_COLUMNS: usize = 32;

/// Maximum length of an attribute column name.
pub const MAX_ATTRIBUTE_NAME_LEN: usize = 64;

/// Column names Firn owns. An attribute may not take one of these,
/// and the leading-underscore rule in [`validate_attribute_name`]
/// keeps the rest of the system namespace free for future use.
const RESERVED_NAMES: &[&str] = &["id", "vector", "vectors", "text"];

/// The type of an attribute column, fixed at declaration.
///
/// Four scalar types, each mapping to one Arrow type. Lists, nested
/// objects, and timestamps are deliberately absent from this set:
/// timestamps need a wire-format decision (epoch integer against
/// RFC 3339 text) and lists need a membership operator in the filter
/// dialect, and neither is needed to make scalar filtering work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttributeType {
    /// UTF-8 text. Arrow `Utf8`.
    String,
    /// 64-bit signed integer. Arrow `Int64`.
    Int,
    /// 64-bit float. Arrow `Float64`.
    Float,
    /// Boolean. Arrow `Boolean`.
    Bool,
}

impl AttributeType {
    /// The Arrow type this attribute is stored as.
    pub fn arrow_type(self) -> DataType {
        match self {
            Self::String => DataType::Utf8,
            Self::Int => DataType::Int64,
            Self::Float => DataType::Float64,
            Self::Bool => DataType::Boolean,
        }
    }

    /// Recover the attribute type from a column's Arrow type, or
    /// `None` for a type no attribute can have.
    pub fn from_arrow(data_type: &DataType) -> Option<Self> {
        match data_type {
            DataType::Utf8 => Some(Self::String),
            DataType::Int64 => Some(Self::Int),
            DataType::Float64 => Some(Self::Float),
            DataType::Boolean => Some(Self::Bool),
            _ => None,
        }
    }

    /// The name this type is declared under in a request body.
    pub fn as_label(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
        }
    }
}

/// One declared attribute column: a name and a type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributeColumn {
    /// Column name, matching `[a-z][a-z0-9_]*`.
    pub name: String,
    /// Storage type, fixed once declared.
    #[serde(rename = "type")]
    pub ty: AttributeType,
}

impl AttributeColumn {
    /// Build the column with the given name and type.
    pub fn new(name: impl Into<String>, ty: AttributeType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }

    /// The Arrow field for this column. Always nullable: a row may
    /// omit any attribute, and rows written before a column was
    /// declared carry a null in it.
    pub fn field(&self) -> Field {
        Field::new(&self.name, self.ty.arrow_type(), true)
    }
}

/// A value written to, or read back from, an attribute column.
///
/// Externally tagged on purpose. The result cache encodes
/// [`crate::QueryResult`] with bincode, which is not self-describing
/// and cannot recover an untagged value, so the tag is what makes a
/// cached hit decodable. The REST layer renders these as bare JSON
/// scalars on the way out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AttributeValue {
    /// A `string` column's value.
    String(String),
    /// An `int` column's value.
    Int(i64),
    /// A `float` column's value.
    Float(f64),
    /// A `bool` column's value.
    Bool(bool),
}

impl AttributeValue {
    /// The column type this value can be stored in.
    pub fn type_of(&self) -> AttributeType {
        match self {
            Self::String(_) => AttributeType::String,
            Self::Int(_) => AttributeType::Int,
            Self::Float(_) => AttributeType::Float,
            Self::Bool(_) => AttributeType::Bool,
        }
    }
}

/// A row's attribute values as they are read back, keyed by column
/// name. A column the row left null is absent from the map rather
/// than present with a null in it.
pub type AttributeMap = BTreeMap<String, AttributeValue>;

/// A row's attribute values as they are written.
///
/// Distinct from [`AttributeMap`] because a write can say null two
/// ways and only one of them is checkable. Omitting a name says
/// nothing at all; sending it as `None` (a JSON `null`) names a
/// column and asks for it to be empty. Both store a null, but the
/// second still has to be a column that exists, or a caller who
/// misspells a name while clearing it gets a `200` for a write that
/// did nothing.
pub type AttributeInput = BTreeMap<String, Option<AttributeValue>>;

/// Validate one attribute column name.
///
/// Accepts `[a-z][a-z0-9_]*` up to [`MAX_ATTRIBUTE_NAME_LEN`], and
/// rejects the names Firn owns. The leading character rule also bars
/// the `_`-prefixed system namespace that `_ingested_at` lives in.
pub fn validate_attribute_name(name: &str) -> Result<(), FirnflowError> {
    if name.is_empty() {
        return Err(FirnflowError::InvalidRequest(
            "attribute name must not be empty".into(),
        ));
    }
    if name.len() > MAX_ATTRIBUTE_NAME_LEN {
        return Err(FirnflowError::InvalidRequest(format!(
            "attribute name {name:?} is longer than {MAX_ATTRIBUTE_NAME_LEN} characters"
        )));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("checked non-empty above");
    if !first.is_ascii_lowercase() {
        return Err(FirnflowError::InvalidRequest(format!(
            "attribute name {name:?} must start with a lowercase letter; \
             names are used verbatim in filter predicates, where SQL lowercases \
             anything that is not quoted"
        )));
    }
    if let Some(bad) = chars.find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_'))
    {
        return Err(FirnflowError::InvalidRequest(format!(
            "attribute name {name:?} contains {bad:?}; \
             allowed characters are lowercase letters, digits, and underscore"
        )));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(FirnflowError::InvalidRequest(format!(
            "attribute name {name:?} is reserved by the engine; \
             reserved names are {RESERVED_NAMES:?}"
        )));
    }
    Ok(())
}

/// Validate a whole declaration: every name legal, no duplicates, and
/// no more than [`MAX_ATTRIBUTE_COLUMNS`] columns.
///
/// Pure, so the API layer can answer 400 before touching storage.
pub fn validate_attribute_declaration(columns: &[AttributeColumn]) -> Result<(), FirnflowError> {
    if columns.is_empty() {
        return Err(FirnflowError::InvalidRequest(
            "declare at least one attribute column".into(),
        ));
    }
    if columns.len() > MAX_ATTRIBUTE_COLUMNS {
        return Err(FirnflowError::InvalidRequest(format!(
            "{} attribute columns declared; the limit is {MAX_ATTRIBUTE_COLUMNS}",
            columns.len()
        )));
    }
    let mut seen = std::collections::HashSet::with_capacity(columns.len());
    for column in columns {
        validate_attribute_name(&column.name)?;
        if !seen.insert(column.name.as_str()) {
            return Err(FirnflowError::InvalidRequest(format!(
                "attribute {:?} is declared more than once",
                column.name
            )));
        }
    }
    Ok(())
}

/// Check a row's attribute values against the namespace's declared
/// columns, widening integers written to `float` columns.
///
/// An undeclared name is an error rather than a silent drop: the
/// caller believes they have written a value that a later filter will
/// match on, and the write is the only place that belief can be
/// corrected. A missing name is not an error; that column is null for
/// this row.
///
/// The widening is the one coercion. JSON has a single number type, so
/// a caller sending `1` for a `float` column has not made a mistake,
/// while `1.5` for an `int` column has lost data by the time it
/// arrives and is rejected.
pub fn coerce_row_attributes(
    row_id: u64,
    values: &mut AttributeInput,
    declared: &[AttributeColumn],
) -> Result<(), FirnflowError> {
    for (name, value) in values.iter_mut() {
        let Some(column) = declared.iter().find(|c| &c.name == name) else {
            let known: Vec<&str> = declared.iter().map(|c| c.name.as_str()).collect();
            return Err(FirnflowError::InvalidRequest(format!(
                "row id {row_id}: attribute {name:?} is not declared on this namespace; \
                 declared attributes are {known:?} (declare it with \
                 POST /ns/{{namespace}}/attributes)"
            )));
        };
        // An explicit null has had its name checked, which is the part
        // that can be wrong. There is nothing left to type-check.
        let Some(value) = value else { continue };
        if let (AttributeType::Float, AttributeValue::Int(i)) = (column.ty, &*value) {
            let widened = *i as f64;
            // A value that comes back different from the one the
            // caller sent is worse than a rejection they can act on:
            // an equality filter on what they wrote would then match
            // nothing. Compare through `i128` rather than casting the
            // float back to `i64`, because that cast saturates. The
            // saturation lands exactly on the interesting value:
            // `i64::MAX as f64` rounds up to 2^63, and 2^63 back to
            // `i64` clamps to `i64::MAX` again, so a round-trip
            // through `i64` reports the one integer most likely to be
            // sent as a boundary probe as unchanged.
            if widened as i128 != *i as i128 {
                return Err(FirnflowError::InvalidRequest(format!(
                    "row id {row_id}: attribute {name:?} is declared float and {i} cannot be \
                     represented exactly as one; send it as a float the target can hold"
                )));
            }
            *value = AttributeValue::Float(widened);
            continue;
        }
        if value.type_of() != column.ty {
            return Err(FirnflowError::InvalidRequest(format!(
                "row id {row_id}: attribute {name:?} is declared {} but the value is {}",
                column.ty.as_label(),
                value.type_of().as_label(),
            )));
        }
    }
    Ok(())
}

/// Build the Arrow array for one attribute column across a batch of
/// rows, taking each row's value for that column (or a null when the
/// row omits it).
///
/// Values are expected to have been checked by
/// [`coerce_row_attributes`] already; a type that slipped through is a
/// [`FirnflowError::Backend`] rather than a caller error, because by
/// this point it means the validation and the writer disagree.
pub(crate) fn build_attribute_array<'a>(
    column: &AttributeColumn,
    values: impl Iterator<Item = Option<&'a AttributeValue>>,
) -> Result<ArrayRef, FirnflowError> {
    fn mismatch(column: &AttributeColumn, value: &AttributeValue) -> FirnflowError {
        FirnflowError::Backend(format!(
            "attribute {:?} is {} but a {} value reached the writer",
            column.name,
            column.ty.as_label(),
            value.type_of().as_label(),
        ))
    }

    match column.ty {
        AttributeType::String => {
            let mut builder = StringBuilder::new();
            for value in values {
                match value {
                    Some(AttributeValue::String(s)) => builder.append_value(s),
                    Some(other) => return Err(mismatch(column, other)),
                    None => builder.append_null(),
                }
            }
            Ok(Arc::new(builder.finish()) as ArrayRef)
        }
        AttributeType::Int => {
            let mut builder = Int64Builder::new();
            for value in values {
                match value {
                    Some(AttributeValue::Int(i)) => builder.append_value(*i),
                    Some(other) => return Err(mismatch(column, other)),
                    None => builder.append_null(),
                }
            }
            Ok(Arc::new(builder.finish()) as ArrayRef)
        }
        AttributeType::Float => {
            let mut builder = Float64Builder::new();
            for value in values {
                match value {
                    Some(AttributeValue::Float(f)) => builder.append_value(*f),
                    Some(other) => return Err(mismatch(column, other)),
                    None => builder.append_null(),
                }
            }
            Ok(Arc::new(builder.finish()) as ArrayRef)
        }
        AttributeType::Bool => {
            let mut builder = BooleanBuilder::new();
            for value in values {
                match value {
                    Some(AttributeValue::Bool(b)) => builder.append_value(*b),
                    Some(other) => return Err(mismatch(column, other)),
                    None => builder.append_null(),
                }
            }
            Ok(Arc::new(builder.finish()) as ArrayRef)
        }
    }
}

/// Per-batch reader for the declared attribute columns.
///
/// Resolves each column once for the whole batch rather than by name
/// per row, which matters on a `k`-row result where the lookup would
/// otherwise run `k × columns` times.
///
/// A declared column missing from the batch is an error rather than
/// something to read past. Every read path projects the declared
/// columns, including the explicit column list `include_vector: false`
/// builds, so a column that is not there means the schema the reader
/// was handed and the batch it is reading disagree. Skipping would
/// turn that into a `200` whose metadata is quietly incomplete, which
/// no caller can distinguish from a row that genuinely has no values.
pub(crate) struct AttributeReaders<'a> {
    columns: Vec<(&'a AttributeColumn, &'a ArrayRef)>,
}

impl<'a> AttributeReaders<'a> {
    /// Bind the declared columns to the arrays in `batch`.
    pub(crate) fn new(
        batch: &'a RecordBatch,
        declared: &'a [AttributeColumn],
    ) -> Result<Self, FirnflowError> {
        let mut columns = Vec::with_capacity(declared.len());
        for column in declared {
            let Some(array) = batch.column_by_name(&column.name) else {
                return Err(FirnflowError::Backend(format!(
                    "attribute {:?} is declared on this namespace but missing from the \
                     result batch",
                    column.name
                )));
            };
            if AttributeType::from_arrow(array.data_type()) != Some(column.ty) {
                return Err(FirnflowError::Backend(format!(
                    "attribute {:?} is declared {} but the result column is {:?}",
                    column.name,
                    column.ty.as_label(),
                    array.data_type(),
                )));
            }
            columns.push((column, array));
        }
        Ok(Self { columns })
    }

    /// Whether any declared column was found in the batch.
    pub(crate) fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Read one row's attribute values. Null cells are left out of the
    /// map rather than represented, matching how they are written.
    pub(crate) fn row(&self, row: usize) -> Result<AttributeMap, FirnflowError> {
        let mut out = AttributeMap::new();
        for (column, array) in &self.columns {
            if array.is_null(row) {
                continue;
            }
            let value = match column.ty {
                AttributeType::String => AttributeValue::String(
                    downcast::<StringArray>(array, column)?
                        .value(row)
                        .to_owned(),
                ),
                AttributeType::Int => {
                    AttributeValue::Int(downcast::<Int64Array>(array, column)?.value(row))
                }
                AttributeType::Float => {
                    AttributeValue::Float(downcast::<Float64Array>(array, column)?.value(row))
                }
                AttributeType::Bool => {
                    AttributeValue::Bool(downcast::<BooleanArray>(array, column)?.value(row))
                }
            };
            out.insert(column.name.clone(), value);
        }
        Ok(out)
    }
}

fn downcast<'a, T: 'static>(
    array: &'a ArrayRef,
    column: &AttributeColumn,
) -> Result<&'a T, FirnflowError> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        FirnflowError::Backend(format!(
            "attribute {:?} did not downcast to its declared {} array",
            column.name,
            column.ty.as_label(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared() -> Vec<AttributeColumn> {
        vec![
            AttributeColumn::new("section", AttributeType::String),
            AttributeColumn::new("year", AttributeType::Int),
            AttributeColumn::new("score", AttributeType::Float),
            AttributeColumn::new("archived", AttributeType::Bool),
        ]
    }

    #[test]
    fn names_accept_the_documented_shape() {
        for name in ["section", "route_2", "a", "tenant_id"] {
            validate_attribute_name(name).unwrap_or_else(|e| panic!("{name:?}: {e}"));
        }
    }

    #[test]
    fn names_reject_uppercase_and_leading_non_letters() {
        for name in ["Section", "_hidden", "2024", "route-name", "route name"] {
            assert!(
                validate_attribute_name(name).is_err(),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn names_reject_engine_owned_columns() {
        for name in ["id", "vector", "vectors", "text"] {
            let err = validate_attribute_name(name).unwrap_err();
            match err {
                FirnflowError::InvalidRequest(msg) => assert!(msg.contains("reserved"), "{msg}"),
                other => panic!("expected InvalidRequest, got {other:?}"),
            }
        }
        // `_ingested_at` is caught by the leading-character rule.
        assert!(validate_attribute_name("_ingested_at").is_err());
    }

    #[test]
    fn declaration_rejects_duplicates_and_overflow() {
        let dupes = vec![
            AttributeColumn::new("section", AttributeType::String),
            AttributeColumn::new("section", AttributeType::Int),
        ];
        let err = validate_attribute_declaration(&dupes).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => assert!(msg.contains("more than once"), "{msg}"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }

        let too_many: Vec<AttributeColumn> = (0..MAX_ATTRIBUTE_COLUMNS + 1)
            .map(|i| AttributeColumn::new(format!("a{i}"), AttributeType::Int))
            .collect();
        assert!(validate_attribute_declaration(&too_many).is_err());

        assert!(validate_attribute_declaration(&[]).is_err());
    }

    fn input(pairs: &[(&str, Option<AttributeValue>)]) -> AttributeInput {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn values_must_match_their_declared_type() {
        let mut values = input(&[("year", Some(AttributeValue::String("2024".into())))]);
        let err = coerce_row_attributes(7, &mut values, &declared()).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(msg.contains("row id 7"), "{msg}");
                assert!(msg.contains("declared int"), "{msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn integers_widen_into_float_columns_but_not_the_reverse() {
        let mut values = input(&[("score", Some(AttributeValue::Int(3)))]);
        coerce_row_attributes(1, &mut values, &declared()).unwrap();
        assert_eq!(values.get("score"), Some(&Some(AttributeValue::Float(3.0))));

        let mut values = input(&[("year", Some(AttributeValue::Float(2024.5)))]);
        assert!(coerce_row_attributes(1, &mut values, &declared()).is_err());
    }

    /// Widening stops where `f64` stops counting. `2^53 + 1` is the
    /// first integer it cannot hold, and storing it as the neighbour
    /// it rounds to would mean a later equality filter on the value
    /// the caller sent finds nothing.
    #[test]
    fn integers_too_large_for_a_float_are_rejected_rather_than_rounded() {
        let too_big = (1_i64 << 53) + 1;
        let mut values = input(&[("score", Some(AttributeValue::Int(too_big)))]);
        let err = coerce_row_attributes(1, &mut values, &declared()).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(msg.contains("cannot be represented exactly"), "{msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }

        // The largest exactly-representable integer still widens.
        let mut values = input(&[("score", Some(AttributeValue::Int(1_i64 << 53)))]);
        coerce_row_attributes(1, &mut values, &declared()).unwrap();

        // `i64::MAX` is the boundary a round-trip through `i64` gets
        // wrong: it rounds up to 2^63 as a float, and casting 2^63
        // back saturates to `i64::MAX`, so the two compare equal.
        let mut values = input(&[("score", Some(AttributeValue::Int(i64::MAX)))]);
        assert!(
            coerce_row_attributes(1, &mut values, &declared()).is_err(),
            "i64::MAX does not survive a trip through f64"
        );
        let mut values = input(&[("score", Some(AttributeValue::Int(i64::MIN)))]);
        assert!(
            coerce_row_attributes(1, &mut values, &declared()).is_ok(),
            "i64::MIN is exactly -2^63 and does survive"
        );
    }

    #[test]
    fn undeclared_attributes_are_rejected_rather_than_dropped() {
        let mut values = input(&[("nope", Some(AttributeValue::Bool(true)))]);
        let err = coerce_row_attributes(4, &mut values, &declared()).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(msg.contains("not declared"), "{msg}");
                assert!(
                    msg.contains("section"),
                    "the message should list what is: {msg}"
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    /// An explicit null carries no value to type-check, but it still
    /// names a column, and a name that does not exist is the caller's
    /// mistake whether or not they attached a value to it.
    #[test]
    fn an_undeclared_name_is_rejected_even_when_its_value_is_null() {
        let mut values = input(&[("nope", None)]);
        let err = coerce_row_attributes(4, &mut values, &declared()).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => assert!(msg.contains("not declared"), "{msg}"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }

        // A declared name with a null value is fine.
        let mut values = input(&[("section", None)]);
        coerce_row_attributes(4, &mut values, &declared()).unwrap();
    }

    #[test]
    fn omitted_attributes_are_allowed() {
        let mut values = input(&[("section", Some(AttributeValue::String("warnings".into())))]);
        coerce_row_attributes(1, &mut values, &declared()).unwrap();
        assert_eq!(values.len(), 1);
    }

    #[test]
    fn arrow_types_round_trip() {
        for ty in [
            AttributeType::String,
            AttributeType::Int,
            AttributeType::Float,
            AttributeType::Bool,
        ] {
            assert_eq!(AttributeType::from_arrow(&ty.arrow_type()), Some(ty));
        }
        assert_eq!(AttributeType::from_arrow(&DataType::Int32), None);
    }
}
