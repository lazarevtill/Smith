//! An in-process, owned loopback fixture. No profile, model, credential,
//! microphone, desktop capture, or external network is touched.
use crate::bridge::{Result, Transport};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const LIMIT: usize = 2 * 1024 * 1024;
const HEADER_LIMIT: usize = 16 * 1024;

#[derive(Default)]
struct State {
    session: String,
    revision: u64,
    chats: BTreeMap<String, Value>,
    reads: BTreeMap<String, u32>,
    counter: u64,
    mutations: usize,
    drafts: BTreeMap<String, Value>,
    trashed: std::collections::BTreeSet<String>,
    history_revision: u64,
    undo: Vec<Value>,
    redo: Vec<Value>,
    answer: Option<String>,
}
struct Owner {
    address: SocketAddr,
    secret: String,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
    }
}
#[derive(Clone)]
pub struct Fixture {
    owner: Arc<Owner>,
}
impl Fixture {
    pub fn start() -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| "Cannot bind owned fixture listener.")?;
        let address = listener
            .local_addr()
            .map_err(|_| "Cannot inspect fixture listener.")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "Cannot configure fixture listener.")?;
        let mut entropy = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut entropy))
            .map_err(|_| "Fixture requires operating-system randomness.")?;
        let secret: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(State {
            session: "fixture-session-1".into(),
            ..State::default()
        }));
        let thread_stop = stop.clone();
        let thread_state = state.clone();
        let thread_secret = secret.clone();
        let worker = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Darwin can inherit the listener's nonblocking flag.
                        // A request may arrive after accept; use the bounded
                        // blocking reader rather than closing on WouldBlock.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                        if let Ok((head, body)) = read_message(&mut stream, false) {
                            let first = head.lines().next().unwrap_or("");
                            let mut words = first.split_whitespace();
                            let method = words.next().unwrap_or("");
                            let path = words.next().unwrap_or("");
                            let owner = header(&head, "x-smith-desktop-owner");
                            let session = header(&head, "x-smith-profile-session");
                            let (status, response) = if owner.as_deref() != Some(&thread_secret) {
                                (403, json!({"error":"Invalid fixture owner."}))
                            } else {
                                route(
                                    &mut thread_state.lock().unwrap(),
                                    method,
                                    path,
                                    session.as_deref(),
                                    serde_json::from_slice(&body).unwrap_or(Value::Null),
                                )
                            };
                            let bytes = serde_json::to_vec(&response).unwrap();
                            let response = format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len());
                            let _ = stream
                                .write_all(response.as_bytes())
                                .and_then(|_| stream.write_all(&bytes));
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            owner: Arc::new(Owner {
                address,
                secret,
                stop,
                state,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }
    pub fn transport(&self) -> FixtureTransport {
        FixtureTransport {
            owner: self.owner.clone(),
        }
    }
    pub fn mutations(&self) -> usize {
        self.owner.state.lock().unwrap().mutations
    }
    pub fn rotate_session(&self) {
        self.owner.state.lock().unwrap().session.push('x');
    }
    pub fn advance_connection(&self) {
        self.owner.state.lock().unwrap().revision += 1;
    }
    /// Simulate another client writing the same draft; used only by the owned
    /// fixture to exercise visible CAS-conflict recovery in the actual window.
    pub fn replace_draft(&self, owner: &str, text: &str) -> Result<()> {
        if text.len() > 100_000 {
            return Err("Synthetic draft exceeds fixture bound.".into());
        }
        let mut state = self.owner.state.lock().unwrap();
        let draft = state
            .drafts
            .get_mut(owner)
            .ok_or("Fixture draft not loaded.")?;
        draft["version"] = json!(draft["version"].as_u64().unwrap() + 1);
        draft["text"] = json!(text);
        Ok(())
    }
    /// Synthetic fixture output only; never used by the normal backend path.
    pub fn set_answer(&self, text: &str) -> Result<()> {
        if text.len() > 100_000 {
            return Err("Synthetic answer exceeds fixture bound.".into());
        }
        self.owner.state.lock().unwrap().answer = Some(text.into());
        Ok(())
    }
}
/// Only Fixture::transport can construct it. The listener stays owned until
/// the last client and fixture handle are dropped, preventing port takeover.
pub struct FixtureTransport {
    owner: Arc<Owner>,
}
impl Transport for FixtureTransport {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value> {
        if !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE")
            || !path.starts_with("/api/")
            || path.bytes().any(|b| b.is_ascii_control() || b == b' ')
        {
            return Err("Invalid fixture request target.".into());
        }
        if session.is_some_and(|s| s.bytes().any(|b| b.is_ascii_control())) {
            return Err("Invalid session header.".into());
        }
        let bytes = body
            .map(|v| serde_json::to_vec(&v).unwrap())
            .unwrap_or_default();
        if bytes.len() > LIMIT {
            return Err("Fixture request limit exceeded.".into());
        }
        let mut socket = TcpStream::connect_timeout(&self.owner.address, Duration::from_secs(1))
            .map_err(|_| "Owned fixture is unavailable.")?;
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|_| "Cannot set response deadline.")?;
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .map_err(|_| "Cannot set request deadline.")?;
        let session_header = session
            .map(|s| format!("X-Smith-Profile-Session: {s}\r\n"))
            .unwrap_or_default();
        let head=format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nX-Smith-Desktop-Owner: {}\r\n{session_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",self.owner.address,self.owner.address,self.owner.secret,bytes.len());
        socket
            .write_all(head.as_bytes())
            .and_then(|_| socket.write_all(&bytes))
            .map_err(|_| "Fixture request failed.")?;
        let (head, body) = read_message(&mut socket, true)?;
        let code = head
            .lines()
            .next()
            .and_then(|s| s.split_whitespace().nth(1))
            .ok_or("Malformed HTTP response.")?;
        let value: Value = serde_json::from_slice(&body).map_err(|_| "Malformed fixture JSON.")?;
        if code != "200" {
            return Err(format!(
                "HTTP {code}: {}",
                value["error"].as_str().unwrap_or("Request rejected.")
            ));
        }
        Ok(value)
    }
}
fn header(head: &str, key: &str) -> Option<String> {
    let matches: Vec<_> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .filter(|(k, _)| k.eq_ignore_ascii_case(key))
        .collect();
    if matches.len() != 1 {
        return None;
    }
    Some(matches[0].1.trim().to_owned())
}
fn read_message(stream: &mut TcpStream, response: bool) -> Result<(String, Vec<u8>)> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    let (offset, head) = loop {
        if Instant::now() > deadline || bytes.len() > HEADER_LIMIT {
            return Err("HTTP header bound exceeded.".into());
        }
        if let Some(n) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            break (
                n + 4,
                String::from_utf8(bytes[..n].to_vec()).map_err(|_| "Invalid HTTP headers.")?,
            );
        }
        let mut chunk = [0; 1024];
        let n = stream
            .read(&mut chunk)
            .map_err(|_| "HTTP header read failed.")?;
        if n == 0 {
            return Err("HTTP ended before headers.".into());
        }
        bytes.extend_from_slice(&chunk[..n]);
    };
    if header(&head, "transfer-encoding").is_some() {
        return Err(
            "Fixture forbids transfer-encoding; production streaming needs its separate adapter."
                .into(),
        );
    }
    let len = header(&head, "content-length")
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or("Missing response size.")?;
    if len > LIMIT || (response && !head.starts_with("HTTP/1.1 ")) {
        return Err("HTTP response bound exceeded.".into());
    }
    let mut body = bytes[offset..].to_vec();
    while body.len() < len {
        if Instant::now() > deadline {
            return Err("HTTP body deadline exceeded.".into());
        }
        let mut chunk = [0; 8192];
        let remaining = (len - body.len()).min(chunk.len());
        let n = stream
            .read(&mut chunk[..remaining])
            .map_err(|_| "HTTP body read failed.")?;
        if n == 0 {
            return Err("Truncated HTTP body.".into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    if body.len() != len {
        return Err("Unexpected trailing HTTP bytes.".into());
    }
    Ok((head, body))
}
fn decoded(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(n) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                result.push(n);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(result).unwrap_or_default()
}
fn fail(code: u16, message: &str) -> (u16, Value) {
    (code, json!({"error":message}))
}
fn history(state: &State) -> Value {
    json!({"revision":state.history_revision,"undo":state.undo.last().map(|v|&v["action"]),"redo":state.redo.last().map(|v|&v["action"])})
}
fn record(state: &mut State, before: Value, after: Value, label: &str) {
    state.history_revision += 1;
    state.redo.clear();
    state.undo.push(json!({"before":before,"after":after,"action":{"id":format!("fixture-action-{}",state.history_revision),"label":label,"chat_ids":after.as_array().unwrap().iter().map(|v|v["id"].clone()).collect::<Vec<_>>()}}));
}
fn metadata(state: &State, ids: &[String]) -> Value {
    json!(ids.iter().map(|id|json!({"id":id,"title":state.chats[id]["chat"]["title"],"trashed":state.trashed.contains(id)})).collect::<Vec<_>>())
}
fn apply_metadata(state: &mut State, values: &Value) {
    for value in values.as_array().unwrap() {
        let id = value["id"].as_str().unwrap();
        let c = state.chats.get_mut(id).unwrap();
        c["chat"]["title"] = value["title"].clone();
        c["chat"]["version"] = json!(c["chat"]["version"].as_u64().unwrap() + 1);
        if value["trashed"] == true {
            state.trashed.insert(id.into());
        } else {
            state.trashed.remove(id);
        }
    }
}
fn route(
    state: &mut State,
    method: &str,
    path: &str,
    session: Option<&str>,
    body: Value,
) -> (u16, Value) {
    if method == "GET" && path == "/api/profile/status" {
        return (
            200,
            json!({"state":"unlocked","session":state.session,"revision":"fixture","synthetic":true}),
        );
    }
    if session != Some(&state.session) {
        return fail(409, "Stale workspace session.");
    }
    let revision = format!("fixture:{}", state.revision);
    if let Some(owner) = path.strip_prefix("/api/drafts/") {
        let owner = decoded(owner);
        let current = state.drafts.entry(owner.clone()).or_insert_with(
            || json!({"owner":owner,"version":0,"text":"","attachments":[],"media":[]}),
        );
        if method == "GET" {
            return (200, current.clone());
        }
        if method != "PUT" || body["version"] != current["version"] {
            return fail(409, "Draft changed in another client.");
        }
        *current = json!({"owner":owner,"version":current["version"].as_u64().unwrap()+1,"text":body["text"],"attachments":body["attachments"],"media":body["media"]});
        state.history_revision += 1;
        state.redo.clear();
        state.mutations += 1;
        return (200, current.clone());
    }
    if method == "GET" && path == "/api/actions" {
        return (200, history(state));
    }
    if method == "GET" && path == "/api/trash" {
        return (
            200,
            json!({"chats":state.trashed.iter().map(|id|state.chats[id]["chat"].clone()).collect::<Vec<_>>()}),
        );
    }
    if method == "POST" && matches!(path, "/api/actions/undo" | "/api/actions/redo") {
        let redo = path.ends_with("redo");
        let stack = if redo {
            &mut state.redo
        } else {
            &mut state.undo
        };
        if body["revision"] != state.history_revision
            || stack
                .last()
                .is_none_or(|e| e["action"]["id"] != body["action_id"])
        {
            return fail(409, "Stale reversible action.");
        }
        let entry = stack.pop().unwrap();
        apply_metadata(state, &entry[if redo { "after" } else { "before" }]);
        if redo {
            state.undo.push(entry);
        } else {
            state.redo.push(entry);
        }
        state.history_revision += 1;
        state.mutations += 1;
        return (200, json!({"ok":true,"history":history(state)}));
    }
    if method == "POST" && matches!(path, "/api/chats/trash" | "/api/chats/restore") {
        if body["history_revision"] != state.history_revision {
            return fail(409, "Stale history revision.");
        }
        let Some(targets) = body["chats"].as_array() else {
            return fail(400, "Missing targets.");
        };
        if targets.iter().any(|t| {
            state
                .chats
                .get(t["id"].as_str().unwrap_or(""))
                .is_none_or(|c| c["chat"]["version"] != t["version"])
        }) {
            return fail(409, "Stale chat version.");
        }
        let ids: Vec<_> = targets
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_owned())
            .collect();
        let before = metadata(state, &ids);
        let mut after = before.clone();
        for v in after.as_array_mut().unwrap() {
            v["trashed"] = json!(path.ends_with("trash"));
        }
        apply_metadata(state, &after);
        record(state, before, after, "Move conversation");
        state.mutations += 1;
        return (200, json!({"ok":true,"history":history(state)}));
    }
    if method == "GET" && path == "/api/config" {
        return (
            200,
            json!({"revision":revision,"provider":"mock","endpoint":"","model":"smith-demo","tested":true}),
        );
    }
    if method == "GET" && path.starts_with("/api/models?") {
        if path
            != format!(
                "/api/models?revision={}",
                crate::bridge::encode_component(&revision)
            )
        {
            return fail(409, "Stale model revision.");
        }
        return (
            200,
            json!({"revision":revision,"provider":"mock","endpoint":"","models":[{"id":"smith-demo"}]}),
        );
    }
    if method == "GET" && path == "/api/media/artifacts" {
        return (
            200,
            json!({"artifacts":[{"id":"fixture-artifact","name":"Synthetic notes.txt","kind":"legacy_text","media_type":"text/plain","byte_len":28}],"next_cursor":null}),
        );
    }
    if method == "GET" && path == "/api/agent/tasks" {
        return (
            200,
            json!({"tasks":[{"id":"fixture-task","input":"Synthetic completed task","state":"completed","revision":1,"last_result":"No tools were executed."}]}),
        );
    }
    if method == "GET" && path == "/api/chats" {
        return (
            200,
            json!(state
                .chats
                .values()
                .filter(|c| !state.trashed.contains(c["chat"]["id"].as_str().unwrap()))
                .map(|c| c["chat"].clone())
                .collect::<Vec<_>>()),
        );
    }
    if method == "POST" && path.starts_with("/api/runs/") && path.ends_with("/cancel") {
        let run = decoded(
            path.trim_start_matches("/api/runs/")
                .trim_end_matches("/cancel"),
        );
        for c in state.chats.values_mut() {
            for key in ["messages", "active_messages"] {
                if let Some(ms) = c[key].as_array_mut() {
                    for m in ms {
                        if m["run_id"] == run {
                            m["status"] = json!("cancelled");
                        }
                    }
                }
            }
        }
        state.mutations += 1;
        return (200, json!({"ok":true}));
    }
    if method == "POST" && body["revision"] != revision {
        return fail(409, "Stale displayed connection; request was not replayed.");
    }
    if method == "POST" && (body["provider"] != "mock" || body["endpoint"] != "") {
        return fail(409, "Wrong displayed destination.");
    }
    if method == "POST" && path == "/api/chats" {
        state.counter += 1;
        let id = format!("copied-chat-{}", state.counter);
        let chat = json!({"id":id,"title":"New conversation","version":0,"preview":""});
        state.chats.insert(
            id,
            json!({"chat":chat,"messages":[],"active_messages":[],"active_leaf":null}),
        );
        state.mutations += 1;
        return (200, chat);
    }
    let Some(rest) = path.strip_prefix("/api/chats/") else {
        return fail(404, "No fixture route.");
    };
    let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
    let id = decoded(id);
    if method == "PATCH" && action.is_empty() {
        if body["revision"] != revision
            || state
                .chats
                .get(&id)
                .is_none_or(|c| c["chat"]["version"] != body["version"])
        {
            return fail(409, "Stale rename.");
        }
        let before = metadata(state, std::slice::from_ref(&id));
        let mut after = before.clone();
        after[0]["title"] = body["title"].clone();
        apply_metadata(state, &after);
        record(state, before, after, "Rename conversation");
        state.mutations += 1;
        return (200, state.chats[&id]["chat"].clone());
    }
    if state.trashed.contains(&id) {
        return fail(404, "Conversation is in Trash.");
    }
    let Some(c) = state.chats.get_mut(&id) else {
        return fail(404, "Conversation missing.");
    };
    if method == "GET" && action.is_empty() {
        let reads = state.reads.entry(id).or_default();
        *reads += 1;
        if *reads >= 4 {
            for key in ["messages", "active_messages"] {
                for m in c[key].as_array_mut().unwrap() {
                    if m["status"] == "streaming" {
                        m["status"] = json!("completed");
                        m["content"]=json!(state.answer.as_deref().unwrap_or("This answer came from the owned synthetic HTTP fixture. No model or tool ran."));
                    }
                }
            }
        }
        return (200, c.clone());
    }
    if body["version"] != c["chat"]["version"] {
        return fail(409, "Stale conversation version.");
    }
    if method == "POST" && matches!(action, "messages" | "regenerate" | "edit") {
        let version = c["chat"]["version"].as_i64().unwrap() + 1;
        let user_id = format!("{id}-user-{version}");
        let assistant_id = format!("{id}-assistant-{version}");
        let run = format!("{id}-run-{version}");
        if action == "edit" {
            let Some(index) = c["active_messages"]
                .as_array()
                .unwrap()
                .iter()
                .position(|m| m["id"] == body["message_id"] && m["role"] == "user")
            else {
                return fail(409, "Wrong edited message.");
            };
            c["active_messages"].as_array_mut().unwrap().truncate(index);
        }
        if action == "messages" || action == "edit" {
            let user = json!({"id":user_id,"role":"user","content":body["content"],"status":"completed","siblings":[user_id],"branch_index":1,"attachments":[],"media":[],"run_id":null});
            c["messages"].as_array_mut().unwrap().push(user.clone());
            c["active_messages"].as_array_mut().unwrap().push(user);
        } else {
            let Some(old) = c["active_messages"].as_array_mut().unwrap().pop() else {
                return fail(409, "No answer to regenerate.");
            };
            if old["id"] != body["message_id"] || old["role"] != "assistant" {
                c["active_messages"].as_array_mut().unwrap().push(old);
                return fail(409, "Wrong regeneration owner.");
            }
        }
        let siblings: Vec<Value> = if action == "regenerate" {
            c["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "assistant")
                .map(|m| m["id"].clone())
                .chain(std::iter::once(json!(assistant_id)))
                .collect()
        } else {
            vec![json!(assistant_id)]
        };
        let assistant = json!({"id":assistant_id,"role":"assistant","content":"","status":"streaming","siblings":siblings,"branch_index":siblings.len(),"attachments":[],"media":[],"run_id":run});
        c["messages"]
            .as_array_mut()
            .unwrap()
            .push(assistant.clone());
        c["active_messages"].as_array_mut().unwrap().push(assistant);
        c["active_leaf"] = json!(assistant_id);
        c["chat"]["version"] = json!(version);
        state.reads.insert(id, 0);
        state.mutations += 1;
        return (
            200,
            json!({"run_id":run,"message_id":assistant_id,"user_message_id":user_id,"version":version,"conversation":c}),
        );
    }
    if method == "POST" && action == "branch" {
        let Some(selected) = c["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == body["message_id"])
            .cloned()
        else {
            return fail(404, "Branch missing.");
        };
        if selected["role"] != "assistant" {
            return fail(
                400,
                "This bounded fixture switches assistant alternatives only.",
            );
        }
        let mut selected = selected;
        let siblings: Vec<_> = c["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "assistant")
            .map(|m| m["id"].clone())
            .collect();
        selected["branch_index"] = json!(
            siblings
                .iter()
                .position(|id| *id == selected["id"])
                .unwrap()
                + 1
        );
        selected["siblings"] = json!(siblings);
        let active = c["active_messages"].as_array_mut().unwrap();
        active.pop();
        active.push(selected);
        c["active_leaf"] = body["message_id"].clone();
        c["chat"]["version"] = json!(c["chat"]["version"].as_i64().unwrap() + 1);
        state.mutations += 1;
        return (200, c.clone());
    }
    fail(404, "No fixture route.")
}
