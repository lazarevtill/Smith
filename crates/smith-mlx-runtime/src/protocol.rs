use crate::*;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::{json, Value};
use std::fmt;

// Duplicate keys are errors at every level; serde_json::Value alone would keep
// only the last copy. Frame bytes and serde's recursion limit bound allocation.
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("bounded JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| Unique(Value::Number(n)))
                    .ok_or_else(|| E::custom("finite number required"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut v = Vec::new();
                while let Some(x) = a.next_element::<Unique>()? {
                    if v.len() >= 8192 {
                        return Err(de::Error::custom("array bound"));
                    }
                    v.push(x.0)
                }
                Ok(Unique(v.into()))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut v = serde_json::Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if v.len() >= 8192 || v.contains_key(&k) {
                        return Err(de::Error::custom("duplicate or excess key"));
                    }
                    v.insert(k, a.next_value::<Unique>()?.0);
                }
                Ok(Unique(Value::Object(v)))
            }
        }
        d.deserialize_any(V)
    }
}
#[derive(Debug)]
pub(crate) struct Frame {
    pub id: String,
    pub event: String,
    pub value: Value,
}
fn depth(v: &Value, n: u8) -> bool {
    n <= 12
        && match v {
            Value::Array(a) => a.iter().all(|x| depth(x, n + 1)),
            Value::Object(o) => o.values().all(|x| depth(x, n + 1)),
            _ => true,
        }
}
pub(crate) fn parse(bytes: &[u8]) -> Result<Frame> {
    if bytes.len() > FRAME_BYTES {
        return Err(Failure::OutputLimit);
    }
    let Unique(value) = serde_json::from_slice(bytes).map_err(|_| Failure::Protocol)?;
    if !depth(&value, 0) || value["v"].as_u64() != Some(1) {
        return Err(Failure::Protocol);
    }
    let id = value["id"]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
        .ok_or(Failure::Protocol)?
        .to_string();
    let event = value["event"]
        .as_str()
        .filter(|s| ["started", "delta", "done", "error"].contains(s))
        .ok_or(Failure::Protocol)?
        .to_string();
    Ok(Frame { id, event, value })
}
pub(crate) fn keys(value: &Value, extra: &[&str]) -> Result<()> {
    let o = value.as_object().ok_or(Failure::Protocol)?;
    if o.len() != 3 + extra.len()
        || !o
            .keys()
            .all(|k| ["v", "id", "event"].contains(&k.as_str()) || extra.contains(&k.as_str()))
    {
        return Err(Failure::Protocol);
    }
    Ok(())
}
pub(crate) fn hello(frame: Frame, id: &str) -> Result<WorkerFeatures> {
    if frame.id != id || frame.event != "done" {
        return Err(Failure::Protocol);
    }
    keys(
        &frame.value,
        &[
            "worker",
            "protocol",
            "backend",
            "capabilities",
            "max_frame_bytes",
            "max_input_tokens",
            "max_output_tokens",
        ],
    )?;
    let v = frame.value;
    if v["worker"] != "smith-mlx-worker"
        || v["protocol"] != 1
        || v["backend"] != "mlx-swift-metal"
        || v["max_frame_bytes"] != FRAME_BYTES
        || v["max_input_tokens"] != 8192
        || v["max_output_tokens"] != 2048
    {
        return Err(Failure::Protocol);
    }
    let qwen3_thinking_control_v1 = if v["capabilities"] == json!(["inspect", "local_text"]) {
        false
    } else if v["capabilities"] == json!(["inspect", "local_text", "qwen3_thinking_control_v1"]) {
        true
    } else {
        return Err(Failure::Protocol);
    };
    Ok(WorkerFeatures {
        qwen3_thinking_control_v1,
    })
}
pub(crate) fn worker_error(frame: &Frame) -> Result<Failure> {
    keys(&frame.value, &["code", "message"])?;
    let code = frame.value["code"]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
        .ok_or(Failure::Protocol)?;
    if frame.value["message"]
        .as_str()
        .is_none_or(|s| s.len() > 512)
    {
        return Err(Failure::Protocol);
    }
    Ok(Failure::Worker { code: code.into() })
}
pub(crate) fn manifest(frame: &Frame, expected: &ModelSelection) -> Result<Manifest> {
    keys(&frame.value, &["manifest", "capabilities"])?;
    let m: Manifest =
        serde_json::from_value(frame.value["manifest"].clone()).map_err(|_| Failure::Protocol)?;
    let caps: Vec<Capability> = serde_json::from_value(frame.value["capabilities"].clone())
        .map_err(|_| Failure::Protocol)?;
    if !["llama", "mistral", "qwen2", "qwen3", "granite"].contains(&m.model_type.as_str())
        || !(64..=10240).contains(&m.context_limit)
        || m.files != expected.files
        || m.package_sha256 != expected.package_sha256
        || caps != m.capabilities()
    {
        return Err(Failure::Protocol);
    }
    Ok(m)
}
pub(crate) fn request(id: &str, request: &Request, model: &ModelSelection) -> Result<Value> {
    Ok(match request {
        Request::Inspect => {
            json!({"v":1,"id":id,"op":"inspect","model_root":model.root,"expected_package_sha256":model.package_sha256})
        }
        Request::Load => {
            json!({"v":1,"id":id,"op":"load","model_root":model.root,"expected_package_sha256":model.package_sha256})
        }
        Request::Unload => json!({"v":1,"id":id,"op":"unload"}),
        Request::Generate(g) => {
            let (mut v, sampling) = match g {
                GenerationRequest::Raw { prompt, sampling } => (
                    json!({"v":1,"id":id,"op":"generate","mode":"raw","prompt":prompt}),
                    sampling,
                ),
                GenerationRequest::Chat { messages, sampling }
                | GenerationRequest::ChatWithThinking {
                    messages, sampling, ..
                } => (
                    json!({"v":1,"id":id,"op":"generate","mode":"chat","messages":messages}),
                    sampling,
                ),
            };
            if let Some(mode) = g.wire_thinking_mode() {
                v["thinking_mode"] =
                    serde_json::to_value(mode).map_err(|_| Failure::InvalidRequest)?;
            }
            for (key, value) in serde_json::to_value(sampling)
                .map_err(|_| Failure::InvalidRequest)?
                .as_object()
                .unwrap()
            {
                if !value.is_null() {
                    v[key] = value.clone();
                }
            }
            v
        }
    })
}
pub(crate) fn patch(frame: &Frame, progress: &mut Progress) -> Result<()> {
    keys(&frame.value, &["replace_from_utf8", "text"])?;
    let offset = frame.value["replace_from_utf8"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or(Failure::Protocol)?;
    let suffix = frame.value["text"].as_str().ok_or(Failure::Protocol)?;
    if suffix.len() > 16384
        || offset > progress.text.len()
        || !progress.text.is_char_boundary(offset)
    {
        return Err(Failure::Protocol);
    }
    if offset
        .checked_add(suffix.len())
        .is_none_or(|n| n > OUTPUT_BYTES)
    {
        return Err(Failure::OutputLimit);
    }
    progress.text.truncate(offset);
    progress.text.push_str(suffix);
    progress.revision += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_feature_negotiation_is_finite_and_unknown_modes_are_not_defaults() {
        let frame = |caps: Value| {
            parse(
                &serde_json::to_vec(&json!({
                    "v":1,"id":"hello","event":"done","worker":"smith-mlx-worker",
                    "protocol":1,"backend":"mlx-swift-metal","capabilities":caps,
                    "max_frame_bytes":131072,"max_input_tokens":8192,"max_output_tokens":2048
                }))
                .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            hello(frame(json!(["inspect", "local_text"])), "hello").unwrap(),
            WorkerFeatures::default()
        );
        assert!(
            hello(
                frame(json!([
                    "inspect",
                    "local_text",
                    "qwen3_thinking_control_v1"
                ])),
                "hello"
            )
            .unwrap()
            .qwen3_thinking_control_v1
        );
        for caps in [
            json!(null),
            json!(["inspect", "local_text", "unknown"]),
            json!(["local_text", "inspect"]),
            json!([
                "inspect",
                "local_text",
                "qwen3_thinking_control_v1",
                "qwen3_thinking_control_v1"
            ]),
        ] {
            assert_eq!(hello(frame(caps), "hello"), Err(Failure::Protocol));
        }
        for value in [
            json!(null),
            json!(false),
            json!(0),
            json!("enabled"),
            json!("auto"),
            json!("DISABLED"),
        ] {
            assert!(serde_json::from_value::<ThinkingMode>(value).is_err());
        }
    }

    #[test]
    fn default_wire_is_identical_and_disabled_requires_feature_and_exact_template() {
        let model = ModelSelection {
            root: "/selected/model".into(),
            files: vec![],
            package_sha256: "a".repeat(64),
        };
        let messages = vec![Message {
            role: Role::User,
            content: "literal /no_think".into(),
        }];
        let default = GenerationRequest::Chat {
            messages: messages.clone(),
            sampling: Sampling::default(),
        };
        let explicit = |thinking_mode| GenerationRequest::ChatWithThinking {
            messages: messages.clone(),
            sampling: Sampling::default(),
            thinking_mode,
        };
        let default_wire = request("x", &Request::Generate(default), &model).unwrap();
        assert_eq!(
            default_wire,
            request(
                "x",
                &Request::Generate(explicit(ThinkingMode::ModelDefault)),
                &model
            )
            .unwrap()
        );
        assert!(default_wire.get("thinking_mode").is_none());
        let disabled = explicit(ThinkingMode::Disabled);
        let wire = request("x", &Request::Generate(disabled.clone()), &model).unwrap();
        assert_eq!(wire["thinking_mode"], "disabled");
        assert_eq!(wire["messages"][0]["content"], "literal /no_think");
        let mut manifest = Manifest {
            model_type: "qwen3".into(),
            files: vec![],
            package_sha256: model.package_sha256,
            template: Some(Template::Qwen3DefaultThinking),
            context_limit: 4096,
        };
        assert_eq!(
            disabled.validate(&manifest, WorkerFeatures::default()),
            Err(Failure::UnsupportedCapability)
        );
        let features = WorkerFeatures {
            qwen3_thinking_control_v1: true,
        };
        assert!(disabled.validate(&manifest, features).is_ok());
        for template in [
            None,
            Some(Template::SimpleChatml),
            Some(Template::SimpleGranite),
        ] {
            manifest.template = template;
            assert_eq!(
                disabled.validate(&manifest, features),
                Err(Failure::UnsupportedCapability)
            );
        }
    }

    #[test]
    fn rejects_duplicate_nested_keys_null_ids_depth_and_excess_frames() {
        for bytes in [
            br#"{"v":1,"id":"test","event":"done","manifest":{"x":1,"x":2}}"#.as_slice(),
            br#"{"v":1,"id":null,"event":"error","code":"invalid","message":"redacted"}"#
                .as_slice(),
            br#"{"v":1,"id":"test","event":"done"} trailing"#.as_slice(),
        ] {
            assert!(matches!(parse(bytes), Err(Failure::Protocol)));
        }
        let mut deep = json!(0);
        for _ in 0..13 {
            deep = json!([deep]);
        }
        assert!(matches!(
            parse(
                &serde_json::to_vec(&json!({"v":1,"id":"test","event":"done","nested":deep}))
                    .unwrap()
            ),
            Err(Failure::Protocol)
        ));
        assert!(matches!(
            parse(&vec![b' '; FRAME_BYTES + 1]),
            Err(Failure::OutputLimit)
        ));
    }

    #[test]
    fn output_limit_is_atomic_and_replacements_can_shrink_unicode_text() {
        let mut p = Progress {
            request_id: "test".into(),
            revision: 0,
            text: "a".repeat(OUTPUT_BYTES),
            incomplete: true,
            finished: false,
        };
        let frame = |offset: usize, text: &str| {
            parse(&serde_json::to_vec(&json!({"v":1,"id":"test","event":"delta","replace_from_utf8":offset,"text":text})).unwrap()).unwrap()
        };
        assert_eq!(
            patch(&frame(OUTPUT_BYTES, "!"), &mut p),
            Err(Failure::OutputLimit)
        );
        assert_eq!(p.text.len(), OUTPUT_BYTES);
        assert_eq!(p.revision, 0);
        patch(&frame(0, "é🌍"), &mut p).unwrap();
        assert_eq!(p.text, "é🌍");
        assert_eq!(
            patch(&frame(1, "invalid boundary"), &mut p),
            Err(Failure::Protocol)
        );
        assert_eq!(p.text, "é🌍");
        patch(&frame(2, ""), &mut p).unwrap();
        assert_eq!(p.text, "é");
        assert_eq!(p.revision, 2);
    }

    #[test]
    fn raw_and_chat_stay_distinct_and_load_is_bound_to_reviewed_package() {
        let model = ModelSelection {
            root: "/selected/model".into(),
            files: vec![],
            package_sha256: "a".repeat(64),
        };
        let load = request("load", &Request::Load, &model).unwrap();
        assert_eq!(load["expected_package_sha256"], model.package_sha256);
        let raw = request(
            "raw",
            &Request::Generate(GenerationRequest::Raw {
                prompt: "literal input".into(),
                sampling: Sampling::default(),
            }),
            &model,
        )
        .unwrap();
        assert_eq!(raw["mode"], "raw");
        assert_eq!(raw["prompt"], "literal input");
        assert!(raw.get("messages").is_none());
        assert!(raw.get("seed").is_none());
        let manifest = Manifest {
            model_type: "llama".into(),
            files: vec![],
            package_sha256: model.package_sha256,
            template: None,
            context_limit: 4096,
        };
        let chat = GenerationRequest::Chat {
            messages: vec![Message {
                role: Role::User,
                content: "hi".into(),
            }],
            sampling: Sampling::default(),
        };
        assert_eq!(
            chat.validate(&manifest, WorkerFeatures::default()),
            Err(Failure::UnsupportedCapability)
        );
        let excessive = GenerationRequest::Raw {
            prompt: "a".repeat(65537),
            sampling: Sampling::default(),
        };
        assert_eq!(
            excessive.validate(&manifest, WorkerFeatures::default()),
            Err(Failure::InvalidRequest)
        );
    }
}
