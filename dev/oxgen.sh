#!/usr/bin/env bash
# oxgen.sh — offload a single-file code-gen to a local ollama cloud model, strip fences,
# write to OUT_FILE, and log token usage. Used to keep Claude/Anthropic usage low: the
# heavy generation runs on ollama cloud models (glm-5.2:cloud, kimi-k2.7-code:cloud) while
# the human/Claude authors the precise spec + authoritative test vectors and runs the
# `cargo` gate (fmt/clippy -D/test) which is the objective verifier.
#
# Usage: dev/oxgen.sh <model> <prompt_file> <out_file> [think]
#   model      e.g. glm-5.2:cloud | kimi-k2.7-code:cloud | gpt-oss:20b-local
#   prompt_file a spec file (see dev/README.md for the spec format that has worked well)
#   out_file    where to write the generated code
#   think       true|false (default false; kimi think=true is slow, prefer false + background)
#
# Requires: `ollama serve` running locally (REST API on :11434) and the cloud models pulled.
set -euo pipefail
model="$1"; pf="$2"; out="$3"; think="${4:-false}"
python3 - "$model" "$pf" "$out" "$think" <<'PY'
import json, sys, re, time, urllib.request, os
model, pf, out, think = sys.argv[1:5]
prompt = open(pf).read()
payload = json.dumps({"model": model, "prompt": prompt, "stream": False,
                      "think": think == "true", "options": {"temperature": 0.15}}).encode()
req = urllib.request.Request("http://localhost:11434/api/generate", data=payload,
                             headers={"Content-Type": "application/json"})
t0 = time.time()
d = json.load(urllib.request.urlopen(req, timeout=600))
dt = time.time() - t0
r = d.get("response", "")
m = re.search(r"```(?:rust)?\s*\n(.*?)```", r, re.S)
code = m.group(1) if m else r
open(out, "w").write(code)
pt, ot = d.get("prompt_eval_count", 0), d.get("eval_count", 0)
with open(os.path.join(os.path.dirname(out) or ".", ".cloud-usage.tsv"), "a") as f:
    f.write(f"{int(time.time())}\t{model}\t{pt}\t{ot}\t{out}\n")
print(f"[{model}] prompt={pt} out={ot} time={dt:.1f}s -> {out} ({len(code)} bytes)")
PY
