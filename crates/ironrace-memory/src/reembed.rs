use crate::error::MemoryError;
use crate::mcp::app::App;

/// Re-embed all drawers using the currently loaded model.
/// Uses the existing upsert path, so IDs and metadata are preserved.
/// Prints progress to stderr.
pub fn reembed_all(app: &App, wing: Option<&str>) -> Result<(), MemoryError> {
    let drawers = app.db.get_drawers(wing, None, usize::MAX)?;
    let total = drawers.len();

    if total == 0 {
        eprintln!("No drawers to re-embed.");
        return Ok(());
    }

    eprintln!("Re-embedding {total} drawers with the current model...");

    let mut done = 0usize;
    let mut skipped = 0usize;

    for drawer in &drawers {
        let embedding = {
            let mut emb = app
                .embedder
                .write()
                .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
            match emb.embed_one(&drawer.content) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Skipping drawer {}: embed failed: {e}", drawer.id);
                    skipped += 1;
                    continue;
                }
            }
        };

        app.db.insert_drawer(
            &drawer.id,
            &drawer.content,
            &embedding,
            &drawer.wing,
            &drawer.room,
            &drawer.source_file,
            &drawer.added_by,
        )?;

        done += 1;
        if done.is_multiple_of(100) {
            eprintln!("  {done}/{total}...");
        }
    }

    eprintln!("Done: {done} re-embedded, {skipped} skipped.");
    Ok(())
}
