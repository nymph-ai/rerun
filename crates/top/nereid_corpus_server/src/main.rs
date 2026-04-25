//! Standalone Rerun Data Platform (Redap) server backed by a Lance corpus
//! index of Opus chunks living in S3.
//!
//! Replaces nereid's `python -m nereid.corpus_streamer.server`. Construction
//! is straight-line: load env-driven [`CorpusConfig`] → build the
//! [`LanceCorpusProvider`] → wrap it in [`LazyChunkStore`] → register the
//! resulting [`ResolvedStore`] as the single dataset of an in-process Redap
//! server. The viewer then connects over `rerun+http://…` and lazily fetches
//! chunks on demand — no per-query materialization step.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use re_chunk_store::LazyChunkStore;
use re_corpus_producer::{CorpusConfig, LanceCorpusProvider};
use re_protos::EntryName;
use re_protos::common::v1alpha1::ext::IfDuplicateBehavior;
use re_server::{RerunCloudHandlerBuilder, ResolvedStore, ServerBuilder};

#[derive(Debug, Parser)]
#[command(author, version, about = "Nereid corpus Redap server")]
struct Cli {
    /// IP address to listen on. Env: `NEREID_CORPUS_STREAMER_REDAP_HOST`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_REDAP_HOST",
        default_value = "0.0.0.0"
    )]
    host: String,

    /// gRPC port. Env: `NEREID_CORPUS_STREAMER_REDAP_PORT`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_REDAP_PORT",
        default_value_t = 51234
    )]
    port: u16,

    /// Catalog/dataset entry name shown in the viewer.
    /// Env: `NEREID_CORPUS_STREAMER_DATASET_NAME`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_DATASET_NAME",
        default_value = "corpus"
    )]
    dataset_name: String,

    /// Stable application id for the synthetic Rerun recording.
    /// Env: `NEREID_CORPUS_STREAMER_APPLICATION_ID`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_APPLICATION_ID",
        default_value = "nereid"
    )]
    application_id: String,

    /// Stable recording id for the synthetic Rerun recording.
    /// Env: `NEREID_CORPUS_STREAMER_RECORDING_ID`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_RECORDING_ID",
        default_value = "corpus"
    )]
    recording_id: String,

    /// Lance dataset URI (e.g. `s3://nereid-audio/index/corpus.lance`).
    /// Env: `NEREID_CORPUS_STREAMER_LANCE_URI`.
    #[arg(long, env = "NEREID_CORPUS_STREAMER_LANCE_URI")]
    lance_uri: Option<String>,

    /// S3 bucket name (used to derive `lance_uri` when `--lance-uri` is unset).
    /// Env: `NEREID_CORPUS_STREAMER_S3_BUCKET` or `NEREID_AUDIO_CORPUS_S3_BUCKET`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_S3_BUCKET",
        default_value = "nereid-audio"
    )]
    s3_bucket: String,

    /// Lance object key inside the bucket (used with `--s3-bucket` when
    /// `--lance-uri` is unset). Env: `NEREID_CORPUS_STREAMER_LANCE_KEY`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_LANCE_KEY",
        default_value = "index/corpus.lance"
    )]
    lance_key: String,

    /// S3 endpoint URL (empty = AWS default).
    /// Env: `NEREID_CORPUS_STREAMER_S3_ENDPOINT`.
    #[arg(long, env = "NEREID_CORPUS_STREAMER_S3_ENDPOINT", default_value = "")]
    s3_endpoint: String,

    /// S3 region. Env: `NEREID_CORPUS_STREAMER_S3_REGION`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_S3_REGION",
        default_value = "us-east-1"
    )]
    s3_region: String,

    /// S3 access key. Env: `NEREID_CORPUS_STREAMER_S3_ACCESS_KEY`.
    #[arg(long, env = "NEREID_CORPUS_STREAMER_S3_ACCESS_KEY", default_value = "")]
    s3_access_key: String,

    /// S3 secret key. Env: `NEREID_CORPUS_STREAMER_S3_SECRET_KEY`.
    #[arg(long, env = "NEREID_CORPUS_STREAMER_S3_SECRET_KEY", default_value = "")]
    s3_secret_key: String,

    /// Use S3 path-style requests (required for `MinIO`).
    /// Env: `NEREID_CORPUS_STREAMER_S3_FORCE_PATH_STYLE`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_S3_FORCE_PATH_STYLE",
        default_value_t = true
    )]
    s3_force_path_style: bool,

    /// Optional cap on initial Lance scan rows.
    /// Env: `NEREID_CORPUS_STREAMER_MAX_INITIAL_ROWS`.
    #[arg(long, env = "NEREID_CORPUS_STREAMER_MAX_INITIAL_ROWS")]
    max_initial_rows: Option<usize>,

    /// Live-edge polling interval in seconds. Set to 0 to disable polling
    /// (the server will only ever surface rows present at startup).
    /// Env: `NEREID_CORPUS_STREAMER_POLL_INTERVAL_SECONDS`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_POLL_INTERVAL_SECONDS",
        default_value_t = 30
    )]
    poll_interval_seconds: u64,

    /// Eviction janitor cadence in seconds. Set to 0 to disable cursor-driven
    /// eviction entirely (useful for tests / small corpora that fit in RAM).
    /// Env: `NEREID_CORPUS_STREAMER_EVICTION_INTERVAL_SECONDS`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_EVICTION_INTERVAL_SECONDS",
        default_value_t = 60
    )]
    eviction_interval_seconds: u64,

    /// Retention behind the viewer's cursor, in seconds. Physical chunks
    /// whose timeline range ends before `cursor.min - retention_before` are
    /// evictable. The corpus timeline is timestamp-based, so this is real
    /// wall-clock seconds. Env: `NEREID_CORPUS_STREAMER_RETENTION_BEFORE_SECONDS`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_RETENTION_BEFORE_SECONDS",
        default_value_t = 600
    )]
    retention_before_seconds: u64,

    /// Retention ahead of the viewer's cursor, in seconds. Physical chunks
    /// whose timeline range starts after `cursor.max + retention_after` are
    /// evictable. Usually small for a mostly-historical corpus.
    /// Env: `NEREID_CORPUS_STREAMER_RETENTION_AFTER_SECONDS`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_RETENTION_AFTER_SECONDS",
        default_value_t = 60
    )]
    retention_after_seconds: u64,

    /// Timeline name to apply retention against. The corpus producer logs
    /// every audio chunk on `capture_time`, which is the only sensible
    /// default — override only if a future producer adds a second
    /// timestamp timeline. Env: `NEREID_CORPUS_STREAMER_RETENTION_TIMELINE`.
    #[arg(
        long,
        env = "NEREID_CORPUS_STREAMER_RETENTION_TIMELINE",
        default_value = "capture_time"
    )]
    retention_timeline: String,
}

