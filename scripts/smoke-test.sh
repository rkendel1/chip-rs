#!/bin/sh
# Packaged local smoke test: take the release artifact, and prove that the `chip` inside it runs on
# the local machine with no other service.
#
#   scripts/smoke-test.sh dist/chip-<version>-<target>.tar.gz
#
#   1. extract the artifact and verify `chip --version` against its manifest;
#   2. start `chip serve`;
#   3. submit a simple work request;
#   4. a real capability executes (the model is a local stand-in that asks for `project.read`);
#   5. the observation comes back through the service;
#   6. terminate cleanly.
#
# The model is a few lines of Python standing in for an endpoint: there is no live model here, and
# the smoke test says so. PAX must resolve for `chip serve` to start; if the machine has none, a
# stand-in that only identifies itself is used, and the test says so. No test run is claimed then.
set -eu

artifact=${1:?usage: smoke-test.sh <chip release tarball>}
work=$(mktemp -d "${TMPDIR:-/tmp}/chip-smoke.XXXXXX")
pids=""
cleanup() {
  for pid in $pids; do kill "$pid" 2>/dev/null || true; done
  for pid in $pids; do wait "$pid" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT HUP INT TERM
fail() { echo "SMOKE FAILED: $*" >&2; exit 1; }

# 1. The artifact.
if [ -f "$artifact.sha256" ]; then
  (cd "$(dirname "$artifact")" && sha256sum -c "$(basename "$artifact").sha256" >/dev/null) || fail "checksum"
fi
tar -xzf "$artifact" -C "$work"
dir=$(find "$work" -mindepth 1 -maxdepth 1 -type d -name 'chip-*' | head -1)
[ -x "$dir/chip" ] || fail "the artifact has no chip executable"
version=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$dir/manifest.json")
[ "$("$dir/chip" --version)" = "chip $version" ] || fail "chip --version is not 'chip $version'"
echo "chip --version: chip $version"

# A project to work on.
project="$work/project"
mkdir -p "$project/src"
printf 'pub fn one() -> u8 { 1 }\n' > "$project/src/lib.rs"
printf '[package]\nname = "smoke"\nversion = "0.1.0"\nedition = "2021"\n' > "$project/Cargo.toml"

# PAX: the real one if present.
pax_note="real PAX"
if ! pax --version >/dev/null 2>&1; then
  mkdir -p "$work/bin"
  printf '#!/bin/sh\necho "pax 9.9.9"\n' > "$work/bin/pax"
  chmod +x "$work/bin/pax"
  PATH="$work/bin:$PATH"
  export PATH
  pax_note="identity-only PAX stand-in (no tests are run)"
fi

# The model stand-in: asks for project.read once, then stops the work.
cat > "$work/model.py" <<'PY'
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

calls = []

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        calls.append(1)
        if len(calls) == 1:
            reply = {"decision": "request_capability", "capability": "project.read",
                     "inputs": {"path": "src/lib.rs"}}
        else:
            reply = {"decision": "block", "reason": "smoke test complete"}
        body = json.dumps({"id": "smoke-%d" % len(calls),
                           "choices": [{"message": {"role": "assistant", "content": json.dumps(reply)}}],
                           "usage": {"prompt_tokens": 1, "completion_tokens": 1}}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass

server = HTTPServer(("127.0.0.1", 0), Handler)
open(sys.argv[1], "w").write(str(server.server_port))
server.serve_forever()
PY
python3 -I "$work/model.py" "$work/model.port" &
pids="$pids $!"
i=0; while [ ! -s "$work/model.port" ]; do i=$((i+1)); [ $i -gt 100 ] && fail "the model stand-in did not start"; sleep 0.1; done

# 2. chip serve.
(
  cd "$project"
  CHIP_PROVIDER=openai-compatible CHIP_MODEL=smoke-model \
    CHIP_ENDPOINT="http://127.0.0.1:$(cat "$work/model.port")/v1/chat/completions" \
    exec "$dir/chip" serve --port 0 > "$work/serve.out" 2> "$work/serve.err"
) &
serve=$!
pids="$pids $serve"
i=0; while ! grep -q 'listening on http://' "$work/serve.out" 2>/dev/null; do
  i=$((i+1)); [ $i -gt 100 ] && { cat "$work/serve.err" >&2; fail "chip serve did not start"; }
  sleep 0.1
done
addr=$(sed -n 's|.*listening on http://\([^ ]*\).*|\1|p' "$work/serve.out" | head -1)
echo "chip serve: $addr ($pax_note)"
[ "$(curl -fsS "http://$addr/health")" = '{"status":"ok"}' ] || fail "health"

# 3. A simple work request.
id=$(curl -fsS -X POST "http://$addr/v1/work" -H 'content-type: application/json' \
  -d '{"goal":"Read src/lib.rs and report."}' | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["work_id"])')

# 4-5. A real capability executed; its observation came back.
i=0
while :; do
  state=$(curl -fsS "http://$addr/v1/work/$id")
  status=$(printf '%s' "$state" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["status"])')
  case $status in running|queued) ;; *) break ;; esac
  i=$((i+1)); [ $i -gt 300 ] && fail "the work did not finish"
  sleep 0.1
done
curl -fsS "http://$addr/v1/work/$id/events" | python3 -I -c '
import json, sys
kinds = [e["kind"] for e in json.load(sys.stdin)["events"]]
for needed in ("CapabilityRequested", "ExecutionStarted", "ExecutionCompleted", "ObservationRecorded"):
    assert needed in kinds, (needed, kinds)
print("events:", " ".join(kinds))' || fail "the capability did not execute and come back as an observation"
printf '%s' "$state" | python3 -I -c '
import json, sys
s = json.load(sys.stdin)
assert s["status"] == "blocked" and s["result"]["verified"] is False, s
assert s["result"]["audit"]["clean"] is True, s
print("work:", s["status"], "(not verified: this work never claimed the goal)")'

# 6. Terminate cleanly.
kill "$serve"
i=0; while kill -0 "$serve" 2>/dev/null; do i=$((i+1)); [ $i -gt 50 ] && fail "chip serve did not stop"; sleep 0.1; done
echo "SMOKE PASSED"
