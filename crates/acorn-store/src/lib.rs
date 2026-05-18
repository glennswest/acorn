//! `acorn-store` — RVF append-only vector store + brute-force kNN.
//!
//! At dim=8 a linear scan beats any ANN index — no HNSW needed. Records are
//! kept in memory after open; the on-disk file is the source of truth and is
//! re-read on reopen. Each `append_batch` advances the in-memory epoch by 1.

#![forbid(unsafe_code)]

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use acorn_proto::rvf::{Metric, RvfHeader, RvfRecord, RVF_DIM, RVF_HEADER_LEN};
use acorn_proto::ProtoError;
use parking_lot::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("proto: {0}")]
    Proto(#[from] ProtoError),
    #[error("metric mismatch: stored={stored:?}, requested={requested:?}")]
    MetricMismatch { stored: Metric, requested: Metric },
    #[error("dim mismatch: stored={stored}, expected={expected}")]
    DimMismatch { stored: u16, expected: u16 },
}

pub struct RvfStore {
    path: PathBuf,
    file: Mutex<File>,
    state: Mutex<StoreState>,
}

struct StoreState {
    header: RvfHeader,
    records: Vec<RvfRecord>,
    epoch: u64,
}

impl RvfStore {
    /// Open the RVF file at `path`. Creates and initializes the header if
    /// the file is empty. If the file exists with a different metric than
    /// `metric`, returns [`StoreError::MetricMismatch`].
    pub fn open_or_create(path: impl Into<PathBuf>, metric: Metric) -> Result<Self, StoreError> {
        let path = path.into();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len();
        let (header, records) = if len == 0 {
            let header = RvfHeader::current(metric, RVF_DIM);
            file.write_all(&header.to_bytes())?;
            file.sync_data()?;
            (header, Vec::new())
        } else {
            file.seek(SeekFrom::Start(0))?;
            let mut hbuf = [0u8; RVF_HEADER_LEN];
            file.read_exact(&mut hbuf)?;
            let header = RvfHeader::from_bytes(&hbuf)?;
            if header.dim != RVF_DIM {
                return Err(StoreError::DimMismatch {
                    stored: header.dim,
                    expected: RVF_DIM,
                });
            }
            if header.metric != metric as u8 {
                return Err(StoreError::MetricMismatch {
                    stored: Metric::from_u8(header.metric)?,
                    requested: metric,
                });
            }
            let mut rest = Vec::new();
            file.read_to_end(&mut rest)?;
            let stride = RvfRecord::WIRE_LEN;
            let mut records = Vec::with_capacity(rest.len() / stride);
            let mut off = 0;
            while off + stride <= rest.len() {
                records.push(RvfRecord::from_bytes(&rest[off..off + stride])?);
                off += stride;
            }
            (header, records)
        };
        Ok(Self {
            path,
            file: Mutex::new(file),
            state: Mutex::new(StoreState {
                header,
                records,
                epoch: 0,
            }),
        })
    }

    pub fn count(&self) -> usize {
        self.state.lock().records.len()
    }

    pub fn epoch(&self) -> u64 {
        self.state.lock().epoch
    }

    pub fn metric(&self) -> Metric {
        Metric::from_u8(self.state.lock().header.metric).unwrap_or(Metric::Cosine)
    }

    pub fn header(&self) -> RvfHeader {
        self.state.lock().header
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a batch of records, persist, advance the epoch. Returns the
    /// new epoch.
    pub fn append_batch(&self, records: &[RvfRecord]) -> Result<u64, StoreError> {
        if records.is_empty() {
            return Ok(self.state.lock().epoch);
        }
        let mut state = self.state.lock();
        let mut file = self.file.lock();
        file.seek(SeekFrom::End(0))?;
        for r in records {
            file.write_all(&r.to_bytes())?;
        }
        file.sync_data()?;
        state.records.extend_from_slice(records);
        state.epoch = state.epoch.saturating_add(1);
        Ok(state.epoch)
    }

    /// Per-record bytes, in append order. Used by the witness chain to
    /// re-walk and verify.
    pub fn records_bytes(&self) -> Vec<Vec<u8>> {
        self.state
            .lock()
            .records
            .iter()
            .map(|r| r.to_bytes().to_vec())
            .collect()
    }

    /// `(id, vector)` snapshot used by cognitive analysis.
    pub fn vectors(&self) -> Vec<(u32, [f32; 8])> {
        self.state
            .lock()
            .records
            .iter()
            .map(|r| (r.id, r.vector))
            .collect()
    }

    /// Whole-file export (header + all records).
    pub fn export(&self) -> Result<Vec<u8>, StoreError> {
        let mut file = self.file.lock();
        file.seek(SeekFrom::Start(0))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Rewrite the file from the in-memory index. No tombstones in v1, so
    /// this is mostly a defragmentation pass — useful after corruption or as
    /// an interop checkpoint. Returns the number of records written.
    pub fn compact(&self) -> Result<usize, StoreError> {
        let state = self.state.lock();
        let tmp = self.path.with_extension("rvf.tmp");
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&state.header.to_bytes())?;
            for r in &state.records {
                f.write_all(&r.to_bytes())?;
            }
            f.sync_data()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        let mut file = self.file.lock();
        *file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)?;
        Ok(state.records.len())
    }

