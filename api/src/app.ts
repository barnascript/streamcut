import Fastify, { type FastifyInstance, type FastifyRequest } from "fastify";
import multipart from "@fastify/multipart";
import fastifyStatic from "@fastify/static";
import { randomUUID } from "node:crypto";
import { createWriteStream } from "node:fs";
import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";
import { pipeline } from "node:stream/promises";
import { z } from "zod";
import type { Config } from "./config.js";
import { safePath, sendFile } from "./files.js";
import type { ClipRow, JobRow, NewJob, ProgressEvent, Queue, Repo } from "./types.js";
import { Settings } from "./types.js";
import { checkUrl } from "./urlguard.js";

export interface Deps {
  repo: Repo;
  queue: Queue;
  config: Config;
  logger?: boolean | object;
}

const Uuid = z.uuid();

class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

const cleanName = (s: string) =>
  s
    .replace(/[^\p{L}\p{N} ._()-]/gu, "")
    .trim()
    .slice(0, 120) || "video";

function clipView(jobId: string, c: ClipRow) {
  const base = `/v1/jobs/${jobId}/clips/${c.rank}`;
  return {
    rank: c.rank,
    title: c.title,
    start: c.start,
    end: c.end,
    seconds: Math.round((c.end - c.start) * 10) / 10,
    score: Math.round(c.score * 100) / 100,
    reasons: c.reasons,
    transcript: c.transcript,
    width: c.width,
    height: c.height,
    sizeBytes: c.sizeBytes,
    videoUrl: `${base}/video`,
    thumbUrl: `${base}/thumb`,
  };
}

function jobView(j: JobRow) {
  return {
    id: j.id,
    status: j.status,
    stage: j.stage,
    progress: j.progress,
    message: j.message,
    sourceName: j.sourceName,
    sourceUrl: j.sourceUrl,
    settings: j.settings,
    durationS: j.durationS,
    clipCount: j.clipCount,
    words: j.words,
    attempts: j.attempts,
    // The raw error is for people running the service, not for the page.
    error: j.status === "failed" ? j.message : null,
    createdAt: j.createdAt,
    updatedAt: j.updatedAt,
    links: { self: `/v1/jobs/${j.id}`, events: `/v1/jobs/${j.id}/events` },
  };
}

