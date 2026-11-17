//! Queue mode: take a job, run the pipeline, save the clips, repeat.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::config::Config;
use crate::fetch;
use crate::media::BadInput;
use crate::pipeline::{self, Progress, Settings};
use crate::queue::{Msg, Queue};
use crate::store::Store;

pub async fn serve(cfg: Config) -> Result<()> {
    let store =
        Arc::new(Store::connect(&cfg.scylla_nodes, &cfg.keyspace, Duration::from_secs(180)).await?);
    let mut queue = Queue::connect(&cfg.redis_url, &cfg.worker_id, cfg.claim_idle_ms).await?;
    queue.ensure_group().await?;
    tracing::info!(worker = %cfg.worker_id, whisper = cfg.tools.whisper_model.is_some(), "ready for jobs");

    let stopping = Arc::new(AtomicBool::new(false));
    let stop_now = Arc::new(Notify::new());
    {
        let (stopping, stop_now) = (stopping.clone(), stop_now.clone());
        tokio::spawn(async move {
            shutdown_signal().await;
            tracing::info!("stop signal received");
            stopping.store(true, Ordering::SeqCst);
            stop_now.notify_waiters();
        });
    }

    while !stopping.load(Ordering::SeqCst) {
        let msg = tokio::select! {
            m = queue.next() => m,
            _ = stop_now.notified() => break,
        };
        match msg {
            Ok(Some(msg)) => {
                if let Err(e) = handle(&cfg, &store, &mut queue, msg, &stop_now).await {
                    tracing::error!("job handling error: {e:#}");
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("queue error: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    tracing::info!("worker stopped");
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn status_json(job: &Uuid, status: &str, stage: &str, pct: u8, message: &str) -> String {
    serde_json::json!({ "jobId": job, "status": status, "stage": stage, "pct": pct, "message": message }).to_string()
}

async fn handle(
    cfg: &Config,
    store: &Arc<Store>,
    queue: &mut Queue,
    msg: Msg,
    stop_now: &Notify,
) -> Result<()> {
    let id = msg.job_id;
    let span = tracing::info_span!("job", %id, try_no = msg.deliveries);
    let _g = span.enter();

    if msg.deliveries > cfg.max_attempts {
        let why = format!("gave up after {} tries", msg.deliveries - 1);
        store
            .fail(id, "failed", &why, "Could not finish this video")
            .await?;
        queue
            .publish(&id, status_json(&id, "failed", "failed", 0, &why))
            .await
            .ok();
        queue.dead_letter(&msg, &why).await?;
        return Ok(());
    }
    let Some(job) = store.job(id).await? else {
        tracing::warn!("job is not in the database, dropping it");
        return queue.ack(&msg.id).await;
    };
    if job.status == "done" {
        // Redis can hand out a message twice (e.g. after a worker restart).
        // The work is already done, so this is safe to skip.
        tracing::info!("already done, skipping duplicate");
        return queue.ack(&msg.id).await;
    }
    store
        .start(id, &cfg.worker_id, msg.deliveries as i32)
        .await?;
    tracing::info!("started");

    // Heartbeat: keep the job marked as mine while I work on it.
    let beat = {
        let mut q = queue.clone();
        let entry = msg.id.clone();
        let every = Duration::from_millis((cfg.claim_idle_ms / 3).max(1000));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                if let Err(e) = q.touch(&entry).await {
                    tracing::warn!("heartbeat failed: {e:#}");
                }
            }
        })
    };

    // Progress goes to Redis (live page) on every update, and to ScyllaDB
    // only when the stage changes or it moved 5%, to keep writes low.
    let (tx, mut rx) = mpsc::unbounded_channel::<Progress>();
    let reporter = {
        let mut q = queue.clone();
        let store = store.clone();
        tokio::spawn(async move {
            let (mut last_stage, mut last_pct) = ("", 0u8);
            while let Some(p) = rx.recv().await {
                q.publish(&id, status_json(&id, "running", p.stage, p.pct, &p.message))
                    .await
                    .ok();
                if p.stage != last_stage || p.pct >= last_pct + 5 {
                    store
                        .progress(id, p.stage, p.pct as i32, &p.message)
                        .await
                        .ok();
                    (last_stage, last_pct) = (p.stage, p.pct);
                }
            }
        })
    };
    let on_progress: pipeline::OnProgress = Arc::new(move |p| {
        let _ = tx.send(p);
    });

    let work = async {
        let src = source_file(cfg, store, id, &job, &on_progress).await?;
        let settings: Settings = job
            .settings
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let work_dir = cfg.data_dir.join("work").join(id.to_string());
        let out_dir = cfg.data_dir.join("clips").join(id.to_string());
        let outcome = pipeline::run(
            &cfg.tools,
            &src,
            &work_dir,
            &out_dir,
            settings,
            cfg.render_slots,
            on_progress.clone(),
        )
        .await;
        // The big temporary files go either way; the clips stay.
        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        outcome
    };

    let result = tokio::select! {
        r = work => Some(r),
        _ = stop_now.notified() => None,
    };
    beat.abort();
    drop(on_progress);
    let _ = reporter.await;

    match result {
        None => {
            // Stopping mid-job: the ffmpeg/whisper children were killed when
            // the work future was dropped. Hand the job straight back.
            store
                .fail(
                    id,
                    "queued",
                    "",
                    "Worker restarted, the job will start again",
                )
                .await
                .ok();
            queue.requeue(&msg).await?;
            tracing::info!("handed back to the queue");
        }
        Some(Ok(outcome)) => {
            let rel = |p: &Path| {
                p.strip_prefix(&cfg.data_dir)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .to_string()
            };
            let clips: Vec<_> = outcome
                .clips
                .iter()
                .map(|c| (c.clone(), rel(&c.video), rel(&c.thumb)))
                .collect();
            store
                .save(id, &clips, &outcome.chart, outcome.duration, outcome.words)
                .await?;
            queue
                .publish(
                    &id,
                    status_json(
                        &id,
                        "done",
                        "done",
                        100,
                        &format!("{} clips ready", clips.len()),
                    ),
                )
                .await
                .ok();
            queue.ack(&msg.id).await?;
            tracing::info!(clips = clips.len(), timings = ?outcome.timings_ms, "done");
        }
        Some(Err(e)) => {
            let permanent = e.downcast_ref::<BadInput>().is_some();
            let text = format!("{e:#}");
            if permanent || msg.deliveries >= cfg.max_attempts {
                tracing::warn!("failed: {text}");
                let shown = if permanent {
                    text.clone()
                } else {
                    "Something went wrong while cutting this video".into()
                };
                store.fail(id, "failed", &text, &shown).await?;
                queue
                    .publish(&id, status_json(&id, "failed", "failed", 0, &shown))
                    .await
                    .ok();
                if permanent {
                    queue.ack(&msg.id).await?
                } else {
                    queue.dead_letter(&msg, &text).await?
                }
            } else {
                // Not acked on purpose: after CLAIM_IDLE_MS a worker takes it again.
                tracing::warn!("will retry: {text}");
                store
                    .fail(id, "queued", &text, "Hit a problem, trying again shortly")
                    .await?;
                queue
                    .publish(
                        &id,
                        status_json(
                            &id,
                            "queued",
                            "retry",
                            0,
                            "Hit a problem, trying again shortly",
                        ),
                    )
                    .await
                    .ok();
            }
        }
    }
    Ok(())
}

async fn source_file(
    cfg: &Config,
    store: &Store,
    id: Uuid,
    job: &crate::store::Job,
    progress: &pipeline::OnProgress,
) -> Result<PathBuf> {
    if let Some(key) = &job.source_key {
        let p = cfg.data_dir.join(key);
        if p.exists() {
            return Ok(p);
        }
        return Err(BadInput("the uploaded file is missing".into()).into());
    }
    let Some(url) = &job.source_url else {
        return Err(BadInput("the job has no file and no link".into()).into());
    };
    progress(Progress {
        stage: "download",
        pct: 1,
        message: "Downloading the video".into(),
    });
    let key = format!("sources/{id}.download");
    let dest = cfg.data_dir.join(&key);
    tokio::fs::create_dir_all(dest.parent().unwrap()).await?;
    let name = fetch::download(
        url,
        &dest,
        cfg.max_download_mb << 20,
        cfg.allow_private_urls,
    )
    .await?;
    store.set_source(id, &key, &name).await?;
    Ok(dest)
}
