# BRAIN-API: OpenJev decision server (for snifrig)

Measured 2026-10-09 on beastly. Read-only plus test requests.

## Service
- Base URL: `http://127.0.0.1:8791` (podman container `open-jev`, image `localhost/open-jev:9b`, WSL mirrored networking).
- Auth: none. Plain HTTP, no keys. Requests with an `Origin` header whose host differs from `Host` get 403, so call from Rust (reqwest) without an Origin header.
- Model: Open-Jev-9B = LoRA (rank 8) + scalar decision head on `Qwen/Qwen3.5-9B` (bf16). Upstream: github.com/Zefan-Cai/Open-Jev, weights ZefanCai/Open-Jev-9B (apache-2.0). It is non-generative: it scores candidates and returns probabilities, never text.
- Server source: `/app/jev/server.py`, `/app/jev/api.py` in the container (stdlib ThreadingHTTPServer; one request at a time, global lock).

## Endpoints
| Method | Path | Notes |
|---|---|---|
| GET | `/health` | `{"status":"ready","model":"Qwen/Qwen3.5-9B","method":"lora_decision_head"}` (port only binds after model load) |
| GET | `/v1/models` | model id, aliases `open-jev`, `jev-latest` |
| POST | `/v1/systemone` | main decision call. Same handler also at `/v1/inference` and `/api/jev` |
| GET | `/examples.json`, `/examples/...` | static workbench, ignore |

## Request (POST, `Content-Type: application/json`)
```json
{"state": "<string | object | array>",
 "questions": { "<id>": {"type": "noul|choice|score", "instructions": "<text|object|array>", "criteria": ...} }}
```
- `noul` (yes/no): `criteria` optional, `{"true": "...", "false": "..."}` descriptions only.
- `choice`: `criteria` = object `{name: description-or-null}`, 1 to 255 candidates.
- `score`: `criteria` = array of 2 to 10 ordinal level descriptions (index 0 is lowest).
- Multiple questions in one call share the state; question ids are arbitrary strings.
- Duplicate JSON keys and NaN are rejected.

## Response
```json
{"answers": {"<id>": ...}, "model": "Qwen/Qwen3.5-9B", "usage": {"input_tokens": N, "output_tokens": 0},
 "metadata": {"inference_seconds": 1.05, "temperature": 1.8969, "candidate_sequences": N, "...": "..."}}
```
- noul: `{"type":"noul","noul":P(yes)}`
- choice: `{"type":"choice","choice":"<name>","probabilities":{name:p,...},"confidence":c}`
- score: `{"type":"score","score":expected_level,"probabilities":{"0":p,...},"confidence":c,"legend":{"0":"...",...}}`

## Real examples
State used for all (JSON-escaped in requests):
`Windows PC, 16 cores, total CPU 94% for 3 minutes. Foreground app: comet.exe (browser). Processes by CPU (% of one core): ffmpeg.exe 457 (child of python build-playout.py, decoding X:\NATV-production\completed\e11.mp4 with -f null), System 111, vmmemWSL 79, comet.exe 74, svchost 63, dwm.exe 61, MsMpEng 51, claude.exe 47, OBSBOT_Main 35. Mouse lag reported by user.`

### (a) yes/no
Request questions: `{"q1":{"type":"noul","instructions":"Is ffmpeg.exe the main cause of the lag?"}}`
Response: `{"answers":{"q1":{"type":"noul","noul":0.1338}}, "usage":{"input_tokens":174,...}}`
Latency ms: 1666, 1104, 1059 (median 1104).

### (b) choice
Request questions: `{"q1":{"type":"choice","instructions":"Which process should be lowered in priority first?","criteria":{"ffmpeg.exe":null,"comet.exe":null,"dwm.exe":null,"MsMpEng.exe":null,"none":null}}}`
Response: `{"answers":{"q1":{"type":"choice","choice":"comet.exe","probabilities":{"ffmpeg.exe":0.191,"comet.exe":0.297,"dwm.exe":0.198,"MsMpEng.exe":0.186,"none":0.128},"confidence":0.121}}}`
Latency ms: 2001, 1918, 1869 (median 1918). 892 input tokens.

