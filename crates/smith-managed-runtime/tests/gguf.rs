use smith_managed_runtime::{inspect_gguf, inspect_gguf_reader, GgufFailure};
use std::io::{Cursor, Read};

fn string(bytes: &mut Vec<u8>, text: &str) {
    bytes.extend((text.len() as u64).to_le_bytes());
    bytes.extend(text.as_bytes());
}
fn fixture(fields: &[(&str, Field)]) -> Vec<u8> {
    let mut b = b"GGUF".to_vec();
    b.extend(3u32.to_le_bytes());
    b.extend(7u64.to_le_bytes());
    b.extend((fields.len() as u64).to_le_bytes());
    for (key, value) in fields {
        string(&mut b, key);
        match value {
            Field::Text(text) => {
                b.extend(8u32.to_le_bytes());
                string(&mut b, text);
            }
            Field::Number(n) => {
                b.extend(4u32.to_le_bytes());
                b.extend(n.to_le_bytes());
            }
            Field::Array => {
                b.extend(9u32.to_le_bytes());
                b.extend(4u32.to_le_bytes());
                b.extend(2u64.to_le_bytes());
                b.extend(8u32.to_le_bytes());
                b.extend(16u32.to_le_bytes());
            }
        }
    }
    b
}
enum Field<'a> {
    Text(&'a str),
    Number(u32),
    Array,
}
fn llama() -> Vec<u8> {
    fixture(&[
        ("general.architecture", Field::Text("llama")),
        ("general.name", Field::Text("Synthetic only")),
        ("general.size_label", Field::Text("1B")),
        ("llama.context_length", Field::Number(8192)),
        ("llama.block_count", Field::Number(24)),
        ("llama.embedding_length", Field::Number(2048)),
        ("llama.attention.head_count", Field::Number(16)),
        ("llama.attention.head_count_kv", Field::Number(4)),
    ])
}

