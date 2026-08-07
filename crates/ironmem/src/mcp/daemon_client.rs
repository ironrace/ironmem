//! Synchronous, bounded client for the already-running MCP daemon.
//!
//! Prompt-hook recall consumes this internal API; the transport stays
//! self-contained here so it cannot initialize, spawn, or load an embedder.
// Only the non-Unix stub leaves items unreferenced; on Unix the dead-code lint
// must stay live so unreachable parsing/validation helpers are caught.
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::Path;
use std::time::{Duration, Instant};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const REQUEST_ID: u64 = 1;

#[cfg(unix)]
#[derive(serde::Serialize)]
struct SearchRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: SearchParams<'a>,
}

#[cfg(unix)]
#[derive(serde::Serialize)]
struct SearchParams<'a> {
    name: &'static str,
    arguments: SearchArguments<'a>,
}

#[cfg(unix)]
#[derive(serde::Serialize)]
struct SearchArguments<'a> {
    query: &'a str,
    limit: usize,
}

#[cfg(unix)]
struct BoundedRequest {
    bytes: Vec<u8>,
}

#[cfg(unix)]
impl BoundedRequest {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(MAX_REQUEST_BYTES),
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(unix)]
impl std::io::Write for BoundedRequest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let remaining = MAX_REQUEST_BYTES.saturating_sub(self.bytes.len());
        if bytes.len() > remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "daemon search request exceeds the size limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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

    let metadata = fs::symlink_metadata(socket_path).ok()?;
    if !metadata.file_type().is_socket() {
        return None;
    }

    let mut stream = connect_with_deadline(socket_path, deadline)?;
    let mut request = BoundedRequest::new();
    serde_json::to_writer(
        &mut request,
        &SearchRequest {
            jsonrpc: "2.0",
            id: REQUEST_ID,
            method: "tools/call",
            params: SearchParams {
                name: "search",
                arguments: SearchArguments { query, limit },
            },
        },
    )
    .ok()?;
    std::io::Write::write_all(&mut request, b"\n").ok()?;
    let request = request.into_bytes();

    write_with_deadline(&mut stream, &request, deadline)?;
    let response = read_bounded_line(&mut stream, deadline)?;
    parse_search_ids(&response, limit)
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
fn connect_with_deadline(
    socket_path: &Path,
    deadline: Instant,
) -> Option<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let (address, address_len) = unix_socket_address(socket_path)?;
    remaining(deadline)?;

    let raw_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw_fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let original_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if original_flags < 0 {
        return None;
    }
    remaining(deadline)?;
    if unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_SETFL,
            original_flags | libc::O_NONBLOCK,
        )
    } < 0
    {
        return None;
    }

    remaining(deadline)?;
    let connect_result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &address as *const libc::sockaddr_un as *const libc::sockaddr,
            address_len,
        )
    };
    if connect_result < 0 {
        let error = std::io::Error::last_os_error();
        let code = error.raw_os_error()?;
        if !is_connect_pending(code) && code != libc::EINTR && code != libc::EISCONN {
            return None;
        }
        if code != libc::EISCONN {
            loop {
                wait_for_fd(fd.as_raw_fd(), libc::POLLOUT, deadline)?;
                remaining(deadline)?;
                let connect_error = socket_error(fd.as_raw_fd())?;
                if connect_error == 0 || connect_error == libc::EISCONN {
                    break;
                }
                if is_connect_pending(connect_error) || connect_error == libc::EINTR {
                    continue;
                }
                return None;
            }
        }
    }

    remaining(deadline)?;
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, original_flags) } < 0 {
        return None;
    }
    Some(std::os::unix::net::UnixStream::from(fd))
}

#[cfg(unix)]
fn unix_socket_address(path: &Path) -> Option<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return None;
    }
    address.sun_family = libc::AF_UNIX as _;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes.iter().copied()) {
        *slot = byte as _;
    }
    let address_len =
        (std::mem::size_of_val(&address.sun_family) + bytes.len() + 1) as libc::socklen_t;
    Some((address, address_len))
}

