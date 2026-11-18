//! cartone — the estate's generation broker (PLAN §2.3, D4/D6/D7).
//!
//! One thin MCP server fronts every generation route the estate
//! uses — local image / video / TTS engines on primo today, hosted
//! routes where licensed tomorrow — and gives every client (rose,
//! phlow, meister, ardor) the same three things:
//!
//! - a **job queue**: jobs are submitted, executed by the route's
//!   configured command template, and tracked to a terminal state;
//! - a **per-job cost ledger**: every job — allowed, denied,
//!   completed, failed, cancelled — writes a metadata-only record
//!   (job id, route, lane, units, cost, status, timestamps; never
//!   prompt text or output content) to an append-only JSONL store;
//! - a **lane-eligibility gate**: a pure `decide()` (see
//!   [`decide`]) refuses a job whose lane is not eligible for its
//!   route at submit time, and the refusal itself is ledgered.
//!
//! The library holds all logic; the binary is transport wiring.

pub mod broker;
pub mod catalog;
pub mod config;
pub mod decide;
pub mod guard;
pub mod ledger;
pub mod server;
