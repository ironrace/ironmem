//! Synchronous, bounded client for the already-running MCP daemon.
//!
//! Hook orchestration consumes this internal API in the follow-up task; keep
//! the transport self-contained here so it cannot initialize, spawn, or load
//! an embedder while it is staged.
#![allow(dead_code)]

use std::path::Path;
use std::time::{Duration, Instant};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const REQUEST_ID: u64 = 1;

/// Search the already-running daemon without initialization, spawning, or
/// touching the local embedder. The deadline is absolute and applies to the
/// complete request/response exchange.
#[cfg(unix)]
pub(crate) fn search_ids(
    socket_path: &Path,
    query: &str,
    limit: usize,
    deadline: Instant,
) -> Option<Vec<String>> {
    use std::fs;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixStream;

    let metadata = fs::symlink_metadata(socket_path).ok()?;
    if !metadata.file_type().is_socket() {
        return None;
    }
    remaining(deadline)?;

    let mut stream = UnixStream::connect(socket_path).ok()?;
    let mut request = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": REQUEST_ID,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": {
                "query": query,
                "limit": limit,
            }
        }
    }))
    .ok()?;
    request.push(b'\n');

    write_with_deadline(&mut stream, &request, deadline)?;
    let response = read_bounded_line(&mut stream, deadline)?;
    parse_search_ids(&response)
}

/// The daemon transport is unavailable on platforms without Unix sockets.
#[cfg(not(unix))]
pub(crate) fn search_ids(
    socket_path: &Path,
    query: &str,
    limit: usize,
    deadline: Instant,
) -> Option<Vec<String>> {
    let _ = (socket_path, query, limit, deadline);
    None
}

fn remaining(deadline: Instant) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    (!remaining.is_zero()).then_some(remaining)
}

#[cfg(unix)]
fn write_with_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    bytes: &[u8],
    deadline: Instant,
) -> Option<()> {
    use std::io::Write;

    let mut offset = 0;
    while offset < bytes.len() {
        wait_for_io(stream, libc::POLLOUT, deadline)?;
        refresh_write_timeout(stream, deadline)?;
        remaining(deadline)?;
        let written = stream.write(&bytes[offset..]).ok()?;
        if written == 0 {
            return None;
        }
        offset += written;
    }
    Some(())
}

#[cfg(unix)]
fn read_bounded_line(
    stream: &mut std::os::unix::net::UnixStream,
    deadline: Instant,
) -> Option<Vec<u8>> {
    use std::io::Read;

    let mut response = Vec::with_capacity(8192);
    let mut chunk = [0_u8; 8192];
    loop {
        wait_for_io(stream, libc::POLLIN, deadline)?;
        refresh_read_timeout(stream, deadline)?;
        remaining(deadline)?;
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }

        let line_end = chunk[..read]
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(read);
        let new_len = response.len().checked_add(line_end)?;
        let frame_len = new_len.checked_add(usize::from(line_end < read))?;
        if frame_len > MAX_RESPONSE_BYTES {
            return None;
        }
        response.extend_from_slice(&chunk[..line_end]);
        if line_end < read {
            return Some(response);
        }
    }
}

#[cfg(unix)]
fn refresh_read_timeout(stream: &std::os::unix::net::UnixStream, deadline: Instant) -> Option<()> {
    let timeout = remaining(deadline)?;
    match stream.set_read_timeout(Some(timeout)) {
        Ok(()) => Some(()),
        // macOS rejects repeated SO_RCVTIMEO updates on AF_UNIX streams. The
        // poll immediately before the read still enforces this same absolute
        // deadline on that platform.
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Some(()),
        Err(_) => None,
    }
}

#[cfg(unix)]
fn refresh_write_timeout(stream: &std::os::unix::net::UnixStream, deadline: Instant) -> Option<()> {
    let timeout = remaining(deadline)?;
    match stream.set_write_timeout(Some(timeout)) {
        Ok(()) => Some(()),
        // See `refresh_read_timeout` for the macOS AF_UNIX behavior. Polling
        // is the fallback deadline guard when the socket option is rejected.
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Some(()),
        Err(_) => None,
    }
}

