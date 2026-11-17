// Streamcut page: no framework, no build step. Hash routes:
//   #/          new video + recent list
//   #/job/<id>  live progress, then the clips

const $ = (id) => document.getElementById(id);
const STAGES = ["download", "probe", "audio", "transcribe", "analyse", "render"];

const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

function clock(t) {
  t = Math.max(0, Math.round(t));
  const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), s = t % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`;
}

function ago(date) {
  const s = (Date.now() - new Date(date).getTime()) / 1000;
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return new Date(date).toLocaleDateString();
}

const mb = (b) => `${(b / 1048576).toFixed(1)} MB`;

async function api(path, opts) {
  const res = await fetch(path, opts);
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(body.message || `Request failed (${res.status})`);
  return body;
}

/* ---------- routing ---------- */

let stopJob = () => {};

function route() {
  stopJob();
  const m = location.hash.match(/^#\/job\/([0-9a-f-]{36})$/);
  $("home").hidden = Boolean(m);
  $("job").hidden = !m;
  if (m) openJob(m[1]);
  else loadRecent();
  window.scrollTo(0, 0);
}
window.addEventListener("hashchange", route);

/* ---------- home ---------- */

async function loadRecent() {
  const list = $("recent");
  try {
    const { jobs } = await api("/v1/jobs?limit=15");
    if (!jobs.length) {
      list.innerHTML = `<li class="muted">Nothing yet. Your videos will show up here.</li>`;
      return;
    }
    list.innerHTML = jobs
      .map(
        (j) => `<li><a href="#/job/${j.id}">
          <span class="name">${esc(j.sourceName || "Video")}</span>
          <span class="pill ${j.status}">${j.status === "done" ? `${j.clipCount} clips` : j.status}</span>
          <span class="when">${ago(j.createdAt)}${j.durationS ? ` · ${clock(j.durationS)} long` : ""}</span>
        </a></li>`,
      )
      .join("");
  } catch (e) {
    list.innerHTML = `<li class="error">${esc(e.message)}</li>`;
  }
}

const drop = $("drop");
const fileInput = $("file");
function showFile() {
  const f = fileInput.files[0];
  drop.classList.toggle("has-file", Boolean(f));
  $("drop-text").textContent = f ? `${f.name} (${mb(f.size)})` : "Drop a video here, or click to choose";
  if (f) $("url").value = "";
}
fileInput.addEventListener("change", showFile);
["dragenter", "dragover"].forEach((t) =>
  drop.addEventListener(t, (e) => {
    e.preventDefault();
    drop.classList.add("over");
  }),
);
["dragleave", "drop"].forEach((t) => drop.addEventListener(t, () => drop.classList.remove("over")));
drop.addEventListener("drop", (e) => {
  e.preventDefault();
  if (e.dataTransfer.files.length) {
    fileInput.files = e.dataTransfer.files;
    showFile();
  }
});
$("url").addEventListener("input", () => {
  if ($("url").value) {
    fileInput.value = "";
    showFile();
  }
});

function settings() {
  return {
    clips: Number($("clips").value),
    minLen: Number($("minLen").value),
    maxLen: Number($("maxLen").value),
    layout: document.querySelector('input[name="layout"]:checked').value,
    captions: $("captions").checked,
  };
}

// XHR instead of fetch, because fetch cannot report upload progress.
function upload(file, s, key) {
  return new Promise((resolve, reject) => {
    const form = new FormData();
    for (const [k, v] of Object.entries(s)) form.append(k, String(v));
    form.append("file", file);
    const xhr = new XMLHttpRequest();
    xhr.open("POST", "/v1/jobs");
    xhr.setRequestHeader("Idempotency-Key", key);
    xhr.upload.onprogress = (e) => {
      if (!e.lengthComputable) return;
      const pct = Math.round((e.loaded / e.total) * 100);
      $("upload-bar").style.width = `${pct}%`;
      $("upload-text").textContent = `Uploading ${mb(e.loaded)} of ${mb(e.total)}`;
    };
    xhr.onload = () => {
      const body = JSON.parse(xhr.responseText || "{}");
      if (xhr.status >= 200 && xhr.status < 300) resolve(body);
      else reject(new Error(body.message || `Upload failed (${xhr.status})`));
    };
    xhr.onerror = () => reject(new Error("The upload was cut off. Check your connection and try again."));
    xhr.send(form);
  });
}

$("new-job").addEventListener("submit", async (e) => {
  e.preventDefault();
  const err = $("form-error");
  err.textContent = "";
  const file = fileInput.files[0];
  const url = $("url").value.trim();
  const s = settings();
  if (!file && !url) return (err.textContent = "Choose a video or paste a link first.");
  if (s.maxLen < s.minLen) return (err.textContent = "Longest must be at least as long as shortest.");
  // One key per click: if the request is retried, the server returns the same job.
  const key = crypto.randomUUID();
  $("go").disabled = true;
  try {
    let res;
    if (file) {
      $("upload").hidden = false;
      res = await upload(file, s, key);
    } else {
      res = await api("/v1/jobs", {
        method: "POST",
        headers: { "Content-Type": "application/json", "Idempotency-Key": key },
        body: JSON.stringify({ url, settings: s }),
      });
    }
    location.hash = `#/job/${res.job.id}`;
    fileInput.value = "";
    $("url").value = "";
    showFile();
  } catch (e2) {
    err.textContent = e2.message;
  } finally {
    $("go").disabled = false;
    $("upload").hidden = true;
    $("upload-bar").style.width = "0";
  }
});

