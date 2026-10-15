//! Immutable requested inference intent. This is historical evidence, never a
//! runtime handle, a permission to load a model, or a claim of successful output.
use crate::*;
use smith_mlx_runtime::{Template, ThinkingMode};

const MAX_BYTES: usize = 16_384;
const MAX_REVISION: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provider {
    Mlx,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProvenanceSampling {
    pub max_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    #[serde(deserialize_with = "present_seed")]
    pub seed: Option<u64>,
}

fn present_seed<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(deserializer)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelProvenance {
    pub format_version: u32,
    pub provider: Provider,
    pub review_id: String,
    pub runtime_plan_id: String,
    /// Revision committed by the successful load, rather than its review input.
    pub settings_revision: u64,
    pub worker_sha256: String,
    pub runtime_bundle_sha256: String,
    pub package_sha256: String,
    pub template: Template,
    pub thinking_mode: ThinkingMode,
    pub sampling: ProvenanceSampling,
}

fn malformed() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Saved inference provenance is malformed or unsupported. Its original record was retained."
            .into(),
    )
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl ModelProvenance {
    pub(crate) fn validate(&self) -> ApiResult<()> {
        if self.format_version != 1
            || self.settings_revision == 0
            || self.settings_revision > MAX_REVISION
            || [
                &self.review_id,
                &self.runtime_plan_id,
                &self.worker_sha256,
                &self.runtime_bundle_sha256,
                &self.package_sha256,
            ]
            .into_iter()
            .any(|value| !digest(value))
            || (self.thinking_mode == ThinkingMode::Disabled
                && self.template != Template::Qwen3DefaultThinking)
            || !(1..=2_048).contains(&self.sampling.max_tokens)
            || !self.sampling.temperature.is_finite()
            || !(0.0..=2.0).contains(&self.sampling.temperature)
            || !self.sampling.top_p.is_finite()
            || self.sampling.top_p <= 0.0
            || self.sampling.top_p > 1.0
        {
            return Err(malformed());
        }
        Ok(())
    }

    fn encode(&self) -> ApiResult<String> {
        self.validate()?;
        let encoded = serde_json::to_string(self).map_err(|_| malformed())?;
        if encoded.len() > MAX_BYTES {
            return Err(malformed());
        }
        Ok(encoded)
    }
}

