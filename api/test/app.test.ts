import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AddressInfo } from "node:net";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { FastifyInstance } from "fastify";
import { buildApp } from "../src/app.js";
import { config } from "../src/config.js";
import { MemQueue, MemRepo } from "./fakes.js";

let app: FastifyInstance;
let repo: MemRepo;
let queue: MemQueue;
let dataDir: string;

beforeEach(async () => {
  dataDir = await mkdtemp(join(tmpdir(), "streamcut-test-"));
  repo = new MemRepo();
  queue = new MemQueue();
  app = await buildApp({ repo, queue, config: { ...config, dataDir, jobsPerMinute: 5 }, logger: false });
});

afterEach(async () => {
  await app.close();
  await rm(dataDir, { recursive: true, force: true });
});

const postUrl = (url: string, extra: Record<string, unknown> = {}, headers: Record<string, string> = {}) =>
  app.inject({ method: "POST", url: "/v1/jobs", payload: { url, ...extra }, headers });

function multipart(fields: Record<string, string>, file?: { name: string; body: Buffer }) {
  const boundary = "----streamcut-test";
  const parts: Buffer[] = [];
  for (const [k, v] of Object.entries(fields)) {
    parts.push(Buffer.from(`--${boundary}\r\nContent-Disposition: form-data; name="${k}"\r\n\r\n${v}\r\n`));
  }
  if (file) {
    parts.push(
      Buffer.from(`--${boundary}\r\nContent-Disposition: form-data; name="file"; filename="${file.name}"\r\nContent-Type: video/mp4\r\n\r\n`),
      file.body,
      Buffer.from("\r\n"),
    );
  }
  parts.push(Buffer.from(`--${boundary}--\r\n`));
  return { payload: Buffer.concat(parts), headers: { "content-type": `multipart/form-data; boundary=${boundary}` } };
}

describe("creating jobs", () => {
  it("queues a job from a link with default settings", async () => {
    const res = await postUrl("https://example.com/videos/My%20Stream.mp4");
    expect(res.statusCode).toBe(202);
    const { job } = res.json();
    expect(job.status).toBe("queued");
    expect(job.sourceName).toBe("My Stream.mp4");
    expect(job.settings).toEqual({ clips: 5, minLen: 20, maxLen: 60, layout: "blur", captions: true });
    expect(res.headers.location).toBe(`/v1/jobs/${job.id}`);
    expect(queue.queued).toEqual([job.id]);
  });

  it("saves an upload to disk and queues it", async () => {
    const { payload, headers } = multipart({ clips: "3", layout: "crop", captions: "false" }, { name: "game.mp4", body: Buffer.from("fake video") });
    const res = await app.inject({ method: "POST", url: "/v1/jobs", payload, headers });
    expect(res.statusCode).toBe(202);
    const { job } = res.json();
    expect(job.settings).toMatchObject({ clips: 3, layout: "crop", captions: false });
    const row = repo.jobs.get(job.id)!;
    expect(row.sourceKey).toBe(`sources/${job.id}.upload`);
  });

  it("rejects an upload without a file", async () => {
    const { payload, headers } = multipart({ clips: "3" });
    const res = await app.inject({ method: "POST", url: "/v1/jobs", payload, headers });
    expect(res.statusCode).toBe(400);
    expect(queue.queued).toHaveLength(0);
  });

  it.each([
    ["http://169.254.169.254/latest/meta-data", "private address"],
    ["http://localhost:6379", "private network"],
    ["http://[::1]/x.mp4", "private address"],
    ["ftp://example.com/a.mp4", "Only http and https"],
    ["https://user:pw@example.com/a.mp4", "username or password"],
    ["not a url", "not a valid link"],
  ])("refuses %s", async (url, why) => {
    const res = await postUrl(url);
    expect(res.statusCode).toBe(400);
    expect(res.json().message).toContain(why);
    expect(queue.queued).toHaveLength(0);
  });

  it("checks settings", async () => {
    let res = await postUrl("https://example.com/a.mp4", { settings: { clips: 0 } });
    expect(res.statusCode).toBe(400);
    res = await postUrl("https://example.com/a.mp4", { settings: { minLen: 60, maxLen: 30 } });
    expect(res.statusCode).toBe(400);
    expect(res.json().message).toContain("maxLen");
  });

  it("returns the same job for the same Idempotency-Key", async () => {
    const h = { "idempotency-key": "abc-123" };
    const a = await postUrl("https://example.com/a.mp4", {}, h);
    const b = await postUrl("https://example.com/a.mp4", {}, h);
    expect(a.statusCode).toBe(202);
    expect(b.statusCode).toBe(200);
    expect(b.json().reused).toBe(true);
    expect(b.json().job.id).toBe(a.json().job.id);
    expect(queue.queued).toHaveLength(1);
  });

  it("frees the Idempotency-Key when saving fails, so a retry works", async () => {
    repo.failCreate = true;
    const h = { "idempotency-key": "retry-me" };
    expect((await postUrl("https://example.com/a.mp4", {}, h)).statusCode).toBe(500);
    repo.failCreate = false;
    expect((await postUrl("https://example.com/a.mp4", {}, h)).statusCode).toBe(202);
  });

  it("marks the job failed and answers 503 when the queue is down", async () => {
    queue.failEnqueue = true;
    const res = await postUrl("https://example.com/a.mp4");
    expect(res.statusCode).toBe(503);
    const [job] = [...repo.jobs.values()];
    expect(job?.status).toBe("failed");
  });

  it("limits how many jobs one client can start a minute", async () => {
    const codes = [];
    for (let i = 0; i < 7; i++) codes.push((await postUrl("https://example.com/a.mp4")).statusCode);
    expect(codes.filter((c) => c === 202)).toHaveLength(5);
    expect(codes.at(-1)).toBe(429);
  });
});

