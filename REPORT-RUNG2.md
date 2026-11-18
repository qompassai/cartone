# Cartone — Rung 2 report (2026-11-18)

Rung 2: wire the three local routes to primo's real engine
invocations, prove one real end-to-end job per route through
the MCP, and publish (crates.io done; Registry blocked on a
GPG unlock — see §4). Rung 1's report is `REPORT.md`.

Commits: `2ad7ae4` (wiring) and `250556c` (server.json fix) on
top of rung 1's `d78f9a3`.

## 1. What was wired

The operator route table ships as `routes.primo.toml` (live
copy: `~/.local/share/cartone/routes.toml` on primo). All
templates are direct argv — Cartone's executor runs no shell.
Unit costs are the PLAN §2.3 broker credits (not dollars):
wai 1.0, ltx-video 10.0, kokoro 0.5. Timeouts: 900 s /
1800 s / 900 s.

- **wai** (image, SFW+NSFW): `drivers/wai_txt2img.py` — a thin
  ComfyUI client built from the asset factory's own `af.py`
  (server `http://127.0.0.1:8191`, the `wf_sdxl` recipe:
  `waiIllustriousSDXL_v170.safetensors`, euler_ancestral,
  28 steps, CFG 6.0). If no server answers, the driver starts
  one the estate way (ComfyUI venv python, `main.py`, pinned
  to the RTX 4070 by GPU UUID) and leaves it running; the
  estate's `reap-idle-comfy` timer owns its lifecycle.
