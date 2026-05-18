//! `acorn-store` — RVF append-only vector store, brute-force kNN, kNN graph.
//!
//! At dim=8 a linear scan beats any ANN index — no HNSW needed.
//!
//! Phase 1: `RvfStore { append, query_knn, compact, export }`
//! Phase 3: kNN graph rebuild (10s cadence) + boundary handoff to acorn-cognitive
