//! The job queue: a Redis stream with a consumer group.
//!
//! - Each job is one stream entry. A worker reads it with XREADGROUP, and it
//!   stays "pending" under that worker's name until the worker ACKs it.
//! - While working, the worker touches the entry every few seconds
//!   (XCLAIM ... JUSTID), which resets its idle time.
//! - If a worker dies, the entry goes idle. After CLAIM_IDLE_MS another
//!   worker takes it over with XAUTOCLAIM. Nothing is lost.
//! - Redis counts deliveries. After MAX_ATTEMPTS the job goes to a dead
//!   letter stream instead of failing forever.

use anyhow::{Context, Result};
use redis::aio::{ConnectionManager, ConnectionManagerConfig, MultiplexedConnection};
use redis::streams::{
    StreamAutoClaimOptions, StreamClaimOptions, StreamId, StreamReadOptions, StreamReadReply,
};
use redis::{AsyncCommands, AsyncConnectionConfig};
use std::time::Duration;
use uuid::Uuid;

pub const STREAM: &str = "streamcut:jobs";
pub const DEAD: &str = "streamcut:jobs:dead";
pub const GROUP: &str = "workers";

pub fn progress_channel(job: &Uuid) -> String {
    format!("streamcut:progress:{job}")
}

#[derive(Debug, Clone)]
pub struct Msg {
    pub id: String,
    pub job_id: Uuid,
    pub deliveries: u64,
}

#[derive(Clone)]
pub struct Queue {
    /// Blocking reads get their own connection so they never hold up
    /// progress messages or heartbeats.
    reader: MultiplexedConnection,
    cmd: ConnectionManager,
    consumer: String,
    claim_idle_ms: u64,
}

fn job_id_of(entry: &StreamId) -> Option<Uuid> {
    let v = entry.map.get("job_id")?;
    let s: String = redis::from_redis_value_ref(v).ok()?;
    Uuid::parse_str(&s).ok()
}

impl Queue {
    pub async fn connect(url: &str, consumer: &str, claim_idle_ms: u64) -> Result<Self> {
        let client = redis::Client::open(url)?;
        let reader = client
            .get_multiplexed_async_connection_with_config(
                &AsyncConnectionConfig::new().set_response_timeout(Some(Duration::from_secs(15))),
            )
            .await
            .context("connecting to Redis")?;
        let cmd = client
            .get_connection_manager_with_config(
                ConnectionManagerConfig::new().set_response_timeout(Some(Duration::from_secs(5))),
            )
            .await?;
        Ok(Queue {
            reader,
            cmd,
            consumer: consumer.to_string(),
            claim_idle_ms,
        })
    }

    pub async fn ensure_group(&mut self) -> Result<()> {
        let r: redis::RedisResult<()> = self.cmd.xgroup_create_mkstream(STREAM, GROUP, "0").await;
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().contains("BUSYGROUP") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Next job for this worker: an abandoned one first, else a new one.
    /// Returns None after a few seconds with nothing to do.
    pub async fn next(&mut self) -> Result<Option<Msg>> {
        let reply: redis::streams::StreamAutoClaimReply = self
            .cmd
            .xautoclaim_options(
                STREAM,
                GROUP,
                &self.consumer,
                self.claim_idle_ms,
                "0-0",
                StreamAutoClaimOptions::default().count(1),
            )
            .await?;
        if let Some(entry) = reply.claimed.into_iter().next() {
            tracing::warn!(id = %entry.id, "took over a job another worker left behind");
            return self.read_entry(entry).await;
        }

        let opts = StreamReadOptions::default()
            .group(GROUP, &self.consumer)
            .block(5000)
            .count(1);
        let reply: Option<StreamReadReply> =
            self.reader.xread_options(&[STREAM], &[">"], &opts).await?;
        let Some(entry) = reply
            .and_then(|r| r.keys.into_iter().next())
            .and_then(|k| k.ids.into_iter().next())
        else {
            return Ok(None);
        };
        self.read_entry(entry).await
    }

    async fn read_entry(&mut self, entry: StreamId) -> Result<Option<Msg>> {
        let Some(job_id) = job_id_of(&entry) else {
            tracing::error!(id = %entry.id, "queue entry has no job_id, dropping it");
            self.ack(&entry.id).await?;
            return Ok(None);
        };
        let pending: redis::streams::StreamPendingCountReply = self
            .cmd
            .xpending_count(STREAM, GROUP, &entry.id, &entry.id, 1)
            .await?;
        let deliveries = pending
            .ids
            .first()
            .map(|p| p.times_delivered as u64)
            .unwrap_or(1);
        Ok(Some(Msg {
            id: entry.id,
            job_id,
            deliveries,
        }))
    }

    pub async fn ack(&mut self, id: &str) -> Result<()> {
        let _: i64 = self.cmd.xack(STREAM, GROUP, &[id]).await?;
        Ok(())
    }

    /// Resets the idle clock on a job I am still working on.
    pub async fn touch(&mut self, id: &str) -> Result<()> {
        let _: redis::Value = self
            .cmd
            .xclaim_options(
                STREAM,
                GROUP,
                &self.consumer,
                0,
                &[id],
                StreamClaimOptions::default().with_justid(),
            )
            .await?;
        Ok(())
    }

    /// Puts the job back at the end of the queue (used on shutdown, so another
    /// worker starts it straight away instead of waiting for the idle timer).
    pub async fn requeue(&mut self, msg: &Msg) -> Result<()> {
        let _: String = self
            .cmd
            .xadd(STREAM, "*", &[("job_id", msg.job_id.to_string())])
            .await?;
        self.ack(&msg.id).await
    }

    pub async fn dead_letter(&mut self, msg: &Msg, reason: &str) -> Result<()> {
        let _: String = self
            .cmd
            .xadd(
                DEAD,
                "*",
                &[
                    ("job_id", msg.job_id.to_string()),
                    ("reason", reason.to_string()),
                ],
            )
            .await?;
        self.ack(&msg.id).await
    }

    pub async fn publish(&mut self, job: &Uuid, json: String) -> Result<()> {
        let _: i64 = self.cmd.publish(progress_channel(job), json).await?;
        Ok(())
    }
}
