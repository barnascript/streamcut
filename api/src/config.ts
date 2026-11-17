import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const env = (key: string, fallback: string): string => {
  const v = process.env[key]?.trim();
  return v ? v : fallback;
};
const num = (key: string, fallback: number): number => {
  const v = Number(process.env[key]);
  return Number.isFinite(v) && v > 0 ? v : fallback;
};

const repoRoot = fileURLToPath(new URL("../../", import.meta.url));

export const config = {
  port: num("PORT", 8080),
  host: env("HOST", "0.0.0.0"),
  logLevel: env("LOG_LEVEL", "info"),
  redisUrl: env("REDIS_URL", "redis://127.0.0.1:6379"),
  scyllaNodes: env("SCYLLA_NODES", "127.0.0.1:9042").split(",").map((s) => s.trim()),
  scyllaDc: env("SCYLLA_DC", "datacenter1"),
  scyllaRf: num("SCYLLA_RF", 1),
  keyspace: "streamcut",
  schemaFile: resolve(env("SCHEMA_FILE", resolve(repoRoot, "db/schema.cql"))),
  dataDir: resolve(env("DATA_DIR", resolve(repoRoot, "data"))),
  webDir: resolve(env("WEB_DIR", resolve(repoRoot, "web"))),
  maxUploadMb: num("MAX_UPLOAD_MB", 2048),
  jobsPerMinute: num("JOBS_PER_MINUTE", 20),
  /** Only for local tests that serve a video from this machine. */
  allowPrivateUrls: env("ALLOW_PRIVATE_URLS", "0") === "1",
};

export type Config = typeof config;
