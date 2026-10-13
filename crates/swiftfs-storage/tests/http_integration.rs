//! Integration test for the HTTP byte-range backend against a local mock server.
//!
//! The mock server is a minimal HTTP/1.1 server implemented directly on a
//! `TcpListener`, so the test has no external dependencies and no network
//! access beyond loopback. It serves a fixed map of object -> bytes and honors
//! a single `Range: bytes=start-end` request.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

use swiftfs_storage::{ChunkLocation, ChunkStore, HttpStore, HttpStoreConfig};

/// Spawn a mock object server on an ephemeral port; returns (base_url, join).
fn spawn_mock(objects: HashMap<String, Vec<u8>>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let objects = Arc::new(objects);

    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };
            let objects = Arc::clone(&objects);

            // Serve requests until the client closes the connection.
            loop {
                // Read the request head (until CRLFCRLF).
                let mut buf = [0u8; 1024];
                let mut head = Vec::new();
                loop {
                    let n = match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                if head.is_empty() {
                    break; // connection closed
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let request_line = head.lines().next().unwrap_or("");
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                let name = path.trim_start_matches('/');
                let range = head.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.eq_ignore_ascii_case("range") {
                        Some(v.trim().to_string())
                    } else {
                        None
                    }
                });

                let Some(data) = objects.get(name) else {
                    let _ =
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    break;
                };

                let (status, body, extra) = if let Some(r) = range {
                    let r = r.trim_start_matches("bytes=");
                    let (s, e) = r.split_once('-').unwrap_or((r, ""));
                    let start: usize = s.parse().unwrap_or(0);
                    let end: usize = if e.is_empty() {
                        data.len() - 1
                    } else {
                        e.parse().unwrap_or(data.len() - 1)
                    };
                    let end = end.min(data.len() - 1);
                    let body = data[start..=end].to_vec();
                    (
                        "206 Partial Content",
                        body,
                        format!("Content-Range: bytes {start}-{end}/{}\r\n", data.len()),
                    )
                } else {
                    ("200 OK", data.clone(), String::new())
                };

                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        }
    });

    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn http_store_fetches_full_and_ranged_bytes() {
    let mut objects = HashMap::new();
    objects.insert("obj0".to_string(), (0u8..=255).collect::<Vec<u8>>());
    let (base_url, _server) = spawn_mock(objects);

    let mut config = HttpStoreConfig::new(base_url);
    config.max_attempts = 1;
    let store = HttpStore::new(config).unwrap();

    // Whole object (0..256): the mock returns 200 OK with the full body.
    let whole = store
        .fetch(&ChunkLocation {
            object: "obj0".into(),
            offset: 0,
            length: 256,
        })
        .await
        .unwrap();
    assert_eq!(whole, (0u8..=255).collect::<Vec<u8>>());

    // A sub-range: the mock returns 206 with exactly the requested bytes.
    let ranged = store
        .fetch(&ChunkLocation {
            object: "obj0".into(),
            offset: 10,
            length: 6,
        })
        .await
        .unwrap();
    assert_eq!(ranged, vec![10, 11, 12, 13, 14, 15]);
}

#[tokio::test]
async fn http_store_returns_error_for_missing_object() {
    let (base_url, _server) = spawn_mock(HashMap::new());
    let mut config = HttpStoreConfig::new(base_url);
    config.max_attempts = 1;
    let store = HttpStore::new(config).unwrap();

    let err = store
        .fetch(&ChunkLocation {
            object: "nope".into(),
            offset: 0,
            length: 4,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, swiftfs_core::SwiftfsError::ChunkNotFound(_)));
}