/// Caller inserts this in the same transaction as the accepted run and its
/// captured destination. INSERT deliberately rejects replacement of history.
pub(crate) fn insert(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
    provenance: &ModelProvenance,
) -> ApiResult<()> {
    let encoded = provenance.encode()?;
    let provider: Option<String> = tx
        .query_row(
            "SELECT provider FROM run_configs WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )
        .optional()?;
    if provider.as_deref() != Some("mlx") {
        return Err(malformed());
    }
    tx.execute(
        "INSERT INTO run_model_provenance(run_id,format_version,provenance) VALUES(?1,1,?2)",
        params![run_id, encoded],
    )?;
    Ok(())
}

/// Absence is unknown legacy provenance. A present malformed value is an error;
/// SQL checks its type and byte length before loading its JSON into memory.
pub(crate) fn read(
    db: &Connection,
    run_id: &str,
    expected_provider: &str,
) -> ApiResult<Option<ModelProvenance>> {
    let row: Option<(Option<i64>, Option<String>)> = db
        .query_row(
            "SELECT CASE WHEN typeof(format_version)='integer' THEN format_version END, CASE WHEN typeof(provenance)='text' AND length(CAST(provenance AS BLOB)) BETWEEN 2 AND 16384 THEN provenance END FROM run_model_provenance WHERE run_id=?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((outer_version, encoded)) = row else {
        return Ok(None);
    };
    if outer_version != Some(1) || expected_provider != "mlx" {
        return Err(malformed());
    }
    let decoded: ModelProvenance =
        serde_json::from_str(encoded.as_deref().ok_or_else(malformed)?).map_err(|_| malformed())?;
    decoded.validate()?;
    Ok(Some(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ModelProvenance {
        ModelProvenance {
            format_version: 1,
            provider: Provider::Mlx,
            review_id: "1".repeat(64),
            runtime_plan_id: "2".repeat(64),
            settings_revision: 7,
            worker_sha256: "3".repeat(64),
            runtime_bundle_sha256: "4".repeat(64),
            package_sha256: "5".repeat(64),
            template: Template::Qwen3DefaultThinking,
            thinking_mode: ThinkingMode::Disabled,
            sampling: ProvenanceSampling {
                max_tokens: 128,
                temperature: 0.0,
                top_p: 1.0,
                seed: Some(1234),
            },
        }
    }

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE run_configs(run_id TEXT PRIMARY KEY,provider TEXT NOT NULL); CREATE TABLE run_model_provenance(run_id TEXT PRIMARY KEY NOT NULL REFERENCES run_configs(run_id),format_version INTEGER NOT NULL,provenance TEXT NOT NULL);").unwrap();
        db
    }

    #[test]
    fn captured_intent_roundtrips_and_missing_history_stays_unknown() {
        let mut db = db();
        assert_eq!(read(&db, "old", "mlx").unwrap(), None);
        let value = sample();
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO run_configs VALUES('run','mlx')", [])
            .unwrap();
        insert(&tx, "run", &value).unwrap();
        assert_eq!(read(&tx, "run", "mlx").unwrap(), Some(value.clone()));
        tx.commit().unwrap();
        assert_eq!(read(&db, "run", "mlx").unwrap(), Some(value));
        assert!(read(&db, "run", "managed").is_err());
    }

    #[test]
    fn duplicate_or_failed_admission_cannot_replace_or_leave_a_record() {
        let mut db = db();
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO run_configs VALUES('run','mlx')", [])
            .unwrap();
        insert(&tx, "run", &sample()).unwrap();
        tx.commit().unwrap();
        let original = read(&db, "run", "mlx").unwrap();
        let tx = db.transaction().unwrap();
        let mut changed = sample();
        changed.settings_revision += 1;
        assert!(insert(&tx, "run", &changed).is_err());
        tx.rollback().unwrap();
        assert_eq!(read(&db, "run", "mlx").unwrap(), original);
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO run_configs VALUES('aborted','mlx')", [])
            .unwrap();
        insert(&tx, "aborted", &sample()).unwrap();
        drop(tx);
        assert_eq!(read(&db, "aborted", "mlx").unwrap(), None);
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO run_configs VALUES('wrong','mock')", [])
            .unwrap();
        assert!(insert(&tx, "wrong", &sample()).is_err());
        assert!(insert(&tx, "missing", &sample()).is_err());
    }

    #[test]
    fn malformed_stored_values_are_retained_and_never_defaulted() {
        let db = db();
        db.execute("INSERT INTO run_configs VALUES('run','mlx')", [])
            .unwrap();
        let valid = serde_json::to_value(sample()).unwrap();
        let mut bad_values = Vec::new();
        for (key, value) in [
            ("format_version", json!(2)),
            ("provider", json!("managed")),
            ("review_id", json!("bad")),
            ("settings_revision", json!(0)),
            ("settings_revision", json!(MAX_REVISION + 1)),
            ("template", json!("simple-chatml-v1")),
            ("thinking_mode", Value::Null),
            ("thinking_mode", json!("auto")),
            ("extra", json!(true)),
        ] {
            let mut changed = valid.clone();
            changed[key] = value;
            bad_values.push(serde_json::to_string(&changed).unwrap());
        }
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove("thinking_mode");
        bad_values.push(serde_json::to_string(&missing).unwrap());
        let mut missing_seed = valid.clone();
        missing_seed["sampling"]
            .as_object_mut()
            .unwrap()
            .remove("seed");
        bad_values.push(serde_json::to_string(&missing_seed).unwrap());
        bad_values.push(" ".repeat(MAX_BYTES + 1));
        bad_values.push(format!(
            "{{\"format_version\":1,{}",
            &sample().encode().unwrap()[1..]
        ));
        for raw in bad_values {
            db.execute(
                "INSERT OR REPLACE INTO run_model_provenance VALUES('run',1,?1)",
                [&raw],
            )
            .unwrap();
            assert!(read(&db, "run", "mlx").is_err());
            let unchanged: String = db
                .query_row(
                    "SELECT provenance FROM run_model_provenance WHERE run_id='run'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(unchanged, raw);
        }
        db.execute(
            "UPDATE run_model_provenance SET format_version=2,provenance=?1",
            [sample().encode().unwrap()],
        )
        .unwrap();
        assert!(read(&db, "run", "mlx").is_err());
        db.execute(
            "UPDATE run_model_provenance SET format_version=1,provenance=?1",
            [sample().encode().unwrap().into_bytes()],
        )
        .unwrap();
        assert!(read(&db, "run", "mlx").is_err());
    }

    #[test]
    fn invalid_sampling_and_template_mode_pairs_are_rejected_before_storage() {
        for temperature in [f32::NAN, f32::INFINITY, -0.1, 2.1] {
            let mut value = sample();
            value.sampling.temperature = temperature;
            assert!(value.encode().is_err());
        }
        for top_p in [f32::NAN, 0.0, -0.1, 1.1] {
            let mut value = sample();
            value.sampling.top_p = top_p;
            assert!(value.encode().is_err());
        }
        for max_tokens in [0, 2_049] {
            let mut value = sample();
            value.sampling.max_tokens = max_tokens;
            assert!(value.encode().is_err());
        }
        let mut value = sample();
        value.template = Template::SimpleChatml;
        assert!(value.encode().is_err());
        value.thinking_mode = ThinkingMode::ModelDefault;
        assert!(value.encode().is_ok());
        value.sampling.seed = None;
        assert_eq!(
            serde_json::from_str::<ModelProvenance>(&value.encode().unwrap()).unwrap(),
            value
        );
    }
}
