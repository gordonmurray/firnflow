//! Facet behavior through `NamespaceService` on local storage.
//!
//! Covers cache-aside repeat hits and filter key splitting without
//! requiring MinIO.

use std::collections::HashMap;
use std::sync::Arc;

use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::{test_metrics, CoreMetrics};
use firnflow_core::{
    FacetRequest, NamespaceId, NamespaceManager, NamespaceService, StorageRoot, UpsertRow,
};
use serde_json::json;
use tempfile::TempDir;

const DIM: usize = 8;

fn unit_vector(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[axis] = 1.0;
    v
}

fn metric_value(body: &str, metric: &str, ns: &NamespaceId) -> u64 {
    let needle = format!(r#"namespace="{}""#, ns.as_str());
    for line in body.lines() {
        if line.starts_with(metric) && line.contains(&needle) {
            if let Some((_, value)) = line.rsplit_once(char::is_whitespace) {
                return value.parse::<f64>().unwrap_or(0.0) as u64;
            }
        }
    }
    0
}

async fn local_service() -> (
    NamespaceService,
    NamespaceId,
    Arc<CoreMetrics>,
    TempDir,
    TempDir,
) {
    let dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let metrics = test_metrics();
    let manager = Arc::new(NamespaceManager::new(
        StorageRoot::local(dir.path()).unwrap(),
        HashMap::new(),
        Arc::clone(&metrics),
    ));
    let cache = Arc::new(
        NamespaceCache::new(
            16 * 1024 * 1024,
            cache_dir.path(),
            64 * 1024 * 1024,
            Arc::clone(&metrics),
        )
        .await
        .expect("cache"),
    );
    let service = NamespaceService::new(Arc::clone(&manager), cache, Arc::clone(&metrics));
    let ns = NamespaceId::new("service-facet").unwrap();

    let attr = |section: &str, route: &str| {
        let mut attributes = serde_json::Map::new();
        attributes.insert("section".into(), json!(section));
        attributes.insert("route".into(), json!(route));
        attributes
    };
    service
        .upsert(
            &ns,
            vec![
                UpsertRow {
                    id: 1,
                    vector: unit_vector(0),
                    vectors: None,
                    text: None,
                    attributes: attr("warnings", "oral"),
                },
                UpsertRow {
                    id: 2,
                    vector: unit_vector(1),
                    vectors: None,
                    text: None,
                    attributes: attr("dosage", "iv"),
                },
                UpsertRow {
                    id: 3,
                    vector: unit_vector(2),
                    vectors: None,
                    text: None,
                    attributes: attr("warnings", "oral"),
                },
            ],
        )
        .await
        .expect("seed upsert");

    (service, ns, metrics, dir, cache_dir)
}

#[tokio::test]
async fn facet_repeats_hit_cache_and_filters_do_not_collide() {
    let (service, ns, metrics, _dir, _cache_dir) = local_service().await;
    let req = FacetRequest {
        filter: None,
        fields: vec!["section".into()],
        top: Some(10),
    };

    let first = service.facet(&ns, &req).await.expect("facet #1");
    assert_eq!(first.facets[0].buckets[0].value, json!("warnings"));
    assert_eq!(first.facets[0].buckets[0].count, 2);

    let second = service.facet(&ns, &req).await.expect("facet #2");
    assert_eq!(second, first);

    let body = metrics.encode().unwrap();
    assert_eq!(metric_value(&body, "firnflow_cache_misses_total", &ns), 1);
    assert_eq!(metric_value(&body, "firnflow_cache_hits_total", &ns), 1);

    let filtered = FacetRequest {
        filter: Some("section = 'dosage'".into()),
        fields: vec!["section".into()],
        top: Some(10),
    };
    let narrowed = service.facet(&ns, &filtered).await.expect("filtered facet");
    assert_eq!(narrowed.facets[0].buckets[0].value, json!("dosage"));
    assert_eq!(narrowed.facets[0].buckets[0].count, 1);

    let body = metrics.encode().unwrap();
    assert_eq!(metric_value(&body, "firnflow_cache_misses_total", &ns), 2);

    let mut attributes = serde_json::Map::new();
    attributes.insert("section".into(), json!("warnings"));
    service
        .upsert(
            &ns,
            vec![UpsertRow {
                id: 4,
                vector: unit_vector(3),
                vectors: None,
                text: None,
                attributes,
            }],
        )
        .await
        .expect("upsert invalidates by generation");

    let after_write = service.facet(&ns, &req).await.expect("facet after write");
    assert_eq!(after_write.facets[0].buckets[0].value, json!("warnings"));
    assert_eq!(after_write.facets[0].buckets[0].count, 3);
    let body = metrics.encode().unwrap();
    assert_eq!(metric_value(&body, "firnflow_cache_misses_total", &ns), 3);
}

#[tokio::test]
async fn facet_field_order_is_canonical_for_cache_hits() {
    let (service, ns, metrics, _dir, _cache_dir) = local_service().await;
    let first_req = FacetRequest {
        filter: None,
        fields: vec!["section".into(), "route".into()],
        top: Some(10),
    };
    let second_req = FacetRequest {
        filter: None,
        fields: vec!["route".into(), "section".into()],
        top: Some(10),
    };

    let first = service.facet(&ns, &first_req).await.expect("facet #1");
    let second = service.facet(&ns, &second_req).await.expect("facet #2");
    assert_eq!(second, first);
    assert_eq!(first.facets[0].field, "route");
    assert_eq!(first.facets[1].field, "section");

    let body = metrics.encode().unwrap();
    assert_eq!(metric_value(&body, "firnflow_cache_misses_total", &ns), 1);
    assert_eq!(metric_value(&body, "firnflow_cache_hits_total", &ns), 1);
}

/// A `[]string` (list) attribute: a facet counts each element, so a row with
/// multiple genres is counted in every bucket it belongs to, and `array_has`
/// filters the set before counting.
#[tokio::test]
async fn string_list_attribute_facets_per_element_and_filters_with_array_has() {
    let dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let metrics = test_metrics();
    let manager = Arc::new(NamespaceManager::new(
        StorageRoot::local(dir.path()).unwrap(),
        HashMap::new(),
        Arc::clone(&metrics),
    ));
    let cache = Arc::new(
        NamespaceCache::new(16 * 1024 * 1024, cache_dir.path(), 64 * 1024 * 1024, Arc::clone(&metrics))
            .await
            .expect("cache"),
    );
    let service = NamespaceService::new(Arc::clone(&manager), cache, Arc::clone(&metrics));
    let ns = NamespaceId::new("service-facet-list").unwrap();

    let with_genres = |id: u64, axis: usize, genres: serde_json::Value| {
        let mut attributes = serde_json::Map::new();
        attributes.insert("genres".into(), genres);
        UpsertRow { id, vector: unit_vector(axis), vectors: None, text: None, attributes }
    };
    service
        .upsert(
            &ns,
            vec![
                with_genres(1, 0, json!(["Fantasy", "Fiction"])),
                with_genres(2, 1, json!(["Fantasy", "Science Fiction"])),
                with_genres(3, 2, json!(["Fiction"])),
            ],
        )
        .await
        .expect("seed list upsert");

    // Per-element counts: Fantasy 2, Fiction 2, Science Fiction 1.
    let all = service
        .facet(&ns, &FacetRequest { filter: None, fields: vec!["genres".into()], top: Some(10) })
        .await
        .expect("facet genres");
    let counts: HashMap<String, u64> = all.facets[0]
        .buckets
        .iter()
        .map(|b| (b.value.as_str().unwrap().to_string(), b.count))
        .collect();
    assert_eq!(counts.get("Fantasy"), Some(&2));
    assert_eq!(counts.get("Fiction"), Some(&2));
    assert_eq!(counts.get("Science Fiction"), Some(&1));

    // array_has narrows the set before counting: only rows 1 and 2.
    let fantasy = service
        .facet(
            &ns,
            &FacetRequest {
                filter: Some("array_has(genres, 'Fantasy')".into()),
                fields: vec!["genres".into()],
                top: Some(10),
            },
        )
        .await
        .expect("filtered facet");
    let fcounts: HashMap<String, u64> = fantasy.facets[0]
        .buckets
        .iter()
        .map(|b| (b.value.as_str().unwrap().to_string(), b.count))
        .collect();
    assert_eq!(fcounts.get("Fantasy"), Some(&2));
    assert_eq!(fcounts.get("Fiction"), Some(&1));
    assert_eq!(fcounts.get("Science Fiction"), Some(&1));
}
