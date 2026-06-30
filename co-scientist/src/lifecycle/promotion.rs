//! Memory consolidation / promotion.
//!
//! Subscribes to the event bus and runs consolidation cycles when enough
//! new memories accumulate. Each cycle:
//!
//! 1. **Backfill embeddings** — compute hash-bag vectors for memories
//!    missing them (pre-embedding rows, or memories saved before the
//!    embedding pipeline was added).
//! 2. **Cluster near-duplicates** — find groups of memories with cosine
//!    similarity >= threshold. Archive all but the representative
//!    (highest importance, most recent).
//! 3. **Rebuild term index** — ensure every non-archived memory has
//!    correct inverted index entries.
//!
//! Inspired by Co-Scientist Python's `proximity` agent and claude-mem's
//! watermark-based Chroma sync.

use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use tokio::sync::watch;

use crate::bus::{EventBus, MemoryEvent};
use crate::embeddings::{self, HASH_DIM};
use crate::memory::Memory;

#[derive(Debug, Clone, Deserialize)]
pub struct PromotionConfig {
    /// Re-cluster when at least this many new memories have arrived since
    /// the last cycle.
    pub new_memory_threshold: usize,
    /// Cosine similarity threshold for two memories to join a cluster.
    pub similarity_threshold: f32,
    /// Minimum cluster size to trigger archival of redundant members.
    pub min_cluster_size: usize,
    /// Max memories to scan per consolidation cycle.
    pub scan_limit: usize,
    /// Minimum time between consolidation cycles.
    pub min_interval: Duration,
    /// Days since last access before `importance` starts decaying.
    /// Set to 0 to disable decay entirely.
    pub decay_after_days: i64,
    /// Multiplier applied per cycle to stale memories' `importance`.
    /// With factor=0.95, importance halves in ~14 cycles.
    pub decay_factor: f64,
    /// Importance floor; rows below this are archived.
    pub decay_archive_threshold: f64,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            new_memory_threshold: 10,
            similarity_threshold: 0.85,
            min_cluster_size: 2,
            scan_limit: 5000,
            min_interval: Duration::from_secs(60),
            decay_after_days: 30,
            decay_factor: 0.95,
            decay_archive_threshold: 0.1,
        }
    }
}

/// Consolidation statistics for logging/monitoring.
#[derive(Debug, Default)]
pub struct ConsolidationStats {
    pub embeddings_backfilled: usize,
    pub embeddings_upgraded: usize,
    pub clusters_found: usize,
    pub memories_archived: usize,
    pub index_entries_added: usize,
    /// Memories whose `importance` was decayed (does not include
    /// those archived as a result of falling below the threshold).
    pub decayed: usize,
}

/// Run a single consolidation cycle. Returns stats for logging.
pub async fn run_consolidation(
    memory: &Memory,
    cfg: &PromotionConfig,
) -> Result<ConsolidationStats> {
    let mut stats = ConsolidationStats::default();

    // Phase 1a: Backfill hash-bag embeddings for memories missing them.
    // Fast, inline, no dependencies.
    stats.embeddings_backfilled = backfill_embeddings(memory, cfg.scan_limit).await?;

    // Phase 1b: Re-embed with fastembed (real semantic vectors).
    // Sequential (hardware constraint): spawn embed.py per text, kill after.
    if let Some(script) = find_embed_script() {
        stats.embeddings_upgraded = upgrade_embeddings(memory, &script, cfg.scan_limit).await?;
    }

    // Phase 2: Cluster near-duplicates and archive redundant members.
    let (clusters, archived) = cluster_and_archive(memory, cfg).await?;
    stats.clusters_found = clusters;
    stats.memories_archived = archived;

    // Phase 3: Rebuild term index for any memories missing entries.
    stats.index_entries_added = rebuild_term_index(memory).await?;

    // Phase 4: Decay importance for memories not accessed in a while.
    // Rows with `importance` below `decay_archive_threshold` get
    // archived. Disabled when `decay_after_days == 0`.
    if cfg.decay_after_days > 0 {
        stats.decayed = decay_unused_memories(
            memory,
            cfg.decay_after_days,
            cfg.decay_factor,
            cfg.decay_archive_threshold,
        )
        .await?;
    }

    Ok(stats)
}

/// Backfill hash-bag embeddings for memories that don't have them yet.
async fn backfill_embeddings(memory: &Memory, limit: usize) -> Result<usize> {
    let unembedded = memory.get_unembedded(limit).await?;
    let count = unembedded.len();
    for mem in &unembedded {
        let text = format!(
            "{} {}",
            mem.summary,
            mem.details
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default()
        );
        let vec = embeddings::hash_bag(&text, HASH_DIM);
        let bytes = embeddings::vec_to_bytes(&vec);
        memory.update_embedding(mem.id, &bytes).await?;
    }
    if count > 0 {
        tracing::info!(count, "backfilled embeddings");
    }
    Ok(count)
}

