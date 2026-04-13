//! ONNX-based sentence embedder using MiniLM-L6-v2.
//!
//! Ported from ironrace-bin/src/embedder.rs with security hardening:
//! - SHA-256 checksum verification of downloaded model files
//! - No silent fallback to CWD when home dir is unavailable
//! - Max input length enforcement

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ndarray::Array2;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::TensorRef;
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

/// MiniLM-L6-v2 produces 384-dimensional embeddings.
pub const EMBED_DIM: usize = 384;

/// Maximum sequence length for the model.
const MAX_SEQ_LEN: usize = 256;

/// Batch size for embedding inference.
const BATCH_SIZE: usize = 64;

/// HuggingFace model repo for the ONNX model.
const HF_MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// Local cache directory name.
const MODEL_DIR_NAME: &str = "all-MiniLM-L6-v2";

/// SHA-256 checksums for model integrity verification.
/// Pinned to sentence-transformers/all-MiniLM-L6-v2 from HuggingFace Hub.
/// Update these when upgrading the model version.
const MODEL_ONNX_SHA256: &str = "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452";
const TOKENIZER_JSON_SHA256: &str =
    "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037";

// Compile-time assertion: SHA-256 hex digests must be exactly 64 characters.
const _: () = {
    assert!(
        MODEL_ONNX_SHA256.len() == 64,
        "MODEL_ONNX_SHA256 must be 64 hex chars"
    );
    assert!(
        TOKENIZER_JSON_SHA256.len() == 64,
        "TOKENIZER_JSON_SHA256 must be 64 hex chars"
    );
};

/// Get the local model cache directory (~/.ironrace/models/all-MiniLM-L6-v2/).
pub fn model_cache_dir() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory. Set HOME env var."))?;
    Ok(home.join(".ironrace").join("models").join(MODEL_DIR_NAME))
}

/// Check if model files exist locally.
fn model_files_exist(dir: &Path) -> bool {
    dir.join("model.onnx").exists() && dir.join("tokenizer.json").exists()
}

/// Compute SHA-256 hex digest of a file.
fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).context("Failed to read file for checksum")?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Verify model file checksums. Returns Ok(true) if valid, Ok(false) if
/// checksums don't match the pinned values (which may happen on first
/// release before we've pinned the real hashes).
fn verify_checksums(dir: &Path) -> Result<bool> {
    let model_hash = sha256_file(&dir.join("model.onnx"))?;
    let tokenizer_hash = sha256_file(&dir.join("tokenizer.json"))?;

    if model_hash != MODEL_ONNX_SHA256 {
        eprintln!(
            "ERROR: model.onnx checksum mismatch.\n  Expected: {}\n  Got:      {}",
            MODEL_ONNX_SHA256, model_hash
        );
        return Ok(false);
    }

    if tokenizer_hash != TOKENIZER_JSON_SHA256 {
        eprintln!(
            "ERROR: tokenizer.json checksum mismatch.\n  Expected: {}\n  Got:      {}",
            TOKENIZER_JSON_SHA256, tokenizer_hash
        );
        return Ok(false);
    }

    Ok(true)
}

/// Download model files from HuggingFace Hub.
fn download_model(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;

    eprintln!("Downloading MiniLM-L6-v2 model from HuggingFace...");

    let api = hf_hub::api::sync::Api::new().context("Failed to create HuggingFace API client")?;
    let repo = api.model(HF_MODEL_REPO.to_string());

    let model_path = repo
        .get("onnx/model.onnx")
        .context("Failed to download model.onnx")?;

    let tokenizer_path = repo
        .get("tokenizer.json")
        .context("Failed to download tokenizer.json")?;

    std::fs::copy(&model_path, dir.join("model.onnx"))
        .context("Failed to copy model.onnx to cache")?;
    std::fs::copy(&tokenizer_path, dir.join("tokenizer.json"))
        .context("Failed to copy tokenizer.json to cache")?;

    // Set file permissions to user-only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(dir.join("model.onnx"), perms.clone());
        let _ = std::fs::set_permissions(dir.join("tokenizer.json"), perms);
    }

    // Verify checksums after download — abort if tampered or corrupted
    if !verify_checksums(dir)? {
        anyhow::bail!(
            "Model checksum verification failed after download. \
             Files may be corrupted or tampered with. Delete {} and retry.",
            dir.display()
        );
    }

    eprintln!("Model downloaded to {}", dir.display());
    Ok(())
}