describe("reading jobs and clips", () => {
  async function doneJob() {
    const { job } = (await postUrl("https://example.com/a.mp4")).json();
    const row = repo.jobs.get(job.id)!;
    Object.assign(row, { status: "done", progress: 100, clipCount: 2, durationS: 600, error: "internal detail" });
    await mkdir(join(dataDir, "clips", job.id), { recursive: true });
    const bytes = Buffer.from(Array.from({ length: 1000 }, (_, i) => i % 256));
    await writeFile(join(dataDir, "clips", job.id, "1.mp4"), bytes);
    repo.clipRows.set(job.id, [
      { rank: 2, start: 50, end: 80, score: 0.5, title: "Second", reasons: ["b"], transcript: "", videoKey: `clips/${job.id}/2.mp4`, thumbKey: "", width: 1080, height: 1920, sizeBytes: 0 },
      { rank: 1, start: 10, end: 40, score: 1.2, title: "First", reasons: ["a"], transcript: "hi", videoKey: `clips/${job.id}/1.mp4`, thumbKey: "", width: 1080, height: 1920, sizeBytes: 1000 },
      { rank: 3, start: 0, end: 5, score: 0, title: "Escape", reasons: [], transcript: "", videoKey: "../../etc/passwd", thumbKey: "", width: 0, height: 0, sizeBytes: 0 },
    ]);
    repo.charts.set(job.id, { step: 1, loudness: [0], speech: [0], hype: [0], score: [0] });
    return { id: job.id as string, bytes };
  }

  it("returns clips best first, with links and no internal errors", async () => {
    const { id } = await doneJob();
    const body = (await app.inject(`/v1/jobs/${id}`)).json();
    expect(body.clips.map((c: { rank: number }) => c.rank)).toEqual([1, 2, 3]);
    expect(body.clips[0].videoUrl).toBe(`/v1/jobs/${id}/clips/1/video`);
    expect(body.job.error).toBeNull();
    expect(body.chart.step).toBe(1);
  });

  it("404s for unknown and malformed ids", async () => {
    expect((await app.inject("/v1/jobs/00000000-0000-4000-8000-000000000000")).statusCode).toBe(404);
    expect((await app.inject("/v1/jobs/../../etc")).statusCode).toBe(404);
  });

  it("serves byte ranges so players can seek", async () => {
    const { id, bytes } = await doneJob();
    const res = await app.inject({ url: `/v1/jobs/${id}/clips/1/video`, headers: { range: "bytes=100-199" } });
    expect(res.statusCode).toBe(206);
    expect(res.headers["content-range"]).toBe("bytes 100-199/1000");
    expect(res.rawPayload.equals(bytes.subarray(100, 200))).toBe(true);
    const tail = await app.inject({ url: `/v1/jobs/${id}/clips/1/video`, headers: { range: "bytes=-10" } });
    expect(tail.rawPayload.equals(bytes.subarray(990))).toBe(true);
    const bad = await app.inject({ url: `/v1/jobs/${id}/clips/1/video`, headers: { range: "bytes=5000-" } });
    expect(bad.statusCode).toBe(416);
    const whole = await app.inject(`/v1/jobs/${id}/clips/1/video`);
    expect(whole.statusCode).toBe(200);
    expect(whole.headers["accept-ranges"]).toBe("bytes");
  });

  it("never serves files outside the data folder", async () => {
    const { id } = await doneJob();
    expect((await app.inject(`/v1/jobs/${id}/clips/3/video`)).statusCode).toBe(404);
  });

  it("lets a failed job be retried, and only a failed one", async () => {
    const { id } = await doneJob();
    expect((await app.inject({ method: "POST", url: `/v1/jobs/${id}/retry` })).statusCode).toBe(409);
    repo.jobs.get(id)!.status = "failed";
    const res = await app.inject({ method: "POST", url: `/v1/jobs/${id}/retry` });
    expect(res.statusCode).toBe(202);
    expect(queue.queued.filter((q) => q === id)).toHaveLength(2);
  });
});

describe("live progress", () => {
  it("sends the current state, then worker updates, then closes when done", async () => {
    const { job } = (await postUrl("https://example.com/a.mp4")).json();
    await app.listen({ port: 0, host: "127.0.0.1" });
    const { port } = app.server.address() as AddressInfo;
    const res = await fetch(`http://127.0.0.1:${port}/v1/jobs/${job.id}/events`);
    expect(res.headers.get("content-type")).toContain("text/event-stream");
    const reader = res.body!.getReader();
    const dec = new TextDecoder();
    let text = "";
    const readUntil = async (needle: string) => {
      while (!text.includes(needle)) {
        const { value, done } = await reader.read();
        if (done) break;
        text += dec.decode(value);
      }
    };
    await readUntil("Waiting for a worker");
    queue.emit({ jobId: job.id, status: "running", stage: "render", pct: 80, message: "Cut clip 1 of 2" });
    queue.emit({ jobId: job.id, status: "done", stage: "done", pct: 100, message: "2 clips ready" });
    await readUntil("2 clips ready");
    const { done } = await reader.read();
    expect(done).toBe(true);
    expect(text).toContain('"stage":"render"');
    expect(queue.listeners.get(job.id)?.size ?? 0).toBe(0);
  });
});
