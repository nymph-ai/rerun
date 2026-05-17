# Vector Search Design Recon

NYM-94 targets the vector-search API surface in Rerun 0.32.0. The main
recon finding is that the OSS server is not a pure stub: dataset component
indexing already routes to Lance-backed chunk indexes. The user-facing Python
API is deprecated and wrapped in `NotImplementedError`, and there is no
TableEntry-native vector search API for Lance tables.

## Stub Locations

- Public Python stubs live in
  `rerun_py/rerun_sdk/rerun/catalog/_entry.py:730` and
  `rerun_py/rerun_sdk/rerun/catalog/_entry.py:799`. Both methods are decorated
  with `@deprecated`, call the pyo3 binding, and then catch every exception and
  raise `NotImplementedError` with the current "not supported" message
  (`_entry.py:773`, `_entry.py:781`, `_entry.py:827`, `_entry.py:830`).
- The draft sandbox wrapper repeats the same public deprecation behavior at
  `rerun_py/tests/api_sandbox/rerun_draft/catalog.py:281` and
  `rerun_py/tests/api_sandbox/rerun_draft/catalog.py:323`.
- The pyo3 methods themselves are not stubs. `DatasetEntryInternal` builds a
  `CreateIndexRequest` for `create_vector_search_index` at
  `rerun_py/src/catalog/dataset_entry.rs:642`, converts
  `VectorDistanceMetricLike` into the proto enum at
  `rerun_py/src/catalog/dataset_entry.rs:661`, and calls `/CreateIndex` at
  `rerun_py/src/catalog/dataset_entry.rs:684`.
- `DatasetEntryInternal.search_vector` builds a `SearchDatasetRequest` with
  `VectorIndexQuery { top_k }` at `rerun_py/src/catalog/dataset_entry.rs:853`
  and constructs a `SearchResultsTableProvider` at
  `rerun_py/src/catalog/dataset_entry.rs:880`.
- The server route is also wired, not stubbed, when compiled with the `lance`
  feature: `/CreateIndex` dispatches to `dataset.indexes().create_index(...)`
  at `crates/store/re_server/src/rerun_cloud.rs:1328`, while `/SearchDataset`
  dispatches to `DatasetChunkIndexes::search_dataset(...)` at
  `crates/store/re_server/src/rerun_cloud.rs:1391`.
- The no-`lance` server build is the only Rust-side unimplemented branch:
  `create_index requires the lance feature` at
  `crates/store/re_server/src/rerun_cloud.rs:1339`, and
  `search_dataset requires the lance feature` at
  `crates/store/re_server/src/rerun_cloud.rs:1402`.

## Lance Integration Surface

- Rerun pins `lance`, `lance-index`, and `lance-linalg` at `3.0.0` in
  `Cargo.toml:292`; `Cargo.lock:5192` confirms the resolved Lance crate is
  `3.0.0`.
- The Rerun CLI makes Lance support opt-in through the `lance` feature:
  `crates/top/rerun-cli/Cargo.toml:90` enables the OSS server, and
  `crates/top/rerun-cli/Cargo.toml:93` enables `re_server/lance`.
  `re_server` itself wires this feature to the three Lance crates at
  `crates/store/re_server/Cargo.toml:26`.
- Lance-backed catalog tables are already first-class server storage. `Table`
  has a `TableType::LanceDataset(Arc<lance::Dataset>)` variant at
  `crates/store/re_server/src/store/table.rs:17`, exposes it as a DataFusion
  provider via `lance::datafusion::LanceTableProvider::new(...)` at
  `crates/store/re_server/src/store/table.rs:105`, writes updates through Lance
  at `crates/store/re_server/src/store/table.rs:142`, reopens the latest
  version at `crates/store/re_server/src/store/table.rs:210`, and creates new
  Lance table entries with `lance::Dataset::write(...)` at
  `crates/store/re_server/src/store/table.rs:233`.
- The catalog server creates default Lance table URLs under its storage
  directory at `crates/store/re_server/src/rerun_cloud.rs:2041`, and accepts
  registered Lance directories through `register_table` at
  `crates/store/re_server/src/rerun_cloud.rs:1815`.
