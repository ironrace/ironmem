use sha2::{Digest, Sha256};

use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::sanitize;

pub const DIARY_ROOM: &str = "diary";

#[derive(Debug, Clone)]
pub struct DiaryEntry {
    pub id: String,
    pub wing: String,
}

pub fn write_entry(
    app: &App,
    content: &str,
    wing: &str,
    added_by: &str,
    max_length: usize,
) -> Result<DiaryEntry, MemoryError> {
    let content = sanitize::sanitize_content(content, max_length)?;
    let wing = sanitize::sanitize_name(wing, "wing")?;
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let id = generate_diary_id(content, &wing, DIARY_ROOM, added_by, &timestamp);

    let embedding = {
        let mut embedder = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        embedder.embed_one(content).map_err(MemoryError::Embed)?
    };

    app.db.with_transaction(|tx| {
        crate::db::schema::Database::insert_drawer_tx(
            tx, &id, content, &embedding, &wing, DIARY_ROOM, "", added_by,
        )?;
        Ok(())
    })?;
    app.insert_into_index(&id, &embedding)?;

    Ok(DiaryEntry { id, wing })
}

fn generate_diary_id(
    content: &str,
    wing: &str,
    room: &str,
    added_by: &str,
    timestamp: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hasher.update(wing.as_bytes());
    hasher.update(room.as_bytes());
    hasher.update(added_by.as_bytes());
    hasher.update(timestamp.as_bytes());
    format!("{:x}", hasher.finalize())[..32].to_string()
}
