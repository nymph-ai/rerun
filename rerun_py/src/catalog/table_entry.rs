use std::sync::Arc;

use arrow::ffi_stream::ArrowArrayStreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::pyarrow::FromPyArrow as _;
use arrow::record_batch::RecordBatch;
use datafusion::catalog::{MemTable, TableProvider};
use datafusion_ffi::table_provider::FFI_TableProvider;
use pyo3::exceptions::PyRuntimeError;
use pyo3::types::{PyAnyMethods as _, PyCapsule};
use pyo3::{Bound, Py, PyAny, PyRef, PyRefMut, PyResult, Python, pyclass, pymethods};
use re_datafusion::TableEntryTableProvider;
use re_protos::cloud::v1alpha1::ext::{EntryDetails, ProviderDetails, TableEntry, TableInsertMode};
use re_protos::cloud::v1alpha1::{
    CreateTableVectorIndexRequest, SearchTableVectorRequest, VectorIvfPqIndex,
};
use tokio_stream::StreamExt as _;

use crate::catalog::entry::set_entry_name;
use crate::catalog::table_provider_adapter::ffi_logical_codec_from_pycapsule;
use crate::catalog::{
    PyCatalogClientInternal, PyEntryDetails, PyTableProviderAdapterInternal,
    VectorDistanceMetricLike, VectorLike, to_py_err,
};
use crate::trace_context::read_trace_context_from_python;
use crate::utils::{get_tokio_runtime, wait_for_future};

/// A table entry in the catalog.
///
/// Note: this object acts as a table provider for DataFusion.
//TODO(ab): expose metadata about the table (e.g. stuff found in `provider_details`).
#[pyclass(name = "TableEntryInternal", module = "rerun_bindings.rerun_bindings")]
pub struct PyTableEntryInternal {
    client: Py<PyCatalogClientInternal>,
    entry_details: EntryDetails,
    lazy_provider: Option<Arc<dyn TableProvider + Send>>,
    url: Option<String>,
}

#[pymethods]
impl PyTableEntryInternal {
    //
    // Entry methods
    //

    fn catalog(&self, py: Python<'_>) -> Py<PyCatalogClientInternal> {
        self.client.clone_ref(py)
    }

    fn entry_details(&self, py: Python<'_>) -> PyResult<Py<PyEntryDetails>> {
        Py::new(py, PyEntryDetails(self.entry_details.clone()))
    }

    /// Delete this entry from the catalog.
    fn delete(&mut self, py: Python<'_>) -> PyResult<()> {
        let _span = read_trace_context_from_python(py, "TableEntry.delete").entered();
        let connection = self.client.borrow_mut(py).connection().clone();
        connection.delete_entry(py, self.entry_details.id)
    }

    fn set_name(&mut self, py: Python<'_>, name: String) -> PyResult<()> {
        let _span = read_trace_context_from_python(py, "TableEntry.set_name").entered();
        set_entry_name(py, name, &mut self.entry_details, &self.client)
    }

    //
    // Table entry methods
    //

    /// Returns a DataFusion table provider capsule.
    fn __datafusion_table_provider__<'py>(
        self_: PyRefMut<'py, Self>,
        session: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyCapsule>> {
        let _span =
            read_trace_context_from_python(self_.py(), "TableEntry.__datafusion_table_provider__")
                .entered();
        let provider = Self::table_provider(self_)?;

        let capsule_name = cr"datafusion_table_provider".into();

        let runtime = get_tokio_runtime().handle().clone();
        let codec = ffi_logical_codec_from_pycapsule(session)?;
        let provider = FFI_TableProvider::new_with_ffi_codec(provider, false, Some(runtime), codec);

        PyCapsule::new(session.py(), provider, Some(capsule_name))
    }

    /// Registers the table with the DataFusion context and return a DataFrame.
    pub fn reader(self_: PyRef<'_, Self>) -> PyResult<Bound<'_, PyAny>> {
        let py = self_.py();

        let client = self_.client.borrow(py);
        let table_name = self_.entry_details.name.clone();
        let ctx = client.ctx(py)?;
        let ctx = ctx.bind(py);

        // Any tables for which we have a TableEntry are already
        // registered with the CatalogProvider.

        let df = ctx.call_method1("table", (table_name.as_str(),))?;

        Ok(df)
    }

    /// Convert this table to a [`pyarrow.RecordBatchReader`][].
    fn to_arrow_reader<'py>(
        self_: PyRef<'py, Self>,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let _span = read_trace_context_from_python(py, "TableEntry.to_arrow_reader").entered();
        let df = Self::reader(self_)?;

        py.import("pyarrow")?
            .getattr("RecordBatchReader")?
            .call_method1("from_stream", (df,))
    }

    /// The table's storage URL.
    #[getter]
    pub fn storage_url(&self) -> String {
        self.url.clone().unwrap_or_default()
    }

    pub fn __str__(&self) -> String {
        format!("TableEntry(url='{}')", self.url.clone().unwrap_or_default())
    }

    /// Write record batches to the table.
    fn write_batches(
        self_: Py<Self>,
        py: Python<'_>,
        batches: &Bound<'_, PyAny>,
        insert_mode: PyTableInsertModeInternal,
    ) -> PyResult<()> {
        let _span = read_trace_context_from_python(py, "TableEntry.write_batches").entered();
        let entry_id = self_.borrow(py).entry_details.id;
        let connection = self_
            .borrow_mut(py)
            .client
            .borrow_mut(py)
            .connection()
            .clone();
        let stream = ArrowArrayStreamReader::from_pyarrow_bound(batches)?;
        connection.write_table(py, entry_id, stream, insert_mode)?;
        Ok(())
    }