- The existing dataset search path creates an internal Lance dataset for each
  RRD component index. `DatasetChunkIndexes` owns a temp dir and in-memory index
  map at `crates/store/re_server/src/chunk_index/mod.rs:80`; `create_index`
  materializes a Lance dataset at
  `crates/store/re_server/src/chunk_index/index.rs:321`; and `create_lance_index`
  calls Lance `create_index` with vector, inverted, or btree params at
  `crates/store/re_server/src/chunk_index/index.rs:396`.
- Lance vector index construction is already reachable from `re_server`:
  `VectorIvfPq` maps to `VectorIndexParams::with_ivf_pq_params(...)` at
  `crates/store/re_server/src/chunk_index/index.rs:418`, distance metrics map
  to `lance_linalg::distance::MetricType` at
  `crates/store/re_server/src/chunk_index/index.rs:433`, and the final call is
  `lance_table.create_index(&["instance"], ...)` at
  `crates/store/re_server/src/chunk_index/index.rs:454`.
- Lance nearest-neighbor search is also reachable from `re_server`:
  `search_index` calls `scanner.nearest(FIELD_INSTANCE, query_data, top_k)` at
  `crates/store/re_server/src/chunk_index/search.rs:45`, applies Rerun scan
  parameters at `crates/store/re_server/src/chunk_index/search.rs:88`, and
  returns a Lance `RecordBatchStream` at
  `crates/store/re_server/src/chunk_index/search.rs:50`.
- Structural issue: the existing `DatasetEntry` API indexes RRD component data
  in a temporary per-dataset Lance index, not a user-created `TableEntry`.
  NYM-94's later acceptance text mentions "TableEntry-managed Lance table";
  that will require a new table search/index route or an API adjustment, because
  `CreateIndexRequest` currently names an `IndexColumn` by Rerun entity path and
  component descriptor (`crates/store/re_protos/proto/rerun/v1alpha1/cloud.proto:542`),
  not a table id and column name.

## Arrow/DataFusion Marshaling

- Search results are already marshaled as a DataFusion `TableProvider`.
  `SearchResultsTableProvider::new(...)` rejects scan parameters at
  `crates/store/re_datafusion/src/search_provider.rs:39`, fetches the schema by
  issuing `/SearchDataset` with `limit_len = Some(0)` at
  `crates/store/re_datafusion/src/search_provider.rs:69`, and decodes each
  `SearchDatasetResponse` into a `RecordBatch` at
  `crates/store/re_datafusion/src/search_provider.rs:144`.
- The generic gRPC-to-DataFusion adapter fetches a schema once in
  `GrpcStreamProvider::prepare(...)` at
  `crates/store/re_datafusion/src/grpc_streaming_provider.rs:79`, then streams
  decoded batches in `GrpcStream` at
  `crates/store/re_datafusion/src/grpc_streaming_provider.rs:210`.
- Table scans are also stream-marshaled as `RecordBatch`es: `/ScanTable` scans
  the table's `TableProvider`, executes the plan, and wraps each batch into
  `ScanTableResponse` at `crates/store/re_server/src/rerun_cloud.rs:1890`.
- The established scoring column name is `_distance`, not `_similarity`.
  Existing Rerun vector-search tests assert it by name at
  `crates/store/re_server/src/chunk_index/mod.rs:504`.
- Lance's default behavior appends scoring columns when no projection is
  specified. For a stable Rerun API, the fork should explicitly return all user
  columns in their existing order plus `_distance` as the final column, and
  should avoid synthesizing `_similarity` in Rust. Cosine similarity can be
  derived by callers when they know vectors are normalized.

## Implementation Sketch: `create_vector_search_index`

Phase 3 should not start by changing proto fields if we keep the existing
`DatasetEntry` API. The minimal change is to remove the Python deprecation
wrappers and make the existing dataset component index path production-worthy:

```rust
async fn create_vector_search_index(req: CreateIndexRequest) -> tonic::Result<CreateIndexResponse> {
    let store = self.store.read().await;
    let dataset_id = get_entry_id_from_headers(&store, &request)?;
    let dataset = store.dataset(dataset_id)?;
    let config: IndexConfig = req.try_into()?;

    validate_vector_index_config(&dataset, &config)?;
    // Existing path:
    // - creates a temporary Lance dataset for this component index
    // - backfills existing chunks
    // - uses lance::Dataset::create_index through DatasetIndexExt
    dataset.indexes().create_index(dataset, config.into()).await
}
```

The existing implementation already handles the main Lance call:

