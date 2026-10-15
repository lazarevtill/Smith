//! One explicit selected model shared by chat, tasks and media operations.
use crate::*;

#[derive(Deserialize)]
pub(crate) struct Selection {
    #[serde(flatten)]
    expected: Expected,
    model: String,
}

pub(crate) async fn select(
    State(app): State<Arc<App>>,
    Json(input): Json<Selection>,
) -> ApiResult<Json<Value>> {
    let mut consent = app.consent.lock().unwrap();
    let mut config = app.config.lock().unwrap().clone();
    validate_expected(&consent, &config, &input.expected)?;
    if app.native.busy() || app.acceleration.busy() || app.mlx.busy() {
        return Err(bad(
            "Finish or cancel the managed operation before selecting a model.",
        ));
    }
    if input.model.is_empty() || input.model.len() > 256 {
        return Err(bad("Choose a model from the tested connection."));
    }
    let available = if config.provider == "mock" {
        input.model == "smith-demo"
    } else if config.provider == "managed" {
        native_api::ready(&app, &config)?;
        input.model == config.model
    } else if config.provider == "mlx" {
        mlx_api::ready(&app, &config)?;
        input.model == config.model
    } else {
        if app.tested.lock().unwrap().as_ref() != Some(&config)
            || app.models_revision.lock().unwrap().as_ref() != Some(&consent.token())
        {
            return Err(bad("Test the current connection before selecting a model."));
        }
        app.models
            .lock()
            .unwrap()
            .iter()
            .any(|model| model["id"] == input.model)
    };
    if !available {
        return Err(bad(
            "The selected model is not in this connection's tested model list.",
        ));
    }
    if config.model != input.model {
        config.model = input.model;
        app.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO settings VALUES('config',?1)",
            [serde_json::to_string(&config).unwrap()],
        )?;
        *app.config.lock().unwrap() = config.clone();
        consent.advance();
        *app.tested.lock().unwrap() = Some(config.clone());
        *app.models_revision.lock().unwrap() = Some(consent.token());
    }
    let mut value = serde_json::to_value(&config).unwrap();
    value["revision"] = json!(consent.token());
    value["tested"] = json!(true);
    value["auth_present"] = json!(auth_present(&app, &config));
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn model_selection_is_persisted_and_invalidates_older_authority() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(&dir.path().join("synthetic.sqlite3")).unwrap();
        let config = Config {
            provider: "openai".into(),
            endpoint: "http://127.0.0.1:18499/v1".into(),
            model: "first".into(),
        };
        let _ = set_config(State(app.clone()), Json(config.clone()))
            .await
            .unwrap();
        let expected = Expected {
            revision: app.consent.lock().unwrap().token(),
            provider: config.provider.clone(),
            endpoint: config.endpoint.clone(),
        };
        *app.tested.lock().unwrap() = Some(config.clone());
        *app.models.lock().unwrap() = vec![json!({"id":"first"}), json!({"id":"second"})];
        *app.models_revision.lock().unwrap() = Some(expected.revision.clone());
        assert!(select(
            State(app.clone()),
            Json(Selection {
                expected: expected.clone(),
                model: "unlisted".into()
            })
        )
        .await
        .is_err());
        assert_eq!(app.config.lock().unwrap().model, "first");
        let selected = select(
            State(app.clone()),
            Json(Selection {
                expected: expected.clone(),
                model: "second".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_ne!(selected["revision"], expected.revision);
        assert_eq!(selected["model"], "second");
        assert_eq!(selected["tested"], true);
        let saved: String = app
            .db
            .lock()
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key='config'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Config>(&saved).unwrap().model,
            "second"
        );
        assert!(app.tested.lock().unwrap().as_ref() == Some(&*app.config.lock().unwrap()));
        assert!(select(
            State(app.clone()),
            Json(Selection {
                expected,
                model: "first".into()
            })
        )
        .await
        .is_err());
    }
}
