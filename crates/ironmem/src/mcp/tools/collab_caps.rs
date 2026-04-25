use serde_json::{json, Value};

use crate::collab::queue::Capability;
use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::sanitize;

use super::shared::{require_agent, require_str, MAX_COLLAB_CAP_FIELD_CHARS};

pub(super) fn handle_collab_register_caps(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    let capabilities = args
        .get("capabilities")
        .and_then(|value| value.as_array())
        .ok_or_else(|| MemoryError::Validation("capabilities must be an array".to_string()))?;

    let mut parsed = Vec::new();
    for capability in capabilities {
        let name = capability
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| MemoryError::Validation("capability name is required".to_string()))?;
        let name = sanitize::sanitize_content(name, MAX_COLLAB_CAP_FIELD_CHARS)?.to_string();
        let description = capability
            .get("description")
            .and_then(|value| value.as_str())
            .map(|value| sanitize::sanitize_content(value, MAX_COLLAB_CAP_FIELD_CHARS))
            .transpose()?
            .map(ToString::to_string);
        parsed.push(Capability {
            agent: agent.to_string(),
            name,
            description,
        });
    }

    let count = parsed.len();
    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        crate::collab::queue::register_caps(tx, session_id, agent.as_str(), &parsed)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_register_caps",
            &json!({
                "session_id": session_id,
                "agent": agent.as_str(),
                "count": count,
            }),
            Some(&json!({ "success": true, "count": count })),
        )?;
        Ok(())
    })?;

    Ok(json!({ "success": true, "count": count }))
}

pub(super) fn handle_collab_get_caps(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = args
        .get("agent")
        .and_then(|value| value.as_str())
        .map(require_agent)
        .transpose()?;
    let capabilities = app
        .db
        .collab_get_caps(session_id, agent.as_ref().map(|a| a.as_str()))?
        .into_iter()
        .map(|capability| {
            json!({
                "agent": capability.agent,
                "name": capability.name,
                "description": capability.description,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "capabilities": capabilities }))
}
