//! Concise, display-only historical rows. Exact typed values stay in Details.
use crate::historical_memory_bridge::{ExactValue, HistoricalData};

fn preview(value: &str, limit: usize) -> String {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}
fn value(value: Option<&ExactValue>) -> String {
    match value {
        Some(ExactValue::Text(s) | ExactValue::Integer(s)) if !s.trim().is_empty() => {
            preview(s, 160)
        }
        _ => "Not recorded".into(),
    }
}
fn dates(created: Option<&ExactValue>, end: Option<&ExactValue>) -> String {
    let mut text = format!("Created: {}", value(created));
    if let Some(ExactValue::Text(s)) = end {
        if !s.trim().is_empty() {
            text.push_str(&format!(" · Valid until: {}", preview(s, 80)));
        }
    }
    text
}
pub fn summary(data: &HistoricalData) -> String {
    data.summary()
        .map(|s| preview(s, 280))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Summary unavailable · see Details".into())
}
pub fn metadata(data: &HistoricalData) -> String {
    let scope = match data.agent_id() {
        Some(ExactValue::Null) => "Unscoped".into(),
        Some(ExactValue::Text(s)) if !s.trim().is_empty() => format!("Agent: {}", preview(s, 80)),
        _ => "Agent not recorded".into(),
    };
    let mut fields = vec![scope];
    if let Some(ExactValue::Text(s)) = data.session_id() {
        if !s.trim().is_empty() {
            fields[0].push_str(&format!(" · Session: {}", preview(s, 80)));
        }
    }
    match data {
        HistoricalData::Episode {
            original_pinned,
            created_at,
            valid_to,
            ..
        } => {
            let pinned = match original_pinned.as_ref() {
                Some(ExactValue::Integer(v)) if v == "1" => "Pinned",
                Some(ExactValue::Integer(v)) if v == "0" => "Not pinned",
                _ => "Pin not recorded",
            };
            fields.push(format!(
                "{pinned} · {}",
                dates(created_at.as_ref(), valid_to.as_ref())
            ));
            // Salience's exact floating-point representation remains in Details.
            // A row does not reinterpret malformed or non-finite original bytes.
        }
        HistoricalData::Lesson {
            polarity,
            scope,
            root_cause,
            corrected_behavior,
            usage_count,
            success_count,
            created_at,
            valid_to,
            ..
        } => {
            let label = match polarity.as_ref().and_then(ExactValue::text) {
                Some("cautionary") => "Cautionary lesson",
                Some("guiding") => "Guiding lesson",
                _ => "Unrecognized polarity · see Details",
            };
            fields.push(format!("{label} · Scope: {}", value(scope.as_ref())));
            fields.push(format!(
                "Root cause: {}\nCorrection: {}",
                value(root_cause.as_ref()),
                value(corrected_behavior.as_ref())
            ));
            fields.push(format!(
                "Uses: {} · Successes: {}",
                value(usage_count.as_ref()),
                value(success_count.as_ref())
            ));
            fields.push(dates(created_at.as_ref(), valid_to.as_ref()));
        }
    }
    fields.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn summaries_are_concise_without_changing_exact_original_values() {
        let data:HistoricalData=serde_json::from_value(json!({"kind":"episode","agent_id":{"type":"null"},"session_id":{"type":"null"},"summary":{"type":"text","value":"Synthetic\n episode"},"original_pinned":{"type":"integer","value":"1"},"salience":{"type":"real_bits","value":"000000000000f83f"},"created_at":{"type":"text","value":"2025-01-01T00:00:00Z"},"valid_to":{"type":"null"}})).unwrap();
        let before = serde_json::to_value(&data).unwrap();
        let shown = metadata(&data);
        assert_eq!(summary(&data), "Synthetic episode");
        assert!(
            shown.contains("Unscoped") && shown.contains("Pinned") && shown.contains("2025-01-01")
        );
        assert!(!shown.contains("NULL") && !shown.contains("REAL") && !shown.contains("f83f"));
        assert_eq!(serde_json::to_value(&data).unwrap(), before);
    }
    #[test]
    fn unknown_lesson_polarity_and_large_integer_stay_truthful() {
        let data:HistoricalData=serde_json::from_value(json!({"kind":"lesson","polarity":{"type":"text","value":"unknown-original-polarity"},"usage_count":{"type":"integer","value":"9223372036854775807"},"trigger":{"type":"text","value":"x".repeat(400)}})).unwrap();
        let shown = metadata(&data);
        assert!(shown.contains("Unrecognized polarity") && shown.contains("9223372036854775807"));
        assert_eq!(summary(&data).chars().count(), 281);
        assert_eq!(data.summary().unwrap().len(), 400);
    }
}