- **ltx-video** (video, SFW+NSFW): the img2video skill's
  `drivers/local_i2v.py` + `drivers/i2v_common.py` (shipped
  unmodified from the skill's `bin/`), run with ComfyUI's
  venv python + the asset factory's `pylibs` overlay, in its
  default **sequential CPU-offload** profile, pinned to the
  RTX 4070 by GPU UUID (`GPU-3281fc60-e4c2-da28-9d27-da51a585c7b9`).
  Why not the A6000: the A6000 carries the standing rose
  server (~43.5 of 46 GiB used), and the factory's resident
  driver (`ltx_gen.py`, `pipe.to("cuda")`) cannot fit in what
  remains. The sequential profile is the skill's proven
  2026-10-12 route on this exact 8 GB card.
- **kokoro** (tts, SFW+NSFW): the voice factory's own
  `kokoro_driver.py` under the asset-factory venv python,
  request/response by file, exactly as the torori/voice work
  invokes it. Audio is written as files only — the driver
  never opens a playback device (AUDIO SILENCE rule).

**Standing-input limitation (plain):** route templates carry
fixed benign inputs (prompt, seed, size, voice). Cartone jobs
carry metadata only — route, lane, kind, units — so a job
cannot yet supply its own prompt. Per-job payloads are a
later rung's design question. The LTX route's input still is
`smoke/ltx-input.png` in the data dir, seeded from the wai
route's verified output.

**Concurrency note:** the broker executes each job in its own
spawned task, so jobs *can* run concurrently. Both GPU routes
are pinned to the same card; an operator running parallel GPU
jobs must serialize them client-side or accept contention.
The smoke driver runs strictly serially under the estate
build lock (`flock …/night-locks/build.lock`).

## 2. Smoke evidence (through the MCP itself)

`scripts/smoke_drive.py` spawns the release binary, speaks
newline JSON-RPC (`server/discover`, era 2026-07-28), submits
one job per route, polls to terminal, and verifies each
artifact by existence + size + file magic. Three runs:

| Run | wai | ltx-video | kokoro |
|---|---|---|---|
| 1 (jobs 1–3) | ✅ job-000001 | ❌ job-000002 | ✅ job-000003 |
| 2 (jobs 4–6) | ✅ job-000005 | ❌ job-000004 | ✅ job-000006 |
| 3 (jobs 7–9) | ✅ job-000008 | ✅ job-000007 | ✅ job-000009 |

Final run artifacts (data dir `~/.local/share/cartone/`):

- `artifacts/job-000007.mp4` — 147,535 bytes, `ftyp` magic,
  49 frames, ~100 s wall.
- `artifacts/job-000008.png` — 1,642,963 bytes, PNG magic.
- `artifacts/job-000009/take0.wav` — 170,444 bytes, RIFF
  magic; `result.json` reports hexgrad/Kokoro-82M
  (`kokoro-v1_0.pth`, voice `af_heart`, 24 kHz), weights
  sha256 `496dba118d1a58f5f3db2efc88dbdc216e0483fc89fe6e47ee1f2c53f18ad1e4`.

All three routes are seed-deterministic: every run produced
byte-identical artifact sizes (the MP4 matches the direct
invocation's 147,535 bytes exactly).

Ledger after run 3 (`ledger.jsonl`, append-only): 9 jobs,
7 completed, 2 failed, 34.5 credits total — wai 3/3 completed
(3.0), kokoro 3/3 completed (1.5), ltx-video 1/3 completed
(30.0). Every job's queued → running → terminal transitions
are recorded.

### The two LTX failures — root causes, both fixed curatively

1. **job-000002 — GPU OOM.** The first wiring used the
   factory's resident driver pinned `CUDA_VISIBLE_DEVICES=1`.
   On primo that index does not select the A6000 in this
   Python environment; the job landed on the 4070 (7.67 GiB,
   ~73 MiB free under a ComfyUI process) and died with
   `torch.OutOfMemoryError`. Even correctly pinned, the
   A6000 had only ~2.5 GiB free under the standing rose
   server. Fix: the skill's sequential-offload profile +
   GPU-UUID pinning (UUIDs cannot drift with enumeration
   order). Rose was not stopped or disturbed.
2. **job-000004 — missing `imageio_ffmpeg`.** Generation
   succeeded; only the MP4 write failed: diffusers'
   `export_to_video` requires `imageio` + `imageio_ffmpeg`,
   and no environment on this route had the latter (the
   skill's 2026 venv no longer exists). Fix: `imageio_ffmpeg`
   0.6.0 copied into the asset-factory `pylibs` overlay from
   the longcat-eval venv (pure-python package + static
   ffmpeg 7.0.2 binary); `imageio.plugins.ffmpeg.get_exe()`
   verified resolving before the re-run. The routes file
   header documents this dependency.

Gates re-run after the wiring: `cargo test --locked
--offline` = **31/0** (26 unit + 3 manifest-agreement +
2 stdio), unchanged from rung 1; release build clean.

## 3. crates.io — published and verified

- Account verified before publishing: PhaedrusFlow
  (id 384741) via the connector helper.
- `cargo package --locked --offline` →
  `cartone-0.1.0.crate`, sha256
  `f99bfd5e4a48aa9d7c3a156ba3d00b7a449c471eea4a09258823e1b445276a77`.
  Publish metadata was built from `cargo metadata` with the
  README carried as file **contents** (fleet lesson) and
  authors `["Qompass AI"]` (D-489).
- Publish returned 200, no warnings; `default_version` 0.1.0.
- Three-way verification: owners list = PhaedrusFlow; sparse
  index (`index.crates.io/ca/rt/cartone`) cksum =
  `f99bfd5e…`; the published crate downloaded from
  static.crates.io hashes to the same `f99bfd5e…`.
- **D-1543:** `cargo install cartone --version 0.1.0
  --locked` on primo; the installed binary
  (`~/.cargo/bin/cartone`) answers `cartone 0.1.0` to
  `--version`.

## 4. MCP Registry — validated, publish blocked on unlock

- `mcp-publisher validate server.json` initially failed
  server-side: the description was 137 chars, over the
  Registry's 100-char limit. Shortened to 91 chars
  (commit `250556c`); validate now passes. Tests stayed 31/0.
  (The crates.io package predates this fix; its bundled
  `server.json` still carries the long description — harmless
  there, fixed in the repo for the next version.)
- The publish itself did not run: the GPG agent was cold at
  every check during this rung (warm at 01:19, TTL expired),
  so `pass` cannot decrypt `auth/mcp-registry-qompass-ai`.
  Per the rung brief, the Registry step stops here rather
  than prompting anyone. **Unblocking act (Owner, one
  command on primo):**
  `gpg --pinentry-mode loopback -d ~/.password-store/auth/mcp-registry-qompass-ai.gpg >/dev/null`
  — then login + publish happen in one motion (JWT TTL is
  minutes): `mcp-publisher login dns --domain qompass.ai`,
  `mcp-publisher publish server.json`, and the public
  read-back must show `ai.qompass/cartone` 0.1.0
  isLatest/active.

## 5. Environment changes on primo (for the record)

- `~/.local/share/cartone/` created: `routes.toml`,
  `drivers/` (3 files, also in the repo), `smoke/`
  (kokoro-request.json, ltx-input.png), `artifacts/`,
  `ledger.jsonl`, `logs/`.
- `imageio_ffmpeg` 0.6.0 added to the asset-factory `pylibs`
  overlay (see §2).
- A ComfyUI server on :8191 was started by the wai driver
  and left running under the `reap-idle-comfy` contract, as
  designed. Nothing else was started, stopped, or changed;
  the standing rose server was never touched.
