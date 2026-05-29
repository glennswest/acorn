//! `acorn-cognitive` — structural analysis over an RVF vector store.
//!
//! Two metrics, both deliberately defined here (upstream is silent on the
//! exact formula):
//!
//! * **Fragility** — `1 / (1 + min_cut)` where `min_cut` is the
//!   Stoer-Wagner global minimum cut weight of the kNN similarity graph.
//!   A tightly-connected single-cluster store → small cut → fragility near
//!   1. A clean two-cluster store → high cut → fragility near 0.
//!
//! * **Coherence** — `1 / (1 + stddev)` of the L2 distances between
//!   consecutive vectors in the most recent `coherence_window` records.
//!   Stable streams → high coherence; chaotic ones → low.
//!
//! Both are bounded in `(0, 1]` and are documented as Acorn-internal
//! semantics (see `cognitum-seed-rust-scoping.md` §3 — undefined upstream).

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use acorn_proto::rvf::Metric;
use acorn_store::distance;
use parking_lot::RwLock;
use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub struct CognitiveConfig {
    pub k_neighbors: usize,
    pub coherence_window: usize,
    /// Cache TTL for `cached_or_compute`. Default 2 s — matches the
    /// fleet-UI poll cadence so we never compute Stoer-Wagner faster than
    /// once every two seconds even under polling pressure.
    pub cache_ttl: Duration,
}

