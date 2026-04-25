use std::sync::Arc;

use ahash::HashMap;
use bytes::Bytes;
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use parking_lot::RwLock;
use sha2::{Digest as _, Sha256};

use crate::config::CorpusConfig;
use crate::error::{CorpusError, Result};

/// Fetch Opus chunks from S3 (MinIO/AWS) by `(bucket, key)`.
///
/// Caches one `AmazonS3` client per bucket — credentials and endpoint
/// come from [`CorpusConfig`] and are shared across buckets, but the
/// `ObjectStore` instance is bucket-scoped.
pub struct S3Fetcher {
    config: Arc<CorpusConfig>,
    clients: RwLock<HashMap<String, Arc<dyn ObjectStore>>>,
}

impl S3Fetcher {
    pub fn new(config: Arc<CorpusConfig>) -> Self {
        Self {
            config,
            clients: RwLock::new(HashMap::default()),
        }
    }

    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        expected_sha256: Option<&str>,
    ) -> Result<Bytes> {
        let store = self.client_for(bucket)?;
        let path = ObjectPath::from(key);
        let result = store.get(&path).await?;
        let bytes = result.bytes().await?;
        if let Some(expected) = expected_sha256 {
            verify_sha256(bucket, key, &bytes, expected)?;
        }
        Ok(bytes)
    }

    fn client_for(&self, bucket: &str) -> Result<Arc<dyn ObjectStore>> {
        if let Some(c) = self.clients.read().get(bucket).cloned() {
            return Ok(c);
        }
        let mut guard = self.clients.write();
        if let Some(c) = guard.get(bucket).cloned() {
            return Ok(c);
        }
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_access_key_id(&self.config.s3_access_key)
            .with_secret_access_key(&self.config.s3_secret_key);
        if !self.config.s3_endpoint.is_empty() {
            builder = builder.with_endpoint(&self.config.s3_endpoint);
        }
        if !self.config.s3_region.is_empty() {
            builder = builder.with_region(&self.config.s3_region);
        }
        if self.config.s3_force_path_style {
            // MinIO requires path-style + plain HTTP on non-TLS deployments.
            builder = builder.with_virtual_hosted_style_request(false);
            builder = builder.with_allow_http(true);
        }
        let store: Arc<dyn ObjectStore> = Arc::new(builder.build()?);
        guard.insert(bucket.to_owned(), store.clone());
        Ok(store)
    }
}

fn verify_sha256(bucket: &str, key: &str, bytes: &Bytes, expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(CorpusError::Sha256Mismatch {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}