```rust
let ivf = lance_index::vector::ivf::IvfBuildParams {
    target_partition_size: target_partition_num_rows.map(|v| v as usize),
    ..Default::default()
};
let pq = lance_index::vector::pq::PQBuildParams {
    num_sub_vectors: num_sub_vectors as usize,
    ..Default::default()
};
let params = lance::index::vector::VectorIndexParams::with_ivf_pq_params(metric, ivf, pq);
lance_table.create_index(&["instance"], IndexType::Vector, None, &params, false).await?;
```

Changes needed for the current dataset path:

- Convert `AlreadyExists` into a clear idempotency policy. Today
  `DatasetChunkIndexes::add_index` returns `AlreadyExists` if a component path
  already has an index (`crates/store/re_server/src/chunk_index/mod.rs:302`),
  while the lower-level Lance helper treats some Lance "already exists" errors
  as `Ok(())` (`crates/store/re_server/src/chunk_index/index.rs:460`).
  Phase 3 should choose one policy; for the deprecated public API text that says
  a second call replaces the index, add `replace` semantics or update the
  docstring.
- Map validation failures to `invalid_argument` and unsupported build states to
  `failed_precondition`, instead of the current broad `IndexingError` ->
  `internal` mapping in `crates/store/re_server/src/store/error.rs:62`.
- Keep the existing "not enough rows to train PQ" soft failure behavior only if
  `list_search_indexes()` clearly reports that the index exists but is
  under-trained. The current code logs and succeeds for Lance training failures
  at `crates/store/re_server/src/chunk_index/index.rs:463`.

If NYM-94 must index `TableEntry` Lance tables, add a new route instead:

```rust
message CreateTableVectorIndexRequest {
    rerun.common.v1alpha1.EntryId table_id = 1;
    string column = 2;
    VectorIvfPqIndex index = 3;
    bool replace = 4;
}

async fn create_table_vector_index(req: CreateTableVectorIndexRequest) -> tonic::Result<()> {
    let mut store = self.store.write().await;
    let table = store.table_mut(req.table_id)?;
    let ds = table.lance_dataset_mut()?;
    validate_fixed_size_list_float_column(ds.schema(), &req.column)?;
    let params = vector_params(req.index)?;
    ds.create_index(&[req.column.as_str()], IndexType::Vector, None, &params, req.replace).await?;
    ds.checkout_latest().await?;
    table.replace_lance_dataset(Arc::new(lance::Dataset::open(ds.uri()).await?));
    Ok(())
}
```

That table route needs a small accessor on `Table` because `TableType` is
private and only exposes a DataFusion provider today.

## Implementation Sketch: `search_vector`

For the existing `DatasetEntry` API, the Rust server path is already:

```rust
async fn search_vector(req: SearchDatasetRequest) -> tonic::Result<SearchDatasetResponseStream> {
    validate_top_k(req.properties.top_k)?;
    let store = self.store.read().await;
    let dataset = store.dataset(entry_id)?;
    let index = dataset.indexes().get(&req.column.entity_path, &req.column.descriptor.component).await
        .ok_or_else(|| Status::not_found("vector index not found"))?;
    let stream = search::search_index(index, req).await?;
    stream_record_batches(stream)
}
```

Needed changes:

- Reject `top_k == 0` at Rerun's API boundary. Lance also rejects zero-k, but
  surfacing it as `invalid_argument` avoids an opaque internal error.
- Validate the query has exactly one column and one logical vector. Existing
  `search_index` uses `if request.query.columns().len() != 1 && request.query.num_rows() != 1`
  at `crates/store/re_server/src/chunk_index/search.rs:22`; that should be
  `||`, not `&&`.
- Keep Lance's dimension validation by passing the Arrow vector to
  `Scanner::nearest`, but map the Lance error to `invalid_argument`. The Python
  binding currently serializes `VectorLike` as one `Float32Array` column named
  `items` at `rerun_py/src/catalog/indexes.rs:255`, and Lance accepts a flat
  float array query for a fixed-size-list vector column.
- Return `_distance` as a `Float32` column. Do not return `_similarity` in v1.
  The current tests only require `_distance` by name, and Lance's distance
  values may be approximate or metric-specific.

For a TableEntry-native search route:

