//! Backend-agnostic storage root.
//!
//! A [`StorageRoot`] captures the parsed shape of a Firn deployment's
//! object-storage URI: which scheme (`s3`, `gs`, or `file`), which
//! bucket (or local directory, for `file`), and optionally a fixed
//! prefix that every namespace lives under. The
//! type is the single hand-off between operator config (the
//! `FIRNFLOW_STORAGE_URI` / `FIRNFLOW_S3_BUCKET` env vars) and the
//! parts of `firnflow-core` that need to construct namespace URIs and
//! `object_store` clients.
//!
//! `s3://`, `gs://`, and `file://` (a local filesystem directory, for
//! embedded mode) are routable schemes. The actual
//! `object_store` client and lancedb backend are picked by
//! [`Scheme`] downstream — see `NamespaceManager::build_object_store`
//! for the dispatch. GCS routing uses the native
//! `object_store::gcp::GoogleCloudStorage` backend (OAuth2 /
//! service-account JSON), not the S3-interop endpoint.
//!
//! Trailing slashes are canonicalised away by the parser so that
//! `s3://foo` and `s3://foo/` produce identical structs. Empty
//! prefixes are stored as `None`, never as `Some("")`. This makes
//! equality of two parsed roots a meaningful "operator pointed both
//! env vars at the same place" check.

use std::path::{Path, PathBuf};

use crate::error::FirnflowError;
use crate::namespace::NamespaceId;

/// Object-storage scheme. `S3` covers any S3-compatible backend
/// (AWS, MinIO, R2, Tigris, DigitalOcean Spaces) — the wire shape is
/// the same; the storage-options map carries provider-specific
/// endpoint and addressing tweaks. `Gcs` routes through lancedb's
/// `gcs` feature and the matching `object_store::gcp` client; auth
/// is OAuth2 via a service-account JSON, not SigV4 — the GCS
/// S3-interop layer is a deliberately separate (and unsupported)
/// path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// Any S3-compatible backend. The wire shape is identical
    /// across providers; per-provider knobs (custom endpoint,
    /// path-style addressing, region) live in the storage-options
    /// map alongside.
    S3,
    /// Native Google Cloud Storage. Lancedb resolves `gs://` URIs
    /// through `lance-io`'s native backend, and the delete path
    /// drops into `object_store::gcp::GoogleCloudStorage` keyed off
    /// the same credentials.
    Gcs,
    /// Local filesystem directory, for embedded mode (no network, no
    /// credentials). The `bucket` field of a [`StorageRoot`] with this
    /// scheme holds the absolute base directory; each namespace is a
    /// subdirectory beneath it that `lancedb::connect` opens as a local
    /// Lance table via a `file://` URI, and the delete path lists and
    /// removes objects through `object_store::local::LocalFileSystem`.
    Local,
}

impl Scheme {
    /// Wire-format prefix for this scheme (`s3`, `gs`). Used when
    /// rebuilding a URI string.
    pub fn as_uri_prefix(self) -> &'static str {
        match self {
            Scheme::S3 => "s3",
            Scheme::Gcs => "gs",
            Scheme::Local => "file",
        }
    }
}

/// Parsed storage root. Carries enough state to build per-namespace
/// URIs and pick the right `object_store` builder; carries nothing
/// about credentials (those flow through the storage-options map).
///
/// Two `StorageRoot` values compare equal iff they would resolve to
/// the same physical location — same scheme, same bucket, same
/// optional prefix. The parser canonicalises trailing slashes and
/// empty prefixes so that `s3://foo`, `s3://foo/`, and a legacy
/// `FIRNFLOW_S3_BUCKET=foo` all produce equal structs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRoot {
    scheme: Scheme,
    bucket: String,
    prefix: Option<String>,
}

