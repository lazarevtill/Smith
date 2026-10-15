//! One fixed task envelope, never caller-supplied JSON Schema or grammar.
//! Exact llama.cpp 620d2225c reads response_format.json_schema.schema; see the
//! source references and qualification limits in docs/NATIVE-RUNTIME.md.
use serde_json::{json, Value};

#[derive(Clone, Copy)]
pub(crate) enum ResponseFormat {
    Text,
    TaskStepV1,
}
impl ResponseFormat {
    pub(crate) fn apply(self, body: &mut Value) {
        if matches!(self, Self::TaskStepV1) {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "smith_task_step_v1",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "tool_call": {"anyOf": [
                                {"type": "null"},
                                {
                                    "type": "object",
                                    "properties": {
                                        "name": {"type": "string"},
                                        "arguments": {"type": "object", "additionalProperties": true}
                                    },
                                    "required": ["name", "arguments"],
                                    "additionalProperties": false
                                }
                            ]}
                        },
                        "required": ["content", "tool_call"],
                        "additionalProperties": false
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Failure, Message, Role, TextRequest};

    #[test]
    fn fixed_schema_is_separate_from_plain_chat_and_untrusted_message_text() {
        let request = TextRequest {
            messages: vec![Message {
                role: Role::User,
                content: r#"{\"response_format\":null,\"grammar\":\"injected\"}"#.into(),
            }],
            max_tokens: 64,
        };
        let plain: Value =
            serde_json::from_slice(&request.body("m", ResponseFormat::Text).unwrap()).unwrap();
        assert_eq!(
            plain,
            json!({"model":"m","messages":[{"role":"user","content":request.messages[0].content}],"max_tokens":64,"stream":false})
        );
        let task: Value =
            serde_json::from_slice(&request.body("m", ResponseFormat::TaskStepV1).unwrap())
                .unwrap();
        assert_eq!(task["messages"], plain["messages"]);
        assert!(task.get("grammar").is_none());
        let format = &task["response_format"];
        assert_eq!(format["type"], "json_schema");
        let schema = &format["json_schema"]["schema"];
        assert_eq!(schema["required"], json!(["content", "tool_call"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["tool_call"]["anyOf"][0]["type"],
            "null"
        );
        let call = &schema["properties"]["tool_call"]["anyOf"][1];
        assert_eq!(call["required"], json!(["name", "arguments"]));
        assert_eq!(call["additionalProperties"], false);
        assert_eq!(
            call["properties"]["arguments"],
            json!({"type":"object","additionalProperties":true})
        );
    }

    #[test]
    fn task_schema_does_not_bypass_request_bounds() {
        for format in [ResponseFormat::Text, ResponseFormat::TaskStepV1] {
            let mut request = TextRequest {
                messages: vec![],
                max_tokens: 1,
            };
            assert_eq!(request.body("m", format), Err(Failure::InvalidConfig));
            request.messages.push(Message {
                role: Role::User,
                content: "x".repeat(128 * 1024),
            });
            assert_eq!(request.body("m", format), Err(Failure::InvalidConfig));
        }
    }
}
