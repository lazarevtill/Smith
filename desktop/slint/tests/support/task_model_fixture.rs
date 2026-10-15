//! Reusable owned-loopback fixture for native task callbacks. Model discovery
//! and queued synthetic completions only; no forwarding or real inference.
#![allow(dead_code)]
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
pub const MODEL: &str = "native-synthetic-task-model";
pub struct Step {
    pub reply: Value,
    pub finish_reason: String,
    pub held: bool,
}
impl Step {
    pub fn tool(name: &str, arguments: Value, held: bool) -> Self {
        Self {
            reply: json!({"content":"Owned synthetic proposal","tool_call":{"name":name,"arguments":arguments}}),
            finish_reason: "stop".into(),
            held,
        }
    }
    pub fn complete(held: bool) -> Self {
        Self {
            reply: json!({"content":"Owned synthetic completion","tool_call":null}),
            finish_reason: "stop".into(),
            held,
        }
    }
}
#[derive(Default)]
struct Control {
    stopping: bool,
    permits: usize,
}
pub struct TaskModelFixture {
    pub endpoint: String,
    control: Arc<(Mutex<Control>, Condvar)>,
    discoveries: Arc<AtomicUsize>,
    completions: Arc<AtomicUsize>,
    failure: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
}
impl TaskModelFixture {
    pub fn launch(steps: Vec<Step>) -> Result<Self, String> {
        if steps.len() > 16 {
            return Err("Fixture permits at most sixteen completions".into());
        }
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|_| "Cannot bind owned fixture listener")?;
        let endpoint = format!(
            "http://{}/v1",
            listener
                .local_addr()
                .map_err(|_| "Fixture address unavailable")?
        );
        listener
            .set_nonblocking(true)
            .map_err(|_| "Fixture nonblocking failed")?;
        let control = Arc::new((Mutex::new(Control::default()), Condvar::new()));
        let worker_control = control.clone();
        let discoveries = Arc::new(AtomicUsize::new(0));
        let discovered = discoveries.clone();
        let completions = Arc::new(AtomicUsize::new(0));
        let completed = completions.clone();
        let failure = Arc::new(Mutex::new(None));
        let failed = failure.clone();
        let worker = thread::spawn(move || {
            let mut steps = VecDeque::from(steps);
            let mut requests = 0;
            while !worker_control.0.lock().unwrap().stopping {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => {
                        *failed.lock().unwrap() = Some("Owned fixture accept failed".into());
                        break;
                    }
                };
                requests += 1;
                let result = (|| -> Result<(), String> {
                    if requests > 64 {
                        return Err("Owned fixture request bound exceeded".into());
                    }
                    let (method, path, body) = read_request(&mut stream)?;
                    let response = match (method.as_str(), path.as_str()) {
                        ("GET", "/v1/models") => {
                            discovered.fetch_add(1, Ordering::SeqCst);
                            json!({"data":[{"id":MODEL}]})
                        }
                        ("POST", "/v1/chat/completions") => {
                            let body: Value = serde_json::from_slice(&body)
                                .map_err(|_| "Invalid synthetic request JSON")?;
                            if body["model"] != MODEL || body["stream"] != false {
                                return Err("Unexpected synthetic request target".into());
                            }
                            let step = steps
                                .pop_front()
                                .ok_or("Unreviewed extra synthetic completion")?;
                            completed.fetch_add(1, Ordering::SeqCst);
                            if step.held {
                                let mut control = worker_control.0.lock().unwrap();
                                while control.permits == 0 && !control.stopping {
                                    control = worker_control.1.wait(control).unwrap();
                                }
                                if control.stopping {
                                    return Ok(());
                                }
                                control.permits -= 1;
                            }
                            json!({"choices":[{"finish_reason":step.finish_reason,"message":{"content":step.reply.to_string()}}]})
                        }
                        _ => return Err("Unexpected route on owned model fixture".into()),
                    };
                    let body = response.to_string();
                    // Cancellation may have closed this client. A late reply
                    // is intentionally harmless and is never retried.
                    let _=write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                    Ok(())
                })();
                if let Err(error) = result {
                    *failed.lock().unwrap() = Some(error);
                    break;
                }
            }
        });
        Ok(Self {
            endpoint,
            control,
            discoveries,
            completions,
            failure,
            worker: Some(worker),
        })
    }
    pub fn release(&self) {
        self.control.0.lock().unwrap().permits += 1;
        self.control.1.notify_all();
    }
    pub fn counts(&self) -> (usize, usize) {
        (
            self.discoveries.load(Ordering::SeqCst),
            self.completions.load(Ordering::SeqCst),
        )
    }
    pub fn assert_healthy(&self) {
        assert_eq!(*self.failure.lock().unwrap(), None);
    }
    pub fn wait_entered(&self, count: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.counts().1 < count {
            self.assert_healthy();
            assert!(
                Instant::now() < deadline,
                "Synthetic completion did not start"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for TaskModelFixture {
    fn drop(&mut self) {
        self.control.0.lock().unwrap().stopping = true;
        self.control.1.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn read_request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>), String> {
    // Darwin may inherit O_NONBLOCK from the listener. Explicitly switch the
    // accepted stream before bounded blocking reads; read timeouts do not do it.
    stream
        .set_nonblocking(false)
        .map_err(|_| "Fixture blocking mode failed")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|_| "Fixture timeout failed")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|_| "Fixture timeout failed")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= 8192 || Instant::now() > deadline {
            return Err("Fixture header bound exceeded".into());
        }
        stream.read_exact(&mut byte).map_err(|error| {
            format!(
                "Fixture header read failed after {} bytes: {:?}",
                header.len(),
                error.kind()
            )
        })?;
        header.push(byte[0]);
    }
    let header = String::from_utf8(header).map_err(|_| "Fixture header was not UTF-8")?;
    let mut lines = header.lines();
    let fields = lines
        .next()
        .ok_or("Missing fixture request line")?
        .split_whitespace()
        .collect::<Vec<_>>();
    if fields.len() != 3 || fields[2] != "HTTP/1.1" {
        return Err("Invalid fixture request line".into());
    }
    let mut length = None;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                if length.is_some() {
                    return Err("Duplicate fixture length".into());
                }
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| "Invalid fixture length")?,
                );
            }
            if key.eq_ignore_ascii_case("transfer-encoding") {
                return Err("Chunked fixture request unsupported".into());
            }
        }
    }
    let length = length.unwrap_or(0);
    if length > 256 * 1024 {
        return Err("Fixture body bound exceeded".into());
    }
    let mut body = vec![0; length];
    stream
        .read_exact(&mut body)
        .map_err(|_| "Fixture body read failed")?;
    Ok((fields[0].into(), fields[1].into(), body))
}