/* ---------- job page ---------- */

function showProgress(p) {
  const panel = $("progress-panel");
  panel.classList.toggle("failed", p.status === "failed");
  $("job-bar").style.width = `${p.status === "failed" ? 100 : p.pct}%`;
  $("job-message").textContent = p.message || "";
  const at = STAGES.indexOf(p.stage);
  for (const li of $("steps").children) {
    const i = STAGES.indexOf(li.dataset.stage);
    li.classList.toggle("now", i === at && p.status === "running");
    li.classList.toggle("done", p.status === "done" || (at >= 0 && i < at));
  }
}

function showHead(job) {
  $("job-title").textContent = job.sourceName || "Video";
  const bits = [];
  if (job.durationS) bits.push(`${clock(job.durationS)} long`);
  if (job.words) bits.push(`${job.words.toLocaleString()} words heard`);
  bits.push(`${job.settings.clips} clips asked, ${job.settings.minLen}–${job.settings.maxLen} s`);
  bits.push(job.settings.layout === "blur" ? "full frame" : "centre crop");
  if (job.attempts > 1) bits.push(`finished on try ${job.attempts}`);
  $("job-meta").textContent = bits.join(" · ");
  $("retry").hidden = job.status !== "failed";
}

async function openJob(id) {
  let source = null;
  let stopped = false;
  stopJob = () => {
    stopped = true;
    source?.close();
  };
  $("clip-list").innerHTML = "";
  $("chart-panel").hidden = true;
  $("progress-panel").hidden = false;
  $("job-title").textContent = "…";
  $("job-meta").textContent = "";

  let data;
  try {
    data = await api(`/v1/jobs/${id}`);
  } catch (e) {
    $("job-title").textContent = "Not found";
    $("job-message").textContent = e.message;
    return;
  }
  if (stopped) return;
  showHead(data.job);
  if (data.job.status === "done") return showResult(data);

  showProgress({ status: data.job.status, stage: data.job.stage, pct: data.job.progress, message: data.job.message });
  source = new EventSource(`/v1/jobs/${id}/events`);
  source.addEventListener("progress", async (ev) => {
    const p = JSON.parse(ev.data);
    showProgress(p);
    if (p.status === "done" || p.status === "failed") {
      source.close();
      const fresh = await api(`/v1/jobs/${id}`);
      if (stopped) return;
      showHead(fresh.job);
      if (fresh.job.status === "done") showResult(fresh);
    }
  });
}

$("retry").addEventListener("click", async () => {
  const id = location.hash.split("/").pop();
  try {
    await api(`/v1/jobs/${id}/retry`, { method: "POST" });
    route();
  } catch (e) {
    $("job-message").textContent = e.message;
  }
});