/// Find embed.py in the project. Checks common locations.
fn find_embed_script() -> Option<String> {
    let candidates = [
        "src/embed.py",
        "co-scientist/src/embed.py",
        "../src/embed.py",
    ];
    for path in &candidates {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }
    // Also check via env var.
    if let Ok(path) = std::env::var("EMBED_SCRIPT") {
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    None
}

/// Re-embed existing hash-bag vectors with fastembed (real semantics).
/// Sequential: spawns embed.py per memory, kills after. Hardware constraint.
/// Only re-embeds memories that have hash-bag vectors (dim = HASH_DIM).
async fn upgrade_embeddings(
    memory: &Memory,
    script_path: &str,
    limit: usize,
) -> Result<usize> {
    // Fetch all embedded memories and filter for hash-bag dim.
    let embedded = memory.get_embedded(limit).await?;
    let hash_dim = HASH_DIM;
    let mut upgraded = 0;

    for (id, vec, _importance) in &embedded {
        if vec.len() != hash_dim {
            continue; // already fastembed dim, skip
        }

        // Fetch the summary+details for this memory to re-embed.
        let mem = memory
            .get_observation(crate::memory::ObservationKind::Semantic, *id)
            .await?;
        let mem = match mem {
            Some(crate::memory::Observation::Semantic(m)) => m,
            _ => continue,
        };
        let text = format!(
            "{} {}",
            mem.summary,
            mem.details
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default()
        );

        // Spawn embed.py, embed, kill.
        if let Some(new_vec) = embeddings::fastembed_one(script_path, &text).await {
            let bytes = embeddings::vec_to_bytes(&new_vec);
            memory.update_embedding(*id, &bytes).await?;
            upgraded += 1;
        }
    }

    if upgraded > 0 {
        tracing::info!(upgraded, "upgraded embeddings to fastembed");
    }
    Ok(upgraded)
}

/// Find clusters of near-duplicate memories and archive redundant members.
/// Returns (clusters_found, memories_archived).
async fn cluster_and_archive(
    memory: &Memory,
    cfg: &PromotionConfig,
) -> Result<(usize, usize)> {
    let embedded = memory.get_embedded(cfg.scan_limit).await?;
    if embedded.len() < 2 {
        return Ok((0, 0));
    }

    // Build clusters using union-find on cosine similarity.
    let n = embedded.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], x: usize) -> usize {
        if parent[x] != x {
            parent[x] = find(parent, parent[x]);
        }
        parent[x]
    }

    fn union(parent: &mut [usize], x: usize, y: usize) {
        let rx = find(parent, x);
        let ry = find(parent, y);
        if rx != ry {
            parent[rx] = ry;
        }
    }

    // Compare all pairs. O(n²) but fine for scan_limit=5000 with
    // 256-dim vectors. For larger scales, switch to FAISS or HNSW.
    for i in 0..n {
        for j in (i + 1)..n {
            let (_, ref vec_a, _) = embedded[i];
            let (_, ref vec_b, _) = embedded[j];
            if vec_a.len() == vec_b.len() {
                let sim = embeddings::cosine_similarity(vec_a, vec_b);
                if sim >= cfg.similarity_threshold {
                    union(&mut parent, i, j);
                }
            }
        }
    }

    // Group by cluster root.
    let mut clusters: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }

    let mut clusters_found = 0;
    let mut memories_archived = 0;

    for (_, members) in &clusters {
        if members.len() < cfg.min_cluster_size {
            continue;
        }
        clusters_found += 1;

        // Pick the representative: highest importance, then most recent (highest id).
        let mut best_idx = members[0];
        let mut best_importance = embedded[members[0]].2;
        let mut best_id = embedded[members[0]].0;
        for &idx in members.iter().skip(1) {
            let (id, _, importance) = embedded[idx];
            if importance > best_importance || (importance == best_importance && id > best_id) {
                best_idx = idx;
                best_importance = importance;
                best_id = id;
            }
        }

        // Archive all members except the representative.
        for &idx in members {
            if idx != best_idx {
                let (id, _, _) = embedded[idx];
                memory.archive_semantic(id).await?;
                memories_archived += 1;
            }
        }
    }

    if clusters_found > 0 {
        tracing::info!(
            clusters = clusters_found,
            archived = memories_archived,
            "consolidation: archived near-duplicates"
        );
    }

    Ok((clusters_found, memories_archived))
}