#[test]
fn derives_only_conventional_scalar_kv_dimensions_without_reading_tensors() {
    struct OnlyHeader(Cursor<Vec<u8>>);
    impl Read for OnlyHeader {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                (self.0.position() as usize) + b.len() <= self.0.get_ref().len(),
                "tensor bytes must not be read"
            );
            self.0.read(b)
        }
    }
    let b = llama();
    let len = b.len();
    let metadata = inspect_gguf_reader(OnlyHeader(Cursor::new(b)), 1_000_000_000_000).unwrap();
    assert_eq!(metadata.inspected_bytes, len as u64);
    assert_eq!(metadata.file_bytes, 1_000_000_000_000);
    assert_eq!(metadata.context_tokens, Some(8192));
    assert_eq!(metadata.layers, Some(24));
    let kv = metadata.kv.unwrap();
    assert_eq!(kv.key_head_length, 128);
    assert_eq!(kv.value_head_length, 128);
    assert_eq!(kv.heads, 4);
    assert_eq!(kv.elements_per_token, 24 * 4 * 256);
}
#[test]
fn missing_hybrid_and_per_layer_metadata_stay_unknown() {
    for fields in [
        vec![
            ("general.architecture", Field::Text("mamba")),
            ("mamba.block_count", Field::Number(24)),
        ],
        vec![
            ("general.architecture", Field::Text("llama")),
            ("llama.block_count", Field::Number(2)),
            ("llama.embedding_length", Field::Number(64)),
            ("llama.attention.head_count", Field::Number(8)),
            ("llama.attention.head_count_kv", Field::Array),
        ],
        vec![],
    ] {
        let b = fixture(&fields);
        let m = inspect_gguf_reader(&b[..], b.len() as u64).unwrap();
        assert!(m.kv.is_none());
        assert!(!m.warnings.is_empty());
    }
}
#[test]
fn explicit_key_value_lengths_are_respected() {
    let b = fixture(&[
        ("general.architecture", Field::Text("qwen2")),
        ("qwen2.block_count", Field::Number(2)),
        ("qwen2.attention.head_count", Field::Number(8)),
        ("qwen2.attention.head_count_kv", Field::Number(2)),
        ("qwen2.attention.key_length", Field::Number(64)),
        ("qwen2.attention.value_length", Field::Number(32)),
    ]);
    assert_eq!(
        inspect_gguf_reader(&b[..], b.len() as u64)
            .unwrap()
            .kv
            .unwrap()
            .elements_per_token,
        384
    );
}
#[test]
fn truncated_headers_values_and_duplicate_keys_reject() {
    let b = llama();
    for n in 0..b.len() {
        assert!(
            inspect_gguf_reader(&b[..n], n as u64).is_err(),
            "accepted cut at {n}"
        );
    }
    let b = fixture(&[
        ("general.architecture", Field::Text("llama")),
        ("general.architecture", Field::Text("qwen2")),
    ]);
    assert_eq!(
        inspect_gguf_reader(&b[..], b.len() as u64).unwrap_err(),
        GgufFailure::InvalidMetadata
    );
}
#[test]
fn malformed_and_unbounded_lengths_fail_before_allocating() {
    let mut b = fixture(&[]);
    b[4..8].copy_from_slice(&99u32.to_le_bytes());
    assert_eq!(
        inspect_gguf_reader(&b[..], b.len() as u64).unwrap_err(),
        GgufFailure::UnsupportedVersion
    );
    let mut b = fixture(&[]);
    b[16..24].copy_from_slice(&1u64.to_le_bytes());
    b.extend(u64::MAX.to_le_bytes());
    assert_eq!(
        inspect_gguf_reader(&b[..], u64::MAX).unwrap_err(),
        GgufFailure::LimitExceeded
    );
    let mut b = fixture(&[]);
    b[16..24].copy_from_slice(&1u64.to_le_bytes());
    string(&mut b, "tokenizer.test");
    b.extend(9u32.to_le_bytes());
    b.extend(8u32.to_le_bytes());
    b.extend(u64::MAX.to_le_bytes());
    assert_eq!(
        inspect_gguf_reader(&b[..], u64::MAX).unwrap_err(),
        GgufFailure::LimitExceeded
    );
}
#[test]
fn split_metadata_and_filename_refuse_without_adjacent_access() {
    let b = fixture(&[
        ("split.count", Field::Number(2)),
        ("split.no", Field::Number(0)),
    ]);
    assert_eq!(
        inspect_gguf_reader(&b[..], b.len() as u64).unwrap_err(),
        GgufFailure::SplitModelUnsupported
    );
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("test-00001-of-00002.gguf");
    std::fs::write(&path, llama()).unwrap();
    assert_eq!(
        inspect_gguf(&path).unwrap_err(),
        GgufFailure::SplitModelUnsupported
    );
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
}
#[test]
fn no_tensor_load_from_sparse_selected_file() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("test.gguf");
    let b = llama();
    std::fs::write(&path, &b).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let m = inspect_gguf(&path).unwrap();
    assert_eq!(m.inspected_bytes, b.len() as u64);
    assert_eq!(m.file_bytes, 1024 * 1024 * 1024);
}
#[test]
fn invalid_utf8_bool_unknown_type_and_nesting_reject() {
    for payload in [
        vec![7, 0, 0, 0, 2],
        vec![99, 0, 0, 0],
        vec![8, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 255],
    ] {
        let mut b = fixture(&[]);
        b[16..24].copy_from_slice(&1u64.to_le_bytes());
        string(&mut b, "test.key");
        b.extend(payload);
        assert!(inspect_gguf_reader(&b[..], b.len() as u64).is_err());
    }
    let mut b = fixture(&[]);
    b[16..24].copy_from_slice(&1u64.to_le_bytes());
    string(&mut b, "test.key");
    b.extend(9u32.to_le_bytes());
    for _ in 0..6 {
        b.extend(9u32.to_le_bytes());
        b.extend(1u64.to_le_bytes());
    }
    assert_eq!(
        inspect_gguf_reader(&b[..], b.len() as u64).unwrap_err(),
        GgufFailure::LimitExceeded
    );
}