    /// Create a vector index on a FixedSizeList<Float32, N> column.
    #[pyo3(signature = (
        column,
        metric = VectorDistanceMetricLike::VectorDistanceMetric(crate::catalog::PyVectorDistanceMetric::Cosine),
        replace = false,
    ))]
    fn create_vector_index(
        self_: PyRef<'_, Self>,
        column: String,
        metric: VectorDistanceMetricLike,
        replace: bool,
    ) -> PyResult<()> {
        let py = self_.py();
        let _span = read_trace_context_from_python(py, "TableEntry.create_vector_index").entered();
        let connection = self_.client.borrow(py).connection().clone();
        let table_id = self_.entry_details.id;
        let metric: re_protos::cloud::v1alpha1::VectorDistanceMetric = metric.try_into()?;

        let request = CreateTableVectorIndexRequest {
            table_id: Some(table_id.into()),
            column,
            index: Some(VectorIvfPqIndex {
                target_partition_num_rows: None,
                num_sub_vectors: Some(1),
                distance_metrics: metric as i32,
            }),
            replace,
        };

        wait_for_future(py, async {
            connection
                .client()
                .await?
                .inner()
                .create_table_vector_index(tonic::Request::new(request))
                .await
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
            Ok(())
        })
    }

    /// Search a vector index on a FixedSizeList<Float32, N> column.
    fn search_vector<'py>(
        self_: PyRef<'py, Self>,
        query: VectorLike<'_>,
        column: String,
        top_k: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = self_.py();
        let _span = read_trace_context_from_python(py, "TableEntry.search_vector").entered();
        let connection = self_.client.borrow(py).connection().clone();
        let table_id = self_.entry_details.id;

        let query = record_batch_to_arrow_ipc(&query.to_record_batch()?)?;
        let request = SearchTableVectorRequest {
            table_id: Some(table_id.into()),
            column,
            query: query.into(),
            top_k,
        };

        let provider: Arc<dyn TableProvider + Send> = wait_for_future(py, async move {
            let mut stream = connection
                .client()
                .await?
                .inner()
                .search_table_vector(tonic::Request::new(request))
                .await
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?
                .into_inner();

            let mut batches = Vec::new();
            while let Some(response) = stream.next().await {
                let response = response.map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
                let data = response
                    .data
                    .ok_or_else(|| PyRuntimeError::new_err("missing record batch payload"))?;
                let batch: RecordBatch =
                    data.try_into()
                        .map_err(|err: re_protos::TypeConversionError| {
                            PyRuntimeError::new_err(err.to_string())
                        })?;
                batches.push(batch);
            }

            let schema = batches
                .first()
                .ok_or_else(|| PyRuntimeError::new_err("empty vector search response"))?
                .schema();
            let provider = MemTable::try_new(schema, vec![batches]).map_err(to_py_err)?;
            Ok::<_, pyo3::PyErr>(Arc::new(provider) as Arc<dyn TableProvider + Send>)
        })?;

        let table = PyTableProviderAdapterInternal::new(provider, false);

        let client = self_.client.borrow(py);
        let ctx = client.ctx(py)?;
        let ctx = ctx.bind(py);
        drop(client);

        ctx.call_method1("read_table", (table,))
    }
}

impl PyTableEntryInternal {
    pub fn new(client: Py<PyCatalogClientInternal>, table_entry: TableEntry) -> Self {
        let url = match &table_entry.provider_details {
            ProviderDetails::LanceTable(p) => Some(p.table_url.to_string()),
            ProviderDetails::SystemTable(_) => None,
        };

        Self {
            client,
            entry_details: table_entry.details,
            lazy_provider: None,
            url,
        }
    }

    fn table_provider(mut self_: PyRefMut<'_, Self>) -> PyResult<Arc<dyn TableProvider + Send>> {
        let py = self_.py();
        if self_.lazy_provider.is_none() {
            let table_id = self_.entry_details.id;
            let connection = self_.client.borrow_mut(py).connection().clone();

            self_.lazy_provider = Some(
                wait_for_future(py, async {
                    TableEntryTableProvider::new(
                        connection.client().await?,
                        table_id,
                        Some(get_tokio_runtime().handle().clone()),
                    )
                    .into_provider()
                    .await
                    .map_err(to_py_err)
                })
                .map_err(|err| {
                    PyRuntimeError::new_err(format!("Error creating TableProvider: {err}"))
                })?,
            );
        }

        let provider = self_
            .lazy_provider
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Missing TableProvider".to_owned()))?
            .clone();

        Ok(provider)
    }
}

#[pyclass(
    name = "TableInsertModeInternal",
    eq,
    eq_int,
    module = "rerun_bindings.rerun_bindings"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum_macros::EnumIter)]
pub enum PyTableInsertModeInternal {
    #[pyo3(name = "APPEND")]
    Append = 1,

    #[pyo3(name = "OVERWRITE")]
    Overwrite = 2,

    #[pyo3(name = "REPLACE")]
    Replace = 3,
}

impl From<PyTableInsertModeInternal> for TableInsertMode {
    fn from(value: PyTableInsertModeInternal) -> Self {
        match value {
            PyTableInsertModeInternal::Append => Self::Append,
            PyTableInsertModeInternal::Overwrite => Self::Overwrite,
            PyTableInsertModeInternal::Replace => Self::Replace,
        }
    }
}

fn record_batch_to_arrow_ipc(batch: &RecordBatch) -> PyResult<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut bytes, batch.schema().as_ref())
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        writer
            .write(batch)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        writer
            .finish()
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    }
    Ok(bytes)
}
