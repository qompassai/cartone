//! The rmcp server: tool dispatch over the broker.
//!
//! Dispatch contract:
//!
//! 1. An unknown tool name returns `unknown tool` as an `isError`
//!    result (an execution failure, never a protocol error).
//! 2. Bad arguments (missing fields, an unparseable lane/kind, an
//!    out-of-range limit or units value) return a typed error
//!    object naming the field — before any state is touched.
//! 3. A lane-denied submit is NOT an error result: the denial is
//!    the job's outcome, returned as its ledgered record.
//!
//! Modern era only: the era guard (see `guard`) owns the stdio
//! front door, and `supported_protocol_versions` pins this
//! handler to 2026-07-28.

use std::borrow::Cow;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};

use crate::broker::{Broker, CancelError, SubmitError};
use crate::catalog;
use crate::decide::{JobKind, Lane};
use crate::ledger::JobStatus;

/// Largest `list_jobs` page.
pub const MAX_LIST_LIMIT: i64 = 200;
/// Default `list_jobs` page size.
pub const DEFAULT_LIST_LIMIT: usize = 50;

fn text_result(value: &Value, is_error: bool) -> CallToolResult {
    let content = vec![ContentBlock::text(value.to_string())];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

fn err_result(code: &str, message: &str) -> CallToolResult {
    text_result(&json!({"error": {"code": code, "message": message}}), true)
}

fn arg_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, CallToolResult> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| err_result("invalid_arguments", &format!("{field} must be a string")))
}

fn route_json(route: &crate::config::Route) -> Value {
    json!({
        "name": route.name,
        "kind": route.kind.as_str(),
        "enabled": route.enabled,
        "lanes": route.lanes.iter().map(Lane::as_str).collect::<Vec<_>>(),
        "unit_cost": route.unit_cost,
        "timeout_secs": route.timeout_secs,
        "configured": route.configured(),
        "available": route.available(),
    })
}

/// The dispatch. Async over the shared broker; unit-testable
/// without any transport.
pub async fn call_tool(broker: &Broker, name: &str, args: &Value) -> CallToolResult {
    if catalog::spec_named(name).is_none() {
        return err_result("unknown_tool", "unknown tool");
    }
    match name {
        "submit_job" => submit_job(broker, args).await,
        "get_job" => match arg_str(args, "job_id") {
            Ok(job_id) => match broker.get(job_id).await {
                Some(record) => text_result(&record.to_json(), false),
                None => err_result("not_found", &format!("unknown job {job_id:?}")),
            },
            Err(e) => e,
        },
        "list_jobs" => list_jobs(broker, args).await,
        "cancel_job" => match arg_str(args, "job_id") {
            Ok(job_id) => match broker.cancel(job_id).await {
                Ok(record) => text_result(&record.to_json(), false),
                Err(CancelError::NotFound(_)) => {
                    err_result("not_found", &format!("unknown job {job_id:?}"))
                }
                Err(CancelError::AlreadyTerminal { status, .. }) => err_result(
                    "not_cancellable",
                    &format!(
                        "job {job_id:?} is already {} and cannot be cancelled",
                        status.as_str()
                    ),
                ),
                Err(CancelError::Ledger(e)) => err_result("ledger_error", &e),
            },
            Err(e) => e,
        },
        "list_routes" => {
            let routes: Vec<Value> = broker.routes().all().iter().map(route_json).collect();
            text_result(&json!({"routes": routes}), false)
        }
        "ledger_summary" => {
            let summary = broker.summary().await;
            match serde_json::to_value(&summary) {
                Ok(value) => text_result(&value, false),
                Err(e) => err_result("internal", &format!("summary serialize: {e}")),
            }
        }
        _ => err_result("unknown_tool", "unknown tool"),
    }
}

