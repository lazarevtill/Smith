//! Independent host lifetime regressions; no listener, model, or user profile.
use super::*;
use axum::body::to_bytes;
use std::sync::atomic::{AtomicBool, Ordering};

const REVIEW_SECRET: &str = "independent synthetic host review passphrase";
fn fixture() -> (tempfile::TempDir, Arc<Host>) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let host = Host::new(root).unwrap();
    (directory, host)
}
async fn open_fixture(host: &Arc<Host>) -> String {
    let revision = host.snapshot()["revision"].as_str().unwrap().to_owned();
    let (status, _) = open(
        host.clone(),
        OpenInput {
            revision,
            passphrase: Password(storage::Secret::new(REVIEW_SECRET.into()).unwrap()),
        },
        true,
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::ACCEPTED);
    host.operations.drain().await;
    let status = host.snapshot();
    assert_eq!(status["state"], "unlocked", "{status}");
    status["session"].as_str().unwrap().to_owned()
}
async fn lock_fixture(host: &Arc<Host>, session: String) {
    let revision = host.snapshot()["revision"].as_str().unwrap().to_owned();
    let (status, _) = lock(State(host.clone()), Json(LockInput { revision, session }))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn shutdown_waits_for_an_admitted_operation_even_before_it_is_spawned() {
    let (_directory, host) = fixture();
    let revision = host.snapshot()["revision"].as_str().unwrap().to_owned();
    let (operation, permit) = host
        .begin(&revision, "review", &[Phase::NeedsCreation])
        .unwrap();
    let closing = host.clone();
    let shutdown = tokio::spawn(async move { closing.shutdown().await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    assert!(host.state.lock().unwrap().stopping);
    let revision = host.snapshot()["revision"].as_str().unwrap().to_owned();
    assert!(host.begin(&revision, "late", &[Phase::Operating]).is_err());
    let completing = host.clone();
    host.operations.spawn_reserved(permit, async move {
        completing.finish(&operation, Ok(json!({})), Phase::NeedsCreation);
    });
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(host.state.lock().unwrap().opened.is_none());
}

#[tokio::test]
async fn late_mlx_cleanup_fault_during_drain_retains_the_open_profile() {
    let (_directory, host) = fixture();
    open_fixture(&host).await;
    let app = host
        .state
        .lock()
        .unwrap()
        .opened
        .as_ref()
        .unwrap()
        .app
        .clone();
    // A launch can still be tracked without having published its actor yet.
    assert!(app.mlx.shutdown().await.is_ok());
    let (release, released) = tokio::sync::oneshot::channel();
    let publisher = app.clone();
    app.spawn_task(async move {
        released.await.unwrap();
        publisher.mlx.inject_cleanup_fault_for_test();
    });
    let closing = host.clone();
    let mut shutdown = tokio::spawn(async move { closing.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut shutdown)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    let state = host.state.lock().unwrap();
    assert!(state.phase == Phase::Failed);
    assert!(
        state.opened.is_some(),
        "unconfirmed runtime cleanup must retain profile ownership"
    );
    assert!(app.mlx.occupied());
}

#[tokio::test]
async fn lock_waits_for_registered_blocking_work_after_its_request_is_gone() {
    let (_directory, host) = fixture();
    let session = open_fixture(&host).await;
    let app = host
        .state
        .lock()
        .unwrap()
        .opened
        .as_ref()
        .unwrap()
        .app
        .clone();
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let worker_app = app.clone();
    app.spawn_task(async move {
        tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(3)).unwrap();
            worker_app
                .db
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO settings VALUES('review-completion','synthetic')",
                    [],
                )
                .unwrap();
        })
        .await
        .unwrap();
    });
    drop(app);
    entering.await.unwrap();
    lock_fixture(&host, session).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), host.operations.drain())
            .await
            .is_err()
    );
    assert_eq!(host.snapshot()["state"], "locking");
    release.send(()).unwrap();
    host.operations.drain().await;
    assert_eq!(host.snapshot()["state"], "locked");
    let db = storage::unlock(
        &host.root.join(storage::DB_FILE),
        &storage::Secret::new(REVIEW_SECRET.into()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='review-completion'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "synthetic"
    );
    storage::close(db).unwrap();
}

