import { z } from "zod";

const flag = z.preprocess((v) => {
  if (typeof v === "string") return ["true", "on", "1", "yes"].includes(v.toLowerCase());
  return v;
}, z.boolean());

/** What the person asks for. The worker clamps these again on its side. */
export const Settings = z
  .object({
    clips: z.coerce.number().int().min(1).max(10).default(5),
    minLen: z.coerce.number().int().min(5).max(90).default(20),
    maxLen: z.coerce.number().int().min(10).max(180).default(60),
    layout: z.enum(["blur", "crop"]).default("blur"),
    captions: flag.default(true),
  })
  .refine((s) => s.maxLen >= s.minLen, { message: "maxLen must be at least minLen", path: ["maxLen"] });

export type Settings = z.infer<typeof Settings>;

export type JobStatus = "queued" | "running" | "done" | "failed";

export interface JobRow {
  id: string;
  status: JobStatus;
  stage: string | null;
  progress: number;
  message: string | null;
  sourceName: string | null;
  sourceUrl: string | null;
  sourceKey: string | null;
  settings: Settings;
  durationS: number | null;
  clipCount: number | null;
  words: number | null;
  attempts: number | null;
  worker: string | null;
  error: string | null;
  createdAt: Date;
  updatedAt: Date;
}

export interface ClipRow {
  rank: number;
  start: number;
  end: number;
  score: number;
  title: string;
  reasons: string[];
  transcript: string;
  videoKey: string;
  thumbKey: string;
  width: number;
  height: number;
  sizeBytes: number;
}

export interface Chart {
  step: number;
  loudness: number[];
  speech: number[];
  hype: number[];
  score: number[];
}

export interface NewJob {
  id: string;
  sourceName: string;
  sourceUrl: string | null;
  sourceKey: string | null;
  settings: Settings;
  createdAt: Date;
}

/** Where jobs and clips live. ScyllaDB in production, a Map in tests. */
export interface Repo {
  createJob(job: NewJob): Promise<void>;
  getJob(id: string): Promise<JobRow | null>;
  recentJobs(limit: number): Promise<JobRow[]>;
  clips(id: string): Promise<ClipRow[]>;
  chart(id: string): Promise<Chart | null>;
  setStatus(id: string, status: JobStatus, message: string, error?: string | null): Promise<void>;
  ping(): Promise<void>;
}

export interface QueueStats {
  waiting: number;
  inProgress: number;
  dead: number;
  workers: { name: string; busy: number; idleMs: number }[];
}

export interface ProgressEvent {
  jobId: string;
  status: JobStatus;
  stage: string;
  pct: number;
  message: string;
}

/** The job queue and the live progress feed. Redis in production. */
export interface Queue {
  enqueue(jobId: string): Promise<void>;
  /** Saves key -> jobId once. Returns the job id stored first for this key. */
  rememberIdempotencyKey(key: string, jobId: string): Promise<string>;
  forgetIdempotencyKey(key: string): Promise<void>;
  /** True while this client is under its per-minute limit. */
  allow(client: string, perMinute: number): Promise<boolean>;
  subscribe(jobId: string, onEvent: (e: ProgressEvent) => void): () => void;
  stats(): Promise<QueueStats>;
  ping(): Promise<void>;
}