impl StorageRoot {
    /// Parse a URI of the form `scheme://bucket[/prefix...]`.
    /// Supported schemes are `s3` (any S3-compatible backend),
    /// `gs` (native Google Cloud Storage), and `file` (a local
    /// filesystem directory for embedded mode, e.g.
    /// `file:///srv/firn_data`; the path is resolved to an absolute
    /// directory via [`StorageRoot::local`]).
    ///
    /// # Errors
    ///
    /// - `InvalidRequest` if the URI is empty, missing the `://`
    ///   separator, or has an empty bucket segment.
    /// - `InvalidRequest` if the scheme is not one of `s3`, `gs`,
    ///   `file`.
    pub fn parse(uri: &str) -> Result<Self, FirnflowError> {
        let uri = uri.trim();
        if uri.is_empty() {
            return Err(FirnflowError::InvalidRequest(
                "storage URI must not be empty".into(),
            ));
        }
        let Some((scheme_part, rest)) = uri.split_once("://") else {
            return Err(FirnflowError::InvalidRequest(format!(
                "storage URI {uri:?} must be of the form scheme://bucket[/prefix]"
            )));
        };

        let scheme = match scheme_part {
            "s3" => Scheme::S3,
            "gs" => Scheme::Gcs,
            // Local filesystem: the remainder is a directory path, not a
            // `bucket/prefix` pair, so route straight to `local` and skip
            // the bucket/prefix split below.
            "file" => return Self::local(rest),
            other => {
                return Err(FirnflowError::InvalidRequest(format!(
                    "storage URI {uri:?} uses unrecognised scheme {other:?}; \
                     supported schemes are s3, gs, file"
                )));
            }
        };

        let trimmed = rest.trim_end_matches('/');
        let (bucket, prefix) = match trimmed.split_once('/') {
            None => (trimmed, None),
            Some((b, "")) => (b, None),
            Some((b, p)) => (b, Some(p.to_string())),
        };

        if bucket.is_empty() {
            return Err(FirnflowError::InvalidRequest(format!(
                "storage URI {uri:?} has an empty bucket segment"
            )));
        }

        Ok(StorageRoot {
            scheme,
            bucket: bucket.to_string(),
            prefix,
        })
    }

    /// Construct an S3 storage root from a bare bucket name. Used by
    /// the legacy-fallback path when only `FIRNFLOW_S3_BUCKET` is
    /// set; canonicalised so that the resulting struct compares equal
    /// to `StorageRoot::parse(&format!("s3://{bucket}"))?`.
    ///
    /// # Errors
    ///
    /// - `InvalidRequest` if the bucket name is empty after trimming.
    pub fn s3_bucket(bucket: impl Into<String>) -> Result<Self, FirnflowError> {
        let bucket = bucket.into();
        let trimmed = bucket.trim();
        if trimmed.is_empty() {
            return Err(FirnflowError::InvalidRequest(
                "storage bucket name must not be empty".into(),
            ));
        }
        Ok(StorageRoot {
            scheme: Scheme::S3,
            bucket: trimmed.to_string(),
            prefix: None,
        })
    }

    /// Construct a local-filesystem storage root from a directory
    /// path, for embedded mode.
    ///
    /// The path is resolved to an absolute path (relative paths are
    /// joined onto the current working directory) so that the
    /// `file://` URI handed to `lancedb::connect` and the local
    /// `object_store` client agree regardless of the process's working
    /// directory. The directory need not exist yet — embedded
    /// namespace state is lazy until the first write. The resulting
    /// `bucket` field holds the absolute directory and `prefix` is
    /// always `None`.
    ///
    /// # Errors
    ///
    /// - `InvalidRequest` if the path is empty or not valid UTF-8.
    /// - `Io` if the current working directory cannot be read while
    ///   resolving a relative path.
    pub fn local(dir: impl Into<PathBuf>) -> Result<Self, FirnflowError> {
        let dir = dir.into();
        if dir.as_os_str().is_empty() {
            return Err(FirnflowError::InvalidRequest(
                "local storage directory must not be empty".into(),
            ));
        }
        Ok(StorageRoot {
            scheme: Scheme::Local,
            bucket: absolute_utf8_dir(&dir)?,
            prefix: None,
        })
    }

    /// Scheme this root resolves to. Used by `NamespaceManager` to
    /// pick an `object_store` builder.
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Bucket name (no scheme prefix, no trailing slash).
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Optional fixed prefix every namespace lives under. Returns
    /// `None` when the URI has no prefix segment; the parser also
    /// returns `None` for `s3://bucket/` (trailing-slash-only).
    pub fn prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// URI for a specific namespace under this root. Format is
    /// `scheme://bucket[/prefix]/namespace`. Used as the `uri`
    /// argument to `lancedb::connect`.
    pub fn namespace_uri(&self, ns: &NamespaceId) -> String {
        match &self.prefix {
            None => format!(
                "{}://{}/{}",
                self.scheme.as_uri_prefix(),
                self.bucket,
                ns.as_str()
            ),
            Some(prefix) => format!(
                "{}://{}/{}/{}",
                self.scheme.as_uri_prefix(),
                self.bucket,
                prefix,
                ns.as_str()
            ),
        }
    }