#[tokio::test]
async fn lock_revokes_waiting_handler_without_allowing_its_late_mutation() {
    let (_directory, host) = fixture();
    let session = open_fixture(&host).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let changed = Arc::new(AtomicBool::new(false));
    let signal = entered.clone();
    let mutation = changed.clone();
    host.state.lock().unwrap().opened.as_mut().unwrap().router = Router::new().route(
        "/api/reviewer",
        get(move || {
            let signal = signal.clone();
            let mutation = mutation.clone();
            async move {
                signal.notify_one();
                std::future::pending::<()>().await;
                mutation.store(true, Ordering::SeqCst);
                "must never reach response"
            }
        }),
    );
    let request_host = host.clone();
    let request_session = session.clone();
    let request = tokio::spawn(async move {
        dispatch(
            State(request_host),
            Request::builder()
                .uri("/api/reviewer")
                .header("x-smith-profile-session", request_session)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    });
    entered.notified().await;
    lock_fixture(&host, session).await;
    assert_eq!(request.await.unwrap().status(), StatusCode::LOCKED);
    host.operations.drain().await;
    assert_eq!(host.snapshot()["state"], "locked");
    assert!(!changed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn revoked_response_stream_drops_buffered_plaintext_before_delivery() {
    let (_directory, host) = fixture();
    let session = open_fixture(&host).await;
    host.state.lock().unwrap().opened.as_mut().unwrap().router = Router::new().route(
        "/api/reviewer",
        get(|| async { "synthetic buffered private response" }),
    );
    let response = dispatch(
        State(host.clone()),
        Request::builder()
            .uri("/api/reviewer")
            .header("x-smith-profile-session", &session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    lock_fixture(&host, session).await;
    host.operations.drain().await;
    assert_eq!(host.snapshot()["state"], "locked");
    assert!(to_bytes(response.into_body(), 1024)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn real_run_event_stream_ends_on_lock_and_its_watch_state_is_cleared() {
    let (_directory, host) = fixture();
    let session = open_fixture(&host).await;
    let (tx, _) = watch::channel(RunState {
        message_id: "synthetic-message".into(),
        content: "synthetic sensitive run state".into(),
        status: "streaming".into(),
        error: None,
        snapshot: Config::default(),
        warning: None,
        finish_reason: None,
        transport_incomplete: false,
        persisted: true,
    });
    let run = Arc::new(Run {
        tx: tx.clone(),
        cancel: Arc::new(Notify::new()),
        chat_id: "synthetic-chat".into(),
    });
    {
        let state = host.state.lock().unwrap();
        let app = &state.opened.as_ref().unwrap().app;
        app.runs
            .lock()
            .unwrap()
            .insert("review-run".into(), run.clone());
        // Every production streaming Run owns a tracked worker. This synthetic
        // worker models that cancellation/terminal contract without a model.
        app.spawn_task(async move {
            run.cancel.notified().await;
            run.tx
                .send_modify(|state| state.status = "cancelled".into());
        });
    }
    let response = dispatch(
        State(host.clone()),
        Request::builder()
            .uri(format!(
                "/api/runs/review-run/events?profile_session={session}"
            ))
            .header(header::HOST, "127.0.0.1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("synthetic sensitive run state"));
    lock_fixture(&host, session).await;
    host.operations.drain().await;
    assert_eq!(host.snapshot()["state"], "locked");
    assert!(tx.borrow().content.is_empty());
    assert!(tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn failed_close_retains_connection_and_blocks_new_unlock_until_retry() {
    let (_directory, host) = fixture();
    let session = open_fixture(&host).await;
    host.state
        .lock()
        .unwrap()
        .opened
        .as_ref()
        .unwrap()
        .app
        .db
        .lock()
        .unwrap()
        .execute_batch(
            "BEGIN IMMEDIATE; INSERT INTO settings VALUES('uncommitted-review','private');",
        )
        .unwrap();
    lock_fixture(&host, session).await;
    host.operations.drain().await;
    assert_eq!(host.snapshot()["state"], "failed");
    assert!(host.state.lock().unwrap().fault_connection.is_some());
    let revision = host.snapshot()["revision"].as_str().unwrap().to_owned();
    assert!(host.begin(&revision, "unlock", &[Phase::Failed]).is_err());
    host.state
        .lock()
        .unwrap()
        .fault_connection
        .as_ref()
        .unwrap()
        .execute_batch("ROLLBACK;")
        .unwrap();
    lock_fixture(&host, String::new()).await;
    host.operations.drain().await;
    assert_eq!(host.snapshot()["state"], "locked");
    assert!(host.state.lock().unwrap().fault_connection.is_none());
}