### (c) score
Request questions: `{"q1":{"type":"score","instructions":"How important is ffmpeg.exe's work to the user right now?","criteria":["0: irrelevant, can be killed","1: low, background batch job","2: moderate","3: high, user is waiting on it","4: critical, must not be interrupted"]}}`
Response: `{"answers":{"q1":{"type":"score","score":1.189,"probabilities":{"0":0.265,"1":0.478,"2":0.121,"3":0.074,"4":0.061},"confidence":0.400,"legend":{...}}}}`
Latency ms: 1686, 1760, 1987 (median 1760). 939 input tokens.

### (d) all three in one call
Request questions: `{"is_cause":<a>,"lower_first":<b>,"importance":<c>}`
Response: is_cause noul 0.131; lower_first comet.exe (conf 0.118, ffmpeg.exe 0.189); importance score 1.196 (conf 0.399).
Latency ms: 4441, 4378, 4444 (median 4444). Batching does not save time: cost is about the sum of the parts (about 4.4 s vs 1.1+1.9+1.8).
Answers matched the standalone calls within 0.01.

## Quality notes
- Probabilities are calibrated (temperature 1.9) and therefore flat. The choice answer was low confidence (0.12) and picked comet.exe, which looks wrong: ffmpeg.exe at 457% is the obvious culprit, and the yes/no gave only 13% to "ffmpeg is the main cause". Score (ffmpeg importance about 1.2, "low background batch") was sensible.
- The model does not do arithmetic reasoning over the numbers in the state; treat it as a weak prior on semantic questions, not as a replacement for rules like "highest CPU child of a batch job and not foreground". Use it for judgments such as "is this process user-interactive or background batch" rather than "which is causing lag".
- Candidate names are visible to the model, so put meaning in the names or descriptions. Choice candidates are scored independently.

## Limits
- Request body max 4 MiB (413 above); `Content-Length` required (no chunked).
- `JEV_MAX_LENGTH=4096` tokens per prompt. Keep state well under about 3000 tokens. State was about 170 tokens.
- Choice 1 to 255 candidates, score 2 to 10 levels. Cost scales with candidates times prompt (each candidate is a sequence): 1 sequence 1.1 s, 5 sequences 1.9 s, 11 sequences 4.4 s. Request-local prefix cache is on.
- Server is serialized by a lock: concurrent calls queue. Socket timeout 30 s on the handler.

## Errors
- 422 `{"error": "..."}` bad request (empty questions, unknown type, bad criteria).
- 415 wrong Content-Type; 413 body size; 404 unknown path/method (GET on `/v1/systemone`); 403 cross-origin; 500 `{"error":"model inference failed","error_type":...}` (never a fake answer).
- Server down or still loading: the port is not bound until the model is loaded, so the client gets connection refused (TCP RST), not an HTTP error. snifrig should treat connect error as "brain unavailable" and fall back to rules; use a short connect timeout and about 15 s request timeout. Poll `GET /health`. (Down behavior inferred from entrypoint and code; the container was not stopped.)

## Resource cost
- Container RAM: about 650 MB (podman stats, limit 10.4 GB). CPU about 3% average, near 0 idle.
- VRAM: per-process figure is N/A under Windows WDDM. RTX 3090 total used was 14.6 GB then 17.8 GB (other apps vary, llama-server also on GPU). Estimated Open-Jev share about 13 to 15 GB: Qwen3.5-9B bf16 backbone on cuda:0, with embeddings, lm_head and vision tower on CPU (`JEV_DEVICE_MAP`). Env: batch size 4, prefill chunk 256, prefix cache on.
- Weights on disk: 19 GB base in image plus 25 MB adapter.
- GPU contention: inference takes the GPU for 1 to 4 s per call; avoid calling more often than every few seconds, and not at all while the user is gaming or rendering.

## Rust sketch
POST `http://127.0.0.1:8791/v1/systemone` with `reqwest::blocking::Client` (timeout 15 s, `.json(&body)`), parse `answers.<id>.noul` / `.choice` / `.score`.