impl Cli {
    fn split(self) -> (CorpusConfig, ServerOpts) {
        let opts = ServerOpts {
            host: self.host,
            port: self.port,
            dataset_name: self.dataset_name,
            poll_interval_seconds: self.poll_interval_seconds,
            eviction: EvictionOpts {
                interval_seconds: self.eviction_interval_seconds,
                retention_before_seconds: self.retention_before_seconds,
                retention_after_seconds: self.retention_after_seconds,
                timeline: self.retention_timeline,
            },
        };
        let lance_uri = self.lance_uri.unwrap_or_else(|| {
            format!(
                "s3://{}/{}",
                self.s3_bucket,
                self.lance_key.trim_start_matches('/')
            )
        });
        let config = CorpusConfig {
            lance_table_uri: lance_uri,
            s3_endpoint: self.s3_endpoint,
            s3_region: self.s3_region,
            s3_access_key: self.s3_access_key,
            s3_secret_key: self.s3_secret_key,
            s3_force_path_style: self.s3_force_path_style,
            application_id: self.application_id,
            recording_id: self.recording_id,
            max_initial_rows: self.max_initial_rows,
        };
        (config, opts)
    }
}

struct ServerOpts {
    host: String,
    port: u16,
    dataset_name: String,
    poll_interval_seconds: u64,
    eviction: EvictionOpts,
}

struct EvictionOpts {
    interval_seconds: u64,
    retention_before_seconds: u64,
    retention_after_seconds: u64,
    timeline: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    re_log::setup_logging();

    let cli = Cli::parse();
    let (config, opts) = cli.split();

    let bind_ip: std::net::IpAddr = opts
        .host
        .parse()
        .with_context(|| format!("host: {:?}", opts.host))?;
    let bind_addr = SocketAddr::new(bind_ip, opts.port);

    let dataset_name = EntryName::new(&opts.dataset_name)
        .with_context(|| format!("invalid dataset name {:?}", opts.dataset_name))?;

    tracing::info!(
        lance = %config.lance_table_uri,
        application_id = %config.application_id,
        recording_id = %config.recording_id,
        "building LanceCorpusProvider"
    );

    let runtime = tokio::runtime::Handle::current();
    let provider = LanceCorpusProvider::build(config, runtime)
        .await
        .context("LanceCorpusProvider::build failed")?;

    let lazy = Arc::new(LazyChunkStore::new(provider));
    let resolved = ResolvedStore::from_lazy(Arc::clone(&lazy));

    let handler = RerunCloudHandlerBuilder::new()
        .with_resolved_as_dataset(dataset_name, resolved, IfDuplicateBehavior::Error)
        .await
        .context("registering corpus dataset")?
        .build();

    let cloud_server =
        re_protos::cloud::v1alpha1::rerun_cloud_service_server::RerunCloudServiceServer::new(
            handler,
        )
        .max_decoding_message_size(re_grpc_server::MAX_DECODING_MESSAGE_SIZE)
        .max_encoding_message_size(re_grpc_server::MAX_ENCODING_MESSAGE_SIZE);

    let server = ServerBuilder::default()
        .with_address(bind_addr)
        .with_service(cloud_server)
        .build();

    let mut handle = server.start().await.context("starting Redap server")?;
    tracing::info!(addr = %handle.connect_addr(), "nereid-corpus-server ready");

