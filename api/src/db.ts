import cassandra from "cassandra-driver";
import { readFile } from "node:fs/promises";
import type { Chart, ClipRow, JobRow, JobStatus, NewJob, Repo } from "./types.js";
import { Settings } from "./types.js";

const { types } = cassandra;
type Row = cassandra.types.Row;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Splits the schema file into statements, dropping comment lines. */
export function schemaStatements(cql: string, rf: number): string[] {
  return cql
    .split("\n")
    .filter((l) => !l.trim().startsWith("--"))
    .join("\n")
    .replaceAll("${RF}", String(rf))
    .split(";")
    .map((s) => s.trim())
    .filter(Boolean);
}

const n = (v: unknown): number | null => {
  if (v === null || v === undefined) return null;
  if (typeof v === "number") return v;
  if (typeof (v as { toNumber?: () => number }).toNumber === "function") return (v as { toNumber: () => number }).toNumber();
  return Number(v);
};

function toJob(r: Row): JobRow {
  const parsed = Settings.safeParse(JSON.parse(r.get("settings") ?? "{}"));
  return {
    id: r.get("job_id").toString(),
    status: (r.get("status") ?? "queued") as JobStatus,
    stage: r.get("stage"),
    progress: n(r.get("progress")) ?? 0,
    message: r.get("message"),
    sourceName: r.get("source_name"),
    sourceUrl: r.get("source_url"),
    sourceKey: r.get("source_key"),
    settings: parsed.success ? parsed.data : Settings.parse({}),
    durationS: n(r.get("duration_s")),
    clipCount: n(r.get("clip_count")),
    words: n(r.get("words")),
    attempts: n(r.get("attempts")),
    worker: r.get("worker"),
    error: r.get("error"),
    createdAt: r.get("created_at"),
    updatedAt: r.get("updated_at"),
  };
}

const JOB_COLUMNS =
  "job_id, status, stage, progress, message, source_name, source_url, source_key, settings, duration_s, clip_count, words, attempts, worker, error, created_at, updated_at";

export class ScyllaRepo implements Repo {
  private constructor(
    private readonly c: cassandra.Client,
    private readonly ks: string,
  ) {}

  /** Connects (retrying while ScyllaDB starts up) and creates the tables. */
  static async connect(opts: { nodes: string[]; dc: string; keyspace: string; rf: number; schemaFile: string; log: (m: string) => void }) {
    const client = new cassandra.Client({
      contactPoints: opts.nodes,
      localDataCenter: opts.dc,
      socketOptions: { readTimeout: 12_000, connectTimeout: 5_000 },
    });
    for (let i = 1; ; i++) {
      try {
        await client.connect();
        break;
      } catch (e) {
        if (i >= 60) throw e;
        opts.log(`waiting for ScyllaDB (${(e as Error).message})`);
        await sleep(3000);
      }
    }
    const cql = await readFile(opts.schemaFile, "utf8");
    for (const stmt of schemaStatements(cql, opts.rf)) {
      await client.execute(stmt);
    }
    return new ScyllaRepo(client, opts.keyspace);
  }

  async createJob(j: NewJob): Promise<void> {
    const day = types.LocalDate.fromDate(j.createdAt);
    const queries = [
      {
        query: `INSERT INTO ${this.ks}.jobs (job_id, status, stage, progress, message, source_name, source_url, source_key, settings, attempts, created_at, updated_at)
                VALUES (?, 'queued', 'queued', 0, 'Waiting for a worker', ?, ?, ?, ?, 0, ?, ?)`,
        params: [j.id, j.sourceName, j.sourceUrl, j.sourceKey, JSON.stringify(j.settings), j.createdAt, j.createdAt],
      },
      {
        query: `INSERT INTO ${this.ks}.jobs_by_day (day, created_at, job_id, source_name) VALUES (?, ?, ?, ?)`,
        params: [day, j.createdAt, j.id, j.sourceName],
      },
    ];
    // A logged batch: both rows land or neither does.
    await this.c.batch(queries, { prepare: true, logged: true });
  }

  async getJob(id: string): Promise<JobRow | null> {
    const rs = await this.c.execute(`SELECT ${JOB_COLUMNS} FROM ${this.ks}.jobs WHERE job_id = ?`, [id], { prepare: true });
    const row = rs.first();
    return row ? toJob(row) : null;
  }

  /** Newest first, walking back one day partition at a time (up to 14 days). */
  async recentJobs(limit: number): Promise<JobRow[]> {
    const ids: string[] = [];
    const now = Date.now();
    for (let d = 0; d < 14 && ids.length < limit; d++) {
      const day = types.LocalDate.fromDate(new Date(now - d * 86_400_000));
      const rs = await this.c.execute(
        `SELECT job_id FROM ${this.ks}.jobs_by_day WHERE day = ? LIMIT ?`,
        [day, limit - ids.length],
        { prepare: true },
      );
      for (const r of rs.rows) ids.push(r.get("job_id").toString());
    }
    if (ids.length === 0) return [];
    const rs = await this.c.execute(`SELECT ${JOB_COLUMNS} FROM ${this.ks}.jobs WHERE job_id IN ?`, [ids], { prepare: true });
    const byId = new Map(rs.rows.map((r) => [r.get("job_id").toString(), toJob(r)]));
    return ids.map((id) => byId.get(id)).filter((j): j is JobRow => Boolean(j));
  }

  async clips(id: string): Promise<ClipRow[]> {
    const rs = await this.c.execute(
      `SELECT rank, start_s, end_s, score, title, reasons, transcript, video_key, thumb_key, width, height, size_bytes
       FROM ${this.ks}.clips_by_job WHERE job_id = ?`,
      [id],
      { prepare: true },
    );
    return rs.rows.map((r) => ({
      rank: r.get("rank"),
      start: r.get("start_s"),
      end: r.get("end_s"),
      score: r.get("score"),
      title: r.get("title") ?? "",
      reasons: r.get("reasons") ?? [],
      transcript: r.get("transcript") ?? "",
      videoKey: r.get("video_key"),
      thumbKey: r.get("thumb_key"),
      width: r.get("width"),
      height: r.get("height"),
      sizeBytes: n(r.get("size_bytes")) ?? 0,
    }));
  }

  async chart(id: string): Promise<Chart | null> {
    const rs = await this.c.execute(
      `SELECT step_s, loudness, speech, hype, score FROM ${this.ks}.signals_by_job WHERE job_id = ?`,
      [id],
      { prepare: true },
    );
    const r = rs.first();
    if (!r) return null;
    return { step: r.get("step_s"), loudness: r.get("loudness"), speech: r.get("speech"), hype: r.get("hype"), score: r.get("score") };
  }

  async setStatus(id: string, status: JobStatus, message: string, error: string | null = null): Promise<void> {
    await this.c.execute(
      `UPDATE ${this.ks}.jobs SET status = ?, message = ?, error = ?, updated_at = ? WHERE job_id = ?`,
      [status, message, error, new Date(), id],
      { prepare: true },
    );
  }

  async ping(): Promise<void> {
    await this.c.execute("SELECT now() FROM system.local");
  }

  async close(): Promise<void> {
    await this.c.shutdown();
  }
}
