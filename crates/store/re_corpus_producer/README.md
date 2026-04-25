# re_corpus_producer

`ChunkProvider` implementation that surfaces an audio corpus stored as
per-track Opus chunks in S3 + a Lance index as a single persistent Rerun
recording.

* **Manifest**: synthesized at startup by scanning the Lance corpus index. One
  Lance row → one virtual chunk in the manifest, mapped to a stable
  `ChunkId` derived from the corpus chunk identifier.
* **Lazy load**: on `load_chunks(ids)`, the provider fetches the corresponding
  Opus bytes from S3 and emits a Rerun chunk containing the `AudioStream`
  segment at the correct timeline timestamp.
* **Live edge** (later): the provider polls the Lance table for new rows and
  extends the manifest in place.
