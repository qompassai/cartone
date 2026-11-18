//! The tool catalog: cartone's whole MCP surface, declared once
//! with complete input schemas and behaviour annotations, so
//! `tools/list` and dispatch can never drift apart.

use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{Map, Value, json};

/// One catalog entry: everything discovery and dispatch need.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input: Value,
    pub annotations: ToolAnnotations,
}

fn s() -> Value {
    json!({"type": "string"})
}

fn s_enum(values: &[&str]) -> Value {
    json!({"type": "string", "enum": values})
}

fn integer() -> Value {
    json!({"type": "integer"})
}

fn obj_schema(props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut map = Map::new();
    for (name, schema) in props {
        map.insert((*name).to_string(), schema.clone());
    }
    let mut schema = json!({"type": "object", "properties": Value::Object(map)});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

fn read_ann() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(true)
        .destructive(false)
        .idempotent(true)
        .open_world(false)
}

fn write_ann() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .destructive(false)
        .idempotent(false)
        .open_world(false)
}

/// The six broker tools. `lane` and `kind` are plain strings in
/// the schemas on purpose: the broker's own validation owns
/// classification, so a bad value reaches dispatch and is refused
/// with a typed error instead of failing as a protocol error.
pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "submit_job",
            description: "Submit a generation job to a route. The lane-eligibility gate decides at submit: a denied job is returned as a ledgered record with status \"denied\" and the gate's reason; an allowed job is queued and executed by the route's configured command template. Jobs carry metadata only (route, lane, kind, units) — never prompt text.",
            input: obj_schema(
                &[
                    ("route", s()),
                    ("lane", s_enum(&["sfw", "nsfw"])),
                    ("kind", s_enum(&["image", "video", "tts"])),
                    ("units", integer()),
                ],
                &["route", "lane", "kind", "units"],
            ),
            annotations: write_ann(),
        },
        ToolSpec {
            name: "get_job",
            description: "Return one job's current ledger record by job id.",
            input: obj_schema(&[("job_id", s())], &["job_id"]),
            annotations: read_ann(),
        },
        ToolSpec {
            name: "list_jobs",
            description: "List jobs in submission order, optionally filtered by status and/or route; limit (1..=200, default 50) caps how many of the newest matches are returned.",
            input: obj_schema(
                &[
                    (
                        "status",
                        s_enum(&[
                            "queued",
                            "running",
                            "completed",
                            "failed",
                            "cancelled",
                            "denied",
                        ]),
                    ),
                    ("route", s()),
                    ("limit", integer()),
                ],
                &[],
            ),
            annotations: read_ann(),
        },
        ToolSpec {
            name: "cancel_job",
            description: "Cancel a queued or running job by job id. A terminal job cannot be cancelled.",
            input: obj_schema(&[("job_id", s())], &["job_id"]),
            annotations: ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(false)
                .open_world(false),
        },
        ToolSpec {
            name: "list_routes",
            description: "List the route table: each route's kind, enablement, eligible lanes, unit cost, whether an executor command is configured, and whether it is available to take a job now. Disabled or unconfigured hosted routes are reported honestly as unavailable.",
            input: obj_schema(&[], &[]),
            annotations: read_ann(),
        },
        ToolSpec {
            name: "ledger_summary",
            description: "Ledger accounting over current job states: totals overall, per route, and per lane (job counts by status and total cost).",
            input: obj_schema(&[], &[]),
            annotations: read_ann(),
        },
    ]
}

/// The served tool definitions for `tools/list`.
pub fn tool_definitions() -> Vec<Tool> {
    specs()
        .into_iter()
        .map(|spec| {
            let Value::Object(input_map) = spec.input else {
                unreachable!("catalog input schemas are objects");
            };
            Tool::new(spec.name, spec.description, input_map).with_annotations(spec.annotations)
        })
        .collect()
}

/// Look up one catalog entry by wire name.
pub fn spec_named(name: &str) -> Option<ToolSpec> {
    specs().into_iter().find(|spec| spec.name == name)
}

/// The tool names, in catalog order (used by the smoke gate).
pub fn tool_names() -> Vec<&'static str> {
    specs().iter().map(|spec| spec.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_the_six_broker_tools() {
        assert_eq!(
            tool_names(),
            [
                "submit_job",
                "get_job",
                "list_jobs",
                "cancel_job",
                "list_routes",
                "ledger_summary"
            ]
        );
        for spec in specs() {
            assert!(spec.input["properties"].is_object(), "{}", spec.name);
        }
        let submit = spec_named("submit_job").expect("submit_job");
        assert_eq!(
            submit.input["required"],
            json!(["route", "lane", "kind", "units"])
        );
    }
}
