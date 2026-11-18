# cartone

mcp-name: ai.qompass/cartone

**The broker holds the cartone; the routes execute from it.** A
thin generation-broker MCP server, written in Rust (edition 2024,
`rmcp` 3.8.0, modern protocol era 2026-07-28 only): one server
fronts every generation route the estate uses — image, video,
and TTS — and gives every client (rose, phlow, meister, ardor)
the same job queue, the same per-job cost ledger, and the same
lane-eligibility gate. It is the estate form of the broker shape
in the Artlist study (PLAN §2.3; decisions D4/D6/D7 in
`~/workspace/artlist-blender-study-20261117/PLAN.md`).

The name: a *cartone* is the full-scale preparatory drawing from
which a fresco or tapestry was executed — and the origin of the
word "cartoon."

## What it does

- **Job queue** — `submit_job` queues a job (route, lane, kind,
  units — metadata only, never prompt text); the route's
  configured command template executes it as a subprocess with a
  bounded timeout; `get_job` / `list_jobs` track it; `cancel_job`
  cancels a queued or running job.
- **Cost ledger** — every job writes its full record to an
  append-only JSONL ledger (`ledger.jsonl` in the data
  directory): job id, route, lane, kind, units, unit cost, cost,
  status, timestamps, exit code. There is deliberately no field
  that could hold prompt text or output content. `ledger_summary`
  aggregates totals overall, per route, and per lane — budgets
  bind jobs, not models.
- **Lane gate** — a pure `decide()` runs at submit: kind
  mismatch, lane ineligibility, and disabled routes are denied
  with named reasons, and the denial itself is ledgered at cost
  0. Lane eligibility comes from each route's own terms (hosted
  terms that bar adult content make a route SFW-only), enforced
  by the route table — never by prompt wording.

## Routes

The route table is TOML (`routes.toml` in the data directory,
or `CARTONE_ROUTES_FILE`); a first run writes the built-in table
there. Built-in routes:

| Route | Kind | Enabled | Lanes | Notes |
|---|---|---|---|---|
| `wai` | image | yes | sfw, nsfw | local; command template not yet wired |
| `ltx-video` | video | yes | sfw, nsfw | local; command template not yet wired |
| `kokoro` | tts | yes | sfw, nsfw | local; command template not yet wired |
| `runway` | video | no | sfw | hosted; disabled until configured |
| `hailuo` | video | no | sfw | hosted; disabled until configured |
| `fal` | image | no | sfw | hosted; disabled until configured |
| `grok` | video | no | sfw | hosted; disabled until configured |

A route's `command` is an argv template executed directly (never
through a shell), with `{job_id}`, `{route}`, `{kind}`, `{lane}`,
and `{units}` substituted per argument. A route with no command
is honestly unwired: `list_routes` reports `configured: false`,
and its jobs fail at run time with a `route_not_configured`
detail rather than pretending to generate.

## Configuration

- `CARTONE_DATA_DIR` — data directory (default
  `~/.local/share/cartone`); holds `ledger.jsonl` and, by
  default, `routes.toml`.
- `CARTONE_ROUTES_FILE` — route table path (default
  `<data dir>/routes.toml`).

Startup is fail-closed: an unresolvable data directory or an
invalid route table refuses to start, naming the problem.

## Tools

`submit_job`, `get_job`, `list_jobs`, `cancel_job`,
`list_routes`, `ledger_summary`. Sessions open with the modern
`server/discover` handshake; legacy `initialize` contact is
refused at the stdio front door with the typed `-32022`
unsupported-version error.

## Status

Rung 1 (2026-11-18): the broker core is real — queue, ledger,
gate, executor, MCP surface, agent card + `server.json`. Not yet
wired: the local engines' command templates (operator
configuration), hosted route enablement (terms + keys), a TUI,
and client integrations (phlow consuming the broker as a client
is the D4 follow-on).