    /// Brute-force top-`k` nearest neighbors under the store's metric.
    /// Returns `(id, distance)` sorted ascending (closer first); on ties,
    /// sort by id for determinism.
    pub fn query_knn(&self, query: &[f32; 8], k: usize) -> Vec<(u32, f32)> {
        if k == 0 {
            return Vec::new();
        }
        let state = self.state.lock();
        let metric = Metric::from_u8(state.header.metric).unwrap_or(Metric::Cosine);
        let mut hits: Vec<(u32, f32)> = state
            .records
            .iter()
            .map(|r| (r.id, distance(metric, query, &r.vector)))
            .collect();
        hits.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        hits.truncate(k);
        hits
    }
}

/// Distance under the chosen metric — smaller means closer.
pub fn distance(metric: Metric, a: &[f32; 8], b: &[f32; 8]) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - cosine(a, b),
        Metric::L2 => l2(a, b),
        Metric::Dot => -dot(a, b),
    }
}

fn dot(a: &[f32; 8], b: &[f32; 8]) -> f32 {
    let mut s = 0.0;
    for i in 0..8 {
        s += a[i] * b[i];
    }
    s
}

fn norm(a: &[f32; 8]) -> f32 {
    dot(a, a).sqrt()
}

fn cosine(a: &[f32; 8], b: &[f32; 8]) -> f32 {
    let na = norm(a);
    let nb = norm(b);
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot(a, b) / (na * nb)
    }
}

fn l2(a: &[f32; 8], b: &[f32; 8]) -> f32 {
    let mut s = 0.0;
    for i in 0..8 {
        let d = a[i] - b[i];
        s += d * d;
    }
    s.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        p.push(format!("acorn-store-test-{pid}-{n}-{name}"));
        p
    }

    fn mkrec(id: u32, v: [f32; 8]) -> RvfRecord {
        RvfRecord {
            id,
            vector: v,
            node_id: 1,
            type_tag: 1,
            timestamp: id,
        }
    }

    #[test]
    fn append_and_reload() {
        let path = temp_path("reload.rvf");
        let store = RvfStore::open_or_create(&path, Metric::Cosine).unwrap();
        assert_eq!(store.count(), 0);
        store
            .append_batch(&[
                mkrec(1, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                mkrec(2, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            ])
            .unwrap();
        assert_eq!(store.count(), 2);
        assert_eq!(store.epoch(), 1);

        drop(store);
        let store2 = RvfStore::open_or_create(&path, Metric::Cosine).unwrap();
        assert_eq!(store2.count(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn knn_returns_closest_first() {
        let path = temp_path("knn.rvf");
        let store = RvfStore::open_or_create(&path, Metric::L2).unwrap();
        store
            .append_batch(&[
                mkrec(10, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                mkrec(20, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                mkrec(30, [0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            ])
            .unwrap();
        let hits = store.query_knn(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 10); // exact match
        assert_eq!(hits[1].0, 30); // closer than 20
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_starts_with_magic() {
        let path = temp_path("export.rvf");
        let store = RvfStore::open_or_create(&path, Metric::Cosine).unwrap();
        let bytes = store.export().unwrap();
        assert_eq!(&bytes[0..4], b"RVF1");
        assert_eq!(bytes.len(), RVF_HEADER_LEN);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn metric_mismatch_on_reopen() {
        let path = temp_path("mismatch.rvf");
        let _ = RvfStore::open_or_create(&path, Metric::Cosine).unwrap();
        match RvfStore::open_or_create(&path, Metric::L2) {
            Err(StoreError::MetricMismatch { .. }) => {}
            other => panic!("expected MetricMismatch, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn compact_preserves_records() {
        let path = temp_path("compact.rvf");
        let store = RvfStore::open_or_create(&path, Metric::Dot).unwrap();
        store
            .append_batch(&[
                mkrec(1, [1.0; 8]),
                mkrec(2, [2.0; 8]),
                mkrec(3, [3.0; 8]),
            ])
            .unwrap();
        let n = store.compact().unwrap();
        assert_eq!(n, 3);
        assert_eq!(store.count(), 3);
        let _ = std::fs::remove_file(&path);
    }
}
