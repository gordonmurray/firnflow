//! Typed attribute columns on a namespace, end to end through
//! `NamespaceManager` against local storage (no MinIO needed).
//!
//! Covers the declaration rules, what a write does with attribute
//! values, what a read gives back, and the two things attribute
//! columns exist for: filtering a query by them and indexing them.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder};
use arrow_array::{RecordBatch, RecordBatchIterator, RecordBatchReader, UInt64Array};
use arrow_schema::{DataType, Field, Schema};
use firnflow_core::metrics::test_metrics;
use firnflow_core::{
    AttributeColumn, AttributeInput, AttributeType, AttributeValue, FirnflowError, NamespaceId,
    NamespaceManager, StorageRoot, UpsertRow,
};
use tempfile::TempDir;

const DIM: usize = 4;

fn unit_vector(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[axis] = 1.0;
    v
}

fn manager() -> (NamespaceManager, NamespaceId, TempDir) {
    let dir = TempDir::new().unwrap();
    let manager = NamespaceManager::new(
        StorageRoot::local(dir.path()).unwrap(),
        HashMap::new(),
        test_metrics(),
    );
    let ns = NamespaceId::new("attributes").unwrap();
    (manager, ns, dir)
}

fn attributes(pairs: &[(&str, AttributeValue)]) -> AttributeInput {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), Some(value.clone())))
        .collect()
}

fn row(id: u64, axis: usize, values: &[(&str, AttributeValue)]) -> UpsertRow {
    UpsertRow {
        id,
        vector: unit_vector(axis),
        vectors: None,
        text: Some(format!("row {id}")),
        attributes: attributes(values),
    }
}

fn declaration() -> Vec<AttributeColumn> {
    vec![
        AttributeColumn::new("section", AttributeType::String),
        AttributeColumn::new("year", AttributeType::Int),
        AttributeColumn::new("score", AttributeType::Float),
        AttributeColumn::new("archived", AttributeType::Bool),
    ]
}

/// Seed a namespace, declare the four attribute types on it, and write
/// three rows carrying values. The intended order of operations: a
/// namespace has to exist before it can be given columns.
async fn seeded() -> (NamespaceManager, NamespaceId, TempDir) {
    let (manager, ns, dir) = manager();
    manager
        .upsert(&ns, vec![(1u64, unit_vector(0)).into()])
        .await
        .expect("create the namespace");
    manager
        .declare_attributes(&ns, &declaration())
        .await
        .expect("declare");

    let rows = vec![
        row(
            1,
            0,
            &[
                ("section", AttributeValue::String("warnings".into())),
                ("year", AttributeValue::Int(2024)),
                ("score", AttributeValue::Float(0.5)),
                ("archived", AttributeValue::Bool(false)),
            ],
        ),
        row(
            2,
            1,
            &[
                ("section", AttributeValue::String("dosage".into())),
                ("year", AttributeValue::Int(2025)),
            ],
        ),
        row(
            3,
            2,
            &[("section", AttributeValue::String("warnings".into()))],
        ),
    ];
    manager.upsert(&ns, rows).await.expect("upsert with values");
    (manager, ns, dir)
}

