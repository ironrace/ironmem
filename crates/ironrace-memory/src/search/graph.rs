//! Memory graph — BFS traversal, tunnel detection, and statistics.
//!
//! Builds a navigable room-graph from drawer metadata. Rooms in the same
//! wing are connected. "Tunnels" are rooms that span multiple wings.
//! Results are cached at the App level and invalidated on writes.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::Serialize;

use crate::error::MemoryError;
use crate::mcp::app::App;

#[derive(Debug, Clone, Serialize)]
pub struct GraphStats {
    pub total_rooms: usize,
    pub total_wings: usize,
    pub tunnels: usize,
    pub edges: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tunnel {
    pub room: String,
    pub wings: Vec<String>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct TraversalResult {
    pub start_room: String,
    pub visited: Vec<String>,
    pub depth: usize,
}

/// Cached memory graph data.
#[derive(Debug, Clone)]
pub struct MemoryGraph {
    pub wing_rooms: HashMap<String, HashSet<String>>,
    pub room_wings: HashMap<String, HashSet<String>>,
}

/// Build the room adjacency graph from drawer metadata.
fn build_graph_from_db(app: &App) -> Result<MemoryGraph, MemoryError> {
    let mut wing_rooms: HashMap<String, HashSet<String>> = HashMap::new();
    let mut room_wings: HashMap<String, HashSet<String>> = HashMap::new();

    for (wing, room) in app.db.wing_room_pairs()? {
        wing_rooms
            .entry(wing.clone())
            .or_default()
            .insert(room.clone());
        room_wings.entry(room).or_default().insert(wing);
    }

    Ok(MemoryGraph {
        wing_rooms,
        room_wings,
    })
}

/// Get the memory graph, using the App-level cache if available.
fn get_graph(app: &App) -> Result<MemoryGraph, MemoryError> {
    // Check cache first
    {
        let cache = app
            .graph_cache
            .read()
            .map_err(|e| MemoryError::Lock(format!("graph_cache lock poisoned: {e}")))?;
        if let Some(ref graph) = *cache {
            return Ok(graph.clone());
        }
    }

    // Cache miss: build from DB and store
    let graph = build_graph_from_db(app)?;
    if let Ok(mut cache) = app.graph_cache.write() {
        *cache = Some(graph.clone());
    }
    Ok(graph)
}

/// BFS traversal from a starting room.
pub fn traverse(
    app: &App,
    start_room: &str,
    max_depth: usize,
) -> Result<TraversalResult, MemoryError> {
    let graph = get_graph(app)?;

    let mut visited: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    let mut max_reached = 0;

    // Find the start room with a best-effort substring match.
    let start = graph
        .room_wings
        .keys()
        .find(|r| r.contains(start_room) || start_room.contains(r.as_str()))
        .cloned()
        .unwrap_or_else(|| start_room.to_string());

    queue.push_back((start.clone(), 0));
    seen.insert(start.clone());

    while let Some((room, depth)) = queue.pop_front() {
        if depth > max_depth {
            break;
        }
        max_reached = max_reached.max(depth);
        visited.push(room.clone());

        // Find adjacent rooms: rooms in the same wing(s)
        if let Some(wings) = graph.room_wings.get(&room) {
            for wing in wings {
                if let Some(rooms) = graph.wing_rooms.get(wing) {
                    for neighbor in rooms {
                        if !seen.contains(neighbor) {
                            seen.insert(neighbor.clone());
                            queue.push_back((neighbor.clone(), depth + 1));
                        }
                    }
                }
            }
        }
    }

    Ok(TraversalResult {
        start_room: start,
        visited,
        depth: max_reached,
    })
}

/// Find rooms that span multiple wings (tunnels).
pub fn find_tunnels(app: &App) -> Result<Vec<Tunnel>, MemoryError> {
    let graph = get_graph(app)?;

    let mut tunnels: Vec<Tunnel> = graph
        .room_wings
        .into_iter()
        .filter(|(_, wings)| wings.len() > 1)
        .map(|(room, wings)| {
            let count = wings.len();
            Tunnel {
                room,
                wings: wings.into_iter().collect(),
                count,
            }
        })
        .collect();

    tunnels.sort_by_key(|tunnel| std::cmp::Reverse(tunnel.count));
    Ok(tunnels)
}

/// Graph statistics.
pub fn graph_stats(app: &App) -> Result<GraphStats, MemoryError> {
    let graph = get_graph(app)?;

    let tunnels = graph.room_wings.values().filter(|ws| ws.len() > 1).count();
    let edges: usize = graph
        .wing_rooms
        .values()
        .map(|rooms| {
            let n = rooms.len();
            if n > 1 {
                n * (n - 1) / 2
            } else {
                0
            }
        })
        .sum();

    Ok(GraphStats {
        total_rooms: graph.room_wings.len(),
        total_wings: graph.wing_rooms.len(),
        tunnels,
        edges,
    })
}