    /// Object-store-relative path for a namespace, i.e. everything
    /// after the bucket. `s3://bucket` + namespace `docs` → `docs`;
    /// `s3://bucket/firn` + namespace `docs` → `firn/docs`. The
    /// returned string is suitable for `object_store::path::Path::from`.
    /// Lives on `StorageRoot` (not `NamespaceManager`) so the prefix
    /// stitching is unit-testable without an `object_store` builder.
    pub fn namespace_object_path(&self, ns: &NamespaceId) -> String {
        match &self.prefix {
            None => ns.as_str().to_string(),
            Some(prefix) => format!("{}/{}", prefix, ns.as_str()),
        }
    }

    /// Re-render the root as a URI string. Equal `StorageRoot` values
    /// produce equal display strings; reciprocally,
    /// `StorageRoot::parse(root.as_uri()).unwrap() == root`. Useful
    /// for log messages and diagnostics.
    pub fn as_uri(&self) -> String {
        match &self.prefix {
            None => format!("{}://{}", self.scheme.as_uri_prefix(), self.bucket),
            Some(prefix) => format!(
                "{}://{}/{}",
                self.scheme.as_uri_prefix(),
                self.bucket,
                prefix
            ),
        }
    }
}

impl std::fmt::Display for StorageRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_uri())
    }
}

