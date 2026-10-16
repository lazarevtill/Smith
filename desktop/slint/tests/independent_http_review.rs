//! Exercise the actual transport module against owned synthetic loopback peers.
//! No native window, desktop capture, model or real profile is involved.
use smith_native_bridge::historical_message_bridge;
mod bridge {
    pub type Result<T> = std::result::Result<T, String>;
}
#[allow(dead_code)]
#[path = "../src/http.rs"]
mod reviewed_http;

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

fn slow_peer(headers: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let stop = finished.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 4096);
        }
        assert!(request
            .windows(b"synthetic-owner".len())
            .any(|s| s == b"synthetic-owner"));
        if !headers {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 256\r\n\r\n")
                .unwrap();
        }
        let started = Instant::now();
        while !stop.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(8) {
            if stream.write_all(b" ").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    let started = Instant::now();
    let result = reviewed_http::json(
        address,
        "synthetic-owner",
        "GET",
        "/api/fixture",
        None,
        None,
    );
    let elapsed = started.elapsed();
    finished.store(true, Ordering::Release);
    server.join().unwrap();
    assert!(
        result.is_err(),
        "A slow incomplete fixture cannot return success"
    );
    assert!(
        elapsed < Duration::from_millis(6500),
        "The whole-request deadline was bypassed: {elapsed:?}"
    );
}

#[test]
fn continuous_header_bytes_do_not_extend_the_request_deadline() {
    slow_peer(true);
}

#[test]
fn continuous_body_bytes_do_not_extend_the_request_deadline() {
    slow_peer(false);
}

#[test]
fn duplicate_security_headers_are_refused_before_body_adoption() {
    for name in [
        "Content-Type",
        "Cache-Control",
        "X-Content-Type-Options",
        "X-Smith-Content-Sha256",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n{name}: first\r\n{}: second\r\n\r\n{{}}",
            name.to_lowercase()
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 4096);
            }
            stream.write_all(wire.as_bytes()).unwrap();
        });
        let reply = reviewed_http::json(
            address,
            "synthetic-owner",
            "GET",
            "/api/fixture",
            None,
            None,
        );
        server.join().unwrap();
        assert_eq!(reply.unwrap_err(), "Duplicate backend security header.");
    }
}
