from __future__ import annotations

from typing import TYPE_CHECKING

import numpy as np
import pyarrow as pa
import pytest
from rerun.catalog import VectorDistanceMetric

if TYPE_CHECKING:
    import pathlib

    from e2e_redap_tests.conftest import EntryFactory


DIMENSION = 16
ROW_COUNT = 320


def _schema() -> pa.Schema:
    return pa.schema([
        pa.field("id", pa.int32(), metadata={"rerun:is_table_index": "true"}),
        pa.field("emb", pa.list_(pa.float32(), DIMENSION)),
        pa.field("label", pa.string()),
    ])


def _batch(schema: pa.Schema) -> tuple[pa.RecordBatch, np.ndarray]:
    rng = np.random.default_rng(94)
    embeddings = rng.normal(size=(ROW_COUNT, DIMENSION)).astype(np.float32)

    batch = pa.RecordBatch.from_arrays(
        [
            pa.array(np.arange(ROW_COUNT, dtype=np.int32)),
            pa.array(embeddings.tolist(), type=pa.list_(pa.float32(), DIMENSION)),
            pa.array([f"row-{row}" for row in range(ROW_COUNT)]),
        ],
        schema=schema,
    )
    return batch, embeddings


@pytest.mark.local_only
def test_table_vector_search_end_to_end(entry_factory: EntryFactory, tmp_path: pathlib.Path) -> None:
    schema = _schema()
    table = entry_factory.create_table("table_vector_search", schema, tmp_path.absolute().as_uri())
    batch, embeddings = _batch(schema)

    table.upsert(batch)
    table.create_vector_index("emb", VectorDistanceMetric.Cosine)
    table.create_vector_index("emb", VectorDistanceMetric.Cosine, replace=True)

    result = table.search_vector(embeddings[7].tolist(), "emb", top_k=5)
    result_table = pa.Table.from_batches(result.collect())

    assert result_table.num_rows == 5
    assert result_table.schema.names == ["id", "emb", "label", "_distance"]
    assert result_table.schema.field("_distance").type == pa.float32()

    distances = result_table.column("_distance").combine_chunks().to_pylist()
    assert all(distance is not None and np.isfinite(distance) for distance in distances)


@pytest.mark.local_only
def test_table_vector_search_rejects_invalid_queries(entry_factory: EntryFactory, tmp_path: pathlib.Path) -> None:
    schema = _schema()
    table = entry_factory.create_table("table_vector_search_invalid", schema, tmp_path.absolute().as_uri())
    batch, embeddings = _batch(schema)

    table.upsert(batch)
    table.create_vector_index("emb", VectorDistanceMetric.Cosine)

    with pytest.raises(Exception, match="top_k must be greater than zero"):
        table.search_vector(embeddings[0].tolist(), "emb", top_k=0)

    with pytest.raises(Exception, match="dimension 3 does not match.*dimension 16"):
        table.search_vector([1.0, 2.0, 3.0], "emb", top_k=5)