async fn submit_job(broker: &Broker, args: &Value) -> CallToolResult {
    let route = match arg_str(args, "route") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let lane_text = match arg_str(args, "lane") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let Some(lane) = Lane::parse(lane_text) else {
        return err_result(
            "invalid_lane",
            &format!("lane must be \"sfw\" or \"nsfw\", got {lane_text:?}"),
        );
    };
    let kind_text = match arg_str(args, "kind") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let Some(kind) = JobKind::parse(kind_text) else {
        return err_result(
            "invalid_kind",
            &format!("kind must be \"image\", \"video\", or \"tts\", got {kind_text:?}"),
        );
    };
    let Some(units_raw) = args.get("units").and_then(Value::as_i64) else {
        return err_result("invalid_arguments", "units must be an integer");
    };
    if units_raw < 0 || units_raw > i64::from(u32::MAX) {
        return err_result(
            "invalid_arguments",
            &format!(
                "units must be 1..={}, got {units_raw}",
                crate::broker::MAX_UNITS
            ),
        );
    }
    match broker.submit(route, lane, kind, units_raw as u32).await {
        Ok(record) => text_result(&record.to_json(), false),
        Err(SubmitError::UnknownRoute(name)) => {
            err_result("unknown_route", &format!("unknown route {name:?}"))
        }
        Err(SubmitError::UnitsOutOfRange(units)) => err_result(
            "invalid_arguments",
            &format!(
                "units must be 1..={}, got {units}",
                crate::broker::MAX_UNITS
            ),
        ),
        Err(SubmitError::QueueFull) => err_result("queue_full", "the job queue is full"),
        Err(SubmitError::Ledger(e)) => err_result("ledger_error", &e),
    }
}

async fn list_jobs(broker: &Broker, args: &Value) -> CallToolResult {
    let status = match args.get("status") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_str().and_then(JobStatus::parse) {
            Some(status) => Some(status),
            None => {
                return err_result(
                    "invalid_arguments",
                    "status must be one of queued, running, completed, failed, cancelled, denied",
                );
            }
        },
    };
    let route = match args.get("route") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_str() {
            Some(route) => Some(route),
            None => return err_result("invalid_arguments", "route must be a string"),
        },
    };
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIST_LIMIT,
        Some(value) => match value.as_i64() {
            Some(n) if (1..=MAX_LIST_LIMIT).contains(&n) => n as usize,
            _ => {
                return err_result(
                    "invalid_arguments",
                    &format!("limit must be 1..={MAX_LIST_LIMIT}"),
                );
            }
        },
    };
    let jobs: Vec<Value> = broker
        .list(status, route, limit)
        .await
        .iter()
        .map(|record| record.to_json())
        .collect();
    text_result(&json!({"jobs": jobs}), false)
}

/// The server handler. Dispatch is over the shared broker.
pub struct CartoneServer {
    broker: Broker,
}

impl CartoneServer {
    pub fn new(broker: Broker) -> Self {
        CartoneServer { broker }
    }
}

