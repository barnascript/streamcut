import { describe, expect, it } from "vitest";
import { schemaStatements } from "../src/db.js";
import { parseRange, safePath } from "../src/files.js";
import { checkUrl } from "../src/urlguard.js";

describe("checkUrl", () => {
  it("allows normal public links", () => {
    expect(checkUrl("https://upload.wikimedia.org/a.webm").ok).toBe(true);
    expect(checkUrl("http://8.8.8.8/x.mp4").ok).toBe(true);
  });
  it.each(["http://10.0.0.5/a", "http://172.20.1.1/a", "http://192.168.0.1/a", "http://100.100.1.1/a", "http://[::ffff:127.0.0.1]/a", "http://[fd00::1]/a", "http://metadata.google.internal/"])(
    "blocks %s",
    (u) => expect(checkUrl(u).ok).toBe(false),
  );
  it("can allow private addresses for local tests", () => {
    expect(checkUrl("http://localhost:9000/a.mp4", true).ok).toBe(true);
  });
});

describe("parseRange", () => {
  it("reads the usual forms", () => {
    expect(parseRange(undefined, 100)).toBe("none");
    expect(parseRange("bytes=0-9", 100)).toEqual({ start: 0, end: 9 });
    expect(parseRange("bytes=90-", 100)).toEqual({ start: 90, end: 99 });
    expect(parseRange("bytes=-5", 100)).toEqual({ start: 95, end: 99 });
    expect(parseRange("bytes=50-500", 100)).toEqual({ start: 50, end: 99 });
  });
  it("refuses what it cannot serve", () => {
    expect(parseRange("bytes=100-", 100)).toBeNull();
    expect(parseRange("bytes=9-3", 100)).toBeNull();
    expect(parseRange("bytes=0-1,5-6", 100)).toBeNull();
    expect(parseRange("items=0-1", 100)).toBeNull();
  });
});

describe("safePath", () => {
  it("keeps paths inside the data folder", () => {
    expect(safePath("/data", "clips/a/1.mp4")).toBe("/data/clips/a/1.mp4");
    expect(safePath("/data", "../etc/passwd")).toBeNull();
    expect(safePath("/data", "/etc/passwd")).toBeNull();
    expect(safePath("/data", "")).toBeNull();
  });
});

describe("schemaStatements", () => {
  it("drops comments and fills in the replication factor", () => {
    const s = schemaStatements("-- hi\nCREATE KEYSPACE x WITH r = ${RF};\n\n-- table\nCREATE TABLE y (a int);\n", 3);
    expect(s).toEqual(["CREATE KEYSPACE x WITH r = 3", "CREATE TABLE y (a int)"]);
  });
});
