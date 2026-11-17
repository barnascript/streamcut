import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import { resolve, sep } from "node:path";
import type { FastifyReply } from "fastify";

/**
 * Turns a key from the database ("clips/<job>/1.mp4") into a path, and
 * refuses anything that would step outside the data folder.
 */
export function safePath(dataDir: string, key: string): string | null {
  const root = resolve(dataDir);
  const p = resolve(root, key);
  return p.startsWith(root + sep) ? p : null;
}

/** Parses a single "bytes=a-b" range. Returns null when it cannot be served. */
export function parseRange(header: string | undefined, size: number): { start: number; end: number } | null | "none" {
  if (!header) return "none";
  const m = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
  if (!m) return null;
  const [, a = "", b = ""] = m;
  if (a === "" && b === "") return null;
  let start: number;
  let end: number;
  if (a === "") {
    // "bytes=-500" means the last 500 bytes.
    const tail = Number(b);
    if (tail === 0) return null;
    start = Math.max(0, size - tail);
    end = size - 1;
  } else {
    start = Number(a);
    end = b === "" ? size - 1 : Math.min(Number(b), size - 1);
  }
  if (start > end || start >= size) return null;
  return { start, end };
}

/** Sends a file with Range support, so players can jump around in a clip. */
export async function sendFile(reply: FastifyReply, path: string, type: string, range: string | undefined) {
  let size: number;
  try {
    size = (await stat(path)).size;
  } catch {
    return reply.code(404).send({ error: "not_found", message: "That file is not there." });
  }
  reply.header("Content-Type", type).header("Accept-Ranges", "bytes").header("Cache-Control", "private, max-age=3600");
  const r = parseRange(range, size);
  if (r === null) {
    return reply.code(416).header("Content-Range", `bytes */${size}`).send();
  }
  if (r === "none") {
    reply.header("Content-Length", size);
    return reply.send(createReadStream(path));
  }
  reply
    .code(206)
    .header("Content-Range", `bytes ${r.start}-${r.end}/${size}`)
    .header("Content-Length", r.end - r.start + 1);
  return reply.send(createReadStream(path, { start: r.start, end: r.end }));
}