export async function buildApp(deps: Deps): Promise<FastifyInstance> {
  const { repo, queue, config } = deps;
  const app = Fastify({
    logger: deps.logger ?? { level: config.logLevel },
    bodyLimit: 64 * 1024,
    // Behind a load balancer the client address is in X-Forwarded-For.
    trustProxy: true,
  });

  await app.register(multipart, { limits: { fileSize: config.maxUploadMb * 1024 * 1024, files: 1, fields: 10 } });

  app.setErrorHandler((err, req, reply) => {
    if (err instanceof HttpError) {
      return reply.code(err.status).send({ error: err.code, message: err.message });
    }
    if (err instanceof z.ZodError) {
      const message = err.issues.map((i) => `${i.path.join(".") || "input"}: ${i.message}`).join("; ");
      return reply.code(400).send({ error: "invalid_input", message });
    }
    const status = (err as { statusCode?: number }).statusCode ?? 500;
    if (status >= 500) req.log.error(err);
    return reply.code(status).send({ error: status >= 500 ? "server_error" : "bad_request", message: status >= 500 ? "Something went wrong on our side." : (err as Error).message });
  });

  app.get("/healthz", async () => ({ ok: true }));
  app.get("/readyz", async (_req, reply) => {
    try {
      await Promise.all([repo.ping(), queue.ping()]);
      return { ok: true };
    } catch (e) {
      return reply.code(503).send({ ok: false, message: (e as Error).message });
    }
  });

  const getJobOr404 = async (id: string) => {
    if (!Uuid.safeParse(id).success) throw new HttpError(404, "not_found", "There is no job with that id.");
    const job = await repo.getJob(id);
    if (!job) throw new HttpError(404, "not_found", "There is no job with that id.");
    return job;
  };

  /**
   * POST /v1/jobs
   *   JSON:      { "url": "https://...", "settings": { "clips": 5 } }
   *   multipart: a "file" part plus optional setting fields (clips, layout...)
   * Send an Idempotency-Key header to make retries safe: the same key always
   * gives back the same job.
   */
  app.post("/v1/jobs", async (req: FastifyRequest, reply) => {
    const client = req.ip;
    if (!(await queue.allow(client, config.jobsPerMinute))) {
      throw new HttpError(429, "too_many_requests", `Slow down: at most ${config.jobsPerMinute} new jobs a minute.`);
    }

    const id = randomUUID();
    const idemKey = req.headers["idempotency-key"];
    if (typeof idemKey === "string" && idemKey.length > 0) {
      if (idemKey.length > 200) throw new HttpError(400, "invalid_input", "Idempotency-Key is too long.");
      const first = await queue.rememberIdempotencyKey(idemKey, id);
      if (first !== id) {
        const existing = await repo.getJob(first);
        if (existing) return reply.code(200).send({ job: jobView(existing), reused: true });
        // The first request with this key never got as far as saving a job.
        await queue.forgetIdempotencyKey(idemKey);
        await queue.rememberIdempotencyKey(idemKey, id);
      }
    }

    let job: NewJob;
    try {
      job = req.isMultipart() ? await fromUpload(req, id) : fromJson(req.body, id);
      await repo.createJob(job);
    } catch (e) {
      if (typeof idemKey === "string" && idemKey) await queue.forgetIdempotencyKey(idemKey).catch(() => {});
      throw e;
    }
    try {
      await queue.enqueue(id);
    } catch (e) {
      req.log.error(e, "could not queue job");
      await repo.setStatus(id, "failed", "Could not queue the job, please try again", String(e)).catch(() => {});
      throw new HttpError(503, "queue_unavailable", "The job queue is not reachable right now. Please try again.");
    }
    const saved = await repo.getJob(id);
    return reply
      .code(202)
      .header("Location", `/v1/jobs/${id}`)
      .send({ job: saved ? jobView(saved) : { id, status: "queued" } });
  });

  function fromJson(body: unknown, id: string): NewJob {
    const parsed = z.object({ url: z.string().min(1), settings: Settings.optional() }).parse(body ?? {});
    const check = checkUrl(parsed.url, config.allowPrivateUrls);
    if (!check.ok) throw new HttpError(400, "invalid_url", check.reason);
    const last = decodeURIComponent(check.url.pathname.split("/").filter(Boolean).pop() ?? "");
    return {
      id,
      sourceName: cleanName(last || check.url.hostname),
      sourceUrl: check.url.toString(),
      sourceKey: null,
      settings: parsed.settings ?? Settings.parse({}),
      createdAt: new Date(),
    };
  }

  async function fromUpload(req: FastifyRequest, id: string): Promise<NewJob> {
    const fields: Record<string, string> = {};
    const key = `sources/${id}.upload`;
    const dest = join(config.dataDir, key);
    let name: string | null = null;
    await mkdir(join(config.dataDir, "sources"), { recursive: true });
    try {
      for await (const part of req.parts()) {
        if (part.type === "file") {
          if (part.fieldname !== "file") {
            part.file.resume();
            continue;
          }
          name = cleanName(part.filename || "video");
          await pipeline(part.file, createWriteStream(dest));
          if (part.file.truncated) {
            throw new HttpError(413, "too_large", `The file is bigger than ${config.maxUploadMb} MB.`);
          }
        } else if (typeof part.value === "string") {
          fields[part.fieldname] = part.value;
        }
      }
      if (!name) throw new HttpError(400, "invalid_input", 'Send the video in a form field called "file".');
      const settings = Settings.parse(fields);
      return { id, sourceName: name, sourceUrl: null, sourceKey: key, settings, createdAt: new Date() };
    } catch (e) {
      await rm(dest, { force: true });
      throw e;
    }
  }

  app.get("/v1/jobs", async (req) => {
    const { limit } = z.object({ limit: z.coerce.number().int().min(1).max(50).default(20) }).parse(req.query);
    const jobs = await repo.recentJobs(limit);
    return { jobs: jobs.map(jobView) };
  });

  app.get<{ Params: { id: string } }>("/v1/jobs/:id", async (req) => {
    const job = await getJobOr404(req.params.id);
    const [clips, chart] = job.status === "done" ? await Promise.all([repo.clips(job.id), repo.chart(job.id)]) : [[], null];
    return { job: jobView(job), clips: clips.sort((a, b) => a.rank - b.rank).map((c) => clipView(job.id, c)), chart };
  });

  /** Lets someone try a failed job again. */
  app.post<{ Params: { id: string } }>("/v1/jobs/:id/retry", async (req, reply) => {
    const job = await getJobOr404(req.params.id);
    if (job.status !== "failed") throw new HttpError(409, "not_failed", "Only failed jobs can be tried again.");
    await repo.setStatus(job.id, "queued", "Waiting for a worker");
    await queue.enqueue(job.id);
    return reply.code(202).send({ job: jobView({ ...job, status: "queued", message: "Waiting for a worker" }) });
  });

  /**
   * Live progress as Server-Sent Events. The first event is the current
   * state from the database, then every update the worker publishes.
   * The stream closes itself when the job is done or failed.
   */
  app.get<{ Params: { id: string } }>("/v1/jobs/:id/events", async (req, reply) => {
    const job = await getJobOr404(req.params.id);
    reply.hijack();
    const res = reply.raw;
    res.writeHead(200, {
      "Content-Type": "text/event-stream; charset=utf-8",
      "Cache-Control": "no-cache, no-transform",
      Connection: "keep-alive",
      "X-Accel-Buffering": "no",
    });
    const send = (e: ProgressEvent) => res.write(`event: progress\ndata: ${JSON.stringify(e)}\n\n`);
    const finished = (s: string) => s === "done" || s === "failed";
    let closed = false;
    const close = () => {
      if (closed) return;
      closed = true;
      clearInterval(ping);
      unsubscribe();
      res.end();
    };
    const unsubscribe = queue.subscribe(job.id, (e) => {
      send(e);
      if (finished(e.status)) close();
    });
    const ping = setInterval(() => res.write(": ping\n\n"), 15_000);
    req.raw.on("close", close);
    send({ jobId: job.id, status: job.status, stage: job.stage ?? "queued", pct: job.progress, message: job.message ?? "" });
    // Re-read after subscribing so an update between the two reads is not lost.
    const fresh = await repo.getJob(job.id);
    if (fresh && (fresh.progress !== job.progress || fresh.status !== job.status)) {
      send({ jobId: fresh.id, status: fresh.status, stage: fresh.stage ?? "", pct: fresh.progress, message: fresh.message ?? "" });
    }
    if (finished((fresh ?? job).status)) close();
  });

  const clipFile = (kind: "video" | "thumb") => async (req: FastifyRequest<{ Params: { id: string; rank: string } }>, reply: import("fastify").FastifyReply) => {
    const job = await getJobOr404(req.params.id);
    const rank = Number(req.params.rank);
    const clip = (await repo.clips(job.id)).find((c) => c.rank === rank);
    if (!clip) throw new HttpError(404, "not_found", "There is no clip with that number.");
    const path = safePath(config.dataDir, kind === "video" ? clip.videoKey : clip.thumbKey);
    if (!path) throw new HttpError(404, "not_found", "That file is not there.");
    if (kind === "video" && (req.query as { download?: string }).download !== undefined) {
      reply.header("Content-Disposition", `attachment; filename="clip-${rank}.mp4"`);
    }
    return sendFile(reply, path, kind === "video" ? "video/mp4" : "image/jpeg", req.headers.range);
  };
  app.get("/v1/jobs/:id/clips/:rank/video", clipFile("video"));
  app.get("/v1/jobs/:id/clips/:rank/thumb", clipFile("thumb"));

  app.get("/v1/queue", async () => queue.stats());

  await app.register(fastifyStatic, { root: config.webDir, prefix: "/", index: ["index.html"] });

  return app;
}