#[cfg(unix)]
fn wait_for_io(
    stream: &std::os::unix::net::UnixStream,
    event: i16,
    deadline: Instant,
) -> Option<()> {
    use std::os::unix::io::AsRawFd;

    loop {
        let timeout = remaining(deadline)?;
        let timeout_ms = timeout
            .as_nanos()
            .saturating_add(999_999)
            .checked_div(1_000_000)?
            .clamp(1, i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: event,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if result == 0 {
            return None;
        }
        if poll_fd.revents & libc::POLLNVAL != 0 {
            return None;
        }
        if poll_fd.revents & (event | libc::POLLERR | libc::POLLHUP) != 0 {
            return Some(());
        }
    }
}

fn parse_search_ids(response: &[u8]) -> Option<Vec<String>> {
    let response: serde_json::Value = serde_json::from_slice(response).ok()?;
    if response.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0")
        || response.get("id").and_then(serde_json::Value::as_u64) != Some(REQUEST_ID)
        || response.get("error").is_some()
    {
        return None;
    }

    let result = response.get("result")?;
    if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    let text = result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(serde_json::Value::as_str)?;
    let payload: serde_json::Value = serde_json::from_str(text).ok()?;
    if payload
        .get("warming_up")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return None;
    }

    let results = crate::mcp::compact::expand_compact_value(payload.get("results")?);
    let rows = results.as_array()?;
    let ids: Vec<String> = rows
        .iter()
        .map(|row| {
            row.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Option<Vec<_>>>()?;
    (!ids.is_empty()).then_some(ids)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    fn search_response(payload: serde_json::Value) -> Vec<u8> {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string(&payload).unwrap()
                }]
            }
        });
        let mut bytes = serde_json::to_vec(&response).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[cfg(unix)]
    fn run_socket_fixture(response: Option<Vec<u8>>) -> Option<Vec<String>> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::thread;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut request_line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request_line)
                .unwrap();
            let mut stream = stream;
            if let Some(response) = response {
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });

        let ids = super::search_ids(
            &socket_path,
            "fixture query",
            5,
            Instant::now() + Duration::from_secs(2),
        );
        server.join().unwrap();
        ids
    }

    #[cfg(unix)]
    #[test]
    fn search_sends_escaped_prompt_and_extracts_ordinary_ids() {
        use serde_json::json;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::thread;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let prompt = "quoted \"prompt\"\nwith newline";
        let expected_prompt = prompt.to_owned();

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut request_line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request_line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&request_line).unwrap();
            assert_eq!(request["method"], "tools/call");
            assert_eq!(request["params"]["name"], "search");
            assert_eq!(request["params"]["arguments"]["query"], expected_prompt);
            assert_eq!(request["params"]["arguments"]["limit"], 5);

            let response = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string(&json!({
                            "results": [
                                {"id": "first"},
                                {"id": "second"}
                            ]
                        })).unwrap()
                    }]
                }
            });
            let mut stream = stream;
            writeln!(stream, "{response}").unwrap();
        });

        let ids = super::search_ids(
            &socket_path,
            prompt,
            5,
            Instant::now() + Duration::from_secs(1),
        );

        let server_result = server.join();
        assert!(
            server_result.is_ok(),
            "server fixture panicked: {server_result:?}"
        );
        assert_eq!(ids, Some(vec!["first".to_string(), "second".to_string()]));
    }

    #[cfg(unix)]
    #[test]
    fn search_extracts_ordered_ids_from_compact_results() {
        let response = search_response(serde_json::json!({
            "results": {
                "__compact_v1": {
                    "columns": {
                        "score": [0.9, 0.8, 0.7],
                        "id": ["compact-first", "compact-second", "compact-third"]
                    }
                }
            }
        }));

        assert_eq!(
            run_socket_fixture(Some(response)),
            Some(vec![
                "compact-first".to_string(),
                "compact-second".to_string(),
                "compact-third".to_string()
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn absent_and_regular_file_socket_paths_are_misses() {
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.sock");
        assert_eq!(
            super::search_ids(&absent, "query", 5, Instant::now() + Duration::from_secs(1)),
            None
        );

        let regular_file = dir.path().join("regular.sock");
        std::fs::write(&regular_file, b"not a socket").unwrap();
        assert_eq!(
            super::search_ids(
                &regular_file,
                "query",
                5,
                Instant::now() + Duration::from_secs(1)
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn malformed_error_warming_and_empty_replies_are_misses() {
        let malformed = b"not-json\n".to_vec();
        assert_eq!(run_socket_fixture(Some(malformed)), None);

        let mut rpc_error = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32603, "message": "search failed"}
        }))
        .unwrap();
        rpc_error.push(b'\n');
        assert_eq!(run_socket_fixture(Some(rpc_error)), None);

        let warming = search_response(serde_json::json!({
            "warming_up": true,
            "message": "not ready",
            "results": []
        }));
        assert_eq!(run_socket_fixture(Some(warming)), None);

        let empty_results = search_response(serde_json::json!({"results": []}));
        assert_eq!(run_socket_fixture(Some(empty_results)), None);
        let malformed_row = search_response(serde_json::json!({
            "results": [{"id": "valid"}, {"score": 0.1}]
        }));
        assert_eq!(run_socket_fixture(Some(malformed_row)), None);
        assert_eq!(run_socket_fixture(None), None);
    }

    #[cfg(unix)]
    #[test]
    fn oversized_unterminated_reply_is_rejected_before_json_parsing() {
        let oversized = vec![b'x'; super::MAX_RESPONSE_BYTES + 1];
        assert_ne!(oversized.last(), Some(&b'\n'));
        assert_eq!(run_socket_fixture(Some(oversized)), None);
    }
}
