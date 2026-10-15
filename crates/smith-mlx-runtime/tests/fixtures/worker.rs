// Synthetic protocol worker only. Does not parse safetensors or invoke inference.
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};
fn field(line: &str, key: &str) -> String {
    let marker = format!("\"{key}\":\"");
    let start = line.find(&marker).unwrap() + marker.len();
    line[start..].split('"').next().unwrap().to_string()
}
fn event(id: &str, event: &str, extra: &str) {
    println!("{{\"v\":1,\"id\":\"{id}\",\"event\":\"{event}\"{extra}}}");
    io::stdout().flush().unwrap();
}
fn error(id: &str, code: &str) {
    event(
        id,
        "error",
        &format!(",\"code\":\"{code}\",\"message\":\"Synthetic redacted failure.\""),
    );
}
fn main() {
    let exe = std::env::current_exe().unwrap();
    let mode = exe.file_stem().unwrap().to_str().unwrap();
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], "--scratch-root");
    assert!(std::env::vars().all(|(k, _)| k == "LANG"));
    let scratch = PathBuf::from(&args[1]);
    assert_eq!(std::env::current_dir().unwrap(), scratch);
    {
        use std::os::unix::fs::MetadataExt;
        let scratch_metadata = std::fs::metadata(&scratch).unwrap();
        assert_eq!(
            scratch_metadata.mode() & 0o777,
            0o700,
            "scratch must be private from creation"
        );
        assert_eq!(
            scratch_metadata.uid(),
            std::fs::metadata(&exe).unwrap().uid()
        );
    }
    std::fs::write(scratch.join("synthetic-scratch"), b"owned").unwrap();
    if mode == "smith-mlx-worker" {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(exe.parent().unwrap(), scratch.join("runtime"));
        for name in [
            "mlx.metallib",
            "mlx-swift_Cmlx.bundle/Contents/Info.plist",
            "mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib",
            "swift-transformers_Hub.bundle/gpt2_tokenizer_config.json",
            "swift-transformers_Hub.bundle/t5_tokenizer_config.json",
            "swift-crypto_Crypto.bundle/PrivacyInfo.xcprivacy",
        ] {
            let p = exe.parent().unwrap().join(name);
            assert!(p.is_file());
            assert_eq!(
                std::fs::metadata(p).unwrap().permissions().mode() & 0o222,
                0
            );
        }
    } else {
        std::fs::write(exe.with_extension("scratch"), scratch.to_str().unwrap()).unwrap();
        std::fs::write(exe.with_extension("pid"), std::process::id().to_string()).unwrap();
    }
    if matches!(
        mode,
        "cleanup_fault" | "bad_shutdown_cleanup" | "bad_hello_cleanup" | "cancel_cleanup"
    ) {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch.join("locked");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("retain-until-retry"), b"synthetic").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let mut active = String::new();
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let id = field(&line, "id");
        let op = field(&line, "op");
        match op.as_str() {
            "hello" => {
                if mode == "bad_hello" || mode == "bad_hello_cleanup" {
                    event(&id, "done", ",\"worker\":\"wrong\"");
                    continue;
                }
                let feature = if mode.starts_with("thinking_") && mode != "thinking_legacy" {
                    ",\"qwen3_thinking_control_v1\""
                } else {
                    ""
                };
                event(&id,"done",&format!(",\"worker\":\"smith-mlx-worker\",\"protocol\":1,\"backend\":\"mlx-swift-metal\",\"capabilities\":[\"inspect\",\"local_text\"{feature}],\"max_frame_bytes\":131072,\"max_input_tokens\":8192,\"max_output_tokens\":2048"));
            }
            "inspect" | "load" => {
                event(&id, "started", "");
                if mode == "worker_error" {
                    error(&id, "unsupported_model");
                    continue;
                }
                let manifest = std::fs::read_to_string(exe.with_extension("manifest")).unwrap();
                let hash = field(&line, "expected_package_sha256");
                assert!(manifest.contains(&hash));
                if mode == "stale" {
                    event(
                        "stale-id",
                        "done",
                        &format!(",{}", &manifest[1..manifest.len() - 1]),
                    );
                } else if mode == "mismatch" {
                    event(
                        &id,
                        "done",
                        &format!(
                            ",{}",
                            &manifest[1..manifest.len() - 1].replace(&hash, &"0".repeat(64))
                        ),
                    );
                } else {
                    event(
                        &id,
                        "done",
                        &format!(",{}", &manifest[1..manifest.len() - 1]),
                    );
                }
            }
            "generate" => {
                event(&id, "started", "");
                if mode.starts_with("thinking_") {
                    writeln!(
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(exe.with_extension("requests"))
                            .unwrap(),
                        "{line}"
                    )
                    .unwrap();
                    event(&id,"delta",",\"replace_from_utf8\":0,\"text\":\"<think>\\n\\n</think>\\n\\nLiteral fixture response.\"");
                    if mode == "thinking_cancel" {
                        active = id;
                        continue;
                    }
                    let supplied = line.contains("\"thinking_mode\":");
                    assert!(
                        mode != "thinking_legacy" || !supplied,
                        "legacy worker must never receive explicit thinking control"
                    );
                    let ack = match mode {
                        "thinking_missing_ack" => "",
                        "thinking_wrong_ack" => ",\"thinking_mode\":\"model_default\"",
                        "thinking_unknown_ack" => ",\"thinking_mode\":\"enabled\"",
                        "thinking_null_ack" => ",\"thinking_mode\":null",
                        "thinking_extra_default_ack" => ",\"thinking_mode\":\"disabled\"",
                        _ if supplied => ",\"thinking_mode\":\"disabled\"",
                        _ => "",
                    };
                    let reason = if mode == "thinking_spontaneous_cancel" {
                        "cancelled"
                    } else {
                        "stop"
                    };
                    event(&id,"done",&format!(",\"input_tokens\":4,\"output_tokens\":2,\"finish_reason\":\"{reason}\"{ack}"));
                    continue;
                }
                event(&id, "delta", ",\"replace_from_utf8\":0,\"text\":\"hél\"");
                match mode {
                    "stall" | "ignore_cancel" | "terminal_first" | "cancel_cleanup" => {
                        active = id;
                        continue;
                    }
                    "bad_boundary" => {
                        event(&id, "delta", ",\"replace_from_utf8\":2,\"text\":\"bad\"");
                        continue;
                    }
                    "oversized" => {
                        println!("{}", "x".repeat(140000));
                        io::stdout().flush().unwrap();
                        continue;
                    }
                    "stderr_flood" => {
                        io::stderr().write_all(&vec![b'x'; 140000]).ok();
                        continue;
                    }
                    "duplicate_key" => {
                        println!(
                            "{{\"v\":1,\"id\":\"{id}\",\"id\":\"{id}\",\"event\":\"started\"}}"
                        );
                        io::stdout().flush().unwrap();
                        continue;
                    }
                    "unfinished" => {
                        print!("{{\"v\":1");
                        io::stdout().flush().unwrap();
                        return;
                    }
                    "invalid_utf8" => {
                        io::stdout().write_all(&[255, b'\n']).unwrap();
                        io::stdout().flush().unwrap();
                        continue;
                    }
                    "unknown_field" => {
                        event(
                            &id,
                            "delta",
                            ",\"replace_from_utf8\":0,\"text\":\"bad\",\"extra\":true",
                        );
                        continue;
                    }
                    "done_early" => {
                        event(&id,"done",",\"input_tokens\":8192,\"output_tokens\":128,\"finish_reason\":\"stop\"");
                        continue;
                    }
                    _ => {}
                }
                event(
                    &id,
                    "delta",
                    ",\"replace_from_utf8\":1,\"text\":\"ello 🌍\"",
                );
                event(
                    &id,
                    "done",
                    ",\"input_tokens\":4,\"output_tokens\":2,\"finish_reason\":\"stop\"",
                );
                if mode == "late_terminal" {
                    event(
                        &id,
                        "done",
                        ",\"input_tokens\":4,\"output_tokens\":2,\"finish_reason\":\"stop\"",
                    );
                }
            }
            "cancel" => {
                if mode == "ignore_cancel" {
                    continue;
                }
                let target = field(&line, "target_id");
                if target != active {
                    error(&id, "not_active");
                    continue;
                }
                if mode == "terminal_first" {
                    error(&active, "cancelled");
                }
                event(
                    &id,
                    "done",
                    &format!(",\"target_id\":\"{target}\",\"cancel_requested\":true"),
                );
                if mode != "terminal_first" {
                    error(&active, "cancelled");
                }
                active.clear();
            }
            "unload" => {
                event(&id, "started", "");
                event(&id, "done", "");
            }
            "shutdown" => {
                if mode == "bad_shutdown" || mode == "bad_shutdown_cleanup" {
                    event("stale-shutdown", "done", "");
                } else {
                    event(&id, "done", "");
                }
                return;
            }
            _ => panic!("unexpected operation"),
        }
    }
}
