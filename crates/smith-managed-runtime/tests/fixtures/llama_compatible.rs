// Compiled only by integration tests into a temporary native executable. This
// deliberately fake GGUF/protocol fixture performs no model inference.
use std::{fs, io::{Read, Write}, os::unix::net::{UnixListener, UnixStream}, path::PathBuf,
    thread, time::Duration};
fn arg(name: &str) -> String {
    let args: Vec<_> = std::env::args().collect();
    args.windows(2).find(|a| a[0] == name).unwrap()[1].clone()
}
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--help") => { println!("--model FILE\n--alias ID\n--host HOST unix socket paths\n--port N\n--parallel N\n--offline\n--no-agent\n--no-webui\n--no-mmproj\n--fit [on|off]\n--device NAME none\n--n-gpu-layers N\n--ctx-size N\n--threads N\n--api-key KEY\n--threads-batch N\n--batch-size N\n--ubatch-size N\n--flash-attn [on|off|auto]\n--cache-type-k TYPE f16 q8_0 q4_0\n--cache-type-v TYPE f16 q8_0 q4_0\n--load-mode MODE none mmap mlock mmap+mlock"); return; }
        Some("--list-devices") => { println!("Available devices:\n  CPU: Synthetic CPU"); return; }
        _ => {}
    }
    let host = arg("--host"); let model = PathBuf::from(arg("--model")); let alias = arg("--alias");
    let bytes = fs::read(&model).unwrap(); let mode = String::from_utf8_lossy(&bytes[4..]).trim().to_owned();
    fs::write(model.with_extension("socket"), &host).unwrap();
    fs::write(model.with_extension("observed"), format!("args={:?}\nenv={:?}",
        std::env::args().skip(1).collect::<Vec<_>>(),
        std::env::vars().map(|(k,_)| k).collect::<Vec<_>>())).unwrap();
    if mode.starts_with("startup_") {
        println!("startup-only stdout marker");
        std::io::stdout().flush().unwrap();
        let key = std::env::var("LLAMA_API_KEY").unwrap();
        eprint!("startup key={}", &key[..32]);
        std::io::stderr().flush().unwrap();
        eprintln!("{}", &key[32..]);
        eprintln!("fixture model path: {}", model.display());
        eprintln!("untrusted synthetic Metal allocation: 123 MiB");
        if mode == "startup_report" {
            eprintln!("load_tensors: offloaded 40/41 layers to GPU");
            eprintln!("load_tensors:  MTL0_Mapped model buffer size =  5099.18 MiB");
            eprintln!("load_tensors:   CPU_Mapped model buffer size =    27.04 MiB");
        }
        thread::sleep(Duration::from_millis(40));
    }
    if mode == "exit" { std::process::exit(12); }
    if mode == "delay" { thread::sleep(Duration::from_millis(200)); }
    if mode == "no_bind" { loop { thread::sleep(Duration::from_secs(60)); } }
    if mode == "stderr_flood" { eprint!("{}", "x".repeat(1_000_000)); }
    let listener = UnixListener::bind(&host).unwrap();
    if mode == "exit_after_ready" { thread::spawn(|| { thread::sleep(Duration::from_millis(300)); std::process::exit(0); }); }
    for connection in listener.incoming() {
        let stream = connection.unwrap(); let alias = alias.clone(); let mode = mode.clone(); let model = model.clone();
        thread::spawn(move || serve(stream, &alias, &mode, &model));
    }
}
fn serve(mut stream: UnixStream, alias: &str, mode: &str, model: &std::path::Path) {
    let mut bytes = Vec::new(); let mut chunk = [0u8; 4096];
    let (headers, body_start) = loop {
        let n = match stream.read(&mut chunk) { Ok(0) | Err(_) => return, Ok(n) => n };
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() > 256 * 1024 { return; }
        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break (String::from_utf8_lossy(&bytes[..end]).to_string(), end + 4);
        }
    };
    let length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") { value.trim().parse::<usize>().ok() } else { None }
    }).unwrap_or(0);
    while bytes.len() < body_start + length {
        let n = match stream.read(&mut chunk) { Ok(0) | Err(_) => return, Ok(n) => n };
        bytes.extend_from_slice(&chunk[..n]);
    }
    let health = headers.starts_with("GET /health ");
    if headers.starts_with("POST") && (mode.starts_with("task_") || mode == "request_capture") {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(model.with_extension("requests")).unwrap();
        file.write_all(&bytes[body_start..body_start + length]).unwrap();
        file.write_all(b"\n").unwrap();
        if mode == "task_schema_reject" {
            stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            return;
        }
        if mode == "task_schema_stall" { loop { thread::sleep(Duration::from_secs(60)); } }
    }
    if (health && (mode == "health_stall" || mode == "startup_stall")) || (headers.starts_with("POST") && mode == "generation_stall") {
        loop { thread::sleep(Duration::from_secs(60)); }
    }
    if headers.starts_with("POST") && mode == "startup_output" {
        println!("late stdout private prompt: {}", String::from_utf8_lossy(&bytes[body_start..]));
        eprintln!("late stderr private prompt: {}", String::from_utf8_lossy(&bytes[body_start..]));
        // Post-readiness output remains drained even after the retained byte cap.
        eprint!("{}", "late-private-output".repeat(65_536));
    }
    let body = if health { r#"{"status":"ok"}"#.to_owned() }
        else if headers.starts_with("GET /v1/models ") {
            format!(r#"{{"data":[{{"id":"{}"}}]}}"#, if mode == "wrong_model" { "other" } else { alias })
        } else if mode == "oversize" {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9999999\r\n\r\n"); return;
        } else if mode == "redirect" {
            let _ = stream.write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:9999/\r\nContent-Length: 0\r\n\r\n"); return;
        } else if mode == "truncated" {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n{"); return;
        } else if mode == "body_stall" {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n{");
            loop { thread::sleep(Duration::from_secs(60)); }
        } else if mode == "malformed" { "not json".to_string() }
        else if mode == "task_schema_ignored" { format!(r#"{{"model":"{alias}","choices":[{{"message":{{"role":"assistant","content":"```json\n{{\"content\":\"synthetic\",\"tool_call\":null}}\n```"}},"finish_reason":"stop"}}]}}"#) }
        else { format!(r#"{{"model":"{alias}","choices":[{{"message":{{"role":"assistant","content":"fixture output"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}}}"#) };
    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
    let _ = stream.write_all(response.as_bytes());
}