/// Ensure a model directory is usable.
///
/// When `allow_download` is false, missing model files are treated as a hard
/// error so deployments can pin a model directory without unexpected network
/// access.
pub fn ensure_model_in_dir(dir: &Path, allow_download: bool) -> Result<PathBuf> {
    if !model_files_exist(dir) {
        if allow_download {
            download_model(dir)?;
        } else {
            anyhow::bail!(
                "Model files not found in {}. Populate the directory or run setup against the default cache.",
                dir.display()
            );
        }
    } else if !verify_checksums(dir)? {
        anyhow::bail!(
            "Model checksum verification failed at startup. \
             Files may be corrupted or tampered with. Delete {} and re-run setup.",
            dir.display()
        );
    }
    Ok(dir.to_path_buf())
}

/// Ensure model is available in the default cache, downloading if needed.
/// Returns model directory.
pub fn ensure_model() -> Result<PathBuf> {
    let dir = model_cache_dir()?;
    ensure_model_in_dir(&dir, true)
}

struct RealEmbedder {
    session: Session,
    tokenizer: Tokenizer,
}

/// Internal representation: real ONNX model or noop (for testing).
enum EmbedderInner {
    Real(Box<RealEmbedder>),
    Noop,
    Failing(String),
}

/// The embedder: wraps either a loaded ONNX session or a noop for testing.
pub struct Embedder {
    inner: EmbedderInner,
}

impl Embedder {
    /// Create a new embedder, loading model from disk.
    pub fn new(model_dir: &Path) -> Result<Self> {
        let model_path = model_dir.join("model.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");

        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("Failed to create ONNX session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("Failed to set optimization level: {e}"))?
            .commit_from_file(&model_path)
            .map_err(|e| anyhow::anyhow!("Failed to load ONNX model: {e}"))?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {e}"))?;

        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SEQ_LEN,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("Failed to set truncation: {e}"))?;

        tokenizer.with_padding(Some(tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            ..Default::default()
        }));

        Ok(Self {
            inner: EmbedderInner::Real(Box::new(RealEmbedder { session, tokenizer })),
        })
    }

    /// Create a noop embedder that returns zero vectors without loading any model.
    /// Intended for testing — zero vectors are valid inputs to the DB and HNSW index.
    pub fn new_noop() -> Self {
        Self {
            inner: EmbedderInner::Noop,
        }
    }

    /// Create a test embedder that fails every embed request with the given message.
    pub fn new_failing_for_test(message: impl Into<String>) -> Self {
        Self {
            inner: EmbedderInner::Failing(message.into()),
        }
    }

    /// Embed a single text, returning a 384-dim vector.
    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        let results = self.embed_batch(&[text])?;
        debug_assert_eq!(
            results.len(),
            EMBED_DIM,
            "embed_one expected {EMBED_DIM} floats, got {}",
            results.len()
        );
        Ok(results)
    }

    /// Embed a batch of texts, returning a flat f32 array (n * EMBED_DIM).
    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<f32>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        if matches!(self.inner, EmbedderInner::Noop) {
            // Return deterministic zero vectors — valid for DB storage and index ops.
            return Ok(vec![0.0f32; texts.len() * EMBED_DIM]);
        }

        if let EmbedderInner::Failing(message) = &self.inner {
            anyhow::bail!(message.clone());
        }

        let mut all_embeddings = Vec::with_capacity(texts.len() * EMBED_DIM);

        for batch_start in (0..texts.len()).step_by(BATCH_SIZE) {
            let batch_end = (batch_start + BATCH_SIZE).min(texts.len());
            let batch = &texts[batch_start..batch_end];
            let batch_embeddings = self.embed_batch_inner(batch)?;
            all_embeddings.extend_from_slice(&batch_embeddings);
        }

        Ok(all_embeddings)
    }

    /// Inner batch embedding — processes a single batch through ONNX.
    /// Only called from the `Real` branch of `embed_batch`.
    fn embed_batch_inner(&mut self, texts: &[&str]) -> Result<Vec<f32>> {
        let EmbedderInner::Real(ref mut inner) = self.inner else {
            unreachable!("embed_batch_inner called on noop embedder");
        };
        let session = &mut inner.session;
        let tokenizer = &mut inner.tokenizer;

        let batch_size = texts.len();

        let encodings = tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("Tokenization failed: {e}"))?;

        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0)
            .min(MAX_SEQ_LEN);

        let mut input_ids = Array2::<i64>::zeros((batch_size, seq_len));
        let mut attention_mask = Array2::<i64>::zeros((batch_size, seq_len));
        let mut token_type_ids = Array2::<i64>::zeros((batch_size, seq_len));

        for (i, encoding) in encodings.iter().enumerate() {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();
            let types = encoding.get_type_ids();

            let len = ids.len().min(seq_len);
            for j in 0..len {
                input_ids[[i, j]] = ids[j] as i64;
                attention_mask[[i, j]] = mask[j] as i64;
                token_type_ids[[i, j]] = types[j] as i64;
            }
        }

        let input_ids_tensor = TensorRef::from_array_view(input_ids.view())
            .map_err(|e| anyhow::anyhow!("Failed to create input_ids tensor: {e}"))?;
        let attention_mask_tensor = TensorRef::from_array_view(attention_mask.view())
            .map_err(|e| anyhow::anyhow!("Failed to create attention_mask tensor: {e}"))?;
        let token_type_ids_tensor = TensorRef::from_array_view(token_type_ids.view())
            .map_err(|e| anyhow::anyhow!("Failed to create token_type_ids tensor: {e}"))?;

        let outputs = session
            .run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            ])
            .map_err(|e| anyhow::anyhow!("ONNX inference failed: {e}"))?;

        let raw_embeddings: Vec<f32> = if let Some(val) = outputs.get("sentence_embedding") {
            let (_shape, data): (&ort::value::Shape, &[f32]) = val
                .try_extract_tensor()
                .map_err(|e| anyhow::anyhow!("Failed to extract sentence_embedding: {e}"))?;
            data.to_vec()
        } else if let Some(val) = outputs.get("token_embeddings") {
            let (shape, data): (&ort::value::Shape, &[f32]) = val
                .try_extract_tensor()
                .map_err(|e| anyhow::anyhow!("Failed to extract token_embeddings: {e}"))?;
            mean_pool_flat(data, shape, &attention_mask)
        } else if let Some(val) = outputs.get("last_hidden_state") {
            let (shape, data): (&ort::value::Shape, &[f32]) = val
                .try_extract_tensor()
                .map_err(|e| anyhow::anyhow!("Failed to extract last_hidden_state: {e}"))?;
            mean_pool_flat(data, shape, &attention_mask)
        } else {
            let val = &outputs[0];
            let (shape, data): (&ort::value::Shape, &[f32]) = val
                .try_extract_tensor()
                .map_err(|e| anyhow::anyhow!("Failed to extract embedding tensor: {e}"))?;

            let dims = &**shape;
            if dims.len() == 2 && dims[1] as usize == EMBED_DIM {
                data.to_vec()
            } else if dims.len() == 3 {
                mean_pool_flat(data, dims, &attention_mask)
            } else {
                anyhow::bail!(
                    "Unexpected output shape: {:?}. Expected [batch, {}] or [batch, seq, {}]",
                    dims,
                    EMBED_DIM,
                    EMBED_DIM
                );
            }
        };

        // L2 normalize each embedding
        let mut result = Vec::with_capacity(batch_size * EMBED_DIM);
        for i in 0..batch_size {
            let start = i * EMBED_DIM;
            let end = start + EMBED_DIM;
            if end > raw_embeddings.len() {
                result.extend(std::iter::repeat_n(0.0f32, EMBED_DIM));
                continue;
            }
            let slice = &raw_embeddings[start..end];
            let norm: f32 = slice.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 1e-12 {
                result.extend(slice.iter().map(|x| x / norm));
            } else {
                result.extend(slice);
            }
        }

        Ok(result)
    }
}

