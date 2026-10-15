//! Actual bounded HTTP parsing against a newly owned loopback server.
mod bridge {
    pub type Result<T> = std::result::Result<T, String>;
}
#[allow(dead_code)]
#[path = "../src/http.rs"]
mod http;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::Duration,
};
fn response(
    path: &str,
    code: u16,
    body: &str,
) -> std::result::Result<serde_json::Value, http::CatalogMutationError> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = body.to_owned();
    let request_path = path.to_owned();
    let worker = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut one = [0];
            socket.read_exact(&mut one).unwrap();
            headers.push(one[0]);
            assert!(headers.len() < 8192);
        }
        let text = String::from_utf8(headers).unwrap();
        assert!(text.starts_with(&format!("POST {request_path} HTTP/1.1")));
        let count: usize = text
            .lines()
            .find_map(|line| {
                line.to_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap();
        let mut request = vec![0; count];
        socket.read_exact(&mut request).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request).unwrap(),
            json!({"synthetic":true})
        );
        write!(
            socket,
            "HTTP/1.1 {code} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let result = http::catalog_mutation(
        address,
        "owned-fixture",
        "POST",
        path,
        "synthetic-session",
        json!({"synthetic":true}),
    );
    worker.join().unwrap();
    result
}
#[test]
fn authoritative_initial_rejection_is_distinct_from_lost_or_malformed_outcome() {
    assert!(matches!(
        response(
            "/api/local-catalog/select",
            409,
            r#"{"error":"stale catalog"}"#
        ),
        Err(http::CatalogMutationError::NotSubmitted(_))
    ));
    for (code, body) in [
        (500, r#"{"error":"uncertain"}"#),
        (409, "invalid JSON"),
        (409, r#"{"error":null}"#),
    ] {
        assert!(matches!(
            response("/api/local-catalog/select", code, body),
            Err(http::CatalogMutationError::OutcomeUnknown(_))
        ));
    }
    assert_eq!(
        response(
            "/api/local-catalog/select",
            202,
            r#"{"operation_id":"fixture"}"#
        )
        .unwrap()["operation_id"],
        "fixture"
    );
}
#[test]
fn classifier_is_limited_to_exact_catalog_mutation_routes() {
    let address = "127.0.0.1:1".parse().unwrap();
    for (method, path) in [
        ("POST", "/api/local-catalog/setup/unreviewed"),
        ("GET", "/api/local-catalog/select"),
        ("POST", "/api/local-catalog/entries/../rename"),
        (
            "DELETE",
            "/api/local-catalog/entries/local-0000000000000001?x=1",
        ),
    ] {
        assert!(matches!(
            http::catalog_mutation(address, "owner", method, path, "session", json!({})),
            Err(http::CatalogMutationError::NotSubmitted(_))
        ));
    }
}

#[test]
fn prepare_missing_inventory_400_is_known_before_admission() {
    assert!(matches!(
        response(
            "/api/local-catalog/prepare",
            400,
            r#"{"error":"Observe this computer first."}"#
        ),
        Err(http::CatalogMutationError::NotSubmitted(_))
    ));
    assert!(matches!(
        response(
            "/api/local-catalog/prepare",
            500,
            r#"{"error":"uncertain"}"#
        ),
        Err(http::CatalogMutationError::OutcomeUnknown(_))
    ));
}
