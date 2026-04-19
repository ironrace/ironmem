//! Ingestion pipeline — mine files into memory with incremental updates.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bootstrap::record_workspace_mine;
use crate::error::MemoryError;
use crate::mcp::app::App;

const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_CHUNK_CHARS: usize = 2_400;
const CHUNK_OVERLAP_CHARS: usize = 240;
const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct MineManifest {
    version: u32,
    root: String,
    files: HashMap<String, MineManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MineManifestEntry {
    content_hash: String,
    chunk_count: usize,
    updated_at: String,
}

struct CandidateFile {
    absolute_path: PathBuf,
    content: String,
    content_hash: String,
}

/// Directories that must never be mined regardless of user input.
const BLOCKED_SYSTEM_PREFIXES: &[&str] = &[
    "/etc", "/usr", "/var", "/sys", "/proc", "/dev", "/boot", "/run", "/bin", "/sbin", "/lib",
    "/lib64",
];

pub fn mine_directory(app: &App, path: &str) -> Result<(), MemoryError> {
    let root = PathBuf::from(path);
    if !root.is_dir() {
        return Err(MemoryError::NotFound(format!(
            "Directory not found for mining: {path}"
        )));
    }

    // Canonicalize to resolve symlinks before checking system-path boundaries.
    let root = root.canonicalize().unwrap_or(root);
    let root_str = root.to_string_lossy();
    if BLOCKED_SYSTEM_PREFIXES
        .iter()
        .any(|prefix| root_str == *prefix || root_str.starts_with(&format!("{}/", prefix)))
    {
        return Err(MemoryError::Validation(format!(
            "Mining of system directory '{root_str}' is not allowed"
        )));
    }

    let manifest_path = manifest_path_for_root(&app.config.state_dir, &root);
    let mut manifest = load_manifest(&manifest_path, &root)?;
    let current_files = collect_candidate_files(&root, include_hidden_paths())?;
    let current_paths: HashSet<String> = current_files
        .iter()
        .map(|file| file.absolute_path.display().to_string())
        .collect();

    for removed in manifest
        .files
        .keys()
        .filter(|path| !current_paths.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>()
    {
        app.db.delete_drawers_by_source_file(&removed)?;
        manifest.files.remove(&removed);
    }

    let root_has_git = root.join(".git").exists();

    for file in current_files {
        let source_file = file.absolute_path.display().to_string();
        let unchanged = manifest
            .files
            .get(&source_file)
            .map(|entry| entry.content_hash == file.content_hash)
            .unwrap_or(false);
        if unchanged {
            continue;
        }

        let (wing, room) = derive_location(&root, &file.absolute_path, root_has_git)?;

        if file_contains_secrets(&file.content) {
            tracing::debug!(
                path = %file.absolute_path.display(),
                "skipping file: possible secrets detected"
            );
            // Evict any previously-indexed drawers so stale sensitive content
            // does not remain searchable after a file gains credentials.
            app.db.delete_drawers_by_source_file(&source_file)?;
            // Record in the manifest with chunk_count=0 so subsequent mine
            // runs see the current hash and skip without retrying.
            manifest.files.insert(
                source_file,
                MineManifestEntry {
                    content_hash: file.content_hash,
                    chunk_count: 0,
                    updated_at: chrono::Utc::now().to_rfc3339(),
                },
            );
            continue;
        }

        let chunks = chunk_text(&file.content);

        let embeddings = if chunks.is_empty() {
            Vec::new()
        } else {
            let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
            let mut embedder = app
                .embedder
                .write()
                .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
            embedder
                .embed_batch(&chunk_refs)
                .map_err(MemoryError::Embed)?
        };

        app.db.with_transaction(|tx| {
            crate::db::schema::Database::delete_drawers_by_source_file_tx(tx, &source_file)?;
            for (chunk_index, chunk) in chunks.iter().enumerate() {
                let start = chunk_index * ironrace_embed::embedder::EMBED_DIM;
                let end = start + ironrace_embed::embedder::EMBED_DIM;
                let embedding = embeddings.get(start..end).ok_or_else(|| {
                    MemoryError::Validation("Embedding batch length mismatch during mining".into())
                })?;
                let id = mined_drawer_id(chunk, &wing, &room, &source_file, chunk_index);
                crate::db::schema::Database::insert_drawer_tx(
                    tx,
                    &id,
                    chunk,
                    embedding,
                    &wing,
                    &room,
                    &source_file,
                    "mine",
                )?;
            }
            Ok(())
        })?;

        if chunks.is_empty() {
            manifest.files.remove(&source_file);
        } else {
            manifest.files.insert(
                source_file,
                MineManifestEntry {
                    content_hash: file.content_hash,
                    chunk_count: chunks.len(),
                    updated_at: chrono::Utc::now().to_rfc3339(),
                },
            );
        }
        app.mark_dirty();
    }

    save_manifest(&manifest_path, &manifest)?;
    record_workspace_mine(&app.config, &root)?;
    Ok(())
}

fn include_hidden_paths() -> bool {
    std::env::var("IRONMEM_MINE_HIDDEN")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
}

fn collect_candidate_files(
    root: &Path,
    include_hidden: bool,
) -> Result<Vec<CandidateFile>, MemoryError> {
    let walker = WalkBuilder::new(root)
        .standard_filters(!include_hidden)
        .hidden(!include_hidden)
        .git_ignore(true)
        .git_exclude(true)
        .filter_entry(|entry| !should_skip_entry(entry.path()))
        .build();

    let mut files = Vec::new();
    for entry in walker {
        let entry = entry.map_err(|err| MemoryError::Io(std::io::Error::other(err.to_string())))?;
        let path = entry.path();
        if !path.is_file() || !is_candidate_file(path) {
            continue;
        }
        let bytes = std::fs::read(path)?;
        if bytes.contains(&0) {
            continue;
        }
        let content = match String::from_utf8(bytes) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut hasher = Sha256::new();
        hasher.update(trimmed.as_bytes());
        files.push(CandidateFile {
            absolute_path: path.to_path_buf(),
            content: trimmed.to_string(),
            content_hash: format!("{:x}", hasher.finalize()),
        });
    }
    Ok(files)
}

fn should_skip_entry(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "build"
            | ".build"
            | "dist"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".pytest_cache"
            | ".ruff_cache"
            | ".idea"
            | ".swiftpm"
            | "DerivedData"
    )
}

fn is_candidate_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if metadata.len() == 0 || metadata.len() > MAX_FILE_BYTES {
        return false;
    }

    let allowed = [
        "md", "txt", "rst", "py", "toml", "json", "yaml", "yml", "swift", "kt", "java", "js", "ts",
        "tsx", "jsx", "go", "rs", "sql", "html", "css", "sh",
    ];
    path.extension()
        .and_then(|value| value.to_str())
        .map(|ext| allowed.contains(&ext))
        .unwrap_or_else(|| {
            path.file_name()
                .and_then(|value| value.to_str())
                .map(|name| {
                    matches!(
                        name,
                        "README" | "README.md" | "TASKS.md" | "IMPLEMENTATION_PLAN.md"
                    )
                })
                .unwrap_or(false)
        })
}

