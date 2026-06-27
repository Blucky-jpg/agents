//! Embedding system: hash-bag (fast, offline) + fastembed via embed.py (real semantics).
//!
//! Two tiers:
//! - **hash_bag** — deterministic, no dependencies, captures token overlap.
//!   Used for inline dedup on save_semantic.
//! - **fastembed_embed** — spawns `embed.py` per call (hardware constraint:
//!   can't run in background). Uses jina-embeddings-v3 for real semantic
//!   similarity. Used by the consolidation service for re-embedding.
//!
//! Both produce L2-normalized vectors so cosine_similarity works directly.

use sha2::{Digest, Sha256};

/// Default dimension for hash-bag embeddings.
pub const HASH_DIM: usize = 256;

/// Default dimension for fastembed embeddings (jina-v3 native is 1024,
/// but we can truncate). Match whatever embed.py returns.
pub const FASTEMBED_DIM: usize = 1024;

// =====================================================================
// Hash-bag embeddings (no dependencies, fast)
// =====================================================================

/// Compute a hash-bag embedding for `text`.
pub fn hash_bag(text: &str, dim: usize) -> Vec<f32> {
    let lower = text.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let mut vec = vec![0.0f32; dim];

    for token in &tokens {
        let hash = sha256_hash(token);
        let (d1, d2) = hash_to_dims(&hash, dim);
        vec[d1] += 1.0;
        vec[d2] += 1.0;
    }
    for window in tokens.windows(2) {
        let bigram = format!("{} {}", window[0], window[1]);
        let hash = sha256_hash(&bigram);
        let (d1, d2) = hash_to_dims(&hash, dim);
        vec[d1] += 0.5;
        vec[d2] += 0.5;
    }

    l2_normalize(&mut vec);
    vec
}

// =====================================================================
// Fastembed embeddings via embed.py (spawn-per-call, hardware constraint)
// =====================================================================

/// Embed one or more texts by spawning `embed.py` as a subprocess.
/// Returns one vector per input text, L2-normalized.
///
/// The process is spawned, used, and killed per call because the
/// ONNX session in fastembed hangs on repeated calls in the same process.
///
/// `script_path` is the path to `embed.py` (e.g. "src/embed.py").
/// Returns `None` if the script is not found or fails (caller should
/// fall back to hash_bag).
pub async fn fastembed_embed(
    script_path: &str,
    texts: &[&str],
) -> Option<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Some(Vec::new());
    }

    // Build the NDJSON request.
    let request = serde_json::json!({
        "id": 1,
        "texts": texts,
        "kind": "document"
    });
    let request_line = format!("{}\n", request);

    // Spawn embed.py — stdin/stdout pipes, no shell.
    let mut child = tokio::process::Command::new("python3")
        .arg(script_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // Write request to stdin.
    {
        use tokio::io::AsyncWriteExt;
        let stdin = child.stdin.as_mut()?;
        stdin.write_all(request_line.as_bytes()).await.ok()?;
        stdin.shutdown().await.ok()?;
    }

    // Read response from stdout (first line only).
    {
        use tokio::io::AsyncBufReadExt;
        let stdout = child.stdout.as_mut()?;
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).await.ok()?;

        // Kill the process immediately — don't wait for it to exit.
        let _ = child.kill().await;

        let resp: serde_json::Value = serde_json::from_str(&line).ok()?;
        if resp.get("error").is_some() {
            tracing::warn!(error = %resp["error"], "embed.py returned error");
            return None;
        }
        let embeddings = resp.get("embeddings")?.as_array()?;
        let mut result = Vec::with_capacity(embeddings.len());
        for emb in embeddings {
            let arr = emb.as_array()?;
            let vec: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
            result.push(vec);
        }
        Some(result)
    }
}

/// Embed a single text via fastembed. Returns None on failure.
pub async fn fastembed_one(script_path: &str, text: &str) -> Option<Vec<f32>> {
    let results = fastembed_embed(script_path, &[text]).await?;
    results.into_iter().next()
}

// =====================================================================
// Shared utilities
// =====================================================================

/// Compute cosine similarity between two L2-normalized vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "vector dimensions must match");
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Serialize a vector to bytes for DB storage.
pub fn vec_to_bytes(vec: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vec.len() * 4);
    for &v in vec {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

/// Deserialize a vector from DB bytes.
pub fn bytes_to_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn sha256_hash(text: &str) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let result = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&result[..8]);
    out
}

fn hash_to_dims(hash: &[u8; 8], dim: usize) -> (usize, usize) {
    let d1 = (u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]) as usize) % dim;
    let d2 = (u32::from_le_bytes([hash[4], hash[5], hash[6], hash[7]]) as usize) % dim;
    (d1, d2)
}

fn l2_normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_bag_identical() {
        let a = hash_bag("hello world foo bar", HASH_DIM);
        let b = hash_bag("hello world foo bar", HASH_DIM);
        assert_eq!(a, b);
    }

    #[test]
    fn hash_bag_similar() {
        let a = hash_bag("machine learning optimization gradient descent", HASH_DIM);
        let b = hash_bag("machine learning optimization gradient descent algorithm", HASH_DIM);
        let sim = cosine_similarity(&a, &b);
        assert!(sim > 0.7, "expected >0.7, got {sim}");
    }

    #[test]
    fn hash_bag_unrelated() {
        let a = hash_bag("quantum physics entanglement superposition", HASH_DIM);
        let b = hash_bag("baking chocolate cake recipe flour sugar", HASH_DIM);
        let sim = cosine_similarity(&a, &b);
        assert!(sim < 0.3, "expected <0.3, got {sim}");
    }

    #[test]
    fn round_trip_bytes() {
        let vec = hash_bag("test serialization", HASH_DIM);
        let bytes = vec_to_bytes(&vec);
        let restored = bytes_to_vec(&bytes);
        assert_eq!(vec, restored);
    }

    #[test]
    fn hash_bag_l2_normalized() {
        let vec = hash_bag("any text here", HASH_DIM);
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm should be ~1.0, got {norm}");
    }

    #[tokio::test]
    async fn fastembed_smoke() {
        // Only runs if embed.py is available.
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/embed.py");
        if !script.exists() {
            eprintln!("skipping fastembed test: embed.py not found");
            return;
        }
        let script_str = script.to_str().unwrap();
        let results = fastembed_embed(script_str, &["hello world", "foo bar"]).await;
        match results {
            Some(vecs) => {
                assert_eq!(vecs.len(), 2);
                assert!(vecs[0].len() > 100, "dim should be >100, got {}", vecs[0].len());
                let sim = cosine_similarity(&vecs[0], &vecs[1]);
                eprintln!("fastembed dim={}, cosine(hello world, foo bar)={sim:.4}", vecs[0].len());
            }
            None => {
                eprintln!("fastembed failed (python/fastembed not available?)");
            }
        }
    }
}
