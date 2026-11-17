import { isIP } from "node:net";

/**
 * A quick first check on a pasted link, so obvious mistakes get a clear
 * error straight away. The worker does the full check (it resolves the name
 * and refuses private addresses) right before it downloads.
 */
export type UrlCheck = { ok: true; url: URL } | { ok: false; reason: string };

function privateV4(ip: string): boolean {
  const [a = 0, b = 0] = ip.split(".").map(Number);
  return (
    a === 0 ||
    a === 10 ||
    a === 127 ||
    (a === 100 && b >= 64 && b <= 127) ||
    (a === 169 && b === 254) ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && b === 168) ||
    (a === 198 && (b === 18 || b === 19)) ||
    a >= 224
  );
}

function privateV6(ip: string): boolean {
  const x = ip.toLowerCase();
  if (x === "::" || x === "::1") return true;
  const mapped = x.match(/^::ffff:(\d+\.\d+\.\d+\.\d+)$/);
  if (mapped?.[1]) return privateV4(mapped[1]);
  // new URL() rewrites [::ffff:127.0.0.1] as [::ffff:7f00:1], so check that form too.
  const hex = x.match(/^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/);
  if (hex?.[1] && hex[2]) {
    const hi = parseInt(hex[1], 16);
    const lo = parseInt(hex[2], 16);
    return privateV4(`${hi >> 8}.${hi & 255}.${lo >> 8}.${lo & 255}`);
  }
  return /^(fc|fd|fe8|fe9|fea|feb|ff)/.test(x);
}

export function checkUrl(raw: string, allowPrivate = false): UrlCheck {
  if (raw.length > 2048) return { ok: false, reason: "That link is too long." };
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return { ok: false, reason: "That is not a valid link." };
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    return { ok: false, reason: "Only http and https links work." };
  }
  if (url.username || url.password) {
    return { ok: false, reason: "Links with a username or password are not allowed." };
  }
  if (allowPrivate) return { ok: true, url };
  const host = url.hostname.replace(/^\[|\]$/g, "").toLowerCase();
  if (host === "localhost" || host.endsWith(".localhost") || host.endsWith(".internal") || host.endsWith(".local")) {
    return { ok: false, reason: "That link points inside a private network." };
  }
  const kind = isIP(host);
  if ((kind === 4 && privateV4(host)) || (kind === 6 && privateV6(host))) {
    return { ok: false, reason: "That link points to a private address." };
  }
  return { ok: true, url };
}