fn derive_location(
    root: &Path,
    file_path: &Path,
    root_has_git: bool,
) -> Result<(String, String), MemoryError> {
    let root_name = sanitize_segment(
        root.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("workspace"),
        "workspace",
    );
    let relative = file_path.strip_prefix(root).map_err(|_| {
        MemoryError::Validation("Failed to compute relative path during mining".into())
    })?;
    let parents: Vec<String> = relative
        .parent()
        .map(|parent| {
            parent
                .components()
                .filter_map(|component| component.as_os_str().to_str())
                .map(|part| sanitize_segment(part, "general"))
                .collect()
        })
        .unwrap_or_default();

    let (wing, room) = if root_has_git {
        (
            root_name,
            parents
                .first()
                .cloned()
                .unwrap_or_else(|| "general".to_string()),
        )
    } else {
        match parents.as_slice() {
            [] => (root_name, "general".to_string()),
            [wing] => (wing.clone(), "general".to_string()),
            [wing, room, ..] => (wing.clone(), room.clone()),
        }
    };

    Ok((
        crate::sanitize::sanitize_name(&wing, "wing")?,
        crate::sanitize::sanitize_name(&room, "room")?,
    ))
}

/// Returns true if the file content appears to contain credentials or secrets.
///
/// Uses simple case-insensitive substring matching — no regex, no external deps.
/// A single matching line skips the whole file; we never redact individual lines.
fn file_contains_secrets(content: &str) -> bool {
    for line in content.lines() {
        let lower = line.to_lowercase();
        // Key/token/password assignment patterns (require `=` to avoid matching
        // variable names like `secret_key_field_name` in docs or code).
        if lower.contains("_key=")
            || lower.contains("_secret=")
            || lower.contains("_token=")
            || lower.contains("_password=")
            || lower.contains("_passwd=")
            || lower.contains("api_key=")
            // Spaced variants: `OPENAI_API_KEY = "..."` style (TOML, .env, INI)
            || lower.contains("_key =")
            || lower.contains("_secret =")
            || lower.contains("_token =")
            || lower.contains("_password =")
            || lower.contains("_passwd =")
            || lower.contains("api_key =")
            // Bare config-style assignments
            || lower.contains("password =")
            || lower.contains("passwd =")
            || lower.contains("secret =")
            // PEM headers (private keys, certs, etc.)
            || lower.contains("-----begin")
            // Service-account JSON field
            || lower.contains("\"private_key\"")
        {
            return true;
        }
    }
    false
}

