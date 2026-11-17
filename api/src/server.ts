import { mkdir } from "node:fs/promises";
import { buildApp } from "./app.js";
import { config } from "./config.js";
import { ScyllaRepo } from "./db.js";
import { RedisQueue } from "./redis.js";

await mkdir(config.dataDir, { recursive: true });

const queue = new RedisQueue(config.redisUrl);
await queue.init();
const repo = await ScyllaRepo.connect({
  nodes: config.scyllaNodes,
  dc: config.scyllaDc,
  keyspace: config.keyspace,
  rf: config.scyllaRf,
  schemaFile: config.schemaFile,
  log: (m) => console.log(m),
});

const app = await buildApp({ repo, queue, config });
await app.listen({ port: config.port, host: config.host });

// On SIGTERM (a deploy), stop taking requests, let open ones finish, then close.
for (const signal of ["SIGTERM", "SIGINT"] as const) {
  process.once(signal, async () => {
    app.log.info(`${signal}: shutting down`);
    await app.close();
    await Promise.allSettled([queue.close(), repo.close()]);
    process.exit(0);
  });
}
