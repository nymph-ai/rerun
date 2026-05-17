use std::collections::HashMap;

use crate::error::{CorpusError, Result};

/// Configuration for the [`crate::LanceCorpusProvider`].
#[derive(Clone, Debug)]
pub struct CorpusConfig {
    /// Lance dataset URI (e.g. `s3://nereid-audio/index/corpus.lance`).
    pub lance_table_uri: String,

    /// S3-compatible endpoint (MinIO/AWS). Empty string ⇒ AWS default.
    pub s3_endpoint: String,
    pub s3_region: String,
    pub s3_access_key: String,
    pub s3_secret_key: String,
    /// MinIO/path-style buckets need this; AWS virtual-hosted does not.
    pub s3_force_path_style: bool,

    /// `app_id` (a.k.a. `application_id`) for the synthetic Rerun recording.
    /// Stable across restarts so the viewer treats it as the same recording.
    pub application_id: String,
    /// `recording_id` (UUID-stable). Same stability requirement as
    /// `application_id`.
    pub recording_id: String,

    /// Optional max rows to consider on initial scan (None = unbounded).
    pub max_initial_rows: Option<usize>,
}

impl CorpusConfig {
    /// Build the `storage_options` map handed to `lance::DatasetBuilder` and
    /// the matching `object_store::ObjectStore` for direct S3 GETs.
    pub fn lance_storage_options(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if !self.s3_endpoint.is_empty() {
            out.insert("aws_endpoint".to_owned(), self.s3_endpoint.clone());
            out.insert("endpoint".to_owned(), self.s3_endpoint.clone());
            out.insert("AWS_ENDPOINT".to_owned(), self.s3_endpoint.clone());
        }
        if !self.s3_region.is_empty() {
            out.insert("aws_region".to_owned(), self.s3_region.clone());
            out.insert("region".to_owned(), self.s3_region.clone());
        }
        if !self.s3_access_key.is_empty() {
            out.insert("aws_access_key_id".to_owned(), self.s3_access_key.clone());
            out.insert("access_key_id".to_owned(), self.s3_access_key.clone());
        }
        if !self.s3_secret_key.is_empty() {
            out.insert(
                "aws_secret_access_key".to_owned(),
                self.s3_secret_key.clone(),
            );
            out.insert("secret_access_key".to_owned(), self.s3_secret_key.clone());
        }
        if self.s3_force_path_style {
            out.insert(
                "aws_virtual_hosted_style_request".to_owned(),
                "false".to_owned(),
            );
            out.insert(
                "virtual_hosted_style_request".to_owned(),
                "false".to_owned(),
            );
            out.insert("aws_allow_http".to_owned(), "true".to_owned());
            out.insert("allow_http".to_owned(), "true".to_owned());
        }
        out
    }

    pub fn validate(&self) -> Result<()> {
        if self.lance_table_uri.is_empty() {
            return Err(CorpusError::Config(
                "lance_table_uri must not be empty".into(),
            ));
        }
        if self.application_id.is_empty() {
            return Err(CorpusError::Config(
                "application_id must not be empty".into(),
            ));
        }
        if self.recording_id.is_empty() {
            return Err(CorpusError::Config("recording_id must not be empty".into()));
        }
        Ok(())
    }
}