/// Whether a non-blocking `connect` result means "still connecting".
///
/// `EAGAIN`/`EWOULDBLOCK` are deliberately excluded. For `AF_UNIX` on Linux
/// they mean the listener's accept backlog is full — the socket is *not*
/// queued for connection — and the kernel then reports `POLLOUT|POLLHUP` with
/// `SO_ERROR == 0` on that unconnected fd. Treating them as pending therefore
/// yields a `UnixStream` that is not connected (the following `write` fails
/// with `ENOTCONN`) and spins the retry loop for the rest of the budget. A
/// saturated daemon is an "unavailable peer, fall back to BM25" signal, so it
/// fails closed here instead.
#[cfg(unix)]
fn is_connect_pending(code: libc::c_int) -> bool {
    code == libc::EINPROGRESS || code == libc::EALREADY
}

#[cfg(unix)]
fn socket_error(fd: std::os::fd::RawFd) -> Option<libc::c_int> {
    let mut error = 0 as libc::c_int;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut error as *mut libc::c_int as *mut libc::c_void,
            &mut length,
        )
    };
    (result == 0).then_some(error)
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

    wait_for_fd(stream.as_raw_fd(), event, deadline)
}

#[cfg(unix)]
fn wait_for_fd(fd: std::os::fd::RawFd, event: i16, deadline: Instant) -> Option<()> {
    loop {
        let timeout = remaining(deadline)?;
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd,
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

fn parse_search_ids(response: &[u8], limit: usize) -> Option<Vec<String>> {
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

    let results = validated_search_results(payload.get("results")?, limit)?;
    let rows = results.as_array()?;
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::with_capacity(limit.min(rows.len()));
    for row in rows {
        let id = row
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)?;
        if ids.len() < limit && seen.insert(id.clone()) {
            ids.push(id);
        }
    }
    (!ids.is_empty()).then_some(ids)
}

/// Validate a `results` payload before it is expanded, and reject anything the
/// request did not ask for.
///
/// The `MAX_RESPONSE_BYTES` frame cap bounds the *wire* size, not the expanded
/// size: `expand_compact_value` materialises one object per row and clones
/// every column key into each one, so a well-formed 1 MiB envelope carrying a
/// single ~500 KB column key over ~260k rows expands to well over 100 GB. The
/// row-count checks below are what make the frame cap an allocation bound — the
/// client asked for `limit` (at most 10) rows, so a peer returning more is out
/// of contract and treated as no vector result.
fn validated_search_results(value: &serde_json::Value, limit: usize) -> Option<serde_json::Value> {
    let Some(envelope) = value.get("__compact_v1") else {
        if value.as_array().is_some_and(|rows| rows.len() > limit) {
            return None;
        }
        return Some(value.clone());
    };
    let columns = envelope.get("columns")?.as_object()?;
    if columns.is_empty() {
        return None;
    }

    let mut row_count = None;
    for column in columns.values() {
        let length = column.as_array()?.len();
        if row_count.is_some_and(|expected| expected != length) {
            return None;
        }
        row_count = Some(length);
    }
    if row_count.is_some_and(|rows| rows > limit) {
        return None;
    }

    Some(crate::mcp::compact::expand_compact_value(value))
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
        run_socket_fixture_with_limit(response, 5)
    }

    #[cfg(unix)]
    fn run_socket_fixture_with_limit(
        response: Option<Vec<u8>>,
        limit: usize,
    ) -> Option<Vec<String>> {
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
            limit,
            Instant::now() + Duration::from_secs(2),
        );
        server.join().unwrap();
        ids
    }

    #[cfg(unix)]
    fn run_socket_fixture_observing_request(query: &str) -> (String, Option<Vec<String>>) {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc::channel;
        use std::thread;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (request_tx, request_rx) = channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut request_line = String::new();
            BufReader::new(stream).read_line(&mut request_line).unwrap();
            request_tx.send(request_line).unwrap();
        });

        let ids = super::search_ids(
            &socket_path,
            query,
            5,
            Instant::now() + Duration::from_secs(2),
        );
        server.join().unwrap();
        (request_rx.recv().unwrap(), ids)
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
    fn oversized_serialized_request_is_rejected_before_write() {
        let oversized_query = "x".repeat(super::MAX_REQUEST_BYTES);
        let (request_line, ids) = run_socket_fixture_observing_request(&oversized_query);

        assert!(request_line.is_empty());
        assert_eq!(ids, None);
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_ids_are_ordered_and_deduplicated() {
        let response = search_response(serde_json::json!({
            "results": [
                {"id": "first"},
                {"id": "first"},
                {"id": "second"},
                {"id": "third"}
            ]
        }));

        assert_eq!(
            run_socket_fixture_with_limit(Some(response), 4),
            Some(vec![
                "first".to_owned(),
                "second".to_owned(),
                "third".to_owned()
            ])
        );
    }

    /// Ordinary rows must obey the same request bound as compact rows before
    /// they are cloned for parsing.
    #[cfg(unix)]
    #[test]
    fn ordinary_row_count_above_the_requested_limit_is_rejected() {
        let response = search_response(serde_json::json!({
            "results": [
                {"id": "first"},
                {"id": "second"},
                {"id": "third"}
            ]
        }));

        assert_eq!(run_socket_fixture_with_limit(Some(response), 2), None);
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

    /// A saturated listener must fail closed, promptly, on every Unix.
    ///
    /// The platforms disagree on how they say so — macOS/BSD refuse the
    /// connect, Linux returns `EAGAIN` on an fd that then polls
    /// `POLLOUT|POLLHUP` with `SO_ERROR == 0` — so the deadline bound below is
    /// what pins the shared contract: neither answer may be mistaken for a
    /// connect in progress and waited on.
    #[cfg(unix)]
    #[test]
    fn connect_to_a_full_listener_fails_closed_within_the_absolute_deadline() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::RawFd;
        use std::path::Path;
        use std::time::{Duration, Instant};

        struct FdGuard(RawFd);
        impl Drop for FdGuard {
            fn drop(&mut self) {
                unsafe {
                    libc::close(self.0);
                }
            }
        }

        fn socket_address(path: &Path) -> Option<(libc::sockaddr_un, libc::socklen_t)> {
            let bytes = path.as_os_str().as_bytes();
            let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            if bytes.is_empty() || bytes.len() >= address.sun_path.len() {
                return None;
            }
            address.sun_family = libc::AF_UNIX as _;
            for (slot, byte) in address.sun_path.iter_mut().zip(bytes.iter().copied()) {
                *slot = byte as _;
            }
            let address_len =
                (std::mem::size_of_val(&address.sun_family) + bytes.len() + 1) as libc::socklen_t;
            Some((address, address_len))
        }

        fn bind_small_listener(path: &Path) -> Option<FdGuard> {
            let (address, address_len) = socket_address(path)?;
            let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                return None;
            }
            let guard = FdGuard(fd);
            let bound = unsafe {
                libc::bind(
                    fd,
                    &address as *const libc::sockaddr_un as *const libc::sockaddr,
                    address_len,
                )
            } == 0;
            let listening = bound && unsafe { libc::listen(fd, 1) } == 0;
            listening.then_some(guard)
        }

        fn fill_listener_backlog(path: &Path) -> Vec<FdGuard> {
            let Some((address, address_len)) = socket_address(path) else {
                return Vec::new();
            };
            let mut clients = Vec::new();
            for _ in 0..32 {
                let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
                if fd < 0 {
                    break;
                }
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                if flags < 0
                    || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
                {
                    unsafe {
                        libc::close(fd);
                    }
                    break;
                }
                let result = unsafe {
                    libc::connect(
                        fd,
                        &address as *const libc::sockaddr_un as *const libc::sockaddr,
                        address_len,
                    )
                };
                let accepted = result == 0
                    || matches!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(code)
                            if code == libc::EINPROGRESS
                                || code == libc::EALREADY
                                || code == libc::EINTR
                    );
                if accepted {
                    clients.push(FdGuard(fd));
                } else {
                    unsafe {
                        libc::close(fd);
                    }
                    break;
                }
            }
            clients
        }

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("backlog.sock");
        let Some(_listener) = bind_small_listener(&socket_path) else {
            panic!("test platform could not bind a Unix listener");
        };
        let clients = fill_listener_backlog(&socket_path);
        assert!(
            !clients.is_empty(),
            "fixture failed to fill the listener backlog: {} clients",
            clients.len()
        );

        let deadline = Instant::now() + Duration::from_millis(75);
        let started = Instant::now();
        let result = super::connect_with_deadline(&socket_path, deadline);
        let elapsed = started.elapsed();

        assert!(
            result.is_none(),
            "a full listener must not produce a stream"
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "connect exceeded its absolute deadline: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn malformed_compact_columns_are_misses() {
        let mismatched_rows = search_response(serde_json::json!({
            "results": {
                "__compact_v1": {
                    "columns": {
                        "id": ["first", "second"],
                        "score": [0.9]
                    }
                }
            }
        }));
        assert_eq!(run_socket_fixture(Some(mismatched_rows)), None);

        let non_array_column = search_response(serde_json::json!({
            "results": {
                "__compact_v1": {
                    "columns": {
                        "id": ["first", "second"],
                        "score": "not-an-array"
                    }
                }
            }
        }));
        assert_eq!(run_socket_fixture(Some(non_array_column)), None);
    }

    /// The 1 MiB frame cap bounds the wire size, not the expanded size:
    /// expanding a compact envelope clones every column key once per row, so a
    /// payload that fits the cap can still demand orders of magnitude more
    /// memory. A row count above the requested limit is out of contract and
    /// must be rejected before expansion rather than after it.
    #[cfg(unix)]
    #[test]
    fn compact_row_count_above_the_requested_limit_is_rejected_before_expansion() {
        let wide_key = "k".repeat(4096);
        let rows: Vec<serde_json::Value> = (0..64).map(|_| serde_json::json!(0)).collect();
        let amplifying = search_response(serde_json::json!({
            "results": {
                "__compact_v1": {
                    "columns": {
                        wide_key: rows,
                    }
                }
            }
        }));
        assert_eq!(run_socket_fixture_with_limit(Some(amplifying), 5), None);

        // A compact envelope inside the requested limit still expands normally.
        let within_limit = search_response(serde_json::json!({
            "results": {
                "__compact_v1": {
                    "columns": {
                        "id": ["bounded-first", "bounded-second"]
                    }
                }
            }
        }));
        assert_eq!(
            run_socket_fixture_with_limit(Some(within_limit), 5),
            Some(vec![
                "bounded-first".to_string(),
                "bounded-second".to_string()
            ])
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

    /// A daemon that writes only a response prefix must not keep prompt-hook
    /// recall blocked past its absolute deadline.
    #[cfg(unix)]
    #[test]
    fn partial_response_that_stalls_respects_the_absolute_deadline() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc::channel;
        use std::thread;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (release_tx, release_rx) = channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request_line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request_line)
                .unwrap();
            stream.write_all(b"{\"jsonrpc\":\"2.0\"").unwrap();
            stream.flush().unwrap();
            release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });

        let deadline = Instant::now() + Duration::from_millis(75);
        let started = Instant::now();
        let ids = super::search_ids(&socket_path, "fixture query", 5, deadline);
        let elapsed = started.elapsed();

        assert_eq!(ids, None);
        assert!(
            elapsed < Duration::from_millis(200),
            "stalled response exceeded its absolute deadline: {elapsed:?}"
        );
        release_tx.send(()).unwrap();
        server.join().unwrap();
    }
}