/// Rebuild term index entries for any non-archived memories missing them.
async fn rebuild_term_index(memory: &Memory) -> Result<usize> {
    let conn = memory.db().conn();

    // Find semantic memories that have no term_index entries.
    let mut rows = conn
        .query(
            "SELECT sm.id, sm.summary, sm.details_json
             FROM semantic_memories sm
             LEFT JOIN term_index ti
               ON ti.memory_kind = 'semantic' AND ti.memory_id = sm.id
             WHERE sm.archived = 0 AND ti.memory_id IS NULL
             LIMIT 500",
            (),
        )
        .await?;
    let mut count = 0;
    while let Some(row) = rows.next().await? {
        let id: i64 = row.get(0)?;
        let summary: String = row.get(1)?;
        let details_str: Option<String> = row.get(2)?;
        let details: Option<serde_json::Value> = details_str
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let text = format!(
            "{} {}",
            summary,
            details
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default()
        );
        let terms = Memory::tokenize(&text);
        for term in terms {
            conn.execute(
                "INSERT OR IGNORE INTO term_index (memory_kind, memory_id, term) VALUES ('semantic', ?1, ?2)",
                (id, term),
            )
            .await?;
            count += 1;
        }
    }

    if count > 0 {
        tracing::info!(entries = count, "rebuilt term index entries");
    }
    Ok(count)
}

/// Decay `importance` for memories whose `last_accessed_at` is older
/// than `days`. Each cycle multiplies by `factor`; rows below
/// `archive_threshold` get archived. Memories with no
/// `last_accessed_at` yet are skipped — they were created after this
/// feature shipped but haven't been read, so decay shouldn't punish
/// fresh writes before retrieval has had a chance to bump them.
async fn decay_unused_memories(
    memory: &Memory,
    days: i64,
    factor: f64,
    archive_threshold: f64,
) -> Result<usize> {
    let conn = memory.db().conn();
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let mut rows = conn
        .query(
            "SELECT id, importance FROM semantic_memories
             WHERE archived = 0
               AND last_accessed_at IS NOT NULL
               AND last_accessed_at < ?1",
            (cutoff,),
        )
        .await?;
    let mut stale: Vec<(i64, f64)> = Vec::new();
    while let Some(row) = rows.next().await? {
        stale.push((row.get::<i64>(0)?, row.get::<f64>(1)?));
    }
    let mut decayed = 0usize;
    for (id, importance) in stale {
        let new_imp = importance * factor;
        if new_imp < archive_threshold {
            memory.archive_semantic(id).await?;
        } else {
            conn.execute(
                "UPDATE semantic_memories SET importance = ?1 WHERE id = ?2",
                (new_imp, id),
            )
            .await?;
        }
        decayed += 1;
    }
    if decayed > 0 {
        tracing::info!(count = decayed, days, factor, "decayed unused memories");
    }
    Ok(decayed)
}

/// Background consolidation service. Subscribes to the event bus and
/// triggers consolidation cycles when enough new memories accumulate.
pub struct ConsolidationService {
    memory: Memory,
    cfg: PromotionConfig,
}

impl ConsolidationService {
    pub fn new(memory: Memory, cfg: PromotionConfig) -> Self {
        Self { memory, cfg }
    }

    /// Run the service until `shutdown` flips to true. Designed to be
    /// spawned as a background tokio task alongside the worker loop.
    pub async fn run(
        self,
        bus: EventBus,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let mut rx = bus.subscribe();
        let mut pending = 0usize;
        let mut last_run = std::time::Instant::now()
            .checked_sub(self.cfg.min_interval)
            .unwrap_or_else(std::time::Instant::now);

        tracing::info!(
            threshold = self.cfg.new_memory_threshold,
            "consolidation service started"
        );

        loop {
            // Wait for a SemanticSaved event or shutdown.
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("consolidation service shutting down");
                        break;
                    }
                }
                ev = rx.recv() => {
                    match ev {
                        Ok(MemoryEvent::SemanticSaved { .. }) => {
                            pending += 1;
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "consolidation service lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }

            // Check if we should run a consolidation cycle.
            if pending >= self.cfg.new_memory_threshold
                && last_run.elapsed() >= self.cfg.min_interval
            {
                tracing::info!(pending, "running consolidation cycle");
                match run_consolidation(&self.memory, &self.cfg).await {
                    Ok(stats) => {
                        tracing::info!(
                            backfilled = stats.embeddings_backfilled,
                            upgraded = stats.embeddings_upgraded,
                            clusters = stats.clusters_found,
                            archived = stats.memories_archived,
                            indexed = stats.index_entries_added,
                            "consolidation cycle complete"
                        );
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "consolidation cycle failed");
                    }
                }
                pending = 0;
                last_run = std::time::Instant::now();
            }
        }

        Ok(())
    }
}
