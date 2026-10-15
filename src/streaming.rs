//! Incremental wire framing. UTF-8 is decoded only after a complete line arrives.
//! SSE events dispatch only at a blank line, per the event-stream specification.
use serde_json::Value;
const FRAME_LIMIT: usize = 1024 * 1024;
/// Ordered frames preserve all earlier data when a later frame fails in the
/// same network read. A terminal parser error permanently stops further data.
pub type Frame = Result<String, String>;
#[derive(Default)]
pub struct Lines {
    pending: Vec<u8>,
    skip_lf: bool,
    started: bool,
    failed: bool,
}
impl Lines {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        if self.failed {
            return out;
        }
        for &b in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if b == b'\n' {
                    continue;
                }
            }
            if b == b'\r' || b == b'\n' {
                let line = self.line();
                let failed = line.is_err();
                out.push(line);
                self.skip_lf = b == b'\r';
                if failed {
                    self.failed = true;
                    break;
                }
            } else {
                self.pending.push(b);
                if self.pending.len() > FRAME_LIMIT {
                    out.push(Err("Server sent an oversized streaming line.".into()));
                    self.failed = true;
                    break;
                }
            }
        }
        out
    }
    fn line(&mut self) -> Frame {
        let bytes = std::mem::take(&mut self.pending);
        let mut line =
            String::from_utf8(bytes).map_err(|_| "Server sent invalid UTF-8.".to_string())?;
        if !self.started {
            self.started = true;
            if line.starts_with('\u{feff}') {
                line.remove(0);
            }
        }
        Ok(line)
    }
    pub fn finish(&mut self) -> Result<Option<String>, String> {
        if self.pending.is_empty() || self.failed {
            Ok(None)
        } else {
            self.line().map(Some)
        }
    }
}
#[derive(Default)]
pub struct Sse {
    lines: Lines,
    data: Vec<String>,
    size: usize,
    event: String,
    failed: bool,
}
impl Sse {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        if self.failed {
            return out;
        }
        for line in self.lines.feed(bytes) {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    out.push(Err(error));
                    self.failed = true;
                    break;
                }
            };
            if line.is_empty() {
                if self.event == "error" {
                    out.push(Err(
                        "Model server emitted an error event. Partial answer is saved.".into(),
                    ));
                    self.failed = true;
                    break;
                }
                self.event.clear();
                if !self.data.is_empty() {
                    out.push(Ok(self.data.join("\n")));
                }
                self.data.clear();
                self.size = 0;
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((&line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            if field == "event" {
                self.event = value.into();
            }
            if field == "data" {
                self.size += value.len() + 1;
                if self.size > FRAME_LIMIT {
                    out.push(Err("Server sent an oversized SSE event.".into()));
                    self.failed = true;
                    break;
                }
                self.data.push(value.into());
            }
        }
        out
    }
    // An unterminated event at EOF is intentionally discarded, never dispatched.
}
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub status: String,
    pub notice: Option<String>,
    pub finish_reason: String,
    pub transport_incomplete: bool,
}
#[derive(Default)]
pub struct Completion {
    finish: Option<String>,
    pub done: bool,
}
impl Completion {
    pub fn accept(&mut self, data: &str) -> Result<Option<String>, String> {
        if data == "[DONE]" {
            if self.finish.is_none() {
                return Err(
                    "Server sent [DONE] without a completion reason. Partial answer is saved."
                        .into(),
                );
            }
            self.done = true;
            return Ok(None);
        }
        let v: Value = serde_json::from_str(data).map_err(|_| {
            "Server sent invalid streaming JSON. Partial answer is saved.".to_string()
        })?;
        if v.get("error").is_some() {
            return Err("Model server reported an error. Check its logs and retry.".into());
        }
        let Some(choices) = v["choices"].as_array() else {
            return Err("Server sent no choices array.".into());
        };
        let Some(choice) = choices
            .iter()
            .find(|c| c["index"].as_u64().unwrap_or(0) == 0)
        else {
            return Ok(None);
        };
        if let Some(reason) = choice["finish_reason"].as_str() {
            if self.finish.as_ref().is_some_and(|r| r != reason) {
                return Err("Server sent conflicting completion reasons.".into());
            }
            self.finish = Some(reason.into());
        }
        if choice["delta"].get("tool_calls").is_some()
            || choice["delta"].get("function_call").is_some()
        {
            return Err("This model requested tools, which are disabled in this chat. Partial answer is saved.".into());
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            match reason {
                "stop" | "length" => {}
                "tool_calls" | "function_call" => {
                    return Err(
                        "Model requested tools, which are disabled. Partial answer is saved."
                            .into(),
                    )
                }
                "content_filter" => {
                    return Err("Model server stopped this answer with a content filter.".into())
                }
                _ => {
                    return Err(format!(
                        "Unsupported completion reason: {reason}. Partial answer is saved."
                    ))
                }
            }
            if self.finish.as_ref().is_some_and(|r| r != reason) {
                return Err("Server sent conflicting completion reasons.".into());
            }
            self.finish = Some(reason.into());
        }
        Ok(choice["delta"]["content"].as_str().map(str::to_owned))
    }
    pub fn finish_reason(&self) -> Option<&str> {
        self.finish.as_deref()
    }
    pub fn outcome(&self) -> Result<Outcome, String> {
        let reason = self.finish.as_deref().ok_or_else(|| {
            "Server disconnected before confirming completion. Partial answer is saved.".to_string()
        })?;
        if !matches!(reason, "stop" | "length") {
            return Err("Model did not confirm a supported text completion.".into());
        }
        Ok(Outcome {
            finish_reason: reason.into(),
            transport_incomplete: !self.done,
            status: if reason == "length" {
                "limited"
            } else {
                "completed"
            }
            .into(),
            notice: if !self.done {
                Some(if reason=="length" {"The model reached its output or context limit. The incomplete answer is saved. Transport also ended without [DONE]; do not retry automatically."}else{"Transport ended after the model confirmed completion. The answer is saved; do not retry automatically."}.into())
            } else if reason == "length" {
                Some("The model reached its output or context limit. This saved answer is incomplete.".into())
            } else {
                None
            },
        })
    }
}
pub fn ollama(data: &str) -> Result<(Option<String>, Option<Outcome>), String> {
    let v: Value = serde_json::from_str(data)
        .map_err(|_| "Server sent invalid streaming JSON. Partial answer is saved.".to_string())?;
    if v.get("error").is_some() {
        return Err("Model server reported an error. Check its logs and retry.".into());
    }
    if v["message"]
        .get("tool_calls")
        .is_some_and(|t| t.as_array().is_none_or(|a| !a.is_empty()))
    {
        return Err("Model requested tools, which are disabled. Partial answer is saved.".into());
    }
    let text = v["message"]["content"].as_str().map(str::to_owned);
    let outcome = if v["done"] == true {
        let reason = v["done_reason"].as_str().unwrap_or("stop");
        Some(match reason{"stop"=>Outcome{status:"completed".into(),notice:None,finish_reason:"stop".into(),transport_incomplete:false},"length"=>Outcome{status:"limited".into(),finish_reason:"length".into(),transport_incomplete:false,notice:Some("The model reached its output or context limit. This saved answer is incomplete.".into())},_=>return Err(format!("Unsupported completion reason: {reason}. Partial answer is saved."))})
    } else {
        None
    };
    Ok((text, outcome))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse(bytes: &[u8], chunk: usize) -> (Vec<String>, Completion) {
        let mut s = Sse::default();
        let mut c = Completion::default();
        let mut text = vec![];
        for bytes in bytes.chunks(chunk) {
            for event in s.feed(bytes) {
                if let Some(t) = c.accept(&event.unwrap()).unwrap() {
                    text.push(t)
                }
            }
        }
        (text, c)
    }
    #[test]
    fn fragmented_utf8_crlf_bom_multiline() {
        for ending in ["\n", "\r\n", "\r"] {
            let body=format!("\u{feff}:comment{e}data: {{\"choices\":[{{\"index\":0,{e}data: \"delta\":{{\"content\":\"Привет 🦀\"}},\"finish_reason\":null}}]}}{e}{e}data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}{e}{e}data: [DONE]{e}{e}",e=ending);
            for chunk in 1..=body.len() {
                let (t, c) = parse(body.as_bytes(), chunk);
                assert_eq!(t.concat(), "Привет 🦀");
                assert_eq!(
                    c.outcome().unwrap(),
                    Outcome {
                        status: "completed".into(),
                        notice: None,
                        finish_reason: "stop".into(),
                        transport_incomplete: false,
                    }
                );
            }
        }
    }
    #[test]
    fn blank_boundary_required() {
        let mut s = Sse::default();
        assert!(s.feed(b"data: [DONE]\n").is_empty());
        assert_eq!(s.feed(b"\n"), vec![Ok("[DONE]".into())]);
    }
    #[test]
    fn done_without_reason_fails() {
        assert!(Completion::default().accept("[DONE]").is_err());
    }
    #[test]
    fn terminal_then_eof_keeps_answer() {
        let(_,c)=parse(b"data: {\"choices\":[{\"delta\":{\"content\":\"saved\"},\"finish_reason\":\"stop\"}]}\n\n",1);
        let o = c.outcome().unwrap();
        assert_eq!(o.status, "completed");
        assert!(o.notice.is_some());
    }
    #[test]
    fn length_is_limited_tools_unknown_fail() {
        for (reason, status) in [("length", "limited"), ("stop", "completed")] {
            let mut c = Completion::default();
            c.accept(&format!(
                "{{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{reason}\"}}]}}"
            ))
            .unwrap();
            assert_eq!(c.outcome().unwrap().status, status);
        }
        for reason in ["tool_calls", "content_filter", "unknown"] {
            assert!(Completion::default()
                .accept(&format!(
                    "{{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{reason}\"}}]}}"
                ))
                .is_err());
        }
    }
    #[test]
    fn ollama_limit_and_error() {
        assert_eq!(
            ollama(r#"{"done":true,"done_reason":"length"}"#)
                .unwrap()
                .1
                .unwrap()
                .status,
            "limited"
        );
        assert!(ollama(r#"{"error":"bad"}"#).is_err());
    }
    #[test]
    fn unterminated_sse_is_not_completion() {
        let (_, c) = parse(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}",
            1,
        );
        assert!(c.outcome().is_err());
    }
    #[test]
    fn explicit_error_event_always_fails() {
        let mut s = Sse::default();
        assert!(s
            .feed(b"event: error\ndata: {\"choices\":[]}\n\n")
            .pop()
            .unwrap()
            .is_err());
    }
    #[test]
    fn earlier_frames_survive_later_error_for_any_chunking() {
        let body=b"data: {\"choices\":[{\"delta\":{\"content\":\"saved prefix\"}}]}\n\nevent: error\ndata: {}\n\ndata: must-not-appear\n\n";
        for chunk in 1..=body.len() {
            let mut parser = Sse::default();
            let mut text = String::new();
            let mut failed = false;
            let mut completion = Completion::default();
            for bytes in body.chunks(chunk) {
                for frame in parser.feed(bytes) {
                    match frame {
                        Ok(event) => {
                            assert!(!failed);
                            if let Some(t) = completion.accept(&event).unwrap() {
                                text.push_str(&t)
                            }
                        }
                        Err(_) => failed = true,
                    }
                }
            }
            assert!(failed);
            assert_eq!(text, "saved prefix");
            assert!(parser.feed(b"data: ignored\n\n").is_empty());
        }
    }
    #[test]
    fn line_error_keeps_earlier_lines() {
        let mut lines = Lines::default();
        let out = lines.feed(b"valid\n\xff\nignored\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], Ok("valid".into()));
        assert!(out[1].is_err());
    }
}
