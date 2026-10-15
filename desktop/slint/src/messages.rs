//! Local message presentation only. Fenced code is inert selectable text; this
//! parser never loads images, opens links, executes code, or interprets HTML.
use crate::bridge::{encode_component, Bridge, Result, Transport};
use serde_json::json;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Markdown(String),
    Heading(u8, String),
    Code { language: String, text: String },
}
/// Display derivation only. Callers keep the original content for persistence,
/// context, Copy and export. Ambiguous/partial markup remains literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Presentation<'a> {
    pub answer: &'a str,
    pub reasoning: &'a str,
    pub derived: bool,
}
pub fn presentation<'a>(
    role: &str,
    raw: &'a str,
    stored_reasoning: Option<&'a str>,
) -> Presentation<'a> {
    let literal = Presentation {
        answer: raw,
        reasoning: stored_reasoning.unwrap_or(""),
        derived: false,
    };
    if role != "assistant" || stored_reasoning.is_some() || raw.len() > 65_536 {
        return literal;
    }
    let Some(body) = raw
        .strip_prefix("<think>\r\n")
        .or_else(|| raw.strip_prefix("<think>\n"))
    else {
        return literal;
    };
    let Some(close) = body.find("</think>") else {
        return literal;
    };
    if close > 16_384 || (close > 0 && !body[..close].ends_with('\n')) {
        return literal;
    }
    let thought = &body[..close];
    let tail = &body[close + "</think>".len()..];
    if (!tail.is_empty() && !tail.starts_with('\n') && !tail.starts_with("\r\n"))
        || body.contains("<think>")
        || tail.contains("</think>")
        || thought.lines().any(|line| {
            fence(line).is_some()
                || line.trim_start().starts_with('>')
                || (!line.trim().is_empty()
                    && (line.starts_with("    ") || line.trim_start_matches(' ').starts_with('\t')))
        })
    {
        return literal;
    }
    Presentation {
        answer: tail.trim_start_matches(['\r', '\n']),
        reasoning: if thought.trim().is_empty() {
            ""
        } else {
            thought
        },
        derived: true,
    }
}
fn fence(line: &str) -> Option<(char, usize, &str)> {
    let line = line.trim_start();
    let c = line.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = line.chars().take_while(|v| *v == c).count();
    (n >= 3).then(|| (c, n, line[n..].trim()))
}
pub fn segments(text: &str) -> Vec<Segment> {
    let mut result = Vec::new();
    let mut paragraph = String::new();
    let mut code: Option<(char, usize, String, String)> = None;
    fn flush(out: &mut Vec<Segment>, text: &mut String) {
        if !text.is_empty() {
            out.push(Segment::Markdown(std::mem::take(text)));
        }
    }
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if let Some((c, n, _, body)) = &mut code {
            if fence(trimmed)
                .is_some_and(|(f, width, tail)| f == *c && width >= *n && tail.is_empty())
            {
                let (_, _, language, text) = code.take().unwrap();
                result.push(Segment::Code { language, text });
            } else {
                body.push_str(line);
            }
            continue;
        }
        if let Some((c, n, tag)) = fence(trimmed) {
            flush(&mut result, &mut paragraph);
            let language = tag
                .split_whitespace()
                .next()
                .unwrap_or("")
                .chars()
                .take(32)
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.'))
                .collect();
            code = Some((c, n, language, String::new()));
            continue;
        }
        let heading = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=3).contains(&heading) && trimmed.as_bytes().get(heading) == Some(&b' ') {
            flush(&mut result, &mut paragraph);
            result.push(Segment::Heading(
                heading as u8,
                trimmed[heading + 1..].to_owned(),
            ));
        } else {
            paragraph.push_str(line);
        }
    }
    if let Some((_, _, language, text)) = code {
        result.push(Segment::Code { language, text });
    }
    flush(&mut result, &mut paragraph);
    result
}
#[derive(Clone)]
pub struct FeedbackReview {
    session: String,
    chat: String,
    chat_version: i64,
    message: String,
    revision: i64,
}
impl<T: Transport> Bridge<T> {
    pub fn feedback_review(&self, id: &str) -> Result<FeedbackReview> {
        let chat = self
            .view
            .conversation
            .as_ref()
            .ok_or("Open the saved answer first.")?;
        let message = chat
            .active_messages
            .iter()
            .find(|m| m.id == id)
            .ok_or("Open this answer's active branch first.")?;
        if message.role != "assistant"
            || !message.persisted
            || message.content.trim().is_empty()
            || !matches!(
                message.status.as_str(),
                "completed" | "limited" | "failed" | "interrupted" | "cancelled"
            )
        {
            return Err(
                "Feedback is available after a saved assistant answer has finished.".into(),
            );
        }
        Ok(FeedbackReview {
            session: self.session.clone().ok_or("Workspace unavailable.")?,
            chat: chat.chat.id.clone(),
            chat_version: chat.chat.version,
            message: message.id.clone(),
            revision: message.feedback_revision,
        })
    }
    pub fn feedback(&mut self, review: &FeedbackReview, kind: &str) -> Result<()> {
        if !matches!(kind, "" | "thumbs_up" | "thumbs_down") {
            return Err("Choose thumbs up, thumbs down, or clear.".into());
        }
        if self.session.as_deref() != Some(&review.session) {
            return Err("Stale feedback review.".into());
        }
        self.check_session()?;
        let response=self.transport.request("PATCH",&format!("/api/chats/{}/messages/{}/feedback",encode_component(&review.chat),encode_component(&review.message)),Some(&review.session),Some(json!({"chat_version":review.chat_version,"expected_revision":review.revision,"kind":kind})));
        self.check_session()?;
        let receipt = response?;
        if receipt["message_id"] != review.message
            || receipt["kind"] != kind
            || receipt["revision"].as_i64() != review.revision.checked_add(1)
        {
            return Err(
                "Feedback receipt did not match the reviewed answer. Inspect before retrying."
                    .into(),
            );
        }
        if self
            .view
            .conversation
            .as_ref()
            .is_some_and(|c| c.chat.id == review.chat)
        {
            self.refresh_conversation(&review.chat)?;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_leading_thinking_is_display_only_and_empty_reasoning_has_no_disclosure() {
        let raw = "<think>\n\n</think>\n\nAnswer.";
        let shown = presentation("assistant", raw, None);
        assert_eq!(
            shown,
            Presentation {
                answer: "Answer.",
                reasoning: "",
                derived: true
            }
        );
        assert_eq!(raw, "<think>\n\n</think>\n\nAnswer.");
        let raw = "<think>\r\nConsider the supplied example.\r\n</think>\r\nAnswer.";
        let shown = presentation("assistant", raw, None);
        assert_eq!(shown.answer, "Answer.");
        assert_eq!(shown.reasoning, "Consider the supplied example.\r\n");
        assert!(shown.derived);
    }
    #[test]
    fn thinking_literal_partial_quoted_code_nested_and_oversized_cases_remain_visible() {
        for raw in [
            "```xml\n<think>\nexample\n</think>\n```",
            "> <think>\nquoted\n</think>\nAnswer",
            "<think>\nunfinished",
            "<think>\ninline</think>answer",
            "<think>\n<think>\nnested\n</think>\nAnswer",
            "<think>\n```rust\ncode\n```\n</think>\nAnswer",
            "<think>\n> quoted\n</think>\nAnswer",
            "<think>\n    literal code example\n</think>\nAnswer",
            "<think>\n\tliteral code example\n</think>\nAnswer",
            "<think>\n \tliteral code example\n</think>\nAnswer",
            "<think>\n   \tliteral code example\n</think>\nAnswer",
        ] {
            let shown = presentation("assistant", raw, None);
            assert_eq!(shown.answer, raw);
            assert!(!shown.derived);
        }
        let raw = format!("<think>\n{}\n</think>\nAnswer", "x".repeat(16_385));
        assert!(!presentation("assistant", &raw, None).derived);
        let raw = format!("<think>\n\n</think>\n{}", "a".repeat(65_536));
        assert!(!presentation("assistant", &raw, None).derived);
        let raw = "<think>\n\n</think>\nAnswer";
        assert!(!presentation("user", raw, None).derived);
        let shown = presentation("assistant", raw, Some("Exact imported reasoning"));
        assert_eq!(shown.answer, raw);
        assert_eq!(shown.reasoning, "Exact imported reasoning");
        assert!(!shown.derived);
        let whitespace = presentation("assistant", "<think>\n    \n\t\n</think>\nAnswer", None);
        assert!(whitespace.derived);
        assert!(whitespace.reasoning.is_empty());
    }
    #[test]
    fn headings_fences_and_partial_stream_are_inert() {
        assert_eq!(
            segments("# Heading\n\n```rust\nfn main() {}\n```\nTail"),
            vec![
                Segment::Heading(1, "Heading".into()),
                Segment::Markdown("\n".into()),
                Segment::Code {
                    language: "rust".into(),
                    text: "fn main() {}\n".into()
                },
                Segment::Markdown("Tail".into())
            ]
        );
        assert_eq!(
            segments("~~~html\n<script>x</script>\n"),
            vec![Segment::Code {
                language: "html".into(),
                text: "<script>x</script>\n".into()
            }]
        );
    }
    #[test]
    fn shorter_or_mismatched_fences_are_code() {
        assert_eq!(
            segments("````txt\na\n```\n~~~\n````"),
            vec![Segment::Code {
                language: "txt".into(),
                text: "a\n```\n~~~\n".into()
            }]
        );
    }
    #[test]
    fn links_and_images_remain_text_data() {
        assert_eq!(
            segments("![remote](https://example.invalid/p.png) <script>x</script>"),
            vec![Segment::Markdown(
                "![remote](https://example.invalid/p.png) <script>x</script>".into()
            )]
        );
    }
}
