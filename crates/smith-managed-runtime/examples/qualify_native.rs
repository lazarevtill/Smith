//! Opt-in, bounded local qualification. Never part of automatic cargo tests.
//! Usage: qualify_native EXE EXE_SHA256 GGUF GGUF_SHA256 RUNTIME_REV MODEL_SOURCE MODEL_ID
//! Invoking this explicitly consents to the printed fixed plan; supply only
//! already-reviewed trusted local artifacts. No downloads or existing servers.
use smith_managed_runtime::*;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    process::Command,
    time::{sleep, timeout, Instant},
};

async fn memory_free_percent() -> Option<u32> {
    let result = timeout(
        Duration::from_secs(3),
        Command::new("/usr/bin/memory_pressure")
            .arg("-Q")
            .env_clear()
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !result.status.success() {
        return None;
    }
    String::from_utf8_lossy(&result.stdout)
        .lines()
        .find_map(|line| {
            line.strip_prefix("System-wide memory free percentage: ")?
                .strip_suffix('%')?
                .parse()
                .ok()
        })
}
async fn child_rss_kib(pid: u32) -> Option<u64> {
    let result = timeout(
        Duration::from_secs(3),
        Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "rss="])
            .env_clear()
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    String::from_utf8_lossy(&result.stdout).trim().parse().ok()
}
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 7 || !cfg!(target_os = "macos") {
        eprintln!("Explicit macOS qualification only: EXE EXE_SHA256 GGUF GGUF_SHA256 RUNTIME_REV MODEL_SOURCE MODEL_ID");
        std::process::exit(2);
    }
    let Some(free) = memory_free_percent().await else {
        eprintln!("Cannot verify available memory; no model load");
        std::process::exit(2);
    };
    if free < 45 {
        eprintln!("Memory headroom insufficient ({free}% free); no model load");
        std::process::exit(2);
    }
    let manifest = Manifest {
        model_id: args[6].clone(),
        executable: Artifact {
            path: PathBuf::from(&args[0]),
            sha256: args[1].clone(),
            source: "explicitly selected installed native runtime".into(),
            source_revision: args[4].clone(),
        },
        model: Artifact {
            path: PathBuf::from(&args[2]),
            sha256: args[3].clone(),
            source: args[5].clone(),
            source_revision: args[3].clone(),
        },
        protocol: Protocol::LlamaCppUnixHttpV1,
        execution_kind: ExecutionKind::Native,
        declares_text_chat: true,
    };
    let config = LoadConfig {
        backend: BackendRequest::Cpu,
        context_tokens: 512,
        threads: 2,
        budgets: Budgets {
            startup: Duration::from_secs(45),
            generation: Duration::from_secs(45),
            reap: Duration::from_secs(2),
        },
    };
    let runtime = Runtime::new();
    let began = Instant::now();
    let operation = async {
        let plan = LoadPlan::prepare(manifest, config, &Cancellation::new()).await?;
        println!(
            "reviewed_plan={}",
            serde_json::to_string(plan.summary()).unwrap()
        );
        let id = plan.summary().id.clone();
        let approved = plan.approve(&id)?;
        let loading = Instant::now();
        let state = runtime
            .load(runtime.status().revision, approved, Cancellation::new())
            .await?;
        println!(
            "loaded_ms={} snapshot={}",
            loading.elapsed().as_millis(),
            serde_json::to_string(&state).unwrap()
        );
        let generating = Instant::now();
        let mut generation = runtime
            .start_generation(
                state.target,
                Capability::TextChat,
                TextRequest {
                    messages: vec![Message {
                        role: Role::User,
                        content: "Reply with exactly the word READY.".into(),
                    }],
                    max_tokens: 16,
                },
                Cancellation::new(),
            )
            .await?;
        let completion = generation.wait().await?;
        println!(
            "generation_ms={} completion={}",
            generating.elapsed().as_millis(),
            serde_json::to_string(&completion).unwrap()
        );
        Ok::<_, Failure>(())
    };
    let watchdog = async {
        loop {
            sleep(Duration::from_secs(1)).await;
            let free = memory_free_percent().await;
            let rss = match runtime.status().pid {
                Some(pid) => child_rss_kib(pid).await,
                None => None,
            };
            if free.is_none_or(|free| free < 20) || rss.is_some_and(|rss| rss > 7 * 1024 * 1024) {
                eprintln!("Memory guard stopped qualification: free_percent={free:?}, child_rss_kib={rss:?}");
                return Err(Failure::Cancelled);
            }
        }
    };
    let outcome = tokio::select! {
        result = operation => result,
        result = watchdog => result,
        _ = sleep(Duration::from_secs(120)) => Err(Failure::GenerationTimeout),
    };
    let cleanup = runtime.shutdown().await;
    println!(
        "total_ms={} outcome={outcome:?} cleanup={}",
        began.elapsed().as_millis(),
        serde_json::to_string(&runtime.status()).unwrap()
    );
    if outcome.is_err() || cleanup.is_err() {
        std::process::exit(1);
    }
}