/// Resolve a directory path to an absolute, UTF-8 string without
/// requiring it to exist on disk. Relative paths are joined onto the
/// current working directory; the path is deliberately *not* run
/// through `fs::canonicalize` (which requires the path to exist) so a
/// not-yet-created `./firn_data` resolves cleanly — embedded namespace
/// state is lazy until the first write. Trailing slashes are trimmed
/// so `./firn_data` and `./firn_data/` produce equal roots, mirroring
/// the trailing-slash canonicalisation the cloud parser applies.
fn absolute_utf8_dir(dir: &Path) -> Result<String, FirnflowError> {
    let abs: PathBuf = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(dir)
    };
    let s = abs.to_str().ok_or_else(|| {
        FirnflowError::InvalidRequest(format!("local storage path {abs:?} is not valid UTF-8"))
    })?;
    let trimmed = s.trim_end_matches('/');
    Ok(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(name: &str) -> NamespaceId {
        NamespaceId::new(name).unwrap()
    }

    #[test]
    fn parse_s3_bare_bucket() {
        let root = StorageRoot::parse("s3://my-bucket").unwrap();
        assert_eq!(root.scheme(), Scheme::S3);
        assert_eq!(root.bucket(), "my-bucket");
        assert_eq!(root.prefix(), None);
    }

    #[test]
    fn parse_s3_with_single_segment_prefix() {
        let root = StorageRoot::parse("s3://my-bucket/firn").unwrap();
        assert_eq!(root.scheme(), Scheme::S3);
        assert_eq!(root.bucket(), "my-bucket");
        assert_eq!(root.prefix(), Some("firn"));
    }

    #[test]
    fn parse_s3_with_multi_segment_prefix() {
        let root = StorageRoot::parse("s3://my-bucket/tenants/acme/prod").unwrap();
        assert_eq!(root.bucket(), "my-bucket");
        assert_eq!(root.prefix(), Some("tenants/acme/prod"));
    }

    #[test]
    fn parse_canonicalises_trailing_slash() {
        // s3://foo and s3://foo/ are the same physical location and
        // must compare equal.
        let with = StorageRoot::parse("s3://my-bucket/").unwrap();
        let without = StorageRoot::parse("s3://my-bucket").unwrap();
        assert_eq!(with, without);
        assert_eq!(with.prefix(), None);
    }

    #[test]
    fn parse_canonicalises_trailing_slash_after_prefix() {
        let with = StorageRoot::parse("s3://my-bucket/firn/").unwrap();
        let without = StorageRoot::parse("s3://my-bucket/firn").unwrap();
        assert_eq!(with, without);
        assert_eq!(with.prefix(), Some("firn"));
    }

    #[test]
    fn s3_bucket_helper_matches_parsed_uri() {
        // The legacy-fallback path constructs a StorageRoot from a
        // bare bucket name; the resulting struct must compare equal
        // to one parsed from the equivalent s3:// URI so that the
        // resolver's "both env vars agree" check sees them as
        // identical.
        let bare = StorageRoot::s3_bucket("my-bucket").unwrap();
        let parsed = StorageRoot::parse("s3://my-bucket").unwrap();
        assert_eq!(bare, parsed);
    }

    #[test]
    fn parse_rejects_empty_uri() {
        let err = StorageRoot::parse("").unwrap_err();
        assert!(matches!(err, FirnflowError::InvalidRequest(_)));
    }

    #[test]
    fn parse_rejects_whitespace_only() {
        let err = StorageRoot::parse("   ").unwrap_err();
        assert!(matches!(err, FirnflowError::InvalidRequest(_)));
    }

    #[test]
    fn parse_rejects_missing_scheme_separator() {
        let err = StorageRoot::parse("my-bucket").unwrap_err();
        assert!(matches!(err, FirnflowError::InvalidRequest(_)));
    }

    #[test]
    fn parse_rejects_empty_bucket() {
        let err = StorageRoot::parse("s3://").unwrap_err();
        assert!(matches!(err, FirnflowError::InvalidRequest(_)));
        let err = StorageRoot::parse("s3:///prefix").unwrap_err();
        assert!(matches!(err, FirnflowError::InvalidRequest(_)));
    }

    #[test]
    fn parse_rejects_unknown_scheme() {
        let err = StorageRoot::parse("ftp://my-bucket").unwrap_err();
        assert!(matches!(err, FirnflowError::InvalidRequest(_)));
        let err = StorageRoot::parse("http://my-bucket").unwrap_err();
        assert!(matches!(err, FirnflowError::InvalidRequest(_)));
    }

    #[test]
    fn parse_gs_bare_bucket() {
        let root = StorageRoot::parse("gs://firn-gcs-bucket").unwrap();
        assert_eq!(root.scheme(), Scheme::Gcs);
        assert_eq!(root.bucket(), "firn-gcs-bucket");
        assert_eq!(root.prefix(), None);
    }

    #[test]
    fn parse_gs_with_prefix() {
        let root = StorageRoot::parse("gs://firn-gcs-bucket/some/prefix").unwrap();
        assert_eq!(root.scheme(), Scheme::Gcs);
        assert_eq!(root.bucket(), "firn-gcs-bucket");
        assert_eq!(root.prefix(), Some("some/prefix"));
    }

    #[test]
    fn namespace_uri_for_gs_scheme() {
        // gs:// must round-trip through namespace_uri so the manager
        // hands lancedb::connect the same scheme the operator
        // configured — never silently rewriting to s3://.
        let root = StorageRoot::parse("gs://firn-gcs-bucket").unwrap();
        assert_eq!(root.namespace_uri(&ns("docs")), "gs://firn-gcs-bucket/docs");
        let root = StorageRoot::parse("gs://firn-gcs-bucket/tenants/acme").unwrap();
        assert_eq!(
            root.namespace_uri(&ns("docs")),
            "gs://firn-gcs-bucket/tenants/acme/docs"
        );
    }

    #[test]
    fn s3_bucket_helper_rejects_empty_input() {
        let err = StorageRoot::s3_bucket("").unwrap_err();
        assert!(matches!(err, FirnflowError::InvalidRequest(_)));
        let err = StorageRoot::s3_bucket("   ").unwrap_err();
        assert!(matches!(err, FirnflowError::InvalidRequest(_)));
    }

    #[test]
    fn namespace_uri_without_prefix() {
        let root = StorageRoot::parse("s3://my-bucket").unwrap();
        assert_eq!(root.namespace_uri(&ns("docs")), "s3://my-bucket/docs");
    }

    #[test]
    fn namespace_uri_with_single_segment_prefix() {
        let root = StorageRoot::parse("s3://my-bucket/firn").unwrap();
        assert_eq!(root.namespace_uri(&ns("docs")), "s3://my-bucket/firn/docs");
    }

    #[test]
    fn namespace_uri_with_multi_segment_prefix() {
        let root = StorageRoot::parse("s3://my-bucket/tenants/acme").unwrap();
        assert_eq!(
            root.namespace_uri(&ns("docs")),
            "s3://my-bucket/tenants/acme/docs"
        );
    }

    #[test]
    fn namespace_object_path_no_prefix() {
        // No prefix on the root: object-store path is just the
        // namespace name. The bucket itself never appears because
        // object_store builders are already scoped to a bucket.
        let root = StorageRoot::parse("s3://my-bucket").unwrap();
        assert_eq!(root.namespace_object_path(&ns("docs")), "docs");
    }

    #[test]
    fn namespace_object_path_with_single_segment_prefix() {
        // Single-segment prefix is the common multi-tenant shape: a
        // bucket shared across deployments with one prefix per env.
        let root = StorageRoot::parse("s3://my-bucket/firn").unwrap();
        assert_eq!(root.namespace_object_path(&ns("docs")), "firn/docs");
    }

    #[test]
    fn namespace_object_path_with_multi_segment_prefix() {
        // Multi-segment prefix exercises the deeper-key case that
        // delete() walks when iterating bucket contents — a missed
        // `/` separator here would silently corrupt the listed keys
        // and cross-tenant deletes could escape their prefix.
        let root = StorageRoot::parse("s3://my-bucket/tenants/acme/prod").unwrap();
        assert_eq!(
            root.namespace_object_path(&ns("docs")),
            "tenants/acme/prod/docs"
        );
    }

    #[test]
    fn namespace_object_path_canonicalises_trailing_slash() {
        // s3://my-bucket/firn/ and s3://my-bucket/firn must produce
        // the same object-store path — no leading or trailing slash,
        // no double-slash — so the parser's canonicalisation flows
        // through to the delete path.
        let with = StorageRoot::parse("s3://my-bucket/firn/").unwrap();
        let without = StorageRoot::parse("s3://my-bucket/firn").unwrap();
        assert_eq!(
            with.namespace_object_path(&ns("docs")),
            without.namespace_object_path(&ns("docs"))
        );
        assert_eq!(with.namespace_object_path(&ns("docs")), "firn/docs");
    }

    #[test]
    fn as_uri_round_trips_through_parse() {
        for input in ["s3://my-bucket", "s3://my-bucket/firn", "s3://b/a/b/c"] {
            let parsed = StorageRoot::parse(input).unwrap();
            let rendered = parsed.as_uri();
            let reparsed = StorageRoot::parse(&rendered).unwrap();
            assert_eq!(parsed, reparsed, "round-trip mismatch for {input:?}");
        }
    }

    #[test]
    fn display_matches_as_uri() {
        let root = StorageRoot::parse("s3://my-bucket/firn").unwrap();
        assert_eq!(format!("{root}"), root.as_uri());
    }

    #[test]
    fn local_constructor_makes_relative_path_absolute() {
        // A relative dir resolves against cwd; the dir need not exist.
        let root = StorageRoot::local("firn_data").unwrap();
        assert_eq!(root.scheme(), Scheme::Local);
        assert!(
            root.bucket().starts_with('/'),
            "local bucket should be an absolute path, got {:?}",
            root.bucket()
        );
        assert!(root.bucket().ends_with("/firn_data"));
        assert_eq!(root.prefix(), None);
    }

    #[test]
    fn local_keeps_absolute_path_as_is() {
        let root = StorageRoot::local("/var/lib/firn").unwrap();
        assert_eq!(root.bucket(), "/var/lib/firn");
        assert_eq!(root.scheme(), Scheme::Local);
    }

    #[test]
    fn local_trims_trailing_slash() {
        let with = StorageRoot::local("/var/lib/firn/").unwrap();
        let without = StorageRoot::local("/var/lib/firn").unwrap();
        assert_eq!(with, without);
        assert_eq!(with.bucket(), "/var/lib/firn");
    }

    #[test]
    fn local_rejects_empty() {
        assert!(matches!(
            StorageRoot::local("").unwrap_err(),
            FirnflowError::InvalidRequest(_)
        ));
    }

    #[test]
    fn parse_file_scheme_is_local_and_absolute() {
        let root = StorageRoot::parse("file:///srv/firn_data").unwrap();
        assert_eq!(root.scheme(), Scheme::Local);
        assert_eq!(root.bucket(), "/srv/firn_data");
        assert_eq!(root.prefix(), None);
    }

    #[test]
    fn local_namespace_uri_is_a_file_url() {
        let root = StorageRoot::local("/srv/firn_data").unwrap();
        assert_eq!(
            root.namespace_uri(&ns("docs")),
            "file:///srv/firn_data/docs"
        );
    }

    #[test]
    fn local_namespace_object_path_is_relative() {
        // The local object store is rooted at the base dir, so the
        // per-namespace path is just the namespace name.
        let root = StorageRoot::local("/srv/firn_data").unwrap();
        assert_eq!(root.namespace_object_path(&ns("docs")), "docs");
    }

    #[test]
    fn local_round_trips_through_parse() {
        let root = StorageRoot::local("/srv/firn_data").unwrap();
        let rendered = root.as_uri();
        assert_eq!(rendered, "file:///srv/firn_data");
        assert_eq!(StorageRoot::parse(&rendered).unwrap(), root);
    }
}
