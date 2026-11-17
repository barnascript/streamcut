#!/usr/bin/env bash
# End-to-end check against a running stack (docker compose or kind).
# Makes a 90 s test video that is quiet except for a loud burst at 40-46 s,
# uploads it, waits for the clips, and checks that clip 1 covers the burst.
#
#   scripts/e2e.sh [http://localhost:8080]
set -euo pipefail
API="${1:-http://localhost:8080}"
FFMPEG="${FFMPEG:-ffmpeg}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "waiting for $API"
for _ in $(seq 1 90); do curl -sf "$API/readyz" >/dev/null && break; sleep 2; done
curl -sf "$API/readyz" >/dev/null || { echo "API never became ready"; exit 1; }

"$FFMPEG" -loglevel error -y \
  -f lavfi -i "testsrc2=size=640x360:rate=25:duration=90" \
  -f lavfi -i "anoisesrc=color=pink:amplitude=0.02:duration=90" \
  -f lavfi -i "sine=frequency=440:duration=90" \
  -filter_complex "[2:a]volume='if(between(t,40,46),0.8,0)':eval=frame[burst];[1:a][burst]amix=inputs=2:normalize=0[a]" \
  -map 0:v -map "[a]" -c:v libx264 -preset ultrafast -c:a aac -shortest "$WORK/test.mp4"

JOB=$(curl -sf -X POST "$API/v1/jobs" -H "Idempotency-Key: e2e-$(date +%s)" \
  -F clips=2 -F minLen=10 -F maxLen=30 -F file=@"$WORK/test.mp4" | python3 -c 'import sys,json; print(json.load(sys.stdin)["job"]["id"])')
echo "job $JOB"

for _ in $(seq 1 120); do
  STATUS=$(curl -sf "$API/v1/jobs/$JOB" | python3 -c 'import sys,json; j=json.load(sys.stdin)["job"]; print(j["status"], j["progress"], j["message"])')
  echo "  $STATUS"
  case "$STATUS" in done*|failed*) break ;; esac
  sleep 3
done

curl -sf "$API/v1/jobs/$JOB" > "$WORK/job.json"
python3 - "$WORK/job.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
assert d["job"]["status"] == "done", d["job"]
clips = d["clips"]
assert len(clips) >= 1, "no clips"
top = clips[0]
print(f"clip 1: {top['start']:.1f}-{top['end']:.1f} s, reasons: {top['reasons']}")
assert top["start"] <= 40 and top["end"] >= 46, "clip 1 should cover the loud burst at 40-46 s"
assert top["width"] == 1080 and top["height"] == 1920
PY

CODE=$(curl -s -o /dev/null -w '%{http_code}' -H 'Range: bytes=0-99' "$API/v1/jobs/$JOB/clips/1/video")
[ "$CODE" = "206" ] || { echo "range request gave $CODE"; exit 1; }
echo "e2e passed"