fn chunk_text(content: &str) -> Vec<String> {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= MAX_CHUNK_CHARS {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + MAX_CHUNK_CHARS).min(chars.len());
        let chunk: String = chars[start..end].iter().collect();
        let trimmed = chunk.trim();
        if !trimmed.is_empty() {
            chunks.push(trimmed.to_string());
        }
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_OVERLAP_CHARS);
    }
    chunks
}

fn mined_drawer_id(
    content: &str,
    wing: &str,
    room: &str,
    source_file: &str,
    chunk_index: usize,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hasher.update(wing.as_bytes());
    hasher.update(room.as_bytes());
    hasher.update(source_file.as_bytes());
    hasher.update(chunk_index.to_le_bytes());
    format!("{:x}", hasher.finalize())[..32].to_string()
}

fn manifest_path_for_root(state_dir: &Path, root: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    state_dir
        .join("mine_manifests")
        .join(format!("{}.json", &key[..16]))
}

fn load_manifest(path: &Path, root: &Path) -> Result<MineManifest, MemoryError> {
    if !path.is_file() {
        return Ok(MineManifest {
            version: MANIFEST_VERSION,
            root: root.display().to_string(),
            files: HashMap::new(),
        });
    }
    let raw = std::fs::read_to_string(path)?;
    let mut manifest: MineManifest = serde_json::from_str(&raw)?;
    if manifest.version == 0 {
        manifest.version = MANIFEST_VERSION;
    }
    if manifest.root.is_empty() {
        manifest.root = root.display().to_string();
    }
    Ok(manifest)
}

