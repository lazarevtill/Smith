//! Actual bounded native HTTP transport, with an owned loopback peer only.
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
fn wav() -> Vec<u8> {
    let n = 60 * 32000u32;
    let mut b = b"RIFF".to_vec();
    b.extend((n + 36).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(16000u32.to_le_bytes());
    b.extend(32000u32.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(n.to_le_bytes());
    b.resize(n as usize + 44, 0);
    b
}
fn base64(bytes: &[u8]) -> String {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for b in bytes.chunks(3) {
        let a = b[0];
        let c = b.get(1).copied().unwrap_or(0);
        let d = b.get(2).copied().unwrap_or(0);
        s.push(table[(a >> 2) as usize] as char);
        s.push(table[(((a & 3) << 4) | (c >> 4)) as usize] as char);
        s.push(if b.len() > 1 {
            table[(((c & 15) << 2) | (d >> 6)) as usize] as char
        } else {
            '='
        });
        s.push(if b.len() > 2 {
            table[(d & 63) as usize] as char
        } else {
            '='
        });
    }
    s
}
#[test]
fn maximum_sixty_second_wav_fits_only_explicit_transcription_route() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let wave = wav();
    assert_eq!(wave.len(), 1920044);
    let body = json!({"review_id":"00000000-0000-4000-8000-000000000001","request_id":"00000000-0000-4000-8000-000000000002","name":"sixty-seconds.wav","language":"en","wav_base64":base64(&wave)});
    let expected = body.clone();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
            assert!(head.len() < 8192);
        }
        let text = String::from_utf8(head).unwrap();
        assert!(text.starts_with("POST /api/media/transcription/start HTTP/1.1"));
        let length: usize = text
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .map(str::parse)
            })
            .unwrap()
            .unwrap();
        assert!(length > 2 * 1024 * 1024 && length < 3 * 1024 * 1024);
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            expected
        );
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"state\":\"running\"}").unwrap();
    });
    let value = http::json(
        address,
        "owned-fixture",
        "POST",
        "/api/media/transcription/start",
        Some("synthetic-session"),
        Some(body.clone()),
    )
    .unwrap();
    assert_eq!(value["state"], "running");
    worker.join().unwrap();
    assert!(http::json(
        address,
        "owned-fixture",
        "POST",
        "/api/media/provider/execute",
        Some("synthetic-session"),
        Some(body)
    )
    .unwrap_err()
    .contains("limit"));
}

#[test]
fn only_typed_authoritative_start_rejections_discard_recovery_identity() {
    for (status, code, not_submitted) in [
        (415, "transcription_audio_unsupported", true),
        (415, "transcription_language_unsupported", true),
        (409, "transcription_busy", true),
        (500, "transcription_invalid", false),
        (400, "unrecognized_error", false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            let mut body = [0; 2];
            stream.read_exact(&mut body).unwrap();
            let reply =
                serde_json::to_vec(&json!({"code":code,"error":"Synthetic rejected input"}))
                    .unwrap();
            write!(
                stream,
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            )
            .unwrap();
            stream.write_all(&reply).unwrap();
        });
        let error = http::transcription_start(address, "owned", "session", json!({})).unwrap_err();
        assert_eq!(
            matches!(error, http::TranscriptionStartError::NotSubmitted(_)),
            not_submitted
        );
        server.join().unwrap();
    }
}

#[test]
fn maximum_supported_stereo_clip_uses_only_normalization_twelve_mib_route() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let n = 60 * 32000 * 4u32;
    let mut wave = b"RIFF".to_vec();
    wave.extend((n + 36).to_le_bytes());
    wave.extend(b"WAVEfmt ");
    wave.extend(16u32.to_le_bytes());
    wave.extend(1u16.to_le_bytes());
    wave.extend(2u16.to_le_bytes());
    wave.extend(32000u32.to_le_bytes());
    wave.extend(128000u32.to_le_bytes());
    wave.extend(4u16.to_le_bytes());
    wave.extend(16u16.to_le_bytes());
    wave.extend(b"data");
    wave.extend(n.to_le_bytes());
    wave.resize(n as usize + 44, 0);
    let body = json!({"review_id":"00000000-0000-4000-8000-000000000001","request_id":"00000000-0000-4000-8000-000000000002","name":"stereo-sixty.wav","language":"en","audio_base64":base64(&wave)});
    let expected = serde_json::to_vec(&body).unwrap();
    assert!(expected.len() > 3 * 1024 * 1024 && expected.len() < 12 * 1024 * 1024);
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
            assert!(head.len() < 8192);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("POST /api/media/transcription/start-file HTTP/1.1"));
        let length: usize = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .map(str::parse)
            })
            .unwrap()
            .unwrap();
        assert_eq!(length, expected.len());
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"state\":\"running\"}").unwrap();
    });
    assert!(http::transcription_start(address, "owned-fixture", "session", body.clone()).is_err());
    assert_eq!(
        http::transcription_file_start(address, "owned-fixture", "session", body).unwrap()["state"],
        "running"
    );
    worker.join().unwrap();
}
