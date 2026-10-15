use smith_media::{
    inspect, sha256, AttachmentRequest, Content, ErrorKind, Format, Limits, Manifest,
};

fn parse(name: &str, bytes: &[u8]) -> Manifest {
    inspect(
        AttachmentRequest::new(name, None),
        bytes,
        &Limits::default(),
    )
    .unwrap()
}
fn fail(name: &str, mime: Option<&str>, bytes: &[u8], kind: ErrorKind) {
    assert_eq!(
        inspect(
            AttachmentRequest::new(name, mime),
            bytes,
            &Limits::default()
        )
        .unwrap_err()
        .kind,
        kind
    );
}
fn context(m: Manifest) -> String {
    match m.content {
        Content::Document { context, .. } => context,
        _ => panic!("expected document"),
    }
}

#[test]
fn unicode_and_line_endings_survive_exactly() {
    let bytes = include_bytes!("fixtures/unicode.txt");
    assert_eq!(context(parse("notes.txt", bytes)).as_bytes(), bytes);
    let bytes = b"\xef\xbb\xbfFirst\r\nSecond\tcolumn\r\n";
    let m = parse("notes.txt", bytes);
    assert_eq!(m.byte_len, bytes.len() as u64);
    assert_eq!(m.sha256, sha256(bytes));
    assert_eq!(context(m), "First\r\nSecond\tcolumn\r\n");
}