fn save_manifest(path: &Path, manifest: &MineManifest) -> Result<(), MemoryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(manifest)?;
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("manifest"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::write(&tmp, &raw)?;
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn sanitize_segment(raw: &str, fallback: &str) -> String {
    let mut sanitized = raw
        .trim_matches(|c: char| !c.is_alphanumeric())
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ' | '\'') {
                c
            } else {
                ' '
            }
        })
        .collect::<String>();
    sanitized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_text_splits_large_content() {
        let content = "a".repeat(MAX_CHUNK_CHARS + 500);
        let chunks = chunk_text(&content);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].len() >= MAX_CHUNK_CHARS - 10);
    }

    #[test]
    fn derive_location_for_single_repo_uses_root_name_as_wing() {
        let root = Path::new("/tmp/glp1-companion-backend");
        let file = root.join("app/models.py");
        let (wing, room) = derive_location(root, &file, true).unwrap();
        assert_eq!(wing, "glp1-companion-backend");
        assert_eq!(room, "app");
    }

    #[test]
    fn derive_location_for_workspace_uses_first_component_as_wing() {
        let root = Path::new("/tmp/jcagentszero");
        let file = root.join("glp1-companion-frontend/Views/Home/HomeView.swift");
        let (wing, room) = derive_location(root, &file, false).unwrap();
        assert_eq!(wing, "glp1-companion-frontend");
        assert_eq!(room, "Views");
    }

    #[test]
    fn candidate_file_filter_rejects_large_or_binary_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("blob.bin");
        std::fs::write(&path, [0u8, 1, 2]).unwrap();
        assert!(!is_candidate_file(&path));
    }

    #[test]
    fn collect_candidate_files_skips_hidden_paths_by_default() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("visible.md"), "visible").unwrap();
        std::fs::create_dir_all(temp.path().join(".codex")).unwrap();
        std::fs::write(temp.path().join(".secret.md"), "secret").unwrap();
        std::fs::write(temp.path().join(".codex").join("notes.md"), "hidden notes").unwrap();

        let files = collect_candidate_files(temp.path(), false).unwrap();
        let paths: Vec<String> = files
            .iter()
            .map(|file| file.absolute_path.display().to_string())
            .collect();

        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("visible.md"));
    }

    #[test]
    fn collect_candidate_files_can_include_hidden_paths_when_opted_in() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("visible.md"), "visible").unwrap();
        std::fs::create_dir_all(temp.path().join(".codex")).unwrap();
        std::fs::write(temp.path().join(".secret.md"), "secret").unwrap();
        std::fs::write(temp.path().join(".codex").join("notes.md"), "hidden notes").unwrap();

        let files = collect_candidate_files(temp.path(), true).unwrap();
        let paths: Vec<String> = files
            .iter()
            .map(|file| file.absolute_path.display().to_string())
            .collect();

        assert_eq!(paths.len(), 3);
        assert!(paths.iter().any(|path| path.ends_with("visible.md")));
        assert!(paths.iter().any(|path| path.ends_with(".secret.md")));
        assert!(paths.iter().any(|path| path.ends_with(".codex/notes.md")));
    }

    #[test]
    fn secret_filter_catches_aws_key_assignment() {
        assert!(file_contains_secrets(
            "export AWS_SECRET_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE\n"
        ));
    }

    #[test]
    fn secret_filter_catches_pem_header() {
        assert!(file_contains_secrets("-----BEGIN RSA PRIVATE KEY-----\n"));
    }

    #[test]
    fn secret_filter_catches_service_account_json() {
        assert!(file_contains_secrets(
            r#"{ "private_key": "-----BEGIN RSA PRIVATE KEY-----" }"#
        ));
    }

    #[test]
    fn secret_filter_catches_bare_password_assignment() {
        assert!(file_contains_secrets("password = hunter2\n"));
        assert!(file_contains_secrets("SECRET = abc123\n"));
    }

    #[test]
    fn secret_filter_catches_spaced_assignment_variants() {
        // TOML / .env / INI style: OPENAI_API_KEY = "sk-..."
        assert!(file_contains_secrets("OPENAI_API_KEY = sk-abc123\n"));
        assert!(file_contains_secrets("GITHUB_TOKEN = ghp_abc123\n"));
        assert!(file_contains_secrets("MY_SECRET = supersecret\n"));
        assert!(file_contains_secrets("DB_PASSWORD = hunter2\n"));
    }

    #[test]
    fn secret_filter_ignores_variable_names_without_assignment() {
        // "secret_key_field_name" in docs or code — no `=` follows the pattern
        assert!(!file_contains_secrets(
            "// See secret_key_field_name for details\n"
        ));
        // Function call: `_key` appears but is followed by `)`, not ` =` or `=`
        assert!(!file_contains_secrets("let value = get_key();\n"));
    }

    #[test]
    fn secret_filter_is_case_insensitive() {
        assert!(file_contains_secrets("GITHUB_TOKEN=ghp_abc123\n"));
        assert!(file_contains_secrets("github_token=ghp_abc123\n"));
    }
}