#[tokio::test]
async fn declaring_needs_a_namespace_that_exists() {
    let (manager, ns, _dir) = manager();
    let err = manager
        .declare_attributes(&ns, &declaration())
        .await
        .expect_err("a namespace with no table cannot take columns");
    match err {
        FirnflowError::InvalidRequest(msg) => {
            assert!(msg.contains("no data has been upserted yet"), "{msg}");
            assert!(msg.contains("write a row first"), "{msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn declared_columns_show_up_on_the_namespace() {
    let (manager, ns, _dir) = seeded().await;
    let info = manager.info(&ns).await.unwrap().expect("namespace exists");
    assert_eq!(info.attributes, declaration());
    assert_eq!(
        manager.attributes_for(&ns).await.unwrap(),
        Some(declaration())
    );
}

/// Re-sending a declaration a caller already made is a no-op, right
/// down to the table version: a client that declares its whole schema
/// on every startup must not churn the cache generation each time.
#[tokio::test]
async fn redeclaring_the_same_columns_commits_nothing() {
    let (manager, ns, _dir) = seeded().await;
    let before = manager.info(&ns).await.unwrap().unwrap().table_version;

    let returned = manager
        .declare_attributes(&ns, &declaration())
        .await
        .expect("redeclare");
    assert_eq!(returned, declaration());

    let after = manager.info(&ns).await.unwrap().unwrap().table_version;
    assert_eq!(before, after, "an unchanged declaration must not commit");
}

#[tokio::test]
async fn a_second_declaration_adds_to_the_first() {
    let (manager, ns, _dir) = seeded().await;
    let returned = manager
        .declare_attributes(&ns, &[AttributeColumn::new("route", AttributeType::String)])
        .await
        .expect("declare an extra column");

    let mut expected = declaration();
    expected.push(AttributeColumn::new("route", AttributeType::String));
    assert_eq!(returned, expected, "the response carries the full set");

    // The rows written before the column existed read back without it.
    let hits = manager
        .query(&ns, unit_vector(0), None, 3, None, None, None, true)
        .await
        .expect("query");
    let first = hits.results.first().expect("a hit");
    assert!(!first.attributes.contains_key("route"));
}

#[tokio::test]
async fn a_column_cannot_change_type() {
    let (manager, ns, _dir) = seeded().await;
    let err = manager
        .declare_attributes(&ns, &[AttributeColumn::new("year", AttributeType::String)])
        .await
        .expect_err("year is already an int");
    match err {
        FirnflowError::InvalidRequest(msg) => {
            assert!(msg.contains("already declared as int"), "{msg}");
            assert!(msg.contains("string"), "{msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn illegal_names_are_rejected_before_any_write() {
    let (manager, ns, _dir) = seeded().await;
    let before = manager.info(&ns).await.unwrap().unwrap().table_version;

    for name in ["Section", "_hidden", "id", "text", "route-name"] {
        let result = manager
            .declare_attributes(&ns, &[AttributeColumn::new(name, AttributeType::String)])
            .await;
        assert!(
            matches!(result, Err(FirnflowError::InvalidRequest(_))),
            "{name:?} should be rejected, got {result:?}"
        );
    }

    let after = manager.info(&ns).await.unwrap().unwrap().table_version;
    assert_eq!(before, after, "a rejected declaration must not commit");
}

#[tokio::test]
async fn values_come_back_on_query_results() {
    let (manager, ns, _dir) = seeded().await;
    let results = manager
        .query(&ns, unit_vector(0), None, 3, None, None, None, true)
        .await
        .expect("query")
        .results;

    let hit = results.iter().find(|r| r.id == 1).expect("row 1");
    assert_eq!(
        hit.attributes.get("section"),
        Some(&AttributeValue::String("warnings".into()))
    );
    assert_eq!(hit.attributes.get("year"), Some(&AttributeValue::Int(2024)));
    assert_eq!(
        hit.attributes.get("score"),
        Some(&AttributeValue::Float(0.5))
    );
    assert_eq!(
        hit.attributes.get("archived"),
        Some(&AttributeValue::Bool(false))
    );

    // Row 3 set only `section`; the columns it left out are absent
    // rather than present with a placeholder.
    let sparse = results.iter().find(|r| r.id == 3).expect("row 3");
    assert_eq!(sparse.attributes.len(), 1);
    assert!(sparse.attributes.contains_key("section"));
}

/// `include_vector: false` builds an explicit column projection, which
/// is exactly where a new response field is easy to drop by accident.
#[tokio::test]
async fn a_vector_light_query_still_carries_attributes() {
    let (manager, ns, _dir) = seeded().await;
    let results = manager
        .query(&ns, unit_vector(0), None, 3, None, None, None, false)
        .await
        .expect("query")
        .results;

    let hit = results.iter().find(|r| r.id == 1).expect("row 1");
    assert!(hit.vector.is_none(), "the caller opted out of the vector");
    assert_eq!(
        hit.attributes.get("section"),
        Some(&AttributeValue::String("warnings".into()))
    );
}

#[tokio::test]
async fn a_filter_over_an_attribute_narrows_the_result_set() {
    let (manager, ns, _dir) = seeded().await;

    let unfiltered = manager
        .query(&ns, unit_vector(0), None, 10, None, None, None, false)
        .await
        .expect("unfiltered")
        .results;
    assert_eq!(unfiltered.len(), 3);

    let filtered = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            10,
            None,
            None,
            Some("section = 'warnings'".into()),
            false,
        )
        .await
        .expect("filtered")
        .results;
    let mut ids: Vec<u64> = filtered.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 3]);

    // A numeric range over an int column, and a predicate over a
    // column most rows left null.
    let recent = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            10,
            None,
            None,
            Some("year >= 2025".into()),
            false,
        )
        .await
        .expect("range filter")
        .results;
    assert_eq!(recent.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2]);

    let not_archived = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            10,
            None,
            None,
            Some("archived = false".into()),
            false,
        )
        .await
        .expect("bool filter")
        .results;
    assert_eq!(
        not_archived.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1],
        "rows that left `archived` null must not match `= false`"
    );
}