impl ServerHandler for CartoneServer {
    fn get_info(&self) -> InitializeResult {
        let mut server_info = Implementation::default();
        server_info.name = "cartone".to_string();
        server_info.title = Some("Cartone".to_string());
        server_info.version = env!("CARGO_PKG_VERSION").to_string();
        let mut info = InitializeResult::default();
        info.protocol_version = ProtocolVersion::V_2026_07_28;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: catalog::tool_definitions(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let result = call_tool(&self.broker, &name, &arguments).await;
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Routes;

    fn result_json(result: &CallToolResult) -> Value {
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(&text.text).expect("result text is JSON")
    }

    fn is_error(result: &CallToolResult) -> bool {
        result.is_error.unwrap_or(false)
    }

    fn test_broker(dir: &std::path::Path) -> Broker {
        let routes = Routes::from_toml(
            r#"
[[route]]
name = "true-route"
kind = "image"
enabled = true
lanes = ["sfw", "nsfw"]
command = ["/bin/true"]
unit_cost = 2.0
timeout_secs = 60

[[route]]
name = "hosted-x"
kind = "video"
enabled = false
lanes = ["sfw"]
command = []
unit_cost = 25.0
timeout_secs = 60
"#,
        )
        .expect("routes");
        Broker::open(routes, dir).expect("broker")
    }

    async fn wait_terminal(broker: &Broker, job_id: &str) -> crate::ledger::JobRecord {
        for _ in 0..200 {
            let record = broker.get(job_id).await.expect("job exists");
            if record.status.is_terminal() {
                return record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("job {job_id} never reached a terminal state");
    }

    #[tokio::test]
    async fn lifecycle_submit_run_complete_and_summary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broker = test_broker(dir.path());
        let result = call_tool(
            &broker,
            "submit_job",
            &json!({"route": "true-route", "lane": "sfw", "kind": "image", "units": 3}),
        )
        .await;
        assert!(!is_error(&result), "{result:?}");
        let record = result_json(&result);
        assert_eq!(record["cost"], 6.0);
        let job_id = record["job_id"].as_str().expect("job id").to_string();
        let finished = wait_terminal(&broker, &job_id).await;
        assert_eq!(finished.status, JobStatus::Completed);
        assert_eq!(finished.exit_code, Some(0));

        let summary = result_json(&call_tool(&broker, "ledger_summary", &json!({})).await);
        assert_eq!(summary["totals"]["jobs"], 1);
        assert_eq!(summary["totals"]["completed"], 1);
        assert_eq!(summary["totals"]["total_cost"], 6.0);
        assert_eq!(summary["by_route"]["true-route"]["total_cost"], 6.0);
    }

    #[tokio::test]
    async fn denied_submit_is_a_ledgered_record_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broker = test_broker(dir.path());
        let result = call_tool(
            &broker,
            "submit_job",
            &json!({"route": "hosted-x", "lane": "nsfw", "kind": "video", "units": 1}),
        )
        .await;
        assert!(!is_error(&result), "denial is an outcome: {result:?}");
        let record = result_json(&result);
        assert_eq!(record["status"], "denied");
        assert_eq!(record["cost"], 0.0);
        assert!(
            record["detail"].as_str().expect("detail").contains("nsfw"),
            "{record}"
        );
        // And it is in the ledger fold: a fresh broker over the
        // same directory sees it.
        let broker2 = test_broker(dir.path());
        let jobs = broker2.list(Some(JobStatus::Denied), None, 10).await;
        assert_eq!(jobs.len(), 1);
    }

    #[tokio::test]
    async fn adversarial_arguments_are_typed_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let broker = test_broker(dir.path());
        for (args, code) in [
            (
                json!({"route": "true-route", "lane": "adult", "kind": "image", "units": 1}),
                "invalid_lane",
            ),
            (
                json!({"route": "true-route", "lane": "sfw", "kind": "hologram", "units": 1}),
                "invalid_kind",
            ),
            (
                json!({"route": "ghost", "lane": "sfw", "kind": "image", "units": 1}),
                "unknown_route",
            ),
            (
                json!({"route": "true-route", "lane": "sfw", "kind": "image", "units": 0}),
                "invalid_arguments",
            ),
            (
                json!({"lane": "sfw", "kind": "image", "units": 1}),
                "invalid_arguments",
            ),
        ] {
            let result = call_tool(&broker, "submit_job", &args).await;
            assert!(is_error(&result), "{args}");
            assert_eq!(result_json(&result)["error"]["code"], code, "{args}");
        }
        let result = call_tool(&broker, "nope", &json!({})).await;
        assert_eq!(result_json(&result)["error"]["code"], "unknown_tool");
        let result = call_tool(&broker, "list_jobs", &json!({"limit": 0})).await;
        assert_eq!(result_json(&result)["error"]["code"], "invalid_arguments");
        let result = call_tool(&broker, "get_job", &json!({"job_id": "job-999999"})).await;
        assert_eq!(result_json(&result)["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn cancel_running_job_and_terminal_refusal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let routes = Routes::from_toml(
            "[[route]]\nname = \"slow\"\nkind = \"video\"\nenabled = true\nlanes = [\"sfw\"]\ncommand = [\"/bin/sleep\", \"30\"]\nunit_cost = 1.0\ntimeout_secs = 120\n",
        )
        .expect("routes");
        let broker = Broker::open(routes, dir.path()).expect("broker");
        let record = broker
            .submit("slow", Lane::Sfw, JobKind::Video, 1)
            .await
            .expect("submit");
        // Wait until it is actually running, then cancel.
        for _ in 0..200 {
            if broker.get(&record.job_id).await.expect("job").status == JobStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = call_tool(&broker, "cancel_job", &json!({"job_id": record.job_id})).await;
        let finished = wait_terminal(&broker, &record.job_id).await;
        assert_eq!(finished.status, JobStatus::Cancelled);
        // A terminal job refuses a second cancel, typed.
        let result = call_tool(&broker, "cancel_job", &json!({"job_id": record.job_id})).await;
        assert_eq!(result_json(&result)["error"]["code"], "not_cancellable");
    }

    #[tokio::test]
    async fn unconfigured_route_fails_honestly() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A default local route (wai) is enabled but has no
        // command template wired yet.
        let broker = Broker::open(Routes::defaults(), dir.path()).expect("broker");
        let record = broker
            .submit("wai", Lane::Sfw, JobKind::Image, 1)
            .await
            .expect("submit");
        let finished = wait_terminal(&broker, &record.job_id).await;
        assert_eq!(finished.status, JobStatus::Failed);
        assert!(
            finished
                .detail
                .as_deref()
                .expect("detail")
                .starts_with("route_not_configured"),
            "{finished:?}"
        );
    }
}