#[test]
fn sha256_has_known_content_identity() {
    assert_eq!(
        sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(parse("a.txt", b"abc").sha256, parse("b.md", b"abc").sha256);
    assert_ne!(
        parse("a.txt", b"abc").sha256,
        parse("a.txt", b"abcd").sha256
    );
}

#[test]
fn metadata_and_declared_mime_are_preserved() {
    let mut request = AttachmentRequest::new("Notes.TXT", Some("Text/Plain; charset=\"UTF-8\""));
    request
        .metadata
        .insert("title".into(), "Résumé 世界".into());
    request.metadata.insert(
        "source".into(),
        "https://example.invalid/never-fetched".into(),
    );
    let result = inspect(request.clone(), b"local bytes", &Limits::default()).unwrap();
    assert_eq!(result.request, request);
    assert_eq!(result.detected_mime, "text/plain");
    assert_eq!(result.format, Format::Text);
}

#[test]
fn scalar_context_budget_does_not_split_utf8() {
    let limits = Limits {
        max_context_chars: 3,
        ..Limits::default()
    };
    let m = inspect(
        AttachmentRequest::new("x.txt", None),
        "a🌎界z".as_bytes(),
        &limits,
    )
    .unwrap();
    assert_eq!(
        m.content,
        Content::Document {
            context: "a🌎界".into(),
            truncated: true,
            total_context_chars: 4,
            rows: None,
            columns: None
        }
    );
    let m = inspect(
        AttachmentRequest::new("x.txt", None),
        "a🌎界".as_bytes(),
        &limits,
    )
    .unwrap();
    assert!(matches!(
        m.content,
        Content::Document {
            truncated: false,
            ..
        }
    ));
}

#[test]
fn rejects_unicode_errors_and_forbidden_controls() {
    for input in [
        b"bad\xff".as_slice(),
        b"bad\xc0\xaf",
        b"\xff\xfea\0",
        b"a\0b",
        b"a\x1b[31m",
        b"a\x7fb",
        "a\u{0085}b".as_bytes(),
        "a\u{202e}b".as_bytes(),
    ] {
        fail("bad.txt", None, input, ErrorKind::InvalidContent);
    }
}

#[test]
fn unknown_formats_and_binary_disguises_fail_closed() {
    for name in [
        "file.docx",
        "file.svg",
        "file.tiff",
        "file.wav",
        "file.mp3",
        "file.mp4",
        "file.exe",
        "file.unknown",
    ] {
        fail(
            name,
            Some("text/plain"),
            b"apparently harmless",
            ErrorKind::UnsupportedFormat,
        );
    }
    for name in ["file.pdf", "file.xls"] {
        fail(
            name,
            Some("text/plain"),
            b"apparently harmless",
            ErrorKind::MimeMismatch,
        );
    }
    fail(
        "file.pdf",
        None,
        b"%PDF-1.7 arbitrary external PDF",
        ErrorKind::UnsupportedFormat,
    );
    for input in [
        b"%PDF-1.7".as_slice(),
        b"PK\x03\x04zip",
        b"GIF89a",
        b"RIFFxxxxWAVE",
        b"\x7fELF",
    ] {
        fail("renamed.txt", None, input, ErrorKind::InvalidContent);
    }
    fail("README", None, b"text", ErrorKind::UnsupportedFormat);
    assert_eq!(
        inspect(
            AttachmentRequest::new("README", Some("text/plain")),
            b"text",
            &Limits::default()
        )
        .unwrap()
        .format,
        Format::Text
    );
}

#[test]
fn mime_extension_and_signature_must_agree() {
    fail(
        "x.csv",
        Some("application/json"),
        b"a,b",
        ErrorKind::MimeMismatch,
    );
    fail(
        "x.txt",
        Some("application/octet-stream"),
        b"text",
        ErrorKind::MimeMismatch,
    );
    fail(
        "x.png",
        Some("image/png"),
        b"plain text",
        ErrorKind::MimeMismatch,
    );
    fail(
        "x.jpg",
        Some("image/jpeg"),
        include_bytes!("fixtures/pixels.png"),
        ErrorKind::MimeMismatch,
    );
    fail(
        "x.txt",
        Some("text/plain; charset=iso-8859-1"),
        b"text",
        ErrorKind::InvalidRequest,
    );
    fail(
        "x.txt",
        Some("text/plain\r\nX-Header: bad"),
        b"text",
        ErrorKind::InvalidRequest,
    );
    fail(
        "x.png",
        Some("image/png; charset=utf-8"),
        include_bytes!("fixtures/pixels.png"),
        ErrorKind::InvalidRequest,
    );
}

#[test]
fn rejects_unsafe_or_oversize_display_metadata() {
    for name in [
        "",
        "../x.txt",
        "a/b.txt",
        "a\\b.txt",
        "C:x.txt",
        "x\n.txt",
        "x\u{202e}.txt",
        " x.txt",
        "x.txt ",
        ".",
        "..",
    ] {
        fail(name, None, b"hello", ErrorKind::InvalidRequest);
    }
    let mut request = AttachmentRequest::new("ok.txt", None);
    request
        .metadata
        .insert("key".into(), "secret\u{001b}value".into());
    assert_eq!(
        inspect(request, b"x", &Limits::default()).unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    let mut request = AttachmentRequest::new("ok.txt", None);
    request.metadata = (0..33).map(|i| (i.to_string(), "x".to_owned())).collect();
    assert_eq!(
        inspect(request, b"x", &Limits::default()).unwrap_err().kind,
        ErrorKind::LimitExceeded
    );
}

#[test]
fn csv_preserves_cells_quotes_newlines_empty_and_formula_text() {
    let m = parse("table.csv", include_bytes!("fixtures/quoted.csv"));
    assert!(matches!(
        &m.content,
        Content::Document {
            rows: Some(3),
            columns: Some(3),
            truncated: false,
            ..
        }
    ));
    let rows: Vec<Vec<String>> = serde_json::from_str(&context(m)).unwrap();
    assert_eq!(rows[1], ["Ada, A.", "said \"hello\"", ""]);
    assert_eq!(rows[2], ["Grace", "line one\nline two", "=SUM(A1:A2)"]);
    let m = parse("table.tsv", "a\tb\r\n\"世界\"\t\"x\ty\"\r\n".as_bytes());
    let rows: Vec<Vec<String>> = serde_json::from_str(&context(m)).unwrap();
    assert_eq!(rows[1], ["世界", "x\ty"]);
}

#[test]
fn csv_empty_and_trailing_cells_have_unambiguous_semantics() {
    assert_eq!(context(parse("empty.csv", b"")), "[]");
    assert_eq!(context(parse("empty.csv", b"\n")), "[[\"\"]]");
    assert_eq!(context(parse("empty.csv", b",")), "[[\"\",\"\"]]");
    assert_eq!(context(parse("empty.csv", b"\"\"")), "[[\"\"]]");
    assert_eq!(context(parse("empty.csv", b"a,b\r\n")), "[[\"a\",\"b\"]]");
}

#[test]
fn malformed_csv_is_not_silently_repaired() {
    for input in [
        b"\"never closed".as_slice(),
        b"a\"b,c",
        b"\"a\"b,c",
        b"a,b\nx",
        b"a,b\rc,d",
    ] {
        fail("bad.csv", None, input, ErrorKind::InvalidContent);
    }
}

#[test]
fn every_csv_structure_budget_is_enforced() {
    for (limits, input) in [
        (
            Limits {
                max_csv_rows: 1,
                ..Limits::default()
            },
            b"a\nb".as_slice(),
        ),
        (
            Limits {
                max_csv_columns: 1,
                ..Limits::default()
            },
            b"a,b",
        ),
        (
            Limits {
                max_csv_field_bytes: 2,
                ..Limits::default()
            },
            b"abc",
        ),
        (
            Limits {
                max_csv_field_bytes: 2,
                ..Limits::default()
            },
            b"\"abc\"",
        ),
    ] {
        assert_eq!(
            inspect(AttachmentRequest::new("x.csv", None), input, &limits)
                .unwrap_err()
                .kind,
            ErrorKind::LimitExceeded
        );
    }
}

#[test]
fn json_preserves_precision_reserved_keys_and_unicode() {
    let bytes = include_bytes!("fixtures/precise.json");
    assert_eq!(context(parse("data.json", bytes)).as_bytes(), bytes);
    assert_eq!(
        context(parse("scalar.json", b"-1.234567890123456789e+200")),
        "-1.234567890123456789e+200"
    );
}

#[test]
fn json_rejects_malformed_duplicate_keys_and_escaped_controls() {
    for input in [
        r#"{"x":1,"x":2}"#,
        r#"{"x":1,"\u0078":2}"#,
        r#"{"nested":{"x":1,"x":2}}"#,
        r#"{"x":"\u0000"}"#,
        r#"{"\u001b":1}"#,
        r#"{"x":"\ud800"}"#,
        r#"{"x":"\u202e"}"#,
        "{",
        "{} {}",
        "NaN",
        "[1,]",
        "",
        "01",
        "Infinity",
    ] {
        fail(
            "bad.json",
            None,
            input.as_bytes(),
            ErrorKind::InvalidContent,
        );
    }
    parse(
        "ok.json",
        br#"{"x":"a\nb\t","array":[1,2],"other":{"x":2}}"#,
    );
}

#[test]
fn json_depth_and_node_budgets_are_checked_before_materializing_value() {
    let limits = Limits {
        max_json_depth: 2,
        ..Limits::default()
    };
    assert!(inspect(AttachmentRequest::new("x.json", None), b"[[0]]", &limits).is_ok());
    assert_eq!(
        inspect(AttachmentRequest::new("x.json", None), b"[[[0]]]", &limits)
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
    let limits = Limits {
        max_json_nodes: 3,
        ..Limits::default()
    };
    assert_eq!(
        inspect(AttachmentRequest::new("x.json", None), b"[0,1,2]", &limits)
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
}

#[test]
fn bytes_and_limit_configuration_have_hard_ceilings() {
    let limits = Limits {
        max_input_bytes: 3,
        ..Limits::default()
    };
    assert!(inspect(AttachmentRequest::new("x.txt", None), b"abc", &limits).is_ok());
    assert_eq!(
        inspect(AttachmentRequest::new("x.txt", None), b"abcd", &limits)
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
    let limits = Limits {
        max_text_bytes: 2,
        ..Limits::default()
    };
    assert_eq!(
        inspect(AttachmentRequest::new("x.txt", None), b"abc", &limits)
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
    for limits in [
        Limits {
            max_input_bytes: usize::MAX,
            ..Limits::default()
        },
        Limits {
            max_json_depth: 0,
            ..Limits::default()
        },
    ] {
        assert_eq!(
            limits.validate().unwrap_err().kind,
            ErrorKind::InvalidRequest
        );
    }
}

#[test]
fn real_raster_fixtures_decode_with_explicit_vision_requirement() {
    for (name, bytes, frames) in [
        (
            "pixels.png",
            include_bytes!("fixtures/pixels.png").as_slice(),
            1,
        ),
        (
            "pixels.jpg",
            include_bytes!("fixtures/pixels.jpg").as_slice(),
            1,
        ),
        (
            "pixels.bmp",
            include_bytes!("fixtures/pixels.bmp").as_slice(),
            1,
        ),
        (
            "pixels.webp",
            include_bytes!("fixtures/pixels.webp").as_slice(),
            1,
        ),
        (
            "frames.gif",
            include_bytes!("fixtures/two-frames.gif").as_slice(),
            2,
        ),
    ] {
        let m = parse(name, bytes);
        assert_eq!(
            m.content,
            Content::Image {
                width: 3,
                height: 2,
                frames,
                requires_vision: true
            }
        );
        assert_eq!(m.sha256, sha256(bytes));
        assert_eq!(m.byte_len, bytes.len() as u64);
    }
}

#[test]
fn truncated_images_are_not_accepted_as_header_probes() {
    for (name, bytes) in [
        (
            "pixels.png",
            include_bytes!("fixtures/pixels.png").as_slice(),
        ),
        (
            "pixels.jpg",
            include_bytes!("fixtures/pixels.jpg").as_slice(),
        ),
        (
            "pixels.bmp",
            include_bytes!("fixtures/pixels.bmp").as_slice(),
        ),
        (
            "pixels.webp",
            include_bytes!("fixtures/pixels.webp").as_slice(),
        ),
        (
            "frames.gif",
            include_bytes!("fixtures/two-frames.gif").as_slice(),
        ),
    ] {
        fail(
            name,
            None,
            &bytes[..bytes.len() - 1],
            ErrorKind::InvalidContent,
        );
    }
    let mut png = include_bytes!("fixtures/pixels.png").to_vec();
    let idat = png.windows(4).position(|w| w == b"IDAT").unwrap();
    png[idat + 5] ^= 0xff;
    fail("broken.png", None, &png, ErrorKind::InvalidContent);
}

#[test]
fn unsupported_animations_are_reported_instead_of_losing_frames() {
    fail(
        "animated.png",
        None,
        include_bytes!("fixtures/animated.png"),
        ErrorKind::UnsupportedFormat,
    );
    fail(
        "animated.webp",
        None,
        include_bytes!("fixtures/animated.webp"),
        ErrorKind::UnsupportedFormat,
    );
}

#[test]
fn webp_canvas_cannot_hide_oversized_or_mismatched_embedded_frames() {
    fn extended(bitstream: &[u8]) -> Vec<u8> {
        // VP8X says 1x1, no animation. The embedded bitstream is checked separately.
        let mut chunks = b"VP8X\x0a\0\0\0".to_vec();
        chunks.extend([0; 10]);
        chunks.extend(bitstream);
        let mut bytes = b"RIFF".to_vec();
        bytes.extend(((chunks.len() + 4) as u32).to_le_bytes());
        bytes.extend(b"WEBP");
        bytes.extend(chunks);
        bytes
    }
    let mismatched = extended(&include_bytes!("fixtures/pixels.webp")[12..]);
    fail("hidden.webp", None, &mismatched, ErrorKind::InvalidContent);
    // A keyframe header declaring 8193x8193 is rejected before lossy decompression,
    // despite the innocuous outer canvas and tiny compressed input.
    let large_vp8 = b"VP8 \x0a\0\0\0\0\0\0\x9d\x01\x2a\x01\x20\x01\x20";
    fail(
        "hidden.webp",
        None,
        &extended(large_vp8),
        ErrorKind::LimitExceeded,
    );
}

#[test]
fn image_dimensions_pixels_decoded_bytes_and_animation_frames_are_bounded() {
    for limits in [
        Limits {
            max_image_dimension: 2,
            ..Limits::default()
        },
        Limits {
            max_image_pixels: 5,
            ..Limits::default()
        },
        Limits {
            max_decoded_bytes: 8,
            ..Limits::default()
        },
    ] {
        assert_eq!(
            inspect(
                AttachmentRequest::new("x.png", None),
                include_bytes!("fixtures/pixels.png"),
                &limits
            )
            .unwrap_err()
            .kind,
            ErrorKind::LimitExceeded
        );
    }
    for limits in [
        Limits {
            max_frames: 1,
            ..Limits::default()
        },
        Limits {
            max_total_frame_pixels: 6,
            ..Limits::default()
        },
    ] {
        assert_eq!(
            inspect(
                AttachmentRequest::new("x.gif", None),
                include_bytes!("fixtures/two-frames.gif"),
                &limits
            )
            .unwrap_err()
            .kind,
            ErrorKind::LimitExceeded
        );
    }
}