#[tokio::test]
async fn an_undeclared_attribute_is_rejected_rather_than_dropped() {
    let (manager, ns, _dir) = seeded().await;
    let err = manager
        .upsert(
            &ns,
            vec![row(
                4,
                3,
                &[("route", AttributeValue::String("ORAL".into()))],
            )],
        )
        .await
        .expect_err("route was never declared");
    match err {
        FirnflowError::InvalidRequest(msg) => {
            assert!(msg.contains("row id 4"), "{msg}");
            assert!(msg.contains("not declared"), "{msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn an_integer_written_to_a_float_column_is_widened() {
    let (manager, ns, _dir) = seeded().await;
    manager
        .upsert(&ns, vec![row(4, 3, &[("score", AttributeValue::Int(2))])])
        .await
        .expect("an int is a legal float");

    let results = manager
        .query(&ns, unit_vector(3), None, 1, None, None, None, false)
        .await
        .expect("query")
        .results;
    assert_eq!(
        results[0].attributes.get("score"),
        Some(&AttributeValue::Float(2.0))
    );

    let err = manager
        .upsert(
            &ns,
            vec![row(5, 3, &[("year", AttributeValue::Float(2024.5))])],
        )
        .await
        .expect_err("a float is not a legal int");
    assert!(matches!(err, FirnflowError::InvalidRequest(_)));
}

/// Upsert replaces a matched row in full, which is already true of
/// `text`. Attributes follow the same rule, and it is worth pinning:
/// a caller re-sending a row to change its vector clears the metadata
/// it does not resend.
#[tokio::test]
async fn re_upserting_without_attributes_clears_them() {
    let (manager, ns, _dir) = seeded().await;
    manager
        .upsert(&ns, vec![row(1, 0, &[])])
        .await
        .expect("re-upsert");

    let results = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            10,
            None,
            None,
            Some("id = 1".into()),
            false,
        )
        .await
        .expect("query")
        .results;
    assert!(
        results[0].attributes.is_empty(),
        "a replaced row keeps only what the replacement carried"
    );
}

#[tokio::test]
async fn list_rows_carry_attributes() {
    let (manager, ns, _dir) = seeded().await;
    let page = manager
        .list(&ns, 10, firnflow_core::ListOrder::Asc, None)
        .await
        .expect("list");
    let row = page.rows.iter().find(|r| r.id == 2).expect("row 2");
    assert_eq!(
        row.attributes.get("section"),
        Some(&AttributeValue::String("dosage".into()))
    );
    assert_eq!(row.attributes.get("year"), Some(&AttributeValue::Int(2025)));
}

#[tokio::test]
async fn a_scalar_index_can_be_built_on_an_attribute_column() {
    let (manager, ns, _dir) = seeded().await;

    manager
        .validate_scalar_index_column(&ns, "section")
        .await
        .expect("a declared column is a valid index target");
    manager
        .create_scalar_index(&ns, "section")
        .await
        .expect("build");

    let info = manager.info(&ns).await.unwrap().unwrap();
    assert!(info.has_scalar_index);

    // The filter still returns the same rows through the index.
    let filtered = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            10,
            None,
            None,
            Some("section = 'warnings'".into()),
            false,
        )
        .await
        .expect("filtered")
        .results;
    let mut ids: Vec<u64> = filtered.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 3]);
}

/// The Arrow bulk path does not carry attribute values, but it still
/// has to write a batch that matches a table which now has extra
/// columns. It fills them with nulls, so the load succeeds and the
/// loaded rows simply hold no metadata. Pins both halves: that
/// `/import` does not fail on a namespace with declared columns, and
/// that the rows it writes will not match a predicate over them.
#[tokio::test]
async fn import_into_a_namespace_with_attributes_writes_nulls() {
    let (manager, ns, _dir) = seeded().await;

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), 4),
            false,
        ),
    ]));

    let mut vectors = FixedSizeListBuilder::new(Float32Builder::new(), 4);
    for value in unit_vector(3) {
        vectors.values().append_value(value);
    }
    vectors.append(true);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(vec![99_u64])),
            Arc::new(vectors.finish()),
        ],
    )
    .unwrap();

    let reader: Box<dyn RecordBatchReader + Send> =
        Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema));
    let imported = manager.import_arrow(&ns, reader).await.expect("import");
    assert_eq!(imported, 1);

    let hits = manager
        .query(
            &ns,
            unit_vector(3),
            None,
            5,
            None,
            None,
            Some("id = 99".into()),
            false,
        )
        .await
        .expect("query")
        .results;
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].attributes.is_empty(),
        "an imported row carries no metadata: {:?}",
        hits[0].attributes
    );

    let filtered = manager
        .query(
            &ns,
            unit_vector(3),
            None,
            5,
            None,
            None,
            Some("section = 'warnings'".into()),
            false,
        )
        .await
        .expect("filtered")
        .results;
    assert!(
        !filtered.iter().any(|r| r.id == 99),
        "an imported row must not match a predicate over a column it has no value for"
    );
}

#[tokio::test]
async fn an_undeclared_column_is_not_a_scalar_index_target() {
    let (manager, ns, _dir) = seeded().await;
    let err = manager
        .validate_scalar_index_column(&ns, "route")
        .await
        .expect_err("route is not declared");
    match err {
        FirnflowError::InvalidRequest(msg) => {
            assert!(msg.contains("not supported"), "{msg}");
            assert!(msg.contains("section"), "the message lists what is: {msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}