/// Mean pooling over flat token embeddings data using attention mask.
fn mean_pool_flat(data: &[f32], shape: &[i64], attention_mask: &Array2<i64>) -> Vec<f32> {
    let batch_size = shape[0] as usize;
    let seq_len = shape[1] as usize;
    let hidden_dim = shape[2] as usize;

    let mut result = Vec::with_capacity(batch_size * hidden_dim);

    for b in 0..batch_size {
        let mut pooled = vec![0.0f32; hidden_dim];
        let mut count = 0.0f32;

        for s in 0..seq_len {
            let mask_val = if s < attention_mask.shape()[1] {
                attention_mask[[b, s]] as f32
            } else {
                0.0
            };

            if mask_val > 0.0 {
                let offset = (b * seq_len + s) * hidden_dim;
                for d in 0..hidden_dim {
                    pooled[d] += data[offset + d] * mask_val;
                }
                count += mask_val;
            }
        }

        if count > 0.0 {
            for val in &mut pooled {
                *val /= count;
            }
        }

        result.extend(pooled);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_cache_dir_is_stable() {
        let dir = model_cache_dir().unwrap();
        assert!(dir
            .to_string_lossy()
            .contains(".ironrace/models/all-MiniLM-L6-v2"));
    }

    #[test]
    fn noop_embed_batch_returns_correct_dimensions() {
        let mut emb = Embedder::new_noop();
        let result = emb.embed_batch(&["hello", "world"]).unwrap();
        assert_eq!(result.len(), 2 * EMBED_DIM);
        assert!(result.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn noop_embed_one_returns_correct_dimensions() {
        let mut emb = Embedder::new_noop();
        let result = emb.embed_one("test").unwrap();
        assert_eq!(result.len(), EMBED_DIM);
    }

    #[test]
    fn noop_embed_batch_empty_returns_empty() {
        let mut emb = Embedder::new_noop();
        let result = emb.embed_batch(&[]).unwrap();
        assert!(result.is_empty());
    }
}