    let poll_handle = spawn_live_edge_poller(Arc::clone(&lazy), opts.poll_interval_seconds);
    let eviction_handle = spawn_eviction_janitor(Arc::clone(&lazy), opts.eviction);

    wait_for_shutdown(&mut handle).await;
    handle.shutdown_and_wait().await;
    if let Some(poll) = poll_handle {
        poll.abort();
    }
    if let Some(eviction) = eviction_handle {
        eviction.abort();
    }
    Ok(())
}

/// Spawn a background task that periodically asks the corpus provider to
/// re-scan the Lance index for new rows and, when any are found, extends
/// the chunk-store's manifest in place.
///
/// Returns `None` when polling is disabled (`interval_seconds == 0`).
fn spawn_live_edge_poller(
    lazy: Arc<re_chunk_store::LazyChunkStore<LanceCorpusProvider>>,
    interval_seconds: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    if interval_seconds == 0 {
        tracing::info!("live-edge polling disabled (interval=0)");
        return None;
    }
    let interval = std::time::Duration::from_secs(interval_seconds);
    tracing::info!(?interval, "starting live-edge poller");

    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately. Skip it so we don't double-scan
        // right after the startup `scan_all` already populated the provider.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            match lazy.provider().poll_for_new_rows().await {
                Ok(Some((manifest, raw_manifest))) => {
                    let n = manifest.num_chunks();
                    lazy.extend_with_manifest(manifest, raw_manifest);
                    tracing::info!(num_chunks = n, "absorbed new corpus rows");
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "live-edge poll failed");
                }
            }
        }
    }))
}

/// Spawn the cursor-driven eviction janitor.
///
/// Reads the latest viewer cursor (recorded by `query_dataset` via
/// [`re_chunk_store::LazyChunkStore::observe_query_cursor`]) and drops
/// physical chunks whose timeline range falls outside
/// `[cursor.min - retention_before, cursor.max + retention_after]`.
/// Eviction is *shallow* — chunks remain virtual and reload on the next
/// query that touches their range.
///
/// Returns `None` (and logs why) when eviction is disabled — either via
/// `interval_seconds == 0` or because both retention bounds are zero, which
/// would make every cycle drop every chunk.
fn spawn_eviction_janitor(
    lazy: Arc<re_chunk_store::LazyChunkStore<LanceCorpusProvider>>,
    opts: EvictionOpts,
) -> Option<tokio::task::JoinHandle<()>> {
    if opts.interval_seconds == 0 {
        tracing::info!("eviction janitor disabled (interval=0)");
        return None;
    }
    if opts.retention_before_seconds == 0 && opts.retention_after_seconds == 0 {
        tracing::warn!(
            "eviction janitor disabled: retention_before=0 AND retention_after=0 \
             would evict every chunk on every cycle"
        );
        return None;
    }

    let EvictionOpts {
        interval_seconds,
        retention_before_seconds,
        retention_after_seconds,
        timeline: timeline_name,
    } = opts;
    let interval = std::time::Duration::from_secs(interval_seconds);
    let timeline = re_chunk_store::TimelineName::new(&timeline_name);
    let retention_before_ns = i64::try_from(retention_before_seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000_000_000);
    let retention_after_ns = i64::try_from(retention_after_seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000_000_000);
    tracing::info!(
        ?interval,
        timeline = %timeline_name,
        retention_before_seconds,
        retention_after_seconds,
        "starting eviction janitor"
    );

    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // skip the immediate first tick
        loop {
            ticker.tick().await;
            let Some(cursor) = lazy.cursor(&timeline) else {
                tracing::debug!("eviction janitor: no cursor observed yet, skipping cycle");
                continue;
            };

            let lo = cursor.min().as_i64().saturating_sub(retention_before_ns);
            let hi = cursor.max().as_i64().saturating_add(retention_after_ns);
            let keep = re_chunk_store::AbsoluteTimeRange::new(lo, hi);

            let stats = lazy.evict_outside_window(&timeline, keep);
            if stats.evicted > 0 {
                tracing::info!(
                    evicted = stats.evicted,
                    retained = stats.retained,
                    cursor_min = cursor.min().as_i64(),
                    cursor_max = cursor.max().as_i64(),
                    "evicted out-of-window chunks"
                );
            } else {
                tracing::debug!(
                    retained = stats.retained,
                    cursor_min = cursor.min().as_i64(),
                    cursor_max = cursor.max().as_i64(),
                    "eviction cycle steady-state"
                );
            }
        }
    }))
}

#[cfg(unix)]
async fn wait_for_shutdown(handle: &mut re_server::ServerHandle) {
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM, shutting down"),
        _ = int.recv() => tracing::info!("received SIGINT, shutting down"),
        () = handle.wait_for_shutdown() => tracing::warn!("gRPC endpoint stopped on its own"),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown(handle: &mut re_server::ServerHandle) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("received Ctrl-C, shutting down"),
        () = handle.wait_for_shutdown() => tracing::warn!("gRPC endpoint stopped on its own"),
    }
}
