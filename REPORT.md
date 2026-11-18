# Cartone — Rung 1 report (2026-11-18)

**Repo:** https://github.com/qompassai/cartone (public, created
2026-11-18 via the GitHub API under the qompassai org; identity
verified beforehand: `GET /user` = PhaedrusFlow).
**Local tree:** primo `~/.GH/Qompass/cartone`, branch `main`.
**Remote head == local head:**
`d697a015ff134ec0015c3ce029cfafd19b4cfa88` (read back
independently via `GET /repos/qompassai/cartone/git/refs/heads/main`
after the push).

## What was built

Crate `cartone` 0.1.0 (binary `cartone`), Rust edition 2024,
rmcp pinned `=3.8.0` (fleet D-113), modern MCP era only
(2026-07-28), `unsafe_code` forbidden.

- **Route table** (`src/config.rs`) — TOML, fail-closed
  validation (duplicate names, empty lane sets, negative costs,
  out-of-bound timeouts all refuse). Built-in table per PLAN
  §2.3: local `wai` (image), `ltx-video` (video), `kokoro`
  (tts), enabled, both lanes; hosted `runway`, `hailuo`, `fal`,
  `grok`, disabled and SFW-only until configured. A first run
  writes the default table to `<data dir>/routes.toml`.
- **decide() gate** (`src/decide.rs`) — pure
  (route, lane, kind) → Allow/Deny with named codes
  (`kind_mismatch`, `lane_not_eligible`, `route_disabled`, in
  that check order). Denied jobs are ledgered at cost 0 with the
  gate's reason.
- **Ledger** (`src/ledger.rs`) — append-only JSONL
  (`ledger.jsonl` in `CARTONE_DATA_DIR`, default
  `~/.local/share/cartone`). Records carry metadata only (job
  id, route, lane, kind, units, unit cost, cost, status,
  timestamps, exit code, detail) — there is no field that could
  hold prompt text or output content. Load folds last-record-
  per-job and counts torn lines. Jobs left queued/running by a
  dead process are marked `failed / interrupted_by_restart` on
  open.
- **Queue + executor** (`src/broker.rs`) — bounded
  (10,000 jobs, 10,000 units/job, per-route timeout ≤ 3600 s).
  Execution is direct argv exec of the route's command template
  (never a shell), with `{job_id}` / `{route}` / `{kind}` /
  `{lane}` / `{units}` substitution, output discarded, cancel
  polled every 50 ms so running jobs can be killed.
- **MCP surface** (`src/server.rs`, `src/catalog.rs`,
  `src/guard.rs`) — six tools: `submit_job`, `get_job`,
  `list_jobs`, `cancel_job`, `list_routes`, `ledger_summary`.
  `server/discover` answers name `cartone`, title `Cartone`,
  version `0.1.0`; the byte-level era guard refuses legacy
  `initialize` with the typed `-32022` error and owns framing
  (4 MiB line bound, `-32700` / `-32600` / `-32601` shapes).
- **Manifests** — `server.json` (name `ai.qompass/cartone`,
  version 0.1.0, cargo package `cartone`, stdio) and
  `.well-known/agent-card.json` per the fleet profile
  (D-1605/D-1607: one interface under the
  `https://qompass.ai/extensions/mcp-interface/v1` custom
  binding, invocation facts in the extension params, no A2A
  endpoint claimed). README carries the marker
  `mcp-name: ai.qompass/cartone` — the form verified from landed
  siblings (lumen, tackle, bloch, broadside, seahawk all use
  `ai.qompass/<name>`; the brief's initial `io.github.…` guess
  was superseded by that evidence).

## Gates (primo, under the build lock, `--locked --offline`)

- `cargo test` — **31 passed / 0 failed**: 26 unit (decide
  allow+deny paths, config validation incl. adversarial tables,
  ledger fold/torn-line/summary accounting, substitution,
  catalog shape, guard classification, dispatch lifecycle:
  submit→run→complete on a `/bin/true` route, denied-submit
  ledgering across reopen, typed argument errors, cancel of a
  running `/bin/sleep` job, unconfigured-route honest failure),
  3 manifest-agreement, 2 stdio (identity + tool list +
  `/bin/true` lifecycle + wire-level denial + summary over the
  real binary; legacy `initialize` refusal). Run logs:
  `/tmp/cartone-test3.log` on primo.
- `cargo clippy --all-targets -- -D warnings` — **rc 0**
  (`/tmp/cartone-clippy2.log`).
- `cargo fmt --check` — **rc 0**.
- Agent card: **VALID** against the official A2A `AgentCard`
  schema (local copy of https://a2a-protocol.org/latest/spec/a2a.json);
  the positive control (emptied `supportedInterfaces` +
  `stateTransitionHistory` added) was **rejected** (1 schema
  error + structural catches), per the local-agent-cards skill.
- `cartone --version` → `cartone 0.1.0`.
- Skill-trust (D-1068): `local-agent-cards` recorded as
  success / independent verification.

## Push notes (deviations from the straight line, honestly)

GitHub's Git Data API returns 409 "Git Repository is empty" for
every git endpoint on a brand-new repo, so `api_push_branch.py`
could not push into the empty repo directly. Following the
tackle precedent: the repo was seeded through the Contents API
with the byte-identical local README (seed commit
`9b5b11ccbbb97002a6449ace25ebb6a707ffa087`), merged locally with
`--allow-unrelated-histories` (clean merge, empty tree diff),
and the chain was then pushed with `api_push_branch.py`
(base = seed, no force). The three rung-1 commits were recreated
byte-identically by the script; the merge commit's tree step
hit a 422 (its diff vs first parent is empty — its tree already
existed remotely as its first parent's tree), so that one commit
object was created directly through the same API route with the
local commit's exact fields and came out byte-identical
(`d697a01…`). The ref was advanced fast-forward only.

## NOT wired yet

- **Local engine command templates**: `wai` / `ltx-video` /
  `kokoro` ship with empty `command` — jobs for them fail at run
  time with a `route_not_configured` detail and `list_routes`
  reports `configured: false`. Wiring each engine's real
  invocation on primo is operator configuration (a routes.toml
  edit), deliberately not invented here.
- **Hosted routes** (`runway`, `hailuo`, `fal`, `grok`):
  disabled, SFW-only, no keys, no adapters. Reported
  unavailable, never faked.
- **Job payloads**: submit carries metadata only (route, lane,
  kind, units). Carrying generation inputs (prompt references,
  input files) is a later rung's design question.
- **TUI** (fleet TUI programme), **client integrations**
  (phlow consuming the broker per D4, generated client configs),
  **crates.io / MCP Registry publication** — all follow-ons.
- The tree is left built: `target/` is 609 MB on primo; the
  source tree excluding `target/` is 428 KB.