function showResult({ job, clips, chart }) {
  $("progress-panel").hidden = true;
  if (chart) drawChart(chart, clips, job.durationS);
  $("clip-list").innerHTML = clips
    .map(
      (c) => `<article class="clip" id="clip-${c.rank}">
        <video controls preload="none" playsinline poster="${c.thumbUrl}" src="${c.videoUrl}"></video>
        <div class="body">
          <span class="rank">Clip ${c.rank}</span>
          <h3>${esc(c.title)}</h3>
          <span class="facts">${clock(c.start)} – ${clock(c.end)} · ${c.seconds} s · ${mb(c.sizeBytes)}</span>
          <ul>${c.reasons.map((r) => `<li>${esc(r)}</li>`).join("")}</ul>
          ${c.transcript ? `<details><summary>What is said</summary><p>${esc(c.transcript)}</p></details>` : ""}
        </div>
        <div class="actions">
          <a class="primary" href="${c.videoUrl}?download" download>Download</a>
        </div>
      </article>`,
    )
    .join("");
}

/* ---------- chart ---------- */

function drawChart(chart, clips, duration) {
  $("chart-panel").hidden = false;
  const W = 1000, H = 190, top = 10, bottom = 22;
  const n = chart.score.length;
  if (!n) return;
  const x = (i) => (i / Math.max(1, n - 1)) * W;
  const tx = (t) => (t / (n * chart.step)) * W;
  const lo = -2, hi = 4;
  const y = (v) => top + (1 - (Math.min(hi, Math.max(lo, v)) - lo) / (hi - lo)) * (H - top - bottom);
  const line = (arr) => arr.map((v, i) => `${i ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)}`).join("");
  const area = (arr) => `${line(arr)}L${W},${y(lo)}L0,${y(lo)}Z`;

  const zones = clips
    .map(
      (c) => `<rect class="clip-zone" data-rank="${c.rank}" x="${tx(c.start)}" y="${top}" width="${Math.max(3, tx(c.end) - tx(c.start))}" height="${H - top - bottom}" rx="3"><title>Clip ${c.rank}: ${esc(c.title)}</title></rect>
       <text class="clip-num" x="${tx(c.start) + 4}" y="${top + 13}">${c.rank}</text>`,
    )
    .join("");

  const ticks = [];
  const total = duration || n * chart.step;
  const every = [30, 60, 120, 300, 600, 900, 1800, 3600].find((s) => total / s <= 8) || 7200;
  for (let t = 0; t <= total; t += every) ticks.push(`<text class="axis" x="${tx(t)}" y="${H - 6}" text-anchor="${t ? "middle" : "start"}">${clock(t)}</text>`);

  $("chart").innerHTML = `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="Score over time with picked clips">
    <path d="${area(chart.hype.map((v) => v - 2))}" fill="var(--accent-2)" opacity=".22"/>
    <path d="${line(chart.loudness)}" fill="none" stroke="var(--loud)" stroke-width="1" opacity=".55" vector-effect="non-scaling-stroke"/>
    <path d="${line(chart.score)}" fill="none" stroke="var(--accent)" stroke-width="1.6" vector-effect="non-scaling-stroke"/>
    ${zones}
    ${ticks.join("")}
  </svg>`;

  $("chart").querySelectorAll(".clip-zone").forEach((r) =>
    r.addEventListener("click", () => {
      const card = $(`clip-${r.dataset.rank}`);
      card.scrollIntoView({ behavior: "smooth", block: "center" });
      card.classList.add("flash");
      setTimeout(() => card.classList.remove("flash"), 1500);
    }),
  );
}

/* ---------- queue status ---------- */

async function queueStatus() {
  try {
    const q = await api("/v1/queue");
    const live = q.workers.filter((w) => w.idleMs < 60_000 || w.busy > 0).length;
    $("queue").innerHTML = `<b>${live}</b> worker${live === 1 ? "" : "s"} online · <b>${q.waiting}</b> waiting · <b>${q.inProgress}</b> in progress`;
  } catch {
    $("queue").textContent = "";
  }
}
queueStatus();
setInterval(queueStatus, 10_000);

route();
