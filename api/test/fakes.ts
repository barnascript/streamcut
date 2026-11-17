import type { Chart, ClipRow, JobRow, JobStatus, NewJob, ProgressEvent, Queue, QueueStats, Repo } from "../src/types.js";

/** In-memory stand-ins for ScyllaDB and Redis, so route tests need no Docker. */
export class MemRepo implements Repo {
  jobs = new Map<string, JobRow>();
  clipRows = new Map<string, ClipRow[]>();
  charts = new Map<string, Chart>();
  failCreate = false;

  async createJob(j: NewJob) {
    if (this.failCreate) throw new Error("db down");
    this.jobs.set(j.id, {
      id: j.id,
      status: "queued",
      stage: "queued",
      progress: 0,
      message: "Waiting for a worker",
      sourceName: j.sourceName,
      sourceUrl: j.sourceUrl,
      sourceKey: j.sourceKey,
      settings: j.settings,
      durationS: null,
      clipCount: null,
      words: null,
      attempts: 0,
      worker: null,
      error: null,
      createdAt: j.createdAt,
      updatedAt: j.createdAt,
    });
  }
  async getJob(id: string) {
    return this.jobs.get(id) ?? null;
  }
  async recentJobs(limit: number) {
    return [...this.jobs.values()].sort((a, b) => +b.createdAt - +a.createdAt).slice(0, limit);
  }
  async clips(id: string) {
    return this.clipRows.get(id) ?? [];
  }
  async chart(id: string) {
    return this.charts.get(id) ?? null;
  }
  async setStatus(id: string, status: JobStatus, message: string, error: string | null = null) {
    const j = this.jobs.get(id);
    if (j) Object.assign(j, { status, message, error });
  }
  async ping() {}
}

export class MemQueue implements Queue {
  queued: string[] = [];
  idem = new Map<string, string>();
  counts = new Map<string, number>();
  listeners = new Map<string, Set<(e: ProgressEvent) => void>>();
  failEnqueue = false;

  async enqueue(id: string) {
    if (this.failEnqueue) throw new Error("redis down");
    this.queued.push(id);
  }
  async rememberIdempotencyKey(key: string, id: string) {
    if (!this.idem.has(key)) this.idem.set(key, id);
    return this.idem.get(key)!;
  }
  async forgetIdempotencyKey(key: string) {
    this.idem.delete(key);
  }
  async allow(client: string, perMinute: number) {
    const n = (this.counts.get(client) ?? 0) + 1;
    this.counts.set(client, n);
    return n <= perMinute;
  }
  subscribe(id: string, fn: (e: ProgressEvent) => void) {
    const set = this.listeners.get(id) ?? new Set();
    set.add(fn);
    this.listeners.set(id, set);
    return () => set.delete(fn);
  }
  emit(e: ProgressEvent) {
    for (const fn of this.listeners.get(e.jobId) ?? []) fn(e);
  }
  async stats(): Promise<QueueStats> {
    return { waiting: this.queued.length, inProgress: 0, dead: 0, workers: [] };
  }
  async ping() {}
}
