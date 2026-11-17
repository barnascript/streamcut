//! Reads and writes jobs and clips in ScyllaDB. The schema is in db/schema.cql.

use anyhow::{Context, Result};
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::statement::prepared::PreparedStatement;
use scylla::value::CqlTimestamp;
use std::time::Duration;
use uuid::Uuid;

use crate::pipeline::{Chart, ClipOut};

fn now() -> CqlTimestamp {
    CqlTimestamp(chrono::Utc::now().timestamp_millis())
}

pub struct Job {
    pub status: String,
    pub source_key: Option<String>,
    pub source_url: Option<String>,
    pub settings: Option<String>,
}

pub struct Store {
    s: Session,
    get_job: PreparedStatement,
    start: PreparedStatement,
    progress: PreparedStatement,
    source: PreparedStatement,
    finish: PreparedStatement,
    fail: PreparedStatement,
    drop_clips: PreparedStatement,
    add_clip: PreparedStatement,
    add_signals: PreparedStatement,
}

impl Store {
    /// Waits for ScyllaDB and for the API to create the tables.
    pub async fn connect(nodes: &[String], ks: &str, wait: Duration) -> Result<Self> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            match Self::try_connect(nodes, ks).await {
                Ok(s) => return Ok(s),
                Err(e) if std::time::Instant::now() < deadline => {
                    tracing::info!("waiting for ScyllaDB: {e:#}");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn try_connect(nodes: &[String], ks: &str) -> Result<Self> {
        let s = SessionBuilder::new()
            .known_nodes(nodes)
            .build()
            .await
            .context("connecting to ScyllaDB")?;
        let p = |q: String| {
            let s = &s;
            async move { s.prepare(q).await.map_err(anyhow::Error::from) }
        };
        Ok(Store {
            get_job: p(format!("SELECT status, source_key, source_url, settings FROM {ks}.jobs WHERE job_id = ?")).await?,
            start: p(format!(
                "UPDATE {ks}.jobs SET status = 'running', worker = ?, attempts = ?, error = null, updated_at = ? WHERE job_id = ?"
            ))
            .await?,
            progress: p(format!(
                "UPDATE {ks}.jobs SET stage = ?, progress = ?, message = ?, updated_at = ? WHERE job_id = ?"
            ))
            .await?,
            source: p(format!("UPDATE {ks}.jobs SET source_key = ?, source_name = ? WHERE job_id = ?")).await?,
            finish: p(format!(
                "UPDATE {ks}.jobs SET status = 'done', stage = 'done', progress = 100, message = ?, duration_s = ?, \
                 clip_count = ?, words = ?, updated_at = ? WHERE job_id = ?"
            ))
            .await?,
            fail: p(format!("UPDATE {ks}.jobs SET status = ?, error = ?, message = ?, updated_at = ? WHERE job_id = ?")).await?,
            drop_clips: p(format!("DELETE FROM {ks}.clips_by_job WHERE job_id = ?")).await?,
            add_clip: p(format!(
                "INSERT INTO {ks}.clips_by_job (job_id, rank, clip_id, start_s, end_s, score, title, reasons, transcript, \
                 video_key, thumb_key, width, height, size_bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ))
            .await?,
            add_signals: p(format!(
                "INSERT INTO {ks}.signals_by_job (job_id, step_s, loudness, speech, hype, score) VALUES (?, ?, ?, ?, ?, ?)"
            ))
            .await?,
            s,
        })
    }

    pub async fn job(&self, id: Uuid) -> Result<Option<Job>> {
        let rows = self
            .s
            .execute_unpaged(&self.get_job, (id,))
            .await?
            .into_rows_result()?;
        let row = rows.maybe_first_row::<(
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>()?;
        Ok(row.map(|(status, source_key, source_url, settings)| Job {
            status: status.unwrap_or_default(),
            source_key,
            source_url,
            settings,
        }))
    }

    pub async fn start(&self, id: Uuid, worker: &str, attempt: i32) -> Result<()> {
        self.s
            .execute_unpaged(&self.start, (worker, attempt, now(), id))
            .await?;
        Ok(())
    }

    pub async fn progress(&self, id: Uuid, stage: &str, pct: i32, message: &str) -> Result<()> {
        self.s
            .execute_unpaged(&self.progress, (stage, pct, message, now(), id))
            .await?;
        Ok(())
    }

    pub async fn set_source(&self, id: Uuid, key: &str, name: &str) -> Result<()> {
        self.s
            .execute_unpaged(&self.source, (key, name, id))
            .await?;
        Ok(())
    }

    /// `status` is "failed" (for good) or "queued" (another try is coming).
    pub async fn fail(&self, id: Uuid, status: &str, error: &str, message: &str) -> Result<()> {
        self.s
            .execute_unpaged(&self.fail, (status, error, message, now(), id))
            .await?;
        Ok(())
    }

    /// Writes the clips, then marks the job done. Old clips from an earlier
    /// try are deleted first, so a retried job never shows a mix.
    pub async fn save(
        &self,
        id: Uuid,
        clips: &[(ClipOut, String, String)],
        chart: &Chart,
        duration: f64,
        words: usize,
    ) -> Result<()> {
        self.s.execute_unpaged(&self.drop_clips, (id,)).await?;
        for (c, video_key, thumb_key) in clips {
            self.s
                .execute_unpaged(
                    &self.add_clip,
                    (
                        id,
                        c.rank as i32,
                        Uuid::new_v4(),
                        c.start,
                        c.end,
                        c.score,
                        c.title.as_str(),
                        &c.reasons,
                        c.text.as_str(),
                        video_key.as_str(),
                        thumb_key.as_str(),
                        c.width as i32,
                        c.height as i32,
                        c.bytes as i64,
                    ),
                )
                .await?;
        }
        self.s
            .execute_unpaged(
                &self.add_signals,
                (
                    id,
                    chart.step,
                    &chart.loudness,
                    &chart.speech,
                    &chart.hype,
                    &chart.score,
                ),
            )
            .await?;
        let msg = format!("{} clips ready", clips.len());
        self.s
            .execute_unpaged(
                &self.finish,
                (
                    msg.as_str(),
                    duration,
                    clips.len() as i32,
                    words as i32,
                    now(),
                    id,
                ),
            )
            .await?;
        Ok(())
    }
}