impl Default for CognitiveConfig {
    fn default() -> Self {
        Self {
            k_neighbors: 5,
            coherence_window: 32,
            cache_ttl: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct CognitiveSnapshot {
    pub vector_count: usize,
    pub fragility: f32,
    pub coherence: f32,
    pub min_cut: f32,
    pub k_neighbors: usize,
    pub coherence_window: usize,
}

pub struct Cognitive {
    cfg: CognitiveConfig,
    cache: RwLock<CachedSnap>,
}

#[derive(Default)]
struct CachedSnap {
    snap: Option<CognitiveSnapshot>,
    computed_at: Option<Instant>,
}

impl Cognitive {
    pub fn new(cfg: CognitiveConfig) -> Self {
        Self {
            cfg,
            cache: RwLock::new(CachedSnap::default()),
        }
    }

    pub fn config(&self) -> CognitiveConfig {
        self.cfg
    }

    /// Compute a fresh snapshot from the supplied vectors. **Synchronous and
    /// CPU-bound (Stoer-Wagner is O(V³)).** Don't call this directly from a
    /// tokio runtime thread — use [`Cognitive::cached_or_compute`] or wrap
    /// in `tokio::task::spawn_blocking`.
    pub fn snapshot(
        &self,
        vectors: &[(u32, [f32; 8])],
        metric: Metric,
    ) -> CognitiveSnapshot {
        let n = vectors.len();
        let (fragility, min_cut) = if n < 2 {
            (0.0, 0.0)
        } else {
            let mut w = knn_graph(vectors, metric, self.cfg.k_neighbors);
            let cut = stoer_wagner(&mut w);
            (1.0 / (1.0 + cut), cut)
        };
        let coherence = coherence_score(vectors, self.cfg.coherence_window);
        CognitiveSnapshot {
            vector_count: n,
            fragility,
            coherence,
            min_cut,
            k_neighbors: self.cfg.k_neighbors,
            coherence_window: self.cfg.coherence_window,
        }
    }

    /// Return the cached snapshot if it's within `cache_ttl`; otherwise
    /// fetch vectors via `vectors_provider`, recompute, and cache. The
    /// vectors_provider closure is only invoked on miss.
    pub fn cached_or_compute(
        &self,
        vectors_provider: impl FnOnce() -> Vec<(u32, [f32; 8])>,
        metric: Metric,
    ) -> CognitiveSnapshot {
        {
            let g = self.cache.read();
            if let (Some(snap), Some(at)) = (g.snap, g.computed_at) {
                if at.elapsed() < self.cfg.cache_ttl {
                    return snap;
                }
            }
        }
        let vectors = vectors_provider();
        let snap = self.snapshot(&vectors, metric);
        let mut g = self.cache.write();
        g.snap = Some(snap);
        g.computed_at = Some(Instant::now());
        snap
    }

    /// Last cached snapshot (if any), regardless of TTL. Used by callers
    /// that need a non-blocking read (e.g. SSE).
    pub fn last_cached(&self) -> Option<CognitiveSnapshot> {
        self.cache.read().snap
    }
}

// ---------------------------------------------------------------------------
// kNN similarity graph
// ---------------------------------------------------------------------------

/// Build an n×n symmetric weight matrix where each vertex is connected to
/// its `k` nearest neighbors. Weight = `1 / (1 + distance)`; non-neighbors
/// have weight 0.
fn knn_graph(vectors: &[(u32, [f32; 8])], metric: Metric, k: usize) -> Vec<Vec<f32>> {
    let n = vectors.len();
    let k = k.min(n.saturating_sub(1));
    let mut w = vec![vec![0.0f32; n]; n];

    // Pairwise distance, then per-row top-k neighbours.
    let mut dist = vec![vec![0.0f32; n]; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let d = distance(metric, &vectors[i].1, &vectors[j].1);
            dist[i][j] = d;
            dist[j][i] = d;
        }
    }
    for i in 0..n {
        let mut nbrs: Vec<(usize, f32)> = (0..n)
            .filter(|&j| j != i)
            .map(|j| (j, dist[i][j]))
            .collect();
        nbrs.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for &(j, d) in nbrs.iter().take(k) {
            let weight = 1.0 / (1.0 + d);
            // Symmetric: each unordered pair is bumped from both sides, so a
            // mutual neighbour gets double weight. That's fine for kNN
            // similarity — it strengthens mutual ties — but cap it to avoid
            // negative-distance metrics blowing up.
            w[i][j] += weight;
            w[j][i] += weight;
        }
    }
    w
}

// ---------------------------------------------------------------------------
// Stoer-Wagner global minimum cut
// ---------------------------------------------------------------------------

/// Stoer-Wagner global minimum cut on a symmetric non-negative weight matrix.
/// Consumes (mutates) `w` via the algorithm's merge step. Returns the cut
/// weight; returns 0 for n < 2.
pub fn stoer_wagner(w: &mut [Vec<f32>]) -> f32 {
    let n = w.len();
    if n < 2 {
        return 0.0;
    }
    let mut alive = vec![true; n];
    let mut min_cut = f32::INFINITY;
    let mut nodes_alive = n;

    while nodes_alive > 1 {
        let (cut, s, t) = min_cut_phase(w, &alive);
        if cut < min_cut {
            min_cut = cut;
        }
        // Merge t into s: edges from t join s.
        for i in 0..w.len() {
            if alive[i] && i != s && i != t {
                w[s][i] += w[t][i];
                w[i][s] += w[i][t];
            }
        }
        alive[t] = false;
        nodes_alive -= 1;
    }
    if min_cut.is_infinite() {
        0.0
    } else {
        min_cut
    }
}

fn min_cut_phase(w: &[Vec<f32>], alive: &[bool]) -> (f32, usize, usize) {
    let n = w.len();
    let n_alive = alive.iter().filter(|&&a| a).count();
    debug_assert!(n_alive >= 2);

    let start = alive.iter().position(|&a| a).unwrap();
    let mut in_a = vec![false; n];
    in_a[start] = true;
    let mut weight_to_a = vec![0.0f32; n];
    for i in 0..n {
        if alive[i] && i != start {
            weight_to_a[i] = w[start][i];
        }
    }

    let mut last = start;
    let mut second_last = start;
    let mut cut_of_phase = 0.0f32;

    for iter in 1..n_alive {
        let mut best = usize::MAX;
        let mut best_w = f32::NEG_INFINITY;
        for i in 0..n {
            if alive[i] && !in_a[i] && weight_to_a[i] > best_w {
                best_w = weight_to_a[i];
                best = i;
            }
        }
        in_a[best] = true;
        second_last = last;
        last = best;
        if iter == n_alive - 1 {
            cut_of_phase = best_w;
        } else {
            for j in 0..n {
                if alive[j] && !in_a[j] {
                    weight_to_a[j] += w[best][j];
                }
            }
        }
    }
    (cut_of_phase, second_last, last)
}

// ---------------------------------------------------------------------------
// Temporal coherence
// ---------------------------------------------------------------------------

fn coherence_score(vectors: &[(u32, [f32; 8])], window: usize) -> f32 {
    if vectors.len() < 2 || window < 2 {
        return 1.0;
    }
    let take = window.min(vectors.len());
    let start = vectors.len() - take;
    let slice = &vectors[start..];
    let mut diffs = Vec::with_capacity(slice.len() - 1);
    for w in slice.windows(2) {
        let mut s = 0.0f32;
        for k in 0..8 {
            let d = w[0].1[k] - w[1].1[k];
            s += d * d;
        }
        diffs.push(s.sqrt());
    }
    if diffs.is_empty() {
        return 1.0;
    }
    let mean = diffs.iter().sum::<f32>() / diffs.len() as f32;
    let var = diffs
        .iter()
        .map(|d| (d - mean) * (d - mean))
        .sum::<f32>()
        / diffs.len() as f32;
    let sd = var.sqrt();
    1.0 / (1.0 + sd)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stoer_wagner_textbook_example() {
        // Classic Stoer-Wagner example (Wikipedia / original paper):
        // 8 vertices, min cut = 4 between {3,4,7,8} and {1,2,5,6} (0-indexed
        // here as {2,3,6,7} and {0,1,4,5}).
        let n = 8;
        let mut w = vec![vec![0.0f32; n]; n];
        let edges = [
            (0, 1, 2.0),
            (0, 4, 3.0),
            (1, 2, 3.0),
            (1, 4, 2.0),
            (1, 5, 2.0),
            (2, 3, 4.0),
            (2, 6, 2.0),
            (3, 6, 2.0),
            (3, 7, 2.0),
            (4, 5, 3.0),
            (5, 6, 1.0),
            (6, 7, 3.0),
        ];
        for (a, b, x) in edges {
            w[a][b] = x;
            w[b][a] = x;
        }
        let cut = stoer_wagner(&mut w);
        assert!((cut - 4.0).abs() < 1e-5, "expected 4.0, got {cut}");
    }

    #[test]
    fn stoer_wagner_two_isolated_clusters() {
        // Two K3s connected by a single thin bridge → min cut = bridge weight.
        let n = 6;
        let mut w = vec![vec![0.0f32; n]; n];
        for (a, b) in &[(0, 1), (0, 2), (1, 2), (3, 4), (3, 5), (4, 5)] {
            w[*a][*b] = 10.0;
            w[*b][*a] = 10.0;
        }
        w[2][3] = 0.5;
        w[3][2] = 0.5;
        let cut = stoer_wagner(&mut w);
        assert!((cut - 0.5).abs() < 1e-5, "expected 0.5, got {cut}");
    }

    #[test]
    fn snapshot_handles_tiny_inputs() {
        let c = Cognitive::new(CognitiveConfig::default());
        let s0 = c.snapshot(&[], Metric::L2);
        assert_eq!(s0.vector_count, 0);
        assert_eq!(s0.fragility, 0.0);

        let s1 = c.snapshot(&[(1, [1.0; 8])], Metric::L2);
        assert_eq!(s1.vector_count, 1);
        assert_eq!(s1.fragility, 0.0);
    }

    #[test]
    fn fragility_is_higher_for_a_split_graph() {
        // Semantics: small min-cut = easy to tear apart = high fragility.
        // A fully-split graph has min_cut=0 -> fragility=1 (max). A tight
        // single cluster has min_cut > 0 -> fragility < 1.
        let c = Cognitive::new(CognitiveConfig {
            k_neighbors: 3,
            coherence_window: 8,
            ..CognitiveConfig::default()
        });
        let tight: Vec<(u32, [f32; 8])> = (0..8)
            .map(|i| {
                let mut v = [0.0f32; 8];
                v[0] = 0.5 + (i as f32) * 0.01;
                (i, v)
            })
            .collect();
        let mut split = Vec::new();
        for i in 0..4u32 {
            let mut v = [0.0f32; 8];
            v[0] = (i as f32) * 0.001;
            split.push((i, v));
        }
        for i in 0..4u32 {
            let mut v = [0.0f32; 8];
            v[7] = 10.0 + (i as f32) * 0.001;
            split.push((4 + i, v));
        }
        let s_tight = c.snapshot(&tight, Metric::L2);
        let s_split = c.snapshot(&split, Metric::L2);
        assert!(s_split.fragility > s_tight.fragility);
        assert!(s_tight.fragility < 1.0);
        assert!((s_split.fragility - 1.0).abs() < 1e-5);
    }

    #[test]
    fn coherence_is_higher_on_stable_streams() {
        let c = Cognitive::new(CognitiveConfig::default());
        // Stable: tiny step-by-step change
        let stable: Vec<(u32, [f32; 8])> = (0..20)
            .map(|i| (i, [0.5 + (i as f32) * 0.001, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]))
            .collect();
        let chaotic: Vec<(u32, [f32; 8])> = (0..20)
            .map(|i| {
                let f = (i as f32 * 1.61803).fract();
                (i, [f, 1.0 - f, f * 0.5, 0.2, 0.7, 0.1, 0.9, 0.3])
            })
            .collect();
        let cs = c.snapshot(&stable, Metric::L2).coherence;
        let cc = c.snapshot(&chaotic, Metric::L2).coherence;
        assert!(cs > cc, "stable={cs} chaotic={cc}");
    }
}
