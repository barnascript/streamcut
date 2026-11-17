import { createHash } from "node:crypto";
import { Redis } from "ioredis";
import type { ProgressEvent, Queue, QueueStats } from "./types.js";

// Same names as worker/src/queue.rs.
export const STREAM = "streamcut:jobs";
export const DEAD = "streamcut:jobs:dead";
export const GROUP = "workers";
const channel = (jobId: string) => `streamcut:progress:${jobId}`;

/**
 * One connection for commands and one for pub/sub (a subscribed Redis
 * connection cannot run other commands). Each job channel is subscribed
 * once per API process, however many browsers are watching it.
 */
export class RedisQueue implements Queue {
  private readonly cmd: Redis;
  private readonly sub: Redis;
  private readonly listeners = new Map<string, Set<(e: ProgressEvent) => void>>();

  constructor(url: string) {
    this.cmd = new Redis(url, { maxRetriesPerRequest: 3 });
    this.sub = new Redis(url);
    this.sub.on("message", (ch: string, raw: string) => {
      const jobId = ch.slice(ch.lastIndexOf(":") + 1);
      const set = this.listeners.get(jobId);
      if (!set) return;
      let e: ProgressEvent;
      try {
        e = JSON.parse(raw) as ProgressEvent;
      } catch {
        return;
      }
      for (const fn of set) fn(e);
    });
  }

  async init(): Promise<void> {
    try {
      await this.cmd.xgroup("CREATE", STREAM, GROUP, "0", "MKSTREAM");
    } catch (e) {
      if (!String(e).includes("BUSYGROUP")) throw e;
    }
  }

  async enqueue(jobId: string): Promise<void> {
    await this.cmd.xadd(STREAM, "*", "job_id", jobId);
  }

  private idemKey(key: string) {
    return `streamcut:idem:${createHash("sha256").update(key).digest("hex")}`;
  }

  async rememberIdempotencyKey(key: string, jobId: string): Promise<string> {
    const k = this.idemKey(key);
    const set = await this.cmd.set(k, jobId, "EX", 86_400, "NX");
    if (set === "OK") return jobId;
    return (await this.cmd.get(k)) ?? jobId;
  }

  async forgetIdempotencyKey(key: string): Promise<void> {
    await this.cmd.del(this.idemKey(key));
  }

  /** Fixed one-minute window per client. Simple, and fine at this scale. */
  async allow(client: string, perMinute: number): Promise<boolean> {
    const k = `streamcut:rate:${client}:${Math.floor(Date.now() / 60_000)}`;
    const [[, count]] = (await this.cmd.multi().incr(k).expire(k, 70).exec()) as [[null, number], [null, number]];
    return count <= perMinute;
  }

  subscribe(jobId: string, onEvent: (e: ProgressEvent) => void): () => void {
    let set = this.listeners.get(jobId);
    if (!set) {
      set = new Set();
      this.listeners.set(jobId, set);
      void this.sub.subscribe(channel(jobId));
    }
    set.add(onEvent);
    return () => {
      const s = this.listeners.get(jobId);
      if (!s) return;
      s.delete(onEvent);
      if (s.size === 0) {
        this.listeners.delete(jobId);
        void this.sub.unsubscribe(channel(jobId));
      }
    };
  }

  async stats(): Promise<QueueStats> {
    const [groups, dead] = await Promise.all([
      this.cmd.xinfo("GROUPS", STREAM) as Promise<unknown[][]>,
      this.cmd.xlen(DEAD),
    ]);
    const asMap = (arr: unknown[]) => {
      const m = new Map<string, unknown>();
      for (let i = 0; i + 1 < arr.length; i += 2) m.set(String(arr[i]), arr[i + 1]);
      return m;
    };
    const g = groups.map(asMap).find((m) => m.get("name") === GROUP);
    const consumers = g ? ((await this.cmd.xinfo("CONSUMERS", STREAM, GROUP)) as unknown[][]).map(asMap) : [];
    return {
      waiting: Number(g?.get("lag") ?? 0),
      inProgress: Number(g?.get("pending") ?? 0),
      dead,
      workers: consumers.map((c) => ({ name: String(c.get("name")), busy: Number(c.get("pending")), idleMs: Number(c.get("idle")) })),
    };
  }

  async ping(): Promise<void> {
    await this.cmd.ping();
  }

  async close(): Promise<void> {
    await Promise.all([this.cmd.quit(), this.sub.quit()]);
  }
}