```rust
async fn search_table_vector(req: SearchTableVectorRequest) -> tonic::Result<SearchTableVectorResponseStream> {
    validate_top_k(req.top_k)?;
    let store = self.store.read().await;
    let table = store.table(req.table_id)?;
    let ds = table.lance_dataset()?;
    validate_fixed_size_list_float_column(ds.schema(), &req.column)?;

    let query = decode_single_float_vector(req.query)?;
    let mut scanner = ds.scan();
    scanner.nearest(&req.column, &query, req.top_k as usize)?;
    scanner.project(&project_all_columns_plus_distance(ds.schema()))?;
    let stream = scanner.try_into_stream().await?;
    stream_record_batches(stream)
}
```

Output shape should be:

1. all original table columns in existing schema order
2. `_distance: Float32` as the final column

The direct Lance stream already includes `_distance`; the explicit projection
should make its position deterministic rather than relying on Lance's legacy
autoprojection behavior.

## Risks Discovered

- **Phase 3 target mismatch.** The public API named in NYM-94 is
  `DatasetEntry.search_vector`, but the acceptance criteria mention
  `TableEntry`-managed Lance tables. Those are different storage paths in
  Rerun 0.32.0. Dataset component search can be enabled mostly by removing
  Python deprecation wrappers and tightening server errors; table-vector search
  needs new proto/service/Python API surface.
- **Indexes are currently temporary for datasets.** `DatasetChunkIndexes` keeps
  its Lance data in a `TempDir` (`crates/store/re_server/src/chunk_index/mod.rs:80`),
  so dataset indexes are lost on server restart. That is acceptable for a cache
  over RRD segments, but not for persistent table search unless the table route
  writes into the table's own Lance dataset.
- **Existing query validation bug.** The query shape check in
  `search_index` uses `&&` where it should use `||`
  (`crates/store/re_server/src/chunk_index/search.rs:22`), so malformed query
  batches can slip through until Lance errors later.
- **Lance training behavior for small data.** Existing code treats "not enough
  rows to train PQ" as success (`crates/store/re_server/src/chunk_index/index.rs:463`).
  For a user-visible API, this needs an explicit result state or a deterministic
  flat-scan fallback.
- **Metric semantics.** The proto exposes L2, cosine, dot, and hamming at
  `crates/store/re_protos/proto/rerun/v1alpha1/cloud.proto:603`, but Lance's
  returned `_distance` values are metric-specific and sometimes approximate.
  The fork should not invent `_similarity` without metric-specific conversion
  rules.
- **Feature availability.** `re_server` only has Lance when built with the
  `lance` feature. The Nix fork package already built a CLI with Lance enabled,
  but docker/image work in later phases must keep that feature on.

## Tests to Model

- Rust catalog index lifecycle:
  `crates/store/re_redap_tests/src/tests/indexes.rs:21` exercises create, list,
  search, delete, duplicate create, and missing-index search for scalar, FTS,
  and vector indexes.
- Rust unit vector search:
  `crates/store/re_server/src/chunk_index/mod.rs:372` creates a tiny vector
  component dataset, builds a vector index, searches, and asserts `_distance`.
- Python table-write/upsert:
  `rerun_py/tests/e2e_redap_tests/test_table_write.py:17` covers DataFusion
  writes, and `rerun_py/tests/e2e_redap_tests/test_table_write.py:125` covers
  `TableEntry.upsert` with the `rerun:is_table_index` metadata.
- Python dataset registration and segment-table joins:
  `rerun_py/tests/api_sandbox/test_draft/test_dataset_basics.py:89` covers
  `DatasetEntry.register`, and
  `rerun_py/tests/e2e_redap_tests/test_datafusion_utils.py:76` covers
  `segment_table(join_meta=...)`.

## Estimated Implementation Size

- Phase 3A, enable existing `DatasetEntry` component vector search:
  about 80-140 lines of Rust for validation/error mapping and idempotency,
  plus 20-40 lines of Python wrapper/docstring cleanup.
- Phase 3B, make dataset indexes production-grade across restarts:
  about 150-250 lines of Rust if index storage remains separate but durable;
  more if it must be registered as catalog state.
- Phase 3C, add `TableEntry` Lance vector index/search, if that is the real
  NYM-92 need:
  about 300-500 lines of Rust/proto/client plumbing and 80-150 lines of Python
  binding/wrapper/tests. This is the phase that needs a re-plan decision before
  implementation.
